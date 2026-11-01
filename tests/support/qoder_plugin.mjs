// node tests/support/qoder_plugin.mjs /tmp/package/index.mjs target/debug/usage
// Pinned original package; synthetic credentials and loopback upstream only.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { pathToFileURL } from 'node:url';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { once } from 'node:events';
const entry = resolve(process.argv[2] ?? 'missing-qoder-package');
const program = resolve(process.argv[3] ?? 'target/debug/usage');
const metadata = JSON.parse(await readFile(join(dirname(entry), 'package.json'), 'utf8'));
assert.equal(metadata.name, '@magpie-community/opencode-qoder-auth');
assert.equal(metadata.version, '0.2.52');
const { _internal } = await import(pathToFileURL(entry));
const root = await mkdtemp(join(tmpdir(), 'usage-qoder-smoke-'));
const env = { ...process.env, HOME: root, XDG_DATA_HOME: join(root, 'data'), XDG_CONFIG_HOME: join(root, 'config'),
  USAGE_CODEX_DIR: join(root, 'manager'), USAGE_PLUGIN_DIR: join(root, 'plugins'),
  USAGE_CONFIG: join(root, 'config.toml'), USAGE_LOG_DIR: join(root, 'logs'), USAGE_LOG: 'debug',
  USAGE_GATEWAY: 'on', NO_PROXY: '127.0.0.1,localhost', no_proxy: '127.0.0.1,localhost' };
delete env.USAGE_GATEWAY_TRANSPORT;
const exec = promisify(execFile);
const cli = async (...args) => (await exec(program, args, { env, cwd: root, timeout: 25000 })).stdout;
const json = async (...args) => JSON.parse(await cli(...args));
const calls = [], errors = [];
const server = createServer(async (req, res) => {
  try {
    const chunks = []; for await (const b of req) chunks.push(b);
    const raw = Buffer.concat(chunks).toString();
    const path = new URL(req.url, 'http://localhost').pathname;
    const host = req.headers['x-test-origin'];
    calls.push({ path, host });
    const token = suffix => `synthetic-${host}-${suffix}`;
    let data;
    if (path === '/api/v1/deviceToken/poll') data = { token: token('device'), refresh_token: token('device-refresh'), user_id: token('uid') };
    else if (path === '/api/v1/me/jobToken') data = { token: token('job'), refresh_token: token('refresh'), expires_in: 3600000 };
    else if (path === '/api/v1/jobToken/exchange') {
      assert.equal(JSON.parse(raw).personal_token, 'pt-synthetic');
      data = { token: token('pat-job'), refresh_token: token('pat-refresh'), expires_in: 3600000 };
    } else if (path === '/api/v1/jobToken/refresh') {
      assert.ok(JSON.parse(raw).refresh_token.startsWith(`synthetic-${host}-`));
      data = { token: token('renewed'), refresh_token: token('renewed-refresh'), expires_in: 3600000 };
    } else if (path === '/api/v1/userinfo') data = { id: token('uid'), email: `${host}@example.test`, name: 'Fixture' };
    else if (path === '/algo/api/v2/model/list') data = { app: [
      { key: 'fast', display_name: 'Fast', enable: true, max_input_tokens: 200000, is_vl: true, price: 0.8 },
    ] };
    else if (path === '/sash/api/v2/me/usage') {
      assert.match(req.headers.authorization, /^Bearer synthetic-/);
      data = { displayMode: 'qoder', qoderUsage: { userType: 'Pro', userQuota: { total: 1000, used: 250 }, addOnQuota: { total: 500, used: 50 } } };
    } else if (path === '/sash/api/v1/me/campaigns') {
      // Only an explicitly confirmed action may reach this endpoint.
      assert.equal(req.method, 'GET'); data = { campaigns: [] };
    } else if (path.endsWith('/agent_chat_generation')) {
      const body = JSON.parse(_internal.decodeBody(raw));
      assert.ok(req.headers.authorization); assert.ok(req.headers['cosy-clienttype']);
      assert.ok(body.messages.some(m => m.role === 'user' && JSON.stringify(m.content).includes('hello')));
      assert.deepEqual(body.tools.map(t => t.function.name), ['functions__exec']);
      assert.ok(body.parameters.max_tokens > 0);
      const events = [
        { choices: [{ delta: { content: '你好' } }] },
        { choices: [{ delta: { tool_calls: [{ index: 0, id: 'tool1', type: 'function', function: { name: 'functions__exec', arguments: '{"input":"ls"}' } }] }, finish_reason: 'tool_calls' }] },
        { usage: { prompt_tokens: 12, completion_tokens: 5, prompt_tokens_details: { cached_tokens: 2 } } },
      ].map(chunk => ({ statusCodeValue: 200, body: JSON.stringify(chunk) }));
      events.push({ statusCodeValue: 200, body: '[DONE]' });
      return res.writeHead(200, { 'content-type': 'text/event-stream' }).end(events.map(v => `data: ${JSON.stringify(v)}\n\n`).join(''));
    } else throw new Error(`unexpected mock request ${host}${path}`);
    res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(data));
  } catch (error) { errors.push(error); res.writeHead(500).end('{}'); }
});
server.listen(0, '127.0.0.1'); await once(server, 'listening');
const origin = `http://127.0.0.1:${server.address().port}`;
const wrapper = join(root, 'qoder.mjs');
await writeFile(wrapper, `
import { QoderAuthPlugin, QoderCNAuthPlugin } from ${JSON.stringify(pathToFileURL(entry).href)};
const upstream = globalThis.fetch;
globalThis.fetch = (input, init = {}) => {
  const url = new URL(input instanceof Request ? input.url : String(input));
  if (!['openapi.qoder.sh', 'api3.qoder.sh', 'openapi.qoder.com.cn', 'gateway.qoder.com.cn',
    'acme-openapi.vpc.qoder.com.cn', 'acme-gateway.vpc.qoder.com.cn'].includes(url.hostname)) throw new Error('external network disabled');
  const headers = new Headers(init.headers); headers.set('x-test-origin', url.hostname);
  return upstream(${JSON.stringify(origin)} + url.pathname + url.search, { ...init, headers });
};
export * from ${JSON.stringify(pathToFileURL(entry).href)};
`);
await writeFile(env.USAGE_CONFIG, '');
try {
  await cli('plugins', 'add', 'qoder', wrapper, '--runtime', process.execPath);
  const registry = join(env.USAGE_PLUGIN_DIR, 'plugins.json');
  const plugins = JSON.parse(await readFile(registry, 'utf8'));
  plugins[0].package = metadata.name; plugins[0].version = metadata.version;
  await writeFile(registry, JSON.stringify(plugins));
  const allProviders = await json('plugins', 'providers', 'qoder');
  assert.equal(allProviders.length, 4); // Original package also exports QwenWork providers.
  const providers = allProviders.filter(p => ['qoder', 'qoder-cn'].includes(p.id));
  assert.deepEqual(providers.map(p => [p.id, p.methods.length, p.protocol]), [['qoder', 2, 'chat'], ['qoder-cn', 3, 'chat']]);
  for (const p of providers) {
    assert.equal(p.capabilities.checkin, true);
    assert.equal(p.capabilities.checkin_status, false);
    assert.equal(p.methods[1].prompts[0].type, 'password');
  }
  const mock = join(root, 'codex');
  await writeFile(mock, `#!/usr/bin/env python3
import os,sys,json,pathlib,tomllib,urllib.request
if '--help' in sys.argv: print('--no-daemon --profile'); sys.exit(0)
if '--version' in sys.argv: print('codex-cli 0.159.3'); sys.exit(0)
c=tomllib.loads((pathlib.Path(os.environ['CODEX_HOME'])/'config.toml').read_text())
p=c['model_provider']; base=c['model_providers'][p]['base_url']; catalog=None
for i,arg in enumerate(sys.argv[:-1]):
 if arg=='-c' and '.base_url=' in sys.argv[i+1]: base=tomllib.loads(sys.argv[i+1])['model_providers'][p]['base_url']
 if arg=='-c' and '.model_catalog_url=' in sys.argv[i+1]: catalog=tomllib.loads(sys.argv[i+1])['model_providers'][p]['model_catalog_url']
h={'authorization':'Bearer '+os.environ['USAGE_CODEX_API_KEY'],'content-type':'application/json'}
with urllib.request.urlopen(urllib.request.Request(catalog,headers=h),timeout=10) as r: assert json.load(r)['models'][0]['slug']=='fast'
for stream in [True,False]:
 body=json.dumps({'model':c['model'],'input':'hello','stream':stream,'tools':[{'type':'namespace','name':'functions','tools':[{'type':'custom','name':'exec'}]},{'type':'web_search'}]}).encode()
 with urllib.request.urlopen(urllib.request.Request(base+'/responses',data=body,headers=h),timeout=10) as r: raw=r.read().decode()
 result=next(json.loads(line[6:])['response'] for line in raw.splitlines() if line.startswith('data: ') and json.loads(line[6:]).get('type')=='response.completed') if stream else json.loads(raw)
 assert result['usage']['input_tokens']==12 and result['usage']['output_tokens']==5,result
 assert result['output'][1]['type']=='custom_tool_call' and result['output'][1]['input']=='ls',result
 assert result['output'][1]['namespace']=='functions',result
print('qoder-inference-ok')
`, { mode: 0o700 });
  for (const [provider, method, name] of [
    ['qoder', '0', 'International'], ['qoder-cn', '0', 'China'],
    ['qoder', '1', 'InternationalPAT'], ['qoder-cn', '1', 'ChinaPAT'], ['qoder-cn', '2', 'VPC'],
  ]) {
    await cli('plugins', 'login', 'qoder', provider, '--method', method, '--no-browser', '--name', name,
      ...(method === '1' ? ['--input', 'pat=pt-synthetic'] : method === '2' ? ['--input', 'vpc=acme'] : []));
    const account = (await json('plugins', 'accounts')).find(a => a.provider === provider && a.name === name.replace('PAT', ''));
    assert.ok(account);
    const file = join(env.USAGE_PLUGIN_DIR, 'accounts', account.id + '.json');
    const saved = JSON.parse(await readFile(file, 'utf8'));
    assert.equal(saved.auth.type, 'oauth');
    if (method === '1') assert.equal(saved.auth.patAuth, true);
    if (method === '2') assert.equal(saved.auth.vpc, 'acme');
    // PAT and browser login for one UID reuse the same record. Force an expired job token.
    saved.auth.expires = 1; await writeFile(file, JSON.stringify(saved));
    const models = await json('plugins', 'models', account.id);
    assert.deepEqual(models.map(m => m.id), ['fast']); assert.equal(models[0].images, true);
    const renewed = JSON.parse(await readFile(file, 'utf8'));
    assert.ok(renewed.auth.access.endsWith('-renewed')); assert.ok(renewed.generation > saved.generation);
    const usage = await json('plugins', 'usage', account.id);
    assert.equal(usage.plan, 'Pro'); assert.equal(usage.windows[0].used, 20);
    assert.equal(usage.windows[0].display, '300 / 1500 credits');
    const preview = await json('plugins', 'checkin', account.id);
    assert.equal(preview.outcome, 'ready');
    assert.equal(calls.filter(c => c.path.includes('/campaigns')).length, 0);
    if (method !== '1') await cli('plugins', 'bind', account.id, '--name', name, '--model', 'fast');
    assert.match(await cli('codex', 'run', name.replace('PAT', ''), '--binary', mock), /qoder-inference-ok/);
  }
  assert.equal(calls.filter(c => c.path.endsWith('/agent_chat_generation')).length, 10);
  assert.deepEqual(errors, []);
  assert.equal(calls.filter(c => c.path.includes('/campaigns')).length, 0);
  console.log('PASS: original Qoder 0.2.52, international/CN browser and PAT, enterprise VPC, credential refresh, catalog, Credits, side-effect-free check-in preview, Codex Responses/SSE and tools');
} catch (error) {
  console.error('Mock upstream failures:', errors);
  console.error('Mock requests:', calls);
  throw error;
} finally {
  try { const status = await json('gateway', 'status'); if (status.pid) process.kill(status.pid, 'SIGTERM'); } catch {}
  server.closeAllConnections(); await new Promise(r => server.close(r));
  await rm(root, { recursive: true, force: true });
}
