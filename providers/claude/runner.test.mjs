// Contract tests for runner.mjs. Every test drives the real runner CLI as a
// subprocess against fixtures/fake-claude.mjs; no real Claude calls are made.
// Scratch files live under ./.test-tmp so writes stay inside this directory.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const RUNNER = path.join(HERE, 'runner.mjs');
const FAKE = path.join(HERE, 'fixtures', 'fake-claude.mjs');
const SCRATCH = path.join(HERE, '.test-tmp', `${Date.now()}-${process.pid}`);
const LOCKS = path.join(SCRATCH, 'locks');
fs.mkdirSync(LOCKS, { recursive: true });

const sleep = ms => new Promise(r => setTimeout(r, ms));
const readJson = p => JSON.parse(fs.readFileSync(p, 'utf8'));
const exists = p => fs.existsSync(p);
const pidAlive = pid => { try { process.kill(pid, 0); return true; } catch (e) { return e.code === 'EPERM'; } };

async function waitFor(fn, ms = 10_000, what = 'condition') {
  const end = Date.now() + ms;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(20);
  }
}

// Start a runner subcommand; resolves {code, lines, last, ms}.
function cli(args) {
  const t0 = Date.now();
  const proc = spawn(process.execPath, [RUNNER, ...args], { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
  let stdout = '', stderr = '';
  proc.stdout.setEncoding('utf8').on('data', d => { stdout += d; });
  proc.stderr.setEncoding('utf8').on('data', d => { stderr += d; });
  const done = new Promise(resolve => proc.on('close', code => {
    const lines = stdout.split('\n').filter(Boolean).map(l => JSON.parse(l));
    resolve({ code, lines, last: lines.at(-1), stderr, ms: Date.now() - t0 });
  }));
  return { proc, done, get stdout() { return stdout; } };
}
const run1 = args => cli(args).done;

function setup(name, scenario = {}) {
  const base = path.join(SCRATCH, `${name}-${randomUUID().slice(0, 8)}`);
  const cwd = path.join(base, 'work');
  fs.mkdirSync(cwd, { recursive: true });
  const argsLog = path.join(base, 'fake-args.jsonl');
  const scenarioFile = path.join(base, 'scenario.json');
  fs.writeFileSync(scenarioFile, JSON.stringify({ argsLog, ...scenario }));
  let n = 0;
  return {
    base, cwd, argsLog, scenarioFile,
    runDir: (label = `run${++n}`) => path.join(base, 'runs', label),
    task(text) { const p = path.join(base, `task-${randomUUID().slice(0, 8)}.txt`); fs.writeFileSync(p, text); return p; },
    fake: ['--claude-bin', process.execPath, '--claude-prefix-arg', FAKE, '--claude-prefix-arg', scenarioFile, '--lock-dir', LOCKS],
    invocations: () => exists(argsLog) ? fs.readFileSync(argsLog, 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l)) : [],
  };
}

function startRun(ctx, runDir, text, extra = []) {
  return cli(['run', '--run-dir', runDir, '--cwd', ctx.cwd, '--task', ctx.task(text), ...ctx.fake, ...extra]);
}

const events = runDir => fs.readFileSync(path.join(runDir, 'events.jsonl'), 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l));
const completion = runDir => readJson(path.join(runDir, 'completion.json'));
const failureCodes = runDir => completion(runDir).failures.map(f => f.code);
const steer = (runDir, ctx, text, extra = []) => run1(['steer', '--run-dir', runDir, '--task', ctx.task(text), ...extra]);
const stateOf = runDir => exists(path.join(runDir, 'state.json')) ? readJson(path.join(runDir, 'state.json')) : null;

test('explicit model selection supports Fable, Opus and future IDs; resume preserves the model', async () => {
  for (const model of ['claude-fable-5-2', 'claude-opus-5-5', 'claude-future-family-9-1']) {
    const ctx = setup(`model-${model}`);
    const first = ctx.runDir('first');
    const result = await startRun(ctx, first, 'WRITE model.txt selected\nSAY done', ['--model', model, '--expect', 'model.txt']).done;
    assert.equal(result.code, 0, JSON.stringify(result.lines));
    const intent = readJson(path.join(first, 'intent.json'));
    assert.equal(intent.model, model);
    assert.equal(intent.command[intent.command.indexOf('--model') + 1], model);
    assert.equal(completion(first).model, model);
    assert.equal(stateOf(first).init.model, model);
    const next = ctx.runDir('next');
    const resumed = await run1(['run', '--run-dir', next, '--resume-from', first, '--task', ctx.task('SAY resumed')]);
    assert.equal(resumed.code, 0, JSON.stringify(resumed.lines));
    assert.equal(completion(next).model, model);
    assert.equal(readJson(path.join(next, 'intent.json')).sessionId, intent.sessionId);
    const refused = await run1(['run', '--run-dir', ctx.runDir('wrong-model'), '--resume-from', next,
      '--model', 'claude-other-model-1', '--task', ctx.task('SAY must not run')]);
    assert.equal(refused.code, 2);
    assert.match(refused.last.error, /resume inherits model/);
  }
});

test('Fable selection refuses initialization or assistant substitution', async () => {
  for (const fault of [{ model: 'claude-opus-5-5' }, { assistantModel: 'claude-opus-5-5' }]) {
    const ctx = setup('fable-substitution', fault);
    const dir = ctx.runDir();
    const result = await startRun(ctx, dir, 'SAY done', ['--model', 'claude-fable-5-2']).done;
    assert.equal(result.code, 1);
    assert.ok(failureCodes(dir).includes('model-mismatch'));
  }
});

test('successful lifecycle: intent before dispatch, filtered auth, artifacts, UTF-8 framing, pending review', async () => {
  const ctx = setup('success', { splitUtf8: true });
  const runDir = ctx.runDir();
  const r = await startRun(ctx, runDir, 'WRITE out/result.txt hello\nSAY héllo 世界', ['--expect', 'out/result.txt']).done;
  assert.equal(r.code, 0, JSON.stringify(r.lines));
  const kinds = r.lines.map(l => l.event);
  for (const k of ['intent', 'auth', 'spawned', 'sent', 'replayed', 'result', 'closing', 'exit', 'terminal']) assert.ok(kinds.includes(k), `missing ${k}`);
  assert.deepEqual(r.last, { ...r.last, event: 'terminal', ok: true, failures: [], reviewStatus: 'pending' });

  const intent = readJson(path.join(runDir, 'intent.json'));
  const c = completion(runDir);
  assert.equal(c.ok, true);
  assert.equal(c.reviewStatus, 'pending');
  assert.equal(c.sessionId, intent.sessionId);
  assert.equal(c.artifacts[0].ok, true);
  assert.equal(fs.readFileSync(path.join(runDir, 'final.txt'), 'utf8'), 'héllo 世界');
  assert.equal(fs.readFileSync(path.join(runDir, 'prompt.txt'), 'utf8'), 'WRITE out/result.txt hello\nSAY héllo 世界');

  // command array fixed by the runner
  const cmd = intent.command;
  assert.equal(intent.model, 'claude-opus-5-5');
  for (const [flag, val] of [['--model', 'claude-opus-5-5'], ['--session-id', intent.sessionId], ['--input-format', 'stream-json'],
    ['--output-format', 'stream-json'], ['--permission-mode', 'dontAsk'], ['--permission-prompts', 'none'], ['--tools', 'Read,Write'], ['--allowedTools', 'Read,Write']])
    assert.equal(cmd[cmd.indexOf(flag) + 1], val, flag);
  for (const f of ['--replay-user-messages', '--verbose', '--safe-mode', '--strict-mcp-config', '--no-chrome']) assert.ok(cmd.includes(f), f);
  assert.ok(!cmd.includes('--continue') && !cmd.includes('--resume'));
  assert.deepEqual(readJson(cmd[cmd.indexOf('--mcp-config') + 1]), { mcpServers: {} });

  // the timeline shows intent/auth before the send, and the child saw exactly the runner's argv
  const timeline = fs.readFileSync(path.join(runDir, 'timeline.jsonl'), 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l));
  const idx = k => timeline.findIndex(e => e.kind === k);
  assert.ok(idx('auth-status') < idx('spawn') && idx('spawn') < idx('sending') && idx('sending') < idx('sent'));
  assert.ok(timeline.some(e => e.kind === 'unterminated-final-line'));
  const inv = ctx.invocations();
  assert.deepEqual(inv.map(i => i.argv[0]), ['auth', '-p']);
  assert.deepEqual(inv[1].argv, cmd.slice(3));

  // auth status saved filtered only
  const auth = fs.readFileSync(path.join(runDir, 'auth-status.json'), 'utf8');
  assert.deepEqual(Object.keys(JSON.parse(auth)).sort(), ['apiProvider', 'authMethod', 'checkedAt', 'exitCode', 'loggedIn', 'subscriptionType']);
  assert.ok(!/secret/.test(auth));

  // full raw JSONL preserved, including the unterminated final line
  const raw = fs.readFileSync(path.join(runDir, 'events.jsonl'), 'utf8');
  assert.ok(!raw.endsWith('\n') && raw.includes('héllo 世界'));
  assert.ok(events(runDir).some(e => e.type === 'user' && e.uuid === intent.taskMessageId && e.isReplay));

  // status is compact and doesn't print the transcript
  const s = await run1(['status', '--run-dir', runDir]);
  assert.equal(s.code, 0);
  assert.equal(s.last.liveness, 'terminal');
  assert.equal(s.last.reviewStatus, 'pending');
  assert.ok(!JSON.stringify(s.last).includes('WRITE') && !JSON.stringify(s.last).includes('héllo'));

  // duplicate run into the same run-dir is refused and leaves the intent untouched
  const before = fs.readFileSync(path.join(runDir, 'intent.json'));
  const dup = await startRun(ctx, runDir, 'SAY again').done;
  assert.equal(dup.code, 2);
  assert.match(dup.last.error, /not empty/);
  assert.deepEqual(fs.readFileSync(path.join(runDir, 'intent.json')), before);

  // review requires a note, then is recorded exactly once
  assert.equal((await run1(['review', '--run-dir', runDir, '--note', '  '])).code, 2);
  const rv = await run1(['review', '--run-dir', runDir, '--note', 'Read out/result.txt independently; content matches.']);
  assert.equal(rv.code, 0);
  assert.equal(completion(runDir).reviewStatus, 'reviewed');
  assert.equal((await run1(['review', '--run-dir', runDir, '--note', 'again'])).code, 2);
});

test('foreground-only mode is child-scoped, persisted, and inherited by exact-session resume', async () => {
  const ctx = setup('foreground-only');
  const original = process.env.CLAUDE_CODE_DISABLE_BACKGROUND_TASKS;
  const first = ctx.runDir('first');
  const result = await startRun(ctx, first, 'SAY done', ['--background-tasks', 'disabled']).done;
  assert.equal(result.code, 0, JSON.stringify(result.last));
  assert.equal(readJson(path.join(first, 'intent.json')).config.backgroundTasks, 'disabled');
  assert.ok(ctx.invocations().every(i => i.backgroundTasksDisabled === '1'));
  assert.equal(process.env.CLAUDE_CODE_DISABLE_BACKGROUND_TASKS, original);
  const resumed = ctx.runDir('resumed');
  const next = await run1(['run', '--run-dir', resumed, '--resume-from', first, '--task', ctx.task('SAY resumed')]);
  assert.equal(next.code, 0, JSON.stringify(next.last));
  assert.equal(readJson(path.join(resumed, 'intent.json')).config.backgroundTasks, 'disabled');
  assert.ok(ctx.invocations().every(i => i.backgroundTasksDisabled === '1'));
  const refused = await run1(['run', '--run-dir', ctx.runDir('mismatch'), '--resume-from', resumed,
    '--task', ctx.task('SAY mismatch'), '--background-tasks', 'inherit']);
  assert.equal(refused.code, 2);
  assert.match(refused.last.error, /resume inherits backgroundTasks/);
  const invalid = await startRun(ctx, ctx.runDir('invalid'), 'SAY invalid', ['--background-tasks', 'wrong']).done;
  assert.equal(invalid.code, 2);
  assert.match(invalid.last.error, /background-tasks must be/);
});

test('CLI background notifications survive without becoming coordinator-message receipts', async () => {
  const ctx = setup('background-notification', { taskNotification: 'valid' });
  const runDir = ctx.runDir();
  const r = await startRun(ctx, runDir, 'WRITE after.txt still-working\nSAY completed', ['--expect', 'after.txt']).done;
  assert.equal(r.code, 0, JSON.stringify(r.last));
  assert.equal(completion(runDir).ok, true);
  assert.equal(fs.readFileSync(path.join(ctx.cwd, 'after.txt'), 'utf8'), 'still-working');
  assert.equal(completion(runDir).messages.length, 1, 'only the coordinator input is tracked');
  const timeline = fs.readFileSync(path.join(runDir, 'timeline.jsonl'), 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l));
  assert.ok(timeline.some(e => e.kind === 'background-task-notification'));
  const replay = events(runDir).find(e => e.type === 'user' && e.origin?.kind === 'task-notification');
  assert.equal(stateOf(runDir).messages[replay.uuid], undefined);
});

test('unmatched or altered notification replays still fail closed', async () => {
  await Promise.all(['unmatched', 'wrong-run', 'wrong-producer', 'wrong-content', 'wrong-session', 'no-origin'].map(async mode => {
    const ctx = setup(`notification-${mode}`, { taskNotification: mode });
    const runDir = ctx.runDir();
    const r = await startRun(ctx, runDir, 'SAY should not succeed').done;
    assert.equal(r.code, 1, `${mode}: ${JSON.stringify(r.last)}`);
    assert.ok(failureCodes(runDir).includes(mode === 'wrong-session' ? 'session-mismatch' : 'unexpected-replay'), mode);
  }));
});

test('internal notification does not replace a missing assignment replay', async () => {
  const ctx = setup('notification-without-input-replay', { taskNotification: 'valid', omitReplay: true, exitAfterTurn: true });
  const runDir = ctx.runDir();
  const r = await startRun(ctx, runDir, 'SAY no input replay').done;
  assert.equal(r.code, 1);
  assert.ok(failureCodes(runDir).includes('unaccounted-message'));
});

test('mid-tool steering with next priority redirects subsequent work; idempotent ids; replace-expect', async () => {
  const ctx = setup('steer-next');
  const runDir = ctx.runDir();
  const owner = startRun(ctx, runDir, 'WRITE checkpoint1.txt one\nSLEEP 1500\nWRITE final.txt original\nSAY original done', ['--expect', 'final.txt']);
  await waitFor(() => exists(path.join(ctx.cwd, 'checkpoint1.txt')), 10_000, 'checkpoint');

  // review before terminal is refused
  assert.equal((await run1(['review', '--run-dir', runDir, '--note', 'too early'])).code, 2);

  const id = randomUUID();
  const text = 'Stop the original task.\nWRITE replacement.txt MARKER-7f3a\nSAY redirected';
  const textFile = ctx.task(text);
  const s1 = await run1(['steer', '--run-dir', runDir, '--task', textFile, '--id', id, '--replace-expect', 'replacement.txt']);
  assert.equal(s1.code, 0, JSON.stringify(s1.last));
  assert.ok(['queued', 'sending', 'sent', 'replayed'].includes(s1.last.status));
  assert.match(s1.last.note, /None of these prove the model applied/);

  const s2 = await run1(['steer', '--run-dir', runDir, '--task', textFile, '--id', id, '--replace-expect', 'replacement.txt']);
  assert.equal(s2.code, 0);
  assert.equal(s2.last.duplicate, true);
  const s3 = await steer(runDir, ctx, 'different text', ['--id', id]);
  assert.equal(s3.code, 2);
  assert.match(s3.last.error, /different/);

  const r = await owner.done;
  assert.equal(r.code, 0, JSON.stringify(r.lines));
  assert.equal(fs.readFileSync(path.join(ctx.cwd, 'replacement.txt'), 'utf8'), 'MARKER-7f3a');
  assert.ok(!exists(path.join(ctx.cwd, 'final.txt')), 'original final artifact must not be written after redirect');
  const c = completion(runDir);
  assert.equal(c.results.length, 1);
  assert.deepEqual(c.messages.find(m => m.id === id), { id, kind: 'steer', priority: 'next', status: 'answered', answeredByResult: 1 });
  assert.deepEqual(c.expect, [path.join(ctx.cwd, 'replacement.txt')]);
  assert.equal(c.expectHistory.length, 1);
  const ev = events(runDir);
  const replays = ev.map((e, i) => [e, i]).filter(([e]) => e.type === 'user' && e.uuid === id);
  assert.equal(replays.length, 1, 'steer delivered exactly once despite the duplicate steer call');
  assert.ok(replays[0][1] < ev.findIndex(e => e.type === 'result'));
  const sent = fs.readFileSync(path.join(runDir, 'mailbox', `${id}.json`), 'utf8');
  assert.equal(JSON.parse(sent).text, text);
});

test('later-priority follow-up keeps stdin open until its replay is followed by a second result', async () => {
  const ctx = setup('steer-later');
  const runDir = ctx.runDir();
  const owner = startRun(ctx, runDir, 'WRITE a.txt one\nSLEEP 800\nSAY first', ['--expect', 'a.txt']);
  await waitFor(() => exists(path.join(ctx.cwd, 'a.txt')), 10_000, 'a.txt');
  const s = await steer(runDir, ctx, 'WRITE b.txt two\nSAY second', ['--priority', 'later', '--wait-seconds', '10']);
  assert.equal(s.code, 0);
  assert.ok(['replayed', 'answered'].includes(s.last.status), s.last.status);
  const r = await owner.done;
  assert.equal(r.code, 0, JSON.stringify(r.lines));
  const c = completion(runDir);
  assert.equal(c.results.length, 2);
  assert.equal(c.messages.find(m => m.id === s.last.id).answeredByResult, 2);
  assert.equal(fs.readFileSync(path.join(ctx.cwd, 'b.txt'), 'utf8'), 'two');
  assert.equal(fs.readFileSync(path.join(runDir, 'final.txt'), 'utf8'), 'second');
  const ev = events(runDir);
  const firstResult = ev.findIndex(e => e.type === 'result');
  assert.ok(firstResult < ev.findIndex(e => e.uuid === s.last.id), 'later message was replayed after the first result');
  const lifecycle = r.lines.map(l => l.event);
  assert.equal(lifecycle.filter(e => e === 'closing').length, 1);
  assert.ok(lifecycle.indexOf('closing') > lifecycle.lastIndexOf('result'), 'stdin closed only after the second result');
});

test('resume inherits exact session and cwd; session lock excludes other run-dirs; predecessor must be terminal', async () => {
  const ctx = setup('resume');
  const a = ctx.runDir('a');
  const ra = await startRun(ctx, a, 'WRITE a.txt x').done;
  assert.equal(ra.code, 0);
  const ia = readJson(path.join(a, 'intent.json'));
  const lockFile = path.join(LOCKS, `${ia.sessionId}.lock`);
  assert.ok(!exists(lockFile), 'lock released at terminal');

  const resume = (dir, text, extra = []) => cli(['run', '--run-dir', dir, '--resume-from', a, '--task', ctx.task(text), ...extra]);
  // cwd/config are inherited, not overridable
  const other = path.join(ctx.base, 'elsewhere');
  fs.mkdirSync(other);
  const bad = await resume(ctx.runDir('bad-cwd'), 'SAY x', ['--cwd', other]).done;
  assert.equal(bad.code, 2);
  assert.match(bad.last.error, /inherits cwd/);

  // a lock held by some other owner blocks the resume and is not removed by it
  fs.writeFileSync(lockFile, JSON.stringify({ runId: 'someone-else', pid: process.pid }));
  const blocked = await resume(ctx.runDir('blocked'), 'SAY x').done;
  assert.equal(blocked.code, 2);
  assert.match(blocked.last.error, /locked/);
  assert.ok(exists(lockFile));
  fs.unlinkSync(lockFile); // the test itself plays the explicit reconciler here

  // two different run-dirs racing to resume the same session: exactly one wins
  const [b, c] = [ctx.runDir('b'), ctx.runDir('c')];
  const [rb, rc] = await Promise.all([resume(b, 'SLEEP 600\nSAY resumed').done, resume(c, 'SLEEP 600\nSAY resumed').done]);
  const codes = [rb.code, rc.code].sort();
  assert.deepEqual(codes, [0, 2], JSON.stringify([rb.last, rc.last]));
  const [win, lose] = rb.code === 0 ? [b, rc] : [c, rb];
  assert.match(lose.last.error, /locked/);

  const ib = readJson(path.join(win, 'intent.json'));
  assert.equal(ib.sessionId, ia.sessionId);
  assert.equal(ib.sessionMode, 'resume');
  assert.equal(ib.cwd, ia.cwd);
  assert.deepEqual(ib.config, ia.config);
  assert.equal(ib.command[ib.command.indexOf('--resume') + 1], ia.sessionId);
  assert.ok(!ib.command.includes('--session-id') && !ib.command.includes('--continue'));
  const streamCalls = ctx.invocations().filter(i => i.argv[0] === '-p');
  assert.equal(streamCalls.length, 2, 'loser never dispatched');
  assert.equal(path.resolve(streamCalls[1].cwd), path.resolve(ia.cwd));
  assert.equal(streamCalls[1].argv[streamCalls[1].argv.indexOf('--resume') + 1], ia.sessionId);
  assert.ok(events(win).every(e => !('session_id' in e) || e.session_id === ia.sessionId));
  assert.equal(completion(win).resumeFrom.runId, ia.runId);

  // a non-terminal predecessor cannot be resumed
  const d = ctx.runDir('d');
  const rd = startRun(ctx, d, 'SLEEP 1500\nSAY d');
  await waitFor(() => stateOf(d)?.phase === 'active', 10_000, 'd active');
  const e = await run1(['run', '--run-dir', ctx.runDir('e'), '--resume-from', d, '--task', ctx.task('SAY e')]);
  assert.equal(e.code, 2);
  assert.match(e.last.error, /not terminal/);
  assert.equal((await rd.done).code, 0);
});

test('unexpected model or session id fails promptly', async () => {
  const cases = [
    [{ model: 'claude-sonnet-5-5' }, 'model-mismatch'],
    [{ assistantModel: 'claude-haiku-5-5' }, 'model-mismatch'],
    [{ sessionId: randomUUID() }, 'session-mismatch'],
    [{ apiKeySource: 'ANTHROPIC_API_KEY' }, 'api-key-source'],
  ];
  await Promise.all(cases.map(async ([scenario, code]) => {
    const ctx = setup(code, scenario);
    const runDir = ctx.runDir();
    const r = await startRun(ctx, runDir, 'WRITE x.txt y\nSLEEP 6000\nSAY z').done;
    assert.equal(r.code, 1, code);
    assert.ok(failureCodes(runDir).includes(code), JSON.stringify(failureCodes(runDir)));
    assert.ok(r.ms < 5000, `${code} aborted promptly (${r.ms}ms)`);
    assert.equal(completion(runDir).ok, false);
  }));
});

test('failure matrix: permission denial, error result, artifacts, nonzero exit, malformed, early EOF, auth', async () => {
  const cases = [
    { name: 'denied', scenario: { permissionDenials: [{ tool_name: 'Bash', tool_use_id: 't1', tool_input: {} }] }, expect: ['permission-denied'] },
    { name: 'error-result', scenario: { subtype: 'error_during_execution', isError: true }, expect: ['result-subtype', 'result-is-error'] },
    { name: 'no-denials-field', scenario: { omitDenials: true }, expect: ['permission-denials-unreported'] },
    { name: 'artifacts', task: 'WRITE empty.txt', args: ['--expect', 'empty.txt', '--expect', 'missing.txt', '--expect', '.'], expect: ['artifact'],
      check: c => assert.deepEqual(c.artifacts.map(a => a.reason ?? 'ok'), ['empty', 'missing', 'not a regular file']) },
    { name: 'nonzero', scenario: { exitCode: 3 }, expect: ['nonzero-exit'] },
    { name: 'malformed', scenario: { malformed: true }, expect: ['malformed-output'] },
    { name: 'early-eof', scenario: { noResult: true, exitAfterTurn: true }, expect: ['missing-result', 'child-exited-early', 'unaccounted-message'] },
    { name: 'missing-init', scenario: { omitInit: true }, expect: ['missing-init'] },
    { name: 'auth-nonzero', scenario: { authExitCode: 9 }, expect: ['auth-refused'],
      check: (c, ctx) => assert.deepEqual(ctx.invocations().map(i => i.argv[0]), ['auth']) },
    { name: 'auth-console', scenario: { auth: { loggedIn: true, authMethod: 'console', apiProvider: 'firstParty' } }, expect: ['auth-refused'],
      check: (c, ctx) => assert.deepEqual(ctx.invocations().map(i => i.argv[0]), ['auth']) },
    { name: 'auth-bedrock', scenario: { auth: { loggedIn: true, authMethod: 'claude.ai', apiProvider: 'bedrock' } }, expect: ['auth-refused'] },
    { name: 'spawn-fail', bin: path.join(HERE, 'does-not-exist.exe'), expect: ['auth-refused'] },
  ];
  await Promise.all(cases.map(async k => {
    const ctx = setup(k.name, k.scenario);
    const runDir = ctx.runDir();
    const fake = k.bin ? ['--claude-bin', k.bin, '--lock-dir', LOCKS] : ctx.fake;
    const r = await run1(['run', '--run-dir', runDir, '--cwd', ctx.cwd, '--task', ctx.task(k.task ?? 'WRITE x.txt y'), '--timeout-seconds', '20', ...fake, ...(k.args ?? [])]);
    assert.equal(r.code, 1, `${k.name}: ${JSON.stringify(r.last)}`);
    const c = completion(runDir);
    assert.equal(c.ok, false);
    assert.equal(c.reviewStatus, 'pending');
    for (const code of k.expect) assert.ok(failureCodes(runDir).includes(code), `${k.name}: ${JSON.stringify(c.failures)}`);
    assert.ok(exists(path.join(runDir, 'intent.json')) && exists(path.join(runDir, 'events.jsonl')), 'evidence retained');
    k.check?.(c, ctx);
    const w = await run1(['wait', '--run-dir', runDir]);
    assert.equal(w.code, 1, `${k.name}: wait reports delegate failure`);
  }));
});

test('run deadline kills the child; wait timeout leaves the owner running and resends nothing; shutdown deadline', async () => {
  const ctx = setup('timeouts');
  const slow = ctx.runDir('slow');
  const r = await startRun(ctx, slow, 'SLEEP 20000', ['--timeout-seconds', '1']).done;
  assert.equal(r.code, 1);
  assert.ok(r.ms < 10_000, `deadline honored (${r.ms}ms)`);
  assert.ok(failureCodes(slow).includes('timeout'));
  assert.equal((await run1(['status', '--run-dir', slow])).last.child.alive, false);

  const live = ctx.runDir('live');
  const owner = startRun(ctx, live, 'SLEEP 1500\nSAY ok');
  await waitFor(() => stateOf(live)?.phase === 'active', 10_000, 'active');
  const w = await run1(['wait', '--run-dir', live, '--timeout-seconds', '0.3']);
  assert.equal(w.code, 3);
  assert.equal(w.last.wait, 'timeout');
  assert.equal(w.last.liveness, 'active');
  const w2 = await run1(['wait', '--run-dir', live]);
  assert.equal(w2.code, 0);
  assert.equal(w2.last.owner.alive, false, 'wait returns after the owner process has exited');
  assert.equal((await owner.done).code, 0);
  const taskId = readJson(path.join(live, 'intent.json')).taskMessageId;
  assert.equal(events(live).filter(e => e.uuid === taskId).length, 1);

  const stuck = setup('stuck', { ignoreStdinEnd: true });
  const sd = stuck.runDir();
  const rs = await startRun(stuck, sd, 'SAY hi', ['--shutdown-seconds', '1']).done;
  assert.equal(rs.code, 1);
  assert.ok(failureCodes(sd).includes('shutdown-timeout'));
});

test('dead owner is visible, rejects steering, and needs explicit reconciliation', async () => {
  const ctx = setup('dead-owner');
  const runDir = ctx.runDir();
  const owner = startRun(ctx, runDir, 'WRITE a.txt x\nSLEEP 2500\nSAY done');
  const st = await waitFor(() => { const s = stateOf(runDir); return s?.phase === 'active' && s.childPid && s; }, 10_000, 'active');
  const intent = readJson(path.join(runDir, 'intent.json'));
  const lockFile = path.join(LOCKS, `${intent.sessionId}.lock`);
  owner.proc.kill('SIGKILL');
  await owner.done;

  const s = await run1(['status', '--run-dir', runDir]);
  assert.equal(s.code, 4);
  assert.equal(s.last.liveness, 'owner-dead');
  assert.equal(s.last.needsReconcile, true);
  const sr = await steer(runDir, ctx, 'SAY too late');
  assert.equal(sr.code, 1);
  assert.equal(sr.last.status, 'rejected');
  assert.equal((await run1(['wait', '--run-dir', runDir, '--timeout-seconds', '5'])).code, 4);
  assert.ok(exists(lockFile), 'stale session lock not silently removed');
  const resumed = await run1(['run', '--run-dir', ctx.runDir('next'), '--resume-from', runDir, '--task', ctx.task('SAY x')]);
  assert.equal(resumed.code, 2);

  let rc = await run1(['reconcile', '--run-dir', runDir, '--note', 'owner killed in test']);
  if (rc.code === 2) {
    assert.match(rc.last.error, /still alive/);
    await waitFor(() => !pidAlive(st.childPid), 15_000, 'orphaned child exit');
    rc = await run1(['reconcile', '--run-dir', runDir, '--note', 'owner killed in test; child exited']);
  }
  assert.equal(rc.code, 0, JSON.stringify(rc.last));
  assert.ok(!pidAlive(st.childPid), 'successful reconciliation requires the child to have exited');
  assert.ok(!exists(lockFile));
  const c = completion(runDir);
  assert.equal(c.ok, false);
  assert.ok(c.failures.some(f => f.code === 'owner-dead-reconciled'));
  assert.equal(c.reviewStatus, 'pending');
  assert.equal((await run1(['status', '--run-dir', runDir])).last.liveness, 'terminal');
});

test('failure to save durable completion never reports success', async () => {
  const ctx = setup('completion-storage-failure');
  const runDir = ctx.runDir();
  const owner = startRun(ctx, runDir, 'SLEEP 800\nSAY done');
  await waitFor(() => stateOf(runDir)?.phase === 'active');
  fs.mkdirSync(path.join(runDir, 'completion.json'));
  const r = await owner.done;
  assert.equal(r.code, 1);
  assert.equal(r.last.ok, false);
  assert.ok(r.last.failures.includes('completion-write-failed'));
  assert.ok(stateOf(runDir).failures.some(f => f.code === 'completion-write-failed'));
});

test('enqueue vs close race: every steer is either answered or explicitly rejected', async () => {
  const ctx = setup('race');
  const runDir = ctx.runDir();
  const owner = startRun(ctx, runDir, 'WRITE a.txt x\nSAY first');
  await waitFor(() => owner.stdout.includes('"event":"intent"'), 10_000, 'intent');
  const ids = Array.from({ length: 12 }, () => randomUUID());
  const steers = [];
  for (const id of ids) {
    steers.push(steer(runDir, ctx, `SAY reply ${id}`, ['--id', id]).then(r => ({ id, ...r })));
    await sleep(25);
  }
  const results = await Promise.all(steers);
  const r = await owner.done;
  const late = await steer(runDir, ctx, 'SAY after terminal');
  assert.equal(late.code, 1);
  assert.equal(late.last.status, 'rejected');

  const c = completion(runDir);
  const ev = events(runDir);
  const lastResult = ev.map(e => e.type).lastIndexOf('result');
  let accepted = 0;
  for (const s of results) {
    if (s.code === 0) {
      accepted++;
      assert.equal(c.messages.find(m => m.id === s.id)?.status, 'answered', `accepted ${s.id} must be answered`);
      const at = ev.findIndex(e => e.type === 'user' && e.uuid === s.id);
      assert.ok(at >= 0 && at < lastResult, 'replayed before a later result');
    } else {
      assert.equal(s.code, 1, JSON.stringify(s.last));
      assert.equal(s.last.status, 'rejected');
      assert.ok(!exists(path.join(runDir, 'mailbox', `${s.id}.json`)), 'rejected steer left no queued request');
      assert.ok(!c.messages.some(m => m.id === s.id && m.status === 'answered'));
    }
  }
  assert.equal(r.code, 0, JSON.stringify(c.failures));
  assert.ok(c.messages.every(m => m.status === 'answered'));
  console.log(`# race: ${accepted} accepted, ${ids.length - accepted} rejected`);
});
