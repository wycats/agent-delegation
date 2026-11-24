import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { get } from 'node:http';
import { copyFile, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { basename, join, resolve } from 'node:path';
import { createInterface } from 'node:readline';

// Exercise what ships: one executable, run outside the checkout without ui/build.
const source = resolve(process.argv[2]);
const dir = await mkdtemp(join(tmpdir(), 'settings-smoke-'));
try {
  const binary = join(dir, basename(source));
  const config = join(dir, 'targets.toml');
  await copyFile(source, binary);
  await writeFile(config, "[targets.sample]\nbackend = 'claude'\nmodel = 'fixture'\n");
  const server = spawn(binary, ['--config', config, 'settings', '--no-open'], {
    cwd: dir, stdio: ['ignore', 'pipe', 'inherit'], timeout: 20_000,
  });
  const closed = once(server, 'close');
  try {
    const lines = createInterface({ input: server.stdout });
    const { value } = await lines[Symbol.asyncIterator]().next();
    const url = new URL(value);
    assert.equal(url.hostname, '127.0.0.1');
    const html = await fetch(url);
    assert.equal(html.status, 200);
    const entry = (await html.text()).match(/_app\/immutable\/entry\/[\w.-]+\.js/);
    assert.ok(entry, 'HTML should reference an embedded JS entry');
    const js = await fetch(new URL(entry[0], url));
    assert.equal(js.status, 200);
    assert.match(js.headers.get('content-type'), /javascript/);
    const api = new URL('/api/targets', url);
    const headers = { Authorization: 'Bearer ' + url.hash.slice(1) };
    const targets = await fetch(api, { headers });
    assert.equal(targets.status, 200);
    assert.equal((await targets.json()).targets[0].name, 'sample');
    assert.equal((await fetch(api)).status, 403);
    const foreignOrigin = await fetch(api, { method: 'POST', headers: {
      ...headers, Origin: 'http://another-site.invalid',
    } });
    assert.equal(foreignOrigin.status, 403);
    // node:http preserves the caller's explicit Host header across Node versions.
    const [foreignHost] = await once(get(api, { headers: { ...headers, Host: 'another-site.invalid' } }), 'response');
    foreignHost.resume();
    assert.equal(foreignHost.statusCode, 403);
    assert.equal((await fetch(new URL('/api/missing', url), { headers })).status, 404);
  } finally {
    server.kill();
    await closed;
  }
} finally {
  await rm(dir, { recursive: true, force: true });
}
