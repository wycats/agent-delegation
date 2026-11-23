#!/usr/bin/env node
// Claude Code delegation runner. One `run` process owns one live Claude CLI
// subprocess over stream-json stdin/stdout; `steer` enqueues feedback into the
// run-dir mailbox; `status`/`wait` observe; `review` records the coordinator's
// independent review; `reconcile` explicitly closes out a dead owner.
// Node standard library only.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { StringDecoder } from 'node:string_decoder';
import { performance } from 'node:perf_hooks';

process.umask(0o077);

const DEFAULT_MODEL = 'claude-opus-5-5';
const DEFAULTS = { maxTurns: 40, maxBudgetUsd: 5, timeoutSeconds: 1800, shutdownSeconds: 20, tools: 'Read,Write' };
const HEARTBEAT_MS = 1000;
const STALE_MS = 10_000;
const POLL_MS = 100;
const MUTEX_TIMEOUT_MS = 10_000;
const AUTH_TIMEOUT_MS = 60_000;
// Environment that could silently switch the CLI to API-key or third-party billing.
const BILLING_ENV = ['ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN', 'ANTHROPIC_BASE_URL',
  'CLAUDE_CODE_USE_BEDROCK', 'CLAUDE_CODE_USE_VERTEX', 'CLAUDE_CODE_USE_FOUNDRY'];
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const TOOL_RE = /^[A-Za-z][A-Za-z0-9_]*$/;
const ALLOWED_TOOL_RE = /^[A-Za-z][A-Za-z0-9_]*(\([^()]*\))?$/;
const MODEL_RE = /^claude-[a-z0-9][a-z0-9.-]*$/;
const PRIORITIES = ['next', 'later', 'now'];
const SETTLED = new Set(['answered', 'rejected', 'unaccounted', 'send-failed', 'replayed-unanswered']);
const DELIVERY_NOTE = 'queued = durably enqueued for the live owner; sending/sent = written to the CLI stdin pipe; ' +
  'replayed = CLI echoed this UUID; answered = a result event followed the replay. None of these prove the model applied the feedback.';
const EXIT = { ok: 0, failed: 1, usage: 2, waitTimeout: 3, needsReconcile: 4 };

class UsageError extends Error {}

const HELP = `Usage: agent-delegation claude <command> [options]

  run       --run-dir ABS --cwd ABS --task FILE [--expect PATH]... [--resume-from OLD-RUN-DIR]
            [--timeout-seconds N] [--shutdown-seconds N] [--max-turns N] [--max-budget-usd N]
            [--tools CSV] [--allowed-tools CSV] [--background-tasks inherit|disabled] [--model ID] [--lock-dir ABS]
            [--claude-bin PATH] [--claude-prefix-arg ARG]...
  steer     --run-dir ABS --task FILE [--id UUID] [--priority next|later|now]
            [--replace-expect PATH]... [--wait-seconds N]
  status    --run-dir ABS
  wait      --run-dir ABS [--timeout-seconds N]
  review    --run-dir ABS --note TEXT
  reconcile --run-dir ABS --note TEXT [--assume-owner-dead]

Exit codes: 0 ok, 1 delegate failed/rejected, 2 usage/refused, 3 wait timeout, 4 owner dead/unknown (reconcile).
See docs/claude.md in the agent-delegation repository.
`;

// ---------- arguments ----------

const SPEC = {
  run: { 'run-dir': 's', cwd: 's', task: 's', expect: 'm', 'resume-from': 's', 'timeout-seconds': 'n',
    'shutdown-seconds': 'n', 'max-turns': 'n', 'max-budget-usd': 'n', tools: 's', 'allowed-tools': 's',
    model: 's', 'background-tasks': 's', 'lock-dir': 's', 'claude-bin': 's', 'claude-prefix-arg': 'm' },
  steer: { 'run-dir': 's', task: 's', id: 's', priority: 's', 'replace-expect': 'm', 'wait-seconds': 'n' },
  status: { 'run-dir': 's' },
  wait: { 'run-dir': 's', 'timeout-seconds': 'n' },
  review: { 'run-dir': 's', note: 's' },
  reconcile: { 'run-dir': 's', note: 's', 'assume-owner-dead': 'b' },
};

function parseArgs(cmd, argv) {
  const spec = SPEC[cmd];
  if (!spec) throw new UsageError(`unknown command "${cmd}"; see --help`);
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const type = a.startsWith('--') ? spec[a.slice(2)] : undefined;
    if (!type) throw new UsageError(`unknown argument ${a} for ${cmd}`);
    const key = a.slice(2);
    if (type === 'b') { out[key] = true; continue; }
    const v = argv[++i];
    if (v === undefined) throw new UsageError(`${a} needs a value`);
    if (type === 'm') { (out[key] ??= []).push(v); continue; }
    if (key in out) throw new UsageError(`${a} given twice`);
    if (type === 'n') {
      const n = Number(v);
      if (!Number.isFinite(n) || n <= 0) throw new UsageError(`${a} must be a positive number`);
      out[key] = n;
    } else out[key] = v;
  }
  return out;
}

function need(o, key) {
  if (o[key] === undefined) throw new UsageError(`--${key} is required`);
  return o[key];
}

function absPath(p, flag) {
  if (!path.isAbsolute(p)) throw new UsageError(`${flag} must be an absolute path: ${p}`);
  return path.resolve(p);
}

function samePath(a, b) {
  const n = p => (process.platform === 'win32' ? path.resolve(p).toLowerCase() : path.resolve(p));
  return n(a) === n(b);
}

function readTaskText(file) {
  let buf;
  try { buf = fs.readFileSync(path.resolve(file)); } catch (e) { throw new UsageError(`cannot read task ${file}: ${e.code}`); }
  let text;
  try { text = new TextDecoder('utf-8', { fatal: true }).decode(buf); } catch { throw new UsageError(`task ${file} is not valid UTF-8`); }
  if (!text.trim()) throw new UsageError(`task ${file} is empty`);
  return { text, buf, abs: path.resolve(file) };
}

function parseCsv(v, re, flag) {
  const items = v.split(',').map(s => s.trim());
  if (!items.length || items.some(s => !re.test(s))) throw new UsageError(`${flag} must be a comma-separated list of tool names: ${v}`);
  return items.join(',');
}

// ---------- filesystem primitives ----------

const sleepSync = ms => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const sha256 = data => createHash('sha256').update(data).digest('hex');
const RETRYABLE = new Set(['EPERM', 'EACCES', 'EBUSY']);
const tmpName = file => `${file}.${process.pid}.${randomUUID()}.tmp`;

function writeAtomic(file, data) {
  const tmp = tmpName(file);
  fs.writeFileSync(tmp, data);
  for (let i = 0; ; i++) {
    try { fs.renameSync(tmp, file); return; } catch (e) {
      if (!RETRYABLE.has(e.code) || i >= 300) { try { fs.unlinkSync(tmp); } catch {} throw e; }
      sleepSync(10);
    }
  }
}

// Atomically create `file` with full contents; throws EEXIST if it already exists.
function createExclusive(file, data) {
  const tmp = tmpName(file);
  fs.writeFileSync(tmp, data);
  try { fs.linkSync(tmp, file); } finally { fs.unlinkSync(tmp); }
}

const json = obj => JSON.stringify(obj, null, 2) + '\n';
const writeJsonAtomic = (file, obj) => writeAtomic(file, json(obj));

function readJson(file) {
  for (let i = 0; ; i++) {
    try { return JSON.parse(fs.readFileSync(file, 'utf8')); } catch (e) {
      if (e.code === 'ENOENT') return null;
      if (i >= 50) throw new Error(`cannot read ${file}: ${e.message}`);
      sleepSync(10);
    }
  }
}

function pidAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try { process.kill(pid, 0); return true; } catch (e) { return e.code === 'EPERM'; }
}

// Short-lived cross-process mutex serializing mailbox enqueue against the owner's
// closing transition. Only the owner (or reconcile) may break a dead holder's lock.
function withMutex(runDir, fn, { breakDeadHolder = false } = {}) {
  const lock = path.join(runDir, 'mailbox.lock');
  const deadline = Date.now() + MUTEX_TIMEOUT_MS;
  let holder = null;
  for (;;) {
    try { fs.writeFileSync(lock, JSON.stringify({ pid: process.pid, at: new Date().toISOString() }), { flag: 'wx' }); break; } catch (e) {
      if (e.code !== 'EEXIST' && !RETRYABLE.has(e.code)) throw e;
      try { holder = JSON.parse(fs.readFileSync(lock, 'utf8')); } catch { holder = null; }
      if (holder && !pidAlive(holder.pid) && breakDeadHolder) { try { fs.unlinkSync(lock); } catch {} continue; }
      if (Date.now() > deadline) throw new Error(`timed out acquiring ${lock} (holder ${JSON.stringify(holder)})`);
      sleepSync(5);
    }
  }
  try { return fn(); } finally {
    for (let i = 0; ; i++) {
      try { fs.unlinkSync(lock); break; } catch (e) { if (!RETRYABLE.has(e.code) || i >= 300) throw e; sleepSync(10); }
    }
  }
}

function defaultLockDir() {
  return process.env.CLAUDE_DELEGATE_LOCK_DIR ||
    path.join(process.env.LOCALAPPDATA || os.tmpdir(), 'claude-delegate-runner', 'session-locks');
}

function acquireSessionLock(lockDir, sessionId, info) {
  fs.mkdirSync(lockDir, { recursive: true });
  const file = path.join(lockDir, `${sessionId}.lock`);
  try { createExclusive(file, json(info)); } catch (e) {
    if (e.code !== 'EEXIST') throw e;
    throw new UsageError(`session ${sessionId} is locked by ${JSON.stringify(readJson(file))} (${file}); ` +
      'refusing to start. A stale lock is only released by `reconcile` on its owning run-dir.');
  }
  return file;
}

function releaseSessionLock(file, runId) {
  const holder = file && readJson(file);
  if (holder?.runId === runId) fs.unlinkSync(file);
  return holder?.runId === runId;
}

function defaultClaudeBin() {
  const p = path.join(process.env.USERPROFILE || os.homedir(), '.local', 'bin', process.platform === 'win32' ? 'claude.exe' : 'claude');
  return fs.existsSync(p) ? p : 'claude';
}

function childEnv() {
  const env = { ...process.env };
  const stripped = [];
  for (const k of Object.keys(env)) if (BILLING_ENV.includes(k.toUpperCase())) { stripped.push(k); delete env[k]; }
  return { env, stripped };
}

function checkArtifacts(list) {
  return (list ?? []).map(p => {
    try {
      const st = fs.lstatSync(p);
      if (!st.isFile()) return { path: p, ok: false, reason: st.isSymbolicLink() ? 'symlink, not a regular file' : 'not a regular file' };
      if (st.size === 0) return { path: p, ok: false, reason: 'empty' };
      return { path: p, ok: true, size: st.size };
    } catch (e) { return { path: p, ok: false, reason: e.code === 'ENOENT' ? 'missing' : e.code }; }
  });
}

const files = runDir => ({
  intent: path.join(runDir, 'intent.json'),
  prompt: path.join(runDir, 'prompt.txt'),
  auth: path.join(runDir, 'auth-status.json'),
  mcp: path.join(runDir, 'empty-mcp.json'),
  state: path.join(runDir, 'state.json'),
  events: path.join(runDir, 'events.jsonl'),
  stderr: path.join(runDir, 'stderr.txt'),
  timeline: path.join(runDir, 'timeline.jsonl'),
  final: path.join(runDir, 'final.txt'),
  completion: path.join(runDir, 'completion.json'),
  review: path.join(runDir, 'review.json'),
  mailbox: path.join(runDir, 'mailbox'),
  rejections: path.join(runDir, 'steer-rejections.jsonl'),
});

function readRequests(runDir) {
  const dir = files(runDir).mailbox;
  let names;
  try { names = fs.readdirSync(dir); } catch (e) { if (e.code === 'ENOENT') return []; throw e; }
  return names.filter(n => /^[0-9a-f-]{36}\.json$/.test(n))
    .map(n => readJson(path.join(dir, n)))
    .filter(Boolean)
    .sort((a, b) => (a.enqueuedAt < b.enqueuedAt ? -1 : a.enqueuedAt > b.enqueuedAt ? 1 : a.id < b.id ? -1 : 1));
}

// Settle every message at terminal time; returns failures for anything unaccounted.
function settleMessages(messages, reason) {
  const failures = [];
  for (const m of Object.values(messages)) {
    if (m.status === 'queued') { m.status = 'rejected'; m.reason = reason; }
    else if (m.status === 'sending' || m.status === 'sent') { m.status = 'unaccounted'; failures.push({ code: 'unaccounted-message', detail: `${m.id} was written to stdin but never replayed` }); }
    else if (m.status === 'replayed') { m.status = 'replayed-unanswered'; failures.push({ code: 'unaccounted-message', detail: `${m.id} replayed but no later result` }); }
  }
  return failures;
}

function summarizeMessages(messages) {
  return Object.values(messages ?? {}).map(m => ({ id: m.id, kind: m.kind, priority: m.priority, status: m.status,
    ...(m.reason ? { reason: m.reason } : {}), ...(m.answeredByResult ? { answeredByResult: m.answeredByResult } : {}) }));
}

function liveness(intent, state, completion) {
  if (completion) return 'terminal';
  const pid = state?.ownerPid ?? intent?.ownerPid;
  if (!pidAlive(pid)) return 'owner-dead';
  const beat = state?.heartbeatAt ?? Date.parse(intent?.createdAt);
  return Date.now() - beat > STALE_MS ? 'owner-unknown' : 'active';
}

function loadRun(runDir) {
  const f = files(runDir);
  const intent = readJson(f.intent);
  if (!intent) throw new UsageError(`no run at ${runDir} (missing intent.json)`);
  return { f, intent, state: readJson(f.state), completion: readJson(f.completion) };
}

const out = obj => process.stdout.write(JSON.stringify(obj) + '\n');

// ---------- run ----------

async function cmdRun(o) {
  const runDir = absPath(need(o, 'run-dir'), '--run-dir');
  const task = readTaskText(need(o, 'task'));
  const prefix = o['claude-prefix-arg'] ?? [];
  if (prefix.some(a => a.startsWith('-'))) throw new UsageError('--claude-prefix-arg values must be positional (e.g. a script path), not flags');
  const bounds = {
    maxTurns: o['max-turns'] ?? DEFAULTS.maxTurns,
    maxBudgetUsd: o['max-budget-usd'] ?? DEFAULTS.maxBudgetUsd,
    timeoutSeconds: o['timeout-seconds'] ?? DEFAULTS.timeoutSeconds,
    shutdownSeconds: o['shutdown-seconds'] ?? DEFAULTS.shutdownSeconds,
  };
  if (!Number.isInteger(bounds.maxTurns)) throw new UsageError('--max-turns must be an integer');
  if (o['background-tasks'] !== undefined && !['inherit', 'disabled'].includes(o['background-tasks']))
    throw new UsageError('--background-tasks must be inherit or disabled');

  let config, sessionId, resumeFrom = null;
  if (o['resume-from']) {
    const prevDir = absPath(o['resume-from'], '--resume-from');
    const prev = loadRun(prevDir);
    if (!prev.completion) throw new UsageError(`predecessor ${prevDir} is not terminal (no completion.json); wait for it or reconcile it`);
    if (prev.state?.ownerPid && liveness(prev.intent, prev.state, null) === 'active')
      throw new UsageError(`predecessor owner pid ${prev.state.ownerPid} is still alive`);
    if (prev.state?.childPid && pidAlive(prev.state.childPid))
      throw new UsageError(`predecessor Claude child pid ${prev.state.childPid} is still alive`);
    config = { ...prev.intent.config };
    config.backgroundTasks ??= 'inherit';
    sessionId = prev.intent.sessionId;
    resumeFrom = { runDir: prevDir, runId: prev.intent.runId, ok: prev.completion.ok };
    const given = { cwd: o.cwd && absPath(o.cwd, '--cwd'), model: o.model, tools: o.tools && parseCsv(o.tools, TOOL_RE, '--tools'),
      allowedTools: o['allowed-tools'] && parseCsv(o['allowed-tools'], ALLOWED_TOOL_RE, '--allowed-tools'),
      claudeBin: o['claude-bin'], lockDir: o['lock-dir'], backgroundTasks: o['background-tasks'] };
    for (const [k, v] of Object.entries(given)) {
      if (v === undefined) continue;
      const same = (k === 'cwd' || k === 'lockDir' || k === 'claudeBin') ? samePath(v, config[k]) : v === config[k];
      if (!same) throw new UsageError(`resume inherits ${k}=${config[k]} from the predecessor; got ${v}`);
    }
    if (o['claude-prefix-arg'] && JSON.stringify(prefix) !== JSON.stringify(config.claudePrefixArgs))
      throw new UsageError('resume inherits --claude-prefix-arg from the predecessor; values differ');
  } else {
    const cwd = absPath(need(o, 'cwd'), '--cwd');
    if (!fs.statSync(cwd, { throwIfNoEntry: false })?.isDirectory()) throw new UsageError(`--cwd is not a directory: ${cwd}`);
    const model = o.model ?? DEFAULT_MODEL;
    if (!MODEL_RE.test(model)) throw new UsageError(`invalid --model ${model}`);
    const tools = parseCsv(o.tools ?? DEFAULTS.tools, TOOL_RE, '--tools');
    config = {
      cwd, model, tools, backgroundTasks: o['background-tasks'] ?? 'inherit',
      allowedTools: o['allowed-tools'] ? parseCsv(o['allowed-tools'], ALLOWED_TOOL_RE, '--allowed-tools') : tools,
      claudeBin: o['claude-bin'] ?? defaultClaudeBin(),
      claudePrefixArgs: prefix,
      lockDir: o['lock-dir'] ? absPath(o['lock-dir'], '--lock-dir') : defaultLockDir(),
      safeMode: true, strictMcp: true, chrome: false, permissionMode: 'dontAsk', permissionPrompts: 'none',
    };
    sessionId = randomUUID();
  }
  const expect = (o.expect ?? []).map(p => path.resolve(config.cwd, p));

  // Claim the run-dir: it must be new/empty, and intent.json is created exclusively.
  fs.mkdirSync(runDir, { recursive: true });
  if (fs.readdirSync(runDir).length) throw new UsageError(`run-dir ${runDir} is not empty; refusing to overwrite another run's intent or logs`);
  const f = files(runDir);
  const runId = randomUUID();
  const taskMessageId = randomUUID();
  const lockFile = acquireSessionLock(config.lockDir, sessionId, { runId, runDir, pid: process.pid, at: new Date().toISOString() });
  const args = ['-p', '--model', config.model,
    '--input-format', 'stream-json', '--output-format', 'stream-json', '--replay-user-messages', '--verbose',
    '--max-turns', String(bounds.maxTurns), '--max-budget-usd', String(bounds.maxBudgetUsd),
    resumeFrom ? '--resume' : '--session-id', sessionId,
    '--safe-mode', '--strict-mcp-config', '--mcp-config', f.mcp, '--no-chrome',
    '--tools', config.tools, '--allowedTools', config.allowedTools,
    '--permission-mode', 'dontAsk', '--permission-prompts', 'none'];
  const { env, stripped } = childEnv();
  // Bounded implementation jobs can opt out of CLI background shells without
  // changing the user's environment, subscription auth, or existing sessions.
  // A final result plus EOF deliberately waits for background servers in the CLI.
  if (config.backgroundTasks === 'disabled') env.CLAUDE_CODE_DISABLE_BACKGROUND_TASKS = '1';
  const intent = {
    schema: 1, runId, runDir, createdAt: new Date().toISOString(), ownerPid: process.pid,
    sessionId, sessionMode: resumeFrom ? 'resume' : 'new', resumeFrom, model: config.model, cwd: config.cwd,
    config, bounds, taskMessageId, task: { source: task.abs, sha256: sha256(task.buf), bytes: task.buf.length },
    expect, command: [config.claudeBin, ...config.claudePrefixArgs, ...args], lockFile,
    strippedBillingEnv: stripped, cliOverride: config.claudeBin !== defaultClaudeBin() || config.claudePrefixArgs.length > 0,
  };
  try { createExclusive(f.intent, json(intent)); } catch (e) {
    releaseSessionLock(lockFile, runId);
    if (e.code === 'EEXIST') throw new UsageError(`another run already claimed ${runDir}`);
    throw e;
  }
  fs.writeFileSync(f.prompt, task.buf);
  fs.writeFileSync(f.mcp, '{"mcpServers":{}}\n');
  fs.mkdirSync(f.mailbox);
  const owner = new Owner(runDir, intent, task.text, env);
  return owner.run();
}

class Owner {
  constructor(runDir, intent, prompt, env) {
    this.runDir = runDir;
    this.f = files(runDir);
    this.intent = intent;
    this.env = env;
    this.sessionId = intent.sessionId;
    this.model = intent.model;
    this.texts = new Map([[intent.taskMessageId, prompt]]);
    this.t0 = performance.now();
    this.seq = 0;
    this.buf = '';
    this.decoder = new StringDecoder('utf8');
    this.closing = false;
    this.stdinEnded = false;
    this.finalized = false;
    this.timers = {};
    // Internal CLI notices use replay-shaped user events too. They can only
    // be recognized after the corresponding structured system notification.
    this.taskNotifications = new Map();
    this.state = {
      schema: 1, runId: intent.runId, phase: 'starting', ownerPid: process.pid, ownerStartedAt: intent.createdAt,
      heartbeatAt: Date.now(), childPid: null, sessionId: this.sessionId, model: this.model,
      expect: intent.expect, expectHistory: [], messages: {}, results: [], failures: [],
      counts: { events: 0, stdoutBytes: 0, stderrBytes: 0 }, init: null, exit: null,
    };
    this.fd = {
      events: fs.openSync(this.f.events, 'a'),
      stderr: fs.openSync(this.f.stderr, 'a'),
      timeline: fs.openSync(this.f.timeline, 'a'),
    };
    this.save();
  }

  now() { return Math.round((performance.now() - this.t0) * 1000) / 1000; }
  log(kind, data = {}) {
    const seq = ++this.seq;
    if (this.fd.timeline !== null) fs.writeSync(this.fd.timeline, JSON.stringify({ seq, t: this.now(), at: new Date().toISOString(), kind, ...data }) + '\n');
    return seq;
  }
  emit(event, data = {}) { out({ event, t: this.now(), ...data }); }
  save() { writeJsonAtomic(this.f.state, this.state); }
  fail(code, detail) {
    if (this.state.failures.length >= 100 || this.state.failures.some(x => x.code === code && x.detail === detail)) return;
    this.state.failures.push({ code, detail });
    this.log('failure', { code, detail });
    this.emit('failure', { code, detail });
    this.save();
  }
  guard(fn) {
    return (...a) => { try { fn.apply(this, a); } catch (e) { this.abort('runner-internal-error', e.stack || String(e)); } };
  }

  run() {
    this.emit('intent', { runId: this.intent.runId, runDir: this.runDir, sessionId: this.sessionId, model: this.model,
      sessionMode: this.intent.sessionMode, taskMessageId: this.intent.taskMessageId });
    this.done = new Promise(r => { this.resolveDone = r; });
    this.timers.heartbeat = setInterval(this.guard(() => { this.state.heartbeatAt = Date.now(); this.save(); }), HEARTBEAT_MS);
    for (const sig of ['SIGINT', 'SIGTERM', 'SIGBREAK']) process.on(sig, this.guard(() => this.abort('owner-interrupted', sig)));
    this.checkAuth().then(this.guard(auth => {
      this.authDone = true;
      if (this.closing) return this.finalize(); // interrupted while checking auth
      if (!auth.ok) { this.fail('auth-refused', auth.reason); this.closeOut('none'); return this.finalize(); }
      this.emit('auth', auth.filtered);
      this.timers.deadline = setTimeout(this.guard(() => this.abort('timeout', `whole-run deadline of ${this.intent.bounds.timeoutSeconds}s elapsed; timeout does not prove rollback`)),
        this.intent.bounds.timeoutSeconds * 1000);
      this.spawnChild();
    }));
    return this.done;
  }

  checkAuth() {
    const [bin, ...pre] = [this.intent.config.claudeBin, ...this.intent.config.claudePrefixArgs];
    return new Promise(resolve => {
      let stdout = '', settled = false, p;
      const finish = r => { if (!settled) { settled = true; clearTimeout(timer); this.log('auth-status', { ok: r.ok, reason: r.reason }); resolve(r); } };
      const timer = setTimeout(() => { p?.kill(); finish({ ok: false, reason: 'auth status timed out' }); }, AUTH_TIMEOUT_MS);
      try { p = spawn(bin, [...pre, 'auth', 'status'], { cwd: this.intent.cwd, env: this.env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] }); }
      catch (e) { return finish({ ok: false, reason: `auth status spawn failed: ${e.message}` }); }
      p.stdout.setEncoding('utf8');
      p.stdout.on('data', d => { stdout += d; });
      p.stderr.resume(); // not saved: it could contain account details
      p.on('error', e => finish({ ok: false, reason: `auth status spawn failed: ${e.code || e.message}` }));
      p.on('close', code => {
        let j;
        try { j = JSON.parse(stdout); } catch { return finish({ ok: false, reason: `auth status exit ${code}; output was not JSON` }); }
        const filtered = { loggedIn: j?.loggedIn, authMethod: j?.authMethod, apiProvider: j?.apiProvider, subscriptionType: j?.subscriptionType };
        writeJsonAtomic(this.f.auth, { checkedAt: new Date().toISOString(), exitCode: code, ...filtered });
        if (code !== 0) return finish({ ok: false, reason: `auth status exited ${code}` });
        if (filtered.loggedIn !== true || filtered.authMethod !== 'claude.ai' || filtered.apiProvider !== 'firstParty')
          return finish({ ok: false, reason: `requires loggedIn claude.ai firstParty subscription auth; got ${JSON.stringify(filtered)}` });
        finish({ ok: true, filtered });
      });
    });
  }

  spawnChild() {
    const [bin, ...args] = this.intent.command;
    this.log('spawn', { command: this.intent.command, cwd: this.intent.cwd });
    let c;
    try { c = spawn(bin, args, { cwd: this.intent.cwd, env: this.env, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] }); }
    catch (e) { this.fail('spawn-failed', e.message); this.closeOut('none'); return this.finalize(); }
    this.child = c;
    c.on('error', this.guard(e => {
      if (!c.pid) { this.fail('spawn-failed', e.code || e.message); this.closeOut('none'); setImmediate(() => this.finalize()); }
      else this.fail('child-error', e.code || e.message);
    }));
    c.stdin.on('error', this.guard(e => { this.fail('stdin-error', e.code || e.message); this.closeOut('abort'); }));
    c.stdout.on('data', this.guard(this.onStdout));
    c.stdout.on('end', this.guard(this.flushStdout));
    c.stderr.on('data', this.guard(b => { fs.writeSync(this.fd.stderr, b); this.state.counts.stderrBytes += b.length; }));
    c.on('exit', this.guard((code, signal) => {
      this.state.exit = { code, signal };
      this.log('child-exit', { code, signal });
      this.emit('exit', { code, signal });
      this.save();
      // stdio normally closes right after exit; don't hang forever if a grandchild holds the pipes
      this.timers.stdio = setTimeout(this.guard(() => { this.fail('stdio-not-closed', 'child exited but stdio stayed open'); this.finalize(); }), 5000);
    }));
    c.on('close', this.guard(() => this.finalize()));
    if (!c.pid) return;
    this.state.childPid = c.pid;
    this.state.phase = 'active';
    this.save();
    this.emit('spawned', { pid: c.pid });
    this.addMessage({ id: this.intent.taskMessageId, kind: 'task', priority: 'next', replaceExpect: null });
    this.send(this.intent.taskMessageId);
    this.timers.poll = setInterval(this.guard(this.pollMailbox), POLL_MS);
  }

  addMessage({ id, kind, priority, replaceExpect }) {
    const m = { id, kind, priority, status: 'queued', replaceExpect, ingestSeq: this.log('ingest', { id, kind, priority }) };
    this.state.messages[id] = m;
    return m;
  }

  // Pull new mailbox requests into state as `queued`. Requests are immutable atomic files.
  ingest() {
    const fresh = [];
    for (const r of readRequests(this.runDir)) {
      if (this.state.messages[r.id]) continue;
      this.texts.set(r.id, r.text);
      fresh.push(this.addMessage({ id: r.id, kind: 'steer', priority: r.priority, replaceExpect: r.replaceExpect }));
    }
    if (fresh.length) this.save();
    return fresh;
  }

  pollMailbox() {
    if (this.closing) return;
    for (const m of this.ingest()) this.send(m.id);
  }

  send(id) {
    const m = this.state.messages[id];
    if (m.replaceExpect) {
      this.state.expectHistory.push({ by: id, from: this.state.expect, to: m.replaceExpect });
      this.state.expect = m.replaceExpect;
    }
    m.status = 'sending';
    m.sendSeq = this.log('sending', { id, priority: m.priority });
    this.save(); // send intent is durable before the pipe write
    const line = JSON.stringify({ type: 'user', uuid: id, session_id: this.sessionId, parent_tool_use_id: null,
      client_composed: true, priority: m.priority, message: { role: 'user', content: this.texts.get(id) } }) + '\n';
    const onWritten = this.guard(err => {
      if (err) {
        if (m.status === 'sending' || m.status === 'sent') m.status = 'send-failed';
        this.fail('stdin-write-failed', `${id}: ${err.code || err.message}; not resent (delivery ambiguous)`);
        return this.closeOut('abort');
      }
      if (m.status === 'sending') m.status = 'sent';
      m.sentSeq = this.log('sent', { id });
      this.emit('sent', { id, kind: m.kind, priority: m.priority });
      this.save();
    });
    try { this.child.stdin.write(line, onWritten); } catch (e) { onWritten(e); }
  }

  onStdout(b) {
    fs.writeSync(this.fd.events, b);
    this.state.counts.stdoutBytes += b.length;
    this.buf += this.decoder.write(b);
    let i;
    while ((i = this.buf.indexOf('\n')) >= 0) {
      const line = this.buf.slice(0, i);
      this.buf = this.buf.slice(i + 1);
      this.onLine(line);
    }
  }

  flushStdout() {
    if (this.flushed) return;
    this.flushed = true;
    this.buf += this.decoder.end();
    if (this.buf.length) {
      const line = this.buf;
      this.buf = '';
      this.log('unterminated-final-line', { length: line.length });
      this.onLine(line);
    }
  }

  onLine(raw) {
    const line = raw.endsWith('\r') ? raw.slice(0, -1) : raw;
    if (!line.trim()) return;
    let ev;
    try { ev = JSON.parse(line); } catch { return this.abort('malformed-output', `unparseable stdout line: ${line.slice(0, 200)}`); }
    if (!ev || typeof ev !== 'object' || Array.isArray(ev) || typeof ev.type !== 'string')
      return this.abort('malformed-output', `stdout line is not a typed event: ${line.slice(0, 200)}`);
    const seq = this.log('event', { type: ev.type, subtype: ev.subtype, uuid: ev.uuid });
    this.state.counts.events++;
    if ('session_id' in ev && ev.session_id !== this.sessionId)
      return this.abort('session-mismatch', `${ev.type} event has session_id ${ev.session_id}, expected ${this.sessionId}`);
    if (ev.type === 'system' && ev.subtype === 'init') {
      this.state.init = { seq, model: ev.model, cwd: ev.cwd, apiKeySource: ev.apiKeySource };
      this.save();
      if (ev.model !== this.model) return this.abort('model-mismatch', `init model ${ev.model}, expected ${this.model}`);
      if (ev.apiKeySource !== undefined && ev.apiKeySource !== 'none') return this.abort('api-key-source', `init apiKeySource ${ev.apiKeySource}`);
      if (typeof ev.cwd === 'string' && !samePath(ev.cwd, this.intent.cwd)) return this.abort('cwd-mismatch', `init cwd ${ev.cwd}`);
    } else if (ev.type === 'assistant') {
      if (ev.message?.model !== this.model) return this.abort('model-mismatch', `assistant model ${ev.message?.model}, expected ${this.model}`);
    } else if (ev.type === 'system' && ev.subtype === 'task_notification') {
      this.rememberTaskNotification(ev);
    } else if (ev.type === 'user' && typeof ev.uuid === 'string' && this.state.messages[ev.uuid]) {
      this.onReplay(this.state.messages[ev.uuid], seq);
    } else if (ev.type === 'user' && ev.isReplay === true) {
      const notice = this.taskNotifications.get(ev.origin?.runId);
      if (ev.session_id === this.sessionId && ev.origin?.kind === 'task-notification' &&
          ev.origin.producer === 'session-task' && notice &&
          ev.message?.role === 'user' && ev.message.content === notice.content) {
        this.log('background-task-notification', { id: ev.uuid, runId: ev.origin.runId, taskId: notice.taskId });
        return; // Not a coordinator input: never acknowledge or settle messages.
      }
      return this.abort('unexpected-replay', `replayed uuid ${ev.uuid} was never sent by this runner`);
    } else if (ev.type === 'result') {
      this.onResult(ev, seq);
    }
  }

  rememberTaskNotification(ev) {
    const fields = ['run_id', 'task_id', 'tool_use_id', 'output_file', 'status', 'summary'];
    if (ev.session_id !== this.sessionId || !fields.every(k => typeof ev[k] === 'string' && ev[k].length)) return;
    const content = `<task-notification>\n<task-id>${ev.task_id}</task-id>\n<tool-use-id>${ev.tool_use_id}</tool-use-id>\n<output-file>${ev.output_file}</output-file>\n<status>${ev.status}</status>\n<summary>${ev.summary}</summary>\n</task-notification>`;
    this.taskNotifications.set(ev.run_id, { taskId: ev.task_id, content });
    if (this.taskNotifications.size > 256) this.taskNotifications.delete(this.taskNotifications.keys().next().value);
  }

  onReplay(m, seq) {
    if (m.replaySeq) { this.log('duplicate-replay', { id: m.id }); return; }
    m.replaySeq = seq;
    if (m.status !== 'answered') m.status = 'replayed';
    this.emit('replayed', { id: m.id, kind: m.kind });
    this.save();
  }

  onResult(ev, seq) {
    const denials = Array.isArray(ev.permission_denials) ? ev.permission_denials : null;
    const r = { n: this.state.results.length + 1, seq, subtype: ev.subtype, is_error: ev.is_error, num_turns: ev.num_turns,
      total_cost_usd_estimate: ev.total_cost_usd, permissionDenials: denials ? denials.length : null };
    this.state.results.push(r);
    if (typeof ev.result === 'string') writeAtomic(this.f.final, ev.result);
    for (const m of Object.values(this.state.messages)) {
      if (m.status === 'replayed') { m.status = 'answered'; m.answeredByResult = r.n; }
    }
    this.emit('result', { n: r.n, subtype: r.subtype, is_error: r.is_error, permissionDenials: r.permissionDenials });
    this.save();
    const bad = [];
    if (ev.subtype !== 'success') bad.push(['result-subtype', `result ${r.n} subtype ${ev.subtype}`]);
    if (ev.is_error !== false) bad.push(['result-is-error', `result ${r.n} is_error ${ev.is_error}`]);
    if (!denials) bad.push(['permission-denials-unreported', `result ${r.n} has no permission_denials array`]);
    else if (denials.length) bad.push(['permission-denied', `result ${r.n}: ${JSON.stringify(denials.map(d => d?.tool_name ?? d))}`]);
    if (bad.length) { for (const [c, d] of bad) this.fail(c, d); return this.closeOut('graceful'); }
    this.tryFinish();
  }

  // Close stdin only once every accepted message has replayed and been followed by a result,
  // and the mailbox is empty under the mutex (so no steer can report `queued` after this).
  tryFinish() {
    if (this.closing || !this.state.results.length) return;
    if (Object.values(this.state.messages).some(m => m.status !== 'answered')) return;
    let fresh = [];
    withMutex(this.runDir, () => {
      fresh = this.ingest();
      if (!fresh.length) this.markClosing('all accepted messages answered');
    }, { breakDeadHolder: true });
    if (fresh.length) { for (const m of fresh) this.send(m.id); return; }
    this.endStdin();
    this.startShutdownTimer();
  }

  markClosing(reason) {
    this.closing = true;
    this.state.phase = 'closing';
    this.state.closing = { reason, seq: this.log('closing', { reason }) };
    this.save();
  }

  // mode: 'graceful' (end stdin, await exit), 'abort' (end stdin and kill), 'none' (no child)
  closeOut(mode) {
    if (!this.closing) {
      try {
        withMutex(this.runDir, () => {
          for (const m of this.ingest()) { m.status = 'rejected'; m.reason = 'run closing after failure; never sent'; this.log('rejected', { id: m.id }); }
          this.markClosing(`closeOut:${mode}`);
        }, { breakDeadHolder: true });
      } catch (e) {
        this.fail('mailbox-mutex', e.message);
        this.markClosing(`closeOut:${mode} without mutex`);
      }
    }
    if (mode === 'none') return;
    this.endStdin();
    if (mode === 'abort') this.killChild();
    this.startShutdownTimer();
  }

  abort(code, detail) {
    this.fail(code, detail);
    if (this.finalized) return;
    if (this.child) return this.closeOut('abort');
    this.closeOut('none');
    if (this.authDone) this.finalize();
  }

  endStdin() {
    if (!this.child || this.stdinEnded) return;
    this.stdinEnded = true;
    this.log('stdin-end');
    this.emit('closing');
    try { this.child.stdin.end(); } catch {}
  }

  killChild() {
    if (!this.child || this.state.exit || this.killed) return;
    this.killed = true;
    this.log('kill', { pid: this.child.pid });
    try { this.child.kill(); } catch {}
  }

  startShutdownTimer() {
    if (this.timers.shutdown || this.finalized) return;
    const ms = this.intent.bounds.shutdownSeconds * 1000;
    this.timers.shutdown = setTimeout(this.guard(() => {
      if (this.state.exit) return;
      this.fail('shutdown-timeout', `child did not exit within ${this.intent.bounds.shutdownSeconds}s of closing; killed`);
      this.killChild();
      this.timers.shutdown2 = setTimeout(this.guard(() => { this.fail('child-exit-unconfirmed', 'child still not exited after kill'); this.finalize(); }), 5000);
    }), ms);
  }

  finalize() {
    if (this.finalized) return;
    this.finalized = true;
    for (const t of Object.values(this.timers)) { clearTimeout(t); clearInterval(t); }
    if (this.child) {
      this.flushStdout();
      this.child.stdout.destroy();
      this.child.stderr.destroy();
    }
    // Any request that slipped in after the closing transition is explicitly rejected, never dropped.
    try {
      withMutex(this.runDir, () => {
        if (!this.closing) this.markClosing('finalize');
        for (const m of this.ingest()) { m.status = 'rejected'; m.reason = 'arrived after close; never sent'; }
      }, { breakDeadHolder: true });
    } catch (e) { this.fail('mailbox-mutex', e.message); }
    const ex = this.state.exit;
    if (this.child?.pid) {
      if (!this.state.init) this.fail('missing-init', 'no initialization event observed; model/provider initialization was not verified');
      if (!ex) this.fail('child-exit-unknown', 'no exit status observed');
      else if (ex.signal) this.fail('signal-exit', `child terminated by ${ex.signal}`);
      else if (ex.code !== 0) this.fail('nonzero-exit', `child exit code ${ex.code}`);
      if (!this.stdinEnded) this.fail('child-exited-early', 'child exited before the runner closed stdin');
      if (!this.state.results.length) this.fail('missing-result', 'no result event observed');
    }
    for (const x of settleMessages(this.state.messages, 'never sent before terminal state')) this.fail(x.code, x.detail);
    const artifacts = checkArtifacts(this.state.expect);
    for (const a of artifacts) if (!a.ok) this.fail('artifact', `${a.path}: ${a.reason}`);
    let ok = this.state.failures.length === 0;
    this.state.phase = 'terminal';
    this.state.terminalAt = new Date().toISOString();
    this.save();
    const lockReleased = releaseSessionLock(this.intent.lockFile, this.intent.runId);
    const completion = {
      schema: 1, runId: this.intent.runId, runDir: this.runDir, sessionId: this.sessionId, model: this.model,
      sessionMode: this.intent.sessionMode, resumeFrom: this.intent.resumeFrom, cwd: this.intent.cwd,
      ok, failures: this.state.failures, exit: ex, results: this.state.results,
      finalTextFile: fs.existsSync(this.f.final) ? this.f.final : null,
      messages: summarizeMessages(this.state.messages), expect: this.state.expect, expectHistory: this.state.expectHistory,
      artifacts, sessionLockReleased: lockReleased, completedAt: this.state.terminalAt,
      reviewStatus: 'pending', review: null,
    };
    try { createExclusive(this.f.completion, json(completion)); } catch (e) {
      this.fail('completion-write-failed', `durable completion could not be saved: ${e.message}`);
      ok = false;
      this.save();
    }
    this.log('terminal', { ok });
    this.emit('terminal', { ok, failures: this.state.failures.map(x => x.code), completion: this.f.completion, reviewStatus: 'pending' });
    for (const k of Object.keys(this.fd)) { fs.closeSync(this.fd[k]); this.fd[k] = null; }
    for (const sig of ['SIGINT', 'SIGTERM', 'SIGBREAK']) process.removeAllListeners(sig);
    this.resolveDone(ok ? EXIT.ok : EXIT.failed);
  }
}

// ---------- steer ----------

async function cmdSteer(o) {
  const runDir = absPath(need(o, 'run-dir'), '--run-dir');
  const { f, intent } = loadRun(runDir);
  const { text } = readTaskText(need(o, 'task'));
  const priority = o.priority ?? 'next';
  if (!PRIORITIES.includes(priority)) throw new UsageError(`--priority must be one of ${PRIORITIES.join('|')}`);
  const id = (o.id ?? randomUUID()).toLowerCase();
  if (!UUID_RE.test(id)) throw new UsageError(`--id must be a UUID: ${o.id}`);
  if (id === intent.taskMessageId) throw new UsageError('--id collides with the task message UUID');
  const replaceExpect = o['replace-expect'] ? o['replace-expect'].map(p => path.resolve(intent.cwd, p)) : null;
  const fingerprint = sha256(JSON.stringify({ text, priority, replaceExpect }));
  const reqFile = path.join(f.mailbox, `${id}.json`);
  let receipt;
  withMutex(runDir, () => {
    const existing = readJson(reqFile);
    if (existing) {
      if (existing.fingerprint !== fingerprint)
        throw new UsageError(`steer id ${id} already exists with different text/priority/expectations; refusing (no resend)`);
      receipt = { id, duplicate: true };
      return;
    }
    const state = readJson(f.state);
    const live = liveness(intent, state, readJson(f.completion));
    if (!state || !['starting', 'active'].includes(state.phase) || live !== 'active') {
      receipt = { id, status: 'rejected', reason: `owner not accepting (phase ${state?.phase ?? 'none'}, liveness ${live})` };
      fs.appendFileSync(f.rejections, JSON.stringify({ ...receipt, priority, fingerprint, at: new Date().toISOString() }) + '\n');
      return;
    }
    createExclusive(reqFile, json({ id, priority, text, textSha256: sha256(text), replaceExpect, fingerprint,
      enqueuedAt: new Date().toISOString(), steerPid: process.pid }));
    receipt = { id, status: 'queued' };
  });
  if (receipt.status !== 'rejected') {
    const deadline = Date.now() + (o['wait-seconds'] ?? 0) * 1000;
    for (;;) {
      const m = readJson(f.state)?.messages?.[id];
      receipt.status = m?.status ?? 'queued';
      if (m?.reason) receipt.reason = m.reason;
      if (SETTLED.has(receipt.status) || receipt.status === 'replayed' || Date.now() >= deadline || readJson(f.completion)) break;
      await sleep(50);
    }
    receipt.status = readJson(f.state)?.messages?.[id]?.status ?? receipt.status;
  }
  out({ ...receipt, priority, runDir, note: DELIVERY_NOTE });
  return ['rejected', 'unaccounted', 'send-failed', 'replayed-unanswered'].includes(receipt.status) ? EXIT.failed : EXIT.ok;
}

// ---------- status / wait / review / reconcile ----------

function statusOf(runDir) {
  const { f, intent, state, completion } = loadRun(runDir);
  const live = liveness(intent, state, completion);
  const pending = readRequests(runDir).filter(r => !state?.messages?.[r.id]).map(r => ({ id: r.id, kind: 'steer', priority: r.priority, status: 'queued' }));
  return {
    runDir, runId: intent.runId, phase: state?.phase ?? 'unknown', liveness: live,
    needsReconcile: live === 'owner-dead' || live === 'owner-unknown',
    owner: { pid: state?.ownerPid ?? intent.ownerPid, alive: pidAlive(state?.ownerPid ?? intent.ownerPid),
      heartbeatAgeMs: state ? Date.now() - state.heartbeatAt : null },
    child: { pid: state?.childPid ?? null, alive: pidAlive(state?.childPid), exit: state?.exit ?? null },
    sessionId: intent.sessionId, model: intent.model, sessionMode: intent.sessionMode, resumeFrom: intent.resumeFrom,
    cwd: intent.cwd, init: state?.init ?? null,
    messages: [...summarizeMessages(completion?.messages ? Object.fromEntries(completion.messages.map(m => [m.id, m])) : state?.messages), ...pending],
    results: state?.results ?? [], failures: completion?.failures ?? state?.failures ?? [],
    artifacts: completion?.artifacts ?? checkArtifacts(state?.expect ?? intent.expect),
    ok: completion ? completion.ok : null,
    reviewStatus: completion?.reviewStatus ?? null, completion: completion ? f.completion : null,
    finalTextFile: completion?.finalTextFile ?? null,
  };
}

async function cmdStatus(o) {
  const s = statusOf(absPath(need(o, 'run-dir'), '--run-dir'));
  out(s);
  return s.needsReconcile ? EXIT.needsReconcile : EXIT.ok;
}

async function cmdWait(o) {
  const runDir = absPath(need(o, 'run-dir'), '--run-dir');
  const deadline = Date.now() + (o['timeout-seconds'] ?? Infinity) * 1000;
  for (;;) {
    let s = statusOf(runDir);
    if (s.liveness === 'terminal') {
      // give the owner a moment to exit so an immediate resume sees no live prior owner
      const until = Math.min(deadline, Date.now() + 10_000);
      while (s.owner.alive && Date.now() < until) { await sleep(50); s = statusOf(runDir); }
      out({ wait: 'terminal', ...s });
      return s.ok ? EXIT.ok : EXIT.failed;
    }
    if (s.needsReconcile) { out({ wait: 'owner-not-live', ...s }); return EXIT.needsReconcile; }
    if (Date.now() >= deadline) { out({ wait: 'timeout', note: 'owner left running; nothing was resent', ...s }); return EXIT.waitTimeout; }
    await sleep(100);
  }
}

async function cmdReview(o) {
  const runDir = absPath(need(o, 'run-dir'), '--run-dir');
  const note = (o.note ?? '').trim();
  if (!note) throw new UsageError('--note with an explicit independent review note is required');
  const { f, completion } = loadRun(runDir);
  if (!completion) throw new UsageError('run is not terminal; review is only allowed after completion (use wait)');
  const review = { reviewedAt: new Date().toISOString(), note, delegateOk: completion.ok, failures: completion.failures.map(x => x.code) };
  try { createExclusive(f.review, json(review)); } catch (e) {
    if (e.code === 'EEXIST') throw new UsageError(`already reviewed: ${JSON.stringify(readJson(f.review))}`);
    throw e;
  }
  writeJsonAtomic(f.completion, { ...completion, reviewStatus: 'reviewed', review });
  out({ runDir, reviewStatus: 'reviewed', delegateOk: completion.ok, review });
  return EXIT.ok;
}

async function cmdReconcile(o) {
  const runDir = absPath(need(o, 'run-dir'), '--run-dir');
  const note = (o.note ?? '').trim();
  if (!note) throw new UsageError('--note explaining the reconciliation is required');
  const { f, intent, state: st, completion } = loadRun(runDir);
  if (completion) throw new UsageError('run is already terminal; nothing to reconcile');
  const live = liveness(intent, st, null);
  if (live === 'active') throw new UsageError('owner is alive and heartbeating; refusing to reconcile');
  if (live === 'owner-unknown' && !o['assume-owner-dead'])
    throw new UsageError(`owner pid is alive but heartbeat is stale; pass --assume-owner-dead only if you verified it is not the owner`);
  if (st?.childPid && pidAlive(st.childPid))
    throw new UsageError(`Claude child pid ${st.childPid} is still alive; stop it or let it exit before reconciling`);
  const state = st ?? { runId: intent.runId, phase: 'unknown', ownerPid: intent.ownerPid, messages: {}, results: [], failures: [], expect: intent.expect };
  withMutex(runDir, () => {
    state.phase = 'closing';
    writeJsonAtomic(f.state, state);
    for (const r of readRequests(runDir)) {
      if (!state.messages[r.id]) state.messages[r.id] = { id: r.id, kind: 'steer', priority: r.priority, status: 'rejected', reason: 'owner dead; never ingested' };
    }
  }, { breakDeadHolder: true });
  const failures = [...state.failures, ...settleMessages(state.messages, 'owner dead; never sent'),
    { code: 'owner-dead-reconciled', detail: note }];
  const artifacts = checkArtifacts(state.expect);
  state.phase = 'terminal';
  state.failures = failures;
  state.reconciled = { at: new Date().toISOString(), note, priorLiveness: live };
  writeJsonAtomic(f.state, state);
  const lockReleased = releaseSessionLock(intent.lockFile, intent.runId);
  const comp = { schema: 1, runId: intent.runId, runDir, sessionId: intent.sessionId, model: intent.model,
    sessionMode: intent.sessionMode, resumeFrom: intent.resumeFrom, cwd: intent.cwd, ok: false, failures,
    exit: state.exit ?? null, results: state.results, finalTextFile: fs.existsSync(f.final) ? f.final : null,
    messages: summarizeMessages(state.messages), expect: state.expect, artifacts, sessionLockReleased: lockReleased,
    reconciled: state.reconciled, completedAt: state.reconciled.at, reviewStatus: 'pending', review: null };
  createExclusive(f.completion, json(comp));
  out({ runDir, reconciled: true, sessionLockReleased: lockReleased, failures: failures.map(x => x.code) });
  return EXIT.ok;
}

// ---------- main ----------

async function main(argv) {
  const [cmd, ...rest] = argv;
  if (!cmd || cmd === '--help' || cmd === 'help') { process.stdout.write(HELP); return EXIT.ok; }
  const o = parseArgs(cmd, rest);
  return { run: cmdRun, steer: cmdSteer, status: cmdStatus, wait: cmdWait, review: cmdReview, reconcile: cmdReconcile }[cmd](o);
}

main(process.argv.slice(2)).then(
  code => { process.exitCode = code; },
  err => {
    out({ ok: false, error: err.message, kind: err instanceof UsageError ? 'usage' : 'internal' });
    process.exitCode = err instanceof UsageError ? EXIT.usage : EXIT.failed;
  });
