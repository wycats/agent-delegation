import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {run} from './runner.mjs';

const model = 'gemini-fixture';
const id = '01234567-1234-4234-abcd-0123456789ab';
const prompt = 'Explain line one.\n\nPreserve é and spaces.  \n';
function setup(t) {
  const cwd = fs.mkdtempSync(path.join(os.tmpdir(), 'google-delegate-'));
  t.after(() => fs.rmSync(cwd, {recursive:true, force:true}));
  const task = path.join(cwd, 'task.txt');
  fs.writeFileSync(task, prompt);
  return {cwd, task, model, 'run-dir':path.join(cwd, 'first')};
}

function host(alter = () => {}, inspect = () => {}) {
  return (bin, args, opts) => {
    assert.equal(bin, 'agy');
    assert.equal(opts.shell, false);
    assert.equal(opts.env.GEMINI_API_KEY, undefined);
    if (args.includes('/usage')) return {status:0, stdout:'Fixture usage report'};
    assert.equal(JSON.parse(opts.input).message.content, prompt);
    assert.equal(args[args.indexOf('--model') + 1], model);
    inspect(args, opts);
    const events = [
      {event:'init', conversation_id:id, init:{model, permission_mode:'request-review'}},
      {event:'step_update', step_update:{conversation_id:id, step_type:'user_input', state:'DONE'}},
      {event:'result', result:{conversation_id:id, status:'SUCCESS', response:'An answer.'}},
    ];
    alter(events);
    fs.writeSync(opts.stdio[1], events.map(value => JSON.stringify(value)).join('\n') + '\n');
    return {status:0};
  };
}

test('records exactly one selected turn and resumes its exact conversation', async t => {
  const options = setup(t);
  const first = await run(options, host());
  assert.equal(first.ok, true);
  assert.equal(first.modelEvidence, 'host-selected');
  assert.equal(fs.readFileSync(path.join(options['run-dir'], 'final.txt'), 'utf8'), 'An answer.');
  const next = {'run-dir':path.join(options.cwd, 'next'), task:options.task, 'resume-from':options['run-dir']};
  await run(next, host(undefined, args => assert.equal(args[args.indexOf('--conversation') + 1], id)));
  await assert.rejects(run(options, host()), /EEXIST/);
  await assert.rejects(run({...next, model:'gemini-another'}, host()), /inherits/);
});

for (const [label, alter] of [
  ['different configured model', e => { e[0].init.model = 'other'; }],
  ['missing input step', e => e.splice(1, 1)],
  ['foreign conversation', e => { e[2].result.conversation_id = 'other'; }],
  ['interrupted turn', e => { e[2].result.status = 'INTERRUPTED'; }],
  ['extra terminal result', e => e.push(e.at(-1))],
]) {
  test(label + ' rejects completion', async t => {
    const options = setup(t);
    await assert.rejects(run(options, host(alter)));
    assert.equal(JSON.parse(fs.readFileSync(path.join(options['run-dir'], 'completion.json'))).ok, false);
  });
}

test('records missing artifacts and possible soft-denials on successful exit', async t => {
  const options = setup(t);
  options.expect = ['missing.txt'];
  const receipt = await run(options, host(undefined, (_, opts) => fs.writeSync(opts.stdio[2], 'tool requires approval')));
  assert.equal(receipt.ok, false);
  assert.equal(receipt.hasDiagnostics, true);
  assert.equal(receipt.missing.length, 1);
});

test('failed account preflight never dispatches a task or retains the session lock', async t => {
  const options = setup(t);
  await assert.rejects(run(options, (_, args) => {
    assert.ok(args.includes('/usage'));
    return {status:1};
  }), /preflight/);
  assert.equal(fs.existsSync(path.join(options['run-dir'], 'owner.json')), false);
  assert.equal(JSON.parse(fs.readFileSync(path.join(options['run-dir'], 'completion.json'))).delivery, 'not-started');
});
