#!/usr/bin/env node
// Deterministic stand-in for the Claude Code CLI, used only by runner.test.mjs.
// Usage: node fake-claude.mjs SCENARIO.json <claude args...>
//
// It speaks the stream-json protocol the runner uses and interprets the user
// message text as a tiny script, one directive per line:
//   WRITE <relpath> <content...>   tool call that writes a file under cwd
//   SLEEP <ms>                     a long-running tool call
//   SAY <text>                     assistant text; the last SAY is the result text
// Like the real CLI's `next` priority, a message arriving mid-turn is read after
// the running tool call finishes and replaces the remaining directives; `later`
// messages wait for the turn to end and start a new turn.
// SCENARIO keys inject faults: auth, model, assistantModel, sessionId, apiKeySource,
// subtype, isError, permissionDenials, omitDenials, noResult, exitCode, malformed,
// splitUtf8, stderr, ignoreStdinEnd, exitAfterTurn, argsLog.
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
import { randomUUID } from 'node:crypto';

const [scenarioPath, ...args] = process.argv.slice(2);
const sc = JSON.parse(fs.readFileSync(scenarioPath, 'utf8'));
if (sc.argsLog) fs.appendFileSync(sc.argsLog, JSON.stringify({ argv: args, cwd: process.cwd(), pid: process.pid,
  backgroundTasksDisabled: process.env.CLAUDE_CODE_DISABLE_BACKGROUND_TASKS ?? null }) + '\n');

if (args[0] === 'auth' && args[1] === 'status') {
  process.stdout.write(JSON.stringify(sc.auth ?? {
    loggedIn: true, authMethod: 'claude.ai', apiProvider: 'firstParty', subscriptionType: 'max',
    email: 'secret@example.invalid', orgId: 'org-secret-id', accountUuid: 'acct-secret-id',
  }, null, 2) + '\n');
  process.exit(sc.authExitCode ?? 0);
}

const flag = name => { const i = args.indexOf(name); return i >= 0 ? args[i + 1] : undefined; };
const session = sc.sessionId ?? flag('--session-id') ?? flag('--resume');
const model = sc.model ?? flag('--model');
const sleep = ms => new Promise(r => setTimeout(r, ms));
const emit = obj => process.stdout.write(JSON.stringify(obj) + '\n');
if (sc.stderr) process.stderr.write(sc.stderr);

const inbox = [];
let stdinEnded = false;
let wake = null;
const poke = () => { wake?.(); wake = null; };
readline.createInterface({ input: process.stdin })
  .on('line', line => { if (line.trim()) { inbox.push(JSON.parse(line)); poke(); } })
  .on('close', () => { stdinEnded = true; poke(); });

async function nextMessage() {
  for (;;) {
    if (inbox.length) return inbox.shift();
    if (stdinEnded) return null;
    await new Promise(r => { wake = r; });
  }
}

const parse = text => String(text).split(/\r?\n/).map(l => l.trim()).filter(Boolean)
  .map(l => { const [op, ...rest] = l.split(' '); return { op, rest }; });

let turns = 0;
let initSent = false;

function replay(msg) {
  emit({ type: 'user', message: msg.message, session_id: session, parent_tool_use_id: null, uuid: msg.uuid, isReplay: true });
}

// Claude Code emits this replay-shaped internal message after a background
// command completes. It is not a coordinator-authored input replay.
function taskNotification(mode) {
  const taskId = 'background-task-1', runId = 'background-run-1';
  const notice = { type: 'system', subtype: 'task_notification', task_id: taskId,
    run_id: runId, tool_use_id: 'toolu_background', status: 'completed',
    output_file: path.join(process.cwd(), 'tasks', 'background.output'),
    summary: 'Background command completed (exit code 0)', uuid: randomUUID(), session_id: session };
  const content = `<task-notification>\n<task-id>${taskId}</task-id>\n<tool-use-id>${notice.tool_use_id}</tool-use-id>\n<output-file>${notice.output_file}</output-file>\n<status>${notice.status}</status>\n<summary>${notice.summary}</summary>\n</task-notification>`;
  if (mode !== 'unmatched') emit(notice);
  emit({ type: 'user', message: { role: 'user', content: mode === 'wrong-content' ? content + ' extra' : content },
    session_id: mode === 'wrong-session' ? randomUUID() : session,
    parent_tool_use_id: null, uuid: randomUUID(), isReplay: true,
    origin: mode === 'no-origin' ? undefined : { kind: 'task-notification',
      producer: mode === 'wrong-producer' ? 'external' : 'session-task',
      runId: mode === 'wrong-run' ? 'unknown-run' : runId } });
}

async function step(d, said) {
  const tid = `toolu_${randomUUID()}`;
  const assistant = content => emit({ type: 'assistant', message: { model: sc.assistantModel ?? model, role: 'assistant', content }, session_id: session, uuid: randomUUID() });
  if (d.op === 'WRITE') {
    const [rel, ...words] = d.rest;
    assistant([{ type: 'tool_use', id: tid, name: 'Write', input: { file_path: rel, content: words.join(' ') } }]);
    fs.mkdirSync(path.dirname(path.resolve(rel)), { recursive: true });
    fs.writeFileSync(path.resolve(rel), words.join(' '));
    emit({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: tid, content: 'ok' }] }, session_id: session, uuid: randomUUID() });
  } else if (d.op === 'SLEEP') {
    assistant([{ type: 'tool_use', id: tid, name: 'Bash', input: { command: `sleep ${d.rest[0]}` } }]);
    await sleep(Number(d.rest[0]));
    emit({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: tid, content: 'slept' }] }, session_id: session, uuid: randomUUID() });
  } else if (d.op === 'SAY') {
    said.text = d.rest.join(' ');
    assistant([{ type: 'text', text: said.text }]);
  }
}

async function turn(first) {
  turns++;
  if (!sc.omitReplay) replay(first);
  if (!initSent) {
    initSent = true;
    if (!sc.omitInit) emit({ type: 'system', subtype: 'init', session_id: session, model, cwd: process.cwd(), apiKeySource: sc.apiKeySource ?? 'none', tools: ['Read', 'Write'], uuid: randomUUID() });
    if (sc.malformed) process.stdout.write('this is not json\n');
  }
  if (sc.taskNotification) taskNotification(sc.taskNotification);
  let directives = parse(first.message.content);
  const said = { text: `turn ${turns} done` };
  while (directives.length) {
    await step(directives.shift(), said);
    const i = inbox.findIndex(m => (m.priority ?? 'next') !== 'later');
    if (i >= 0) { const m = inbox.splice(i, 1)[0]; replay(m); directives = parse(m.message.content); }
  }
  if (sc.noResult) return;
  const result = { type: 'result', subtype: sc.subtype ?? 'success', is_error: sc.isError ?? false, num_turns: turns,
    result: said.text, session_id: session, total_cost_usd: 0.01, uuid: randomUUID() };
  if (!sc.omitDenials) result.permission_denials = sc.permissionDenials ?? [];
  const line = JSON.stringify(result);
  if (sc.splitUtf8) {
    // split a multibyte character across writes and leave the final line unterminated
    const buf = Buffer.from(line, 'utf8');
    const cut = buf.indexOf(Buffer.from('世', 'utf8')) + 1;
    process.stdout.write(buf.subarray(0, cut));
    await sleep(60);
    // Flush the final bytes and terminate, delivering actual pipe EOF. The
    // runner then parses the unterminated line during its stdout-end handling.
    // Waiting for stdin here would deadlock both sides of the framing test.
    await new Promise(resolve => process.stdout.write(buf.subarray(cut), resolve));
    process.exit(sc.exitCode ?? 0);
  } else process.stdout.write(line + '\n');
}

for (;;) {
  const msg = await nextMessage();
  if (!msg) break;
  await turn(msg);
  if (sc.exitAfterTurn) { process.stdin.destroy(); break; } // early EOF without waiting for stdin
}
if (sc.ignoreStdinEnd) setInterval(() => {}, 1000);
else process.exitCode = sc.exitCode ?? 0;
