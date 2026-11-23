import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
import { spawnSync } from 'node:child_process';
import { parseArgs } from 'node:util';
import { pathToFileURL } from 'node:url';

const help = `agent-delegation google run --cwd DIR --model GEMINI_SLUG --task FILE --run-dir NEW_DIR
agent-delegation google run --resume-from PREVIOUS_DIR --task FILE --run-dir NEW_DIR
agent-delegation google review --run-dir DIR --note TEXT
agent-delegation google reconcile --run-dir DIR --note TEXT
Optional: --expect FILE (repeatable), --timeout-seconds N (default 600)

Uses the signed-in Antigravity CLI (agy). Run agy models to see available slugs.
Review final.txt, stderr.txt and any requested files before accepting the work.`;
const uuid = /^[a-f0-9]{8}-(?:[a-f0-9]{4}-){3}[a-f0-9]{12}$/i;
const routeOverrides = new Set(['GEMINI_API_KEY', 'GOOGLE_API_KEY', 'GOOGLE_GEMINI_BASE_URL',
  'AGY_ADC_AUTH', 'GOOGLE_APPLICATION_CREDENTIALS', 'GOOGLE_GENAI_USE_VERTEXAI']);

function record(dir, name, value) {
  fs.writeFileSync(path.join(dir, name), JSON.stringify(value, null, 2) + '\n', {flag: 'wx', mode: 0o600});
}
function read(dir, name) {
  return JSON.parse(fs.readFileSync(path.join(dir, name), 'utf8'));
}

async function readTurn(file, model, previousId) {
  let id, result, observedInput = false, toolFailed = false;
  const lines = readline.createInterface({input: fs.createReadStream(file), crlfDelay: Infinity});
  for await (const line of lines) {
    if (!line.trim()) continue;
    const event = JSON.parse(line);
    if (result) throw new Error('Unexpected output after the terminal result');
    if (event.event === 'init') {
      if (id || !uuid.test(event.conversation_id) || event.init?.model !== model ||
          event.init?.permission_mode !== 'request-review' || (previousId && event.conversation_id !== previousId))
        throw new Error('Antigravity initialization did not match the requested conversation, model or permission mode');
      id = event.conversation_id;
    } else if (event.event === 'step_update') {
      const step = event.step_update;
      if (!id || step?.conversation_id !== id) throw new Error('Unexpected conversation in step update');
      if (step.step_type === 'user_input' && step.state === 'DONE') {
        if (observedInput) throw new Error('Multiple completed user inputs for one assignment');
        observedInput = true;
      }
      if (step.tool_info?.error || !['ACTIVE', 'DONE'].includes(step.state)) toolFailed = true;
    } else if (event.event === 'result') {
      result = event.result;
      if (!id || result?.conversation_id !== id || result.status !== 'SUCCESS' || typeof result.response !== 'string')
        throw new Error('Antigravity did not return a successful result for the selected conversation');
    }
  }
  if (!result || !observedInput) throw new Error('Missing completed user-input step or terminal result');
  return {id, response: result.response, toolFailed};
}

export async function run(options, invoke = spawnSync) {
  if (!options.task || !options['run-dir']) throw new Error('--task and --run-dir are required');
  const previous = options['resume-from'] && read(options['resume-from'], 'completion.json');
  if (previous && (previous.provider !== 'antigravity' || !previous.resumable || !uuid.test(previous.conversationId)))
    throw new Error('Exact resume requires a terminal Antigravity conversation receipt');
  const model = options.model ?? previous?.model;
  if (!/^gemini-[a-z0-9][a-z0-9.-]*$/.test(model ?? '')) throw new Error('Select an explicit Gemini slug from agy models');
  const cwd = fs.realpathSync(options.cwd ?? previous?.cwd ?? '');
  if ((!options.cwd && !previous) || !fs.statSync(cwd).isDirectory()) throw new Error('Select a workspace with --cwd');
  if (previous && (cwd !== previous.cwd || model !== previous.model)) throw new Error('Resume inherits cwd and model');
  const seconds = Number(options['timeout-seconds'] ?? 600);
  if (!Number.isSafeInteger(seconds) || seconds < 1 || seconds > 86400) throw new Error('Invalid timeout (1..86400 seconds)');
  const prompt = fs.readFileSync(options.task, 'utf8');
  if (!prompt.trim()) throw new Error('Empty task');
  const expected = (options.expect ?? []).map(p => path.resolve(cwd, p));
  const dir = path.resolve(options['run-dir']);
  fs.mkdirSync(dir, {mode: 0o700}); // Exclusively owns a fresh directory; never overwrites a receipt.
  const lineage = previous?.lineage ?? dir;
  const lock = path.join(lineage, 'owner.json');
  record(lineage, 'owner.json', {dir, pid: process.pid});
  let attempted = false, completed = false;
  const base = {provider: 'antigravity', model, cwd, lineage, previous: options['resume-from'] ?? null};
  try {
    const stripped = Object.keys(process.env).filter(key => routeOverrides.has(key.toUpperCase()));
    const env = {...process.env};
    for (const key of stripped) delete env[key];
    record(dir, 'intent.json', {...base, expected, stripped, requestedAt: new Date().toISOString()});
    // /usage is answered by the CLI itself, before any user task is submitted.
    // Its output may identify the user, so it stays in memory.
    const auth = invoke('agy', ['-p', '/usage', '--output-format', 'text'], {
      cwd, env, windowsHide:true, shell:false, encoding:'utf8', timeout:45000,
      killSignal:'SIGKILL', stdio:['ignore','pipe','pipe'],
    });
    if (auth.error || auth.status !== 0 || !auth.stdout?.trim())
      throw new Error('Google account preflight failed; sign in with personal Google in agy and verify agy -p /usage');
    if (/Gemini API key|Gemini Enterprise|application default credentials/i.test(auth.stdout))
      throw new Error('Antigravity usage report indicates a different account route');
    fs.writeFileSync(path.join(dir, 'prompt.txt'), prompt, {flag:'wx', mode:0o600});
    const args = ['--input-format', 'stream-json', '--output-format', 'stream-json', '--model', model,
      '--disable-slash-commands', '--sandbox', '--settings', JSON.stringify({toolPermission:'request-review'}),
      '--print-timeout', seconds + 's'];
    if (previous) args.push('--conversation', previous.conversationId);
    const events = path.join(dir, 'events.jsonl'), diagnostic = path.join(dir, 'stderr.txt');
    const stdout = fs.openSync(events, 'wx', 0o600), stderr = fs.openSync(diagnostic, 'wx', 0o600);
    let child;
    try {
      attempted = true;
      child = invoke('agy', args, {cwd, env, windowsHide:true, shell:false, timeout:(seconds + 5) * 1000,
        killSignal:'SIGKILL', stdio:['pipe', stdout, stderr],
        input:JSON.stringify({event:'user', message:{content:prompt}}) + '\n'});
    } finally {
      fs.closeSync(stdout);
      fs.closeSync(stderr);
    }
    if (child.error) throw new Error('Cannot complete agy process: ' + child.error.code);
    if (child.status !== 0) throw new Error('Antigravity exited unsuccessfully; inspect private run diagnostics');
    const turn = await readTurn(events, model, previous?.conversationId);
    fs.writeFileSync(path.join(dir, 'final.txt'), turn.response, {flag:'wx', mode:0o600});
    const missing = expected.filter(file => {
      const stat = fs.lstatSync(file, {throwIfNoEntry:false});
      return !stat?.isFile() || !stat.size;
    });
    const hasDiagnostics = fs.statSync(diagnostic).size > 0;
    const ok = !missing.length && !hasDiagnostics && !turn.toolFailed;
    const receipt = {...base, ok, resumable:true, conversationId:turn.id, modelEvidence:'host-selected',
      authEvidence:'operator-configured profile; CLI usage check passed', inputEvidence:'one completed user input step',
      reviewStatus:'pending', hasDiagnostics, toolFailed:turn.toolFailed, missing, final:'final.txt'};
    record(dir, 'completion.json', receipt);
    completed = true;
    return receipt;
  } catch (error) {
    record(dir, 'completion.json', {...base, ok:false, delivery:attempted ? 'unconfirmed' : 'not-started', error:error.message});
    throw error;
  } finally {
    // An ambiguous delivery needs human reconciliation before this same conversation is reused.
    if (completed || !attempted) fs.unlinkSync(lock);
  }
}

async function main(argv) {
  const [cmd, ...args] = argv;
  const targetModel = process.env.AGENT_DELEGATION_TARGET_MODEL;
  if (!cmd || cmd === '--help' || cmd === 'help') { console.log(help); return; }
  if (cmd === 'review' || cmd === 'reconcile') {
    const {values} = parseArgs({args, options:{'run-dir':{type:'string'}, note:{type:'string'}}});
    if (!values['run-dir'] || !values.note?.trim()) throw new Error(help);
    const dir = path.resolve(values['run-dir']);
    if (targetModel) {
      const intent = read(dir, 'intent.json');
      if (intent.provider !== 'antigravity' || intent.model !== targetModel)
        throw new Error('Run does not belong to the selected Google target model');
    }
    if (cmd === 'review') {
      const receipt = read(dir, 'completion.json');
      record(dir, 'review.json', {reviewedAt:new Date().toISOString(), ok:receipt.ok, note:values.note});
    } else {
      const intent = read(dir, 'intent.json');
      const owner = read(intent.lineage, 'owner.json');
      if (owner.dir !== dir) throw new Error('Lock belongs to a different run');
      try {
        process.kill(owner.pid, 0);
        throw new Error('Owner process is still present');
      } catch (error) {
        if (error.code !== 'ESRCH') throw error;
      }
      record(dir, 'reconciliation.json', {at:new Date().toISOString(), note:values.note});
      fs.unlinkSync(path.join(intent.lineage, 'owner.json'));
    }
    console.log(JSON.stringify({runDir:dir, action:cmd}));
    return;
  }
  if (cmd !== 'run') throw new Error(help);
  const options = {expect:{type:'string', multiple:true}};
  for (const key of ['cwd','model','task','run-dir','resume-from','timeout-seconds']) options[key] = {type:'string'};
  const values = parseArgs({args, options}).values;
  if (targetModel) {
    if (values.model !== undefined && values.model !== targetModel) throw new Error('Model differs from selected target');
    values.model = targetModel;
  }
  const receipt = await run(values);
  console.log(JSON.stringify(receipt));
  if (!receipt.ok) process.exitCode = 1;
}
if (process.argv[1] === '-' || (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href)) {
  main(process.argv.slice(2)).catch(error => { console.error(error.message); process.exitCode = 1; });
}
