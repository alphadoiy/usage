// node tests/support/zcode_plugin.mjs /tmp/package/index.mjs target/debug/usage
// Original pinned plugin, synthetic accounts, and loopback upstream only.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { pathToFileURL } from 'node:url';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { once } from 'node:events';
const entry = resolve(process.argv[2] ?? 'missing-zcode-package');
const program = resolve(process.argv[3] ?? 'target/debug/usage');
const metadata = JSON.parse(await readFile(join(dirname(entry), 'package.json'), 'utf8'));
assert.equal(metadata.name, '@magpie-community/opencode-zcode-auth');
assert.equal(metadata.version, '0.1.27');
const root = await mkdtemp(join(tmpdir(), 'usage-zcode-smoke-'));
const env = { ...process.env, HOME: root, XDG_DATA_HOME: join(root, 'data'), XDG_CONFIG_HOME: join(root, 'config'),
  USAGE_CODEX_DIR: join(root, 'manager'), USAGE_PLUGIN_DIR: join(root, 'plugins'),
  USAGE_CONFIG: join(root, 'config.toml'), USAGE_LOG_DIR: join(root, 'logs'), USAGE_LOG: 'debug',
  USAGE_GATEWAY: 'on', NO_PROXY: '127.0.0.1,localhost', no_proxy: '127.0.0.1,localhost' };
delete env.USAGE_GATEWAY_TRANSPORT;
const exec = promisify(execFile);
const cli = async (...args) => (await exec(program, args, { env, cwd: root, timeout: 20000 })).stdout;
const json = async (...args) => JSON.parse(await cli(...args));
const calls = [], errors = [];
const server = createServer(async (req, res) => {
  try {
    const chunks = []; for await (const b of req) chunks.push(b);
    const body = JSON.parse(Buffer.concat(chunks).toString() || '{}');
    const url = new URL(req.url, 'http://localhost'), path = url.pathname;
    const host = req.headers['x-test-origin'];
    calls.push({ path, host });
    let data;
    if (path.endsWith('/oauth/cli/init')) {
      assert.ok(['zai', 'bigmodel'].includes(body.provider));
      data = { flow_id: body.provider, authorize_url: `https://${body.provider === 'zai' ? 'z.ai' : 'bigmodel.cn'}/login`, poll_interval_sec: 1 };
    } else if (path.includes('/oauth/cli/poll/')) {
      const site = path.split('/').at(-1);
      data = { status: 'ready', [site]: { access_token: 'synthetic-business-token' }, user: { email: `${site}@example.test` } };
    } else if (path === '/api/auth/z/login') data = { access_token: 'synthetic-biz' };
    else if (path.endsWith('/getCustomerInfo')) data = { organizations: [{ organizationId: 'org', projects: [
      { projectId: 'personal', projectType: 1 }, { projectId: 'team', projectType: 2 },
    ] }] };
    else if (path.endsWith('/querySubscribeDetail')) data = { status: 'EFFECTIVE', memberGrantStatus: 'VALID', productName: 'Team Pro' };
    else if (path.endsWith('/subscription/list')) data = [{ status: 'VALID', productName: 'GLM Coding Pro' }];
    else if (path.endsWith('/quota/limit')) data = { level: 'pro', limits: [{ unit: 3, number: 5, usage: 100, remaining: 75 }] };
    else if (path.endsWith('/customer-package-reset/list')) data = {};
    else if (path.includes('/api_keys/copy/')) data = { secretKey: 'secret' };
    else if (path.endsWith('/api_keys')) data = [
      { name: 'zcode-api-key', apiKey: 'personal' }, { name: 'zcode-team-api-key', apiKey: 'team', keyType: 2 },
    ];
    else if (path.endsWith('/messages')) {
      assert.equal(body.model, 'GLM-5.3');
      assert.equal(body.stream, true); assert.ok(body.max_tokens > 0);
      assert.equal(req.headers['anthropic-version'], '2023-06-01');
      assert.equal(req.headers.authorization, `Bearer ${req.headers['x-api-key']}`);
      assert.ok(req.headers['x-api-key'].endsWith('.secret'));
      assert.deepEqual(body.tools.map(t => t.name), ['functions__exec']);
      assert.equal(body.messages[0].content[0].text, 'hello');
      const events = [
        { type: 'message_start', message: { model: body.model, usage: { input_tokens: 10, cache_read_input_tokens: 2 } } },
        { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } },
        { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: '你好' } },
        { type: 'content_block_stop', index: 0 },
        { type: 'content_block_start', index: 1, content_block: { type: 'tool_use', id: 'tool1', name: 'functions__exec', input: {} } },
        { type: 'content_block_delta', index: 1, delta: { type: 'input_json_delta', partial_json: '{"input":"ls"}' } },
        { type: 'content_block_stop', index: 1 },
        { type: 'message_delta', delta: { stop_reason: 'tool_use' }, usage: { output_tokens: 5 } },
        { type: 'message_stop' },
      ];
      return res.writeHead(200, { 'content-type': 'text/event-stream' }).end(events.map(v => `event: ${v.type}\ndata: ${JSON.stringify(v)}\n\n`).join(''));
    } else throw new Error(`unexpected mock request ${host}${path}`);
    res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify({ code: 200, data }));
  } catch (error) { errors.push(error); res.writeHead(500).end('{}'); }
});
server.listen(0, '127.0.0.1'); await once(server, 'listening');
const origin = `http://127.0.0.1:${server.address().port}`;
const wrapper = join(root, 'zcode.mjs');
await writeFile(wrapper, `
import { ZCodeAuthPlugin, _internal } from ${JSON.stringify(pathToFileURL(entry).href)};
_internal.config({config: {
  providerConfigRules: {providerRules: ['zai', 'bigmodel'].map(site => ({providerId: 'account:' + site + '-individual-coding-plan', builtinModelIds: ['GLM-5.3']}))},
  modelConfigRules: {modelRules: [{modelMatch: 'GLM-5.3', config: {properties: {inputFormat: {supportsImage: true}}}}]}
}});
const upstream = globalThis.fetch;
globalThis.fetch = (input, init = {}) => {
  const url = new URL(input instanceof Request ? input.url : String(input));
  if (!['api.z.ai', 'open.bigmodel.cn', 'bigmodel.cn', 'zcode.z.ai'].includes(url.hostname)) throw new Error('external network disabled');
  const headers = new Headers(init.headers); headers.set('x-test-origin', url.hostname);
  return upstream(${JSON.stringify(origin)} + url.pathname + url.search, { ...init, headers });
};
export { ZCodeAuthPlugin };
`);
await writeFile(env.USAGE_CONFIG, '');
await mkdir(join(root, '.zcode/v2'), { recursive: true });
await writeFile(join(root, '.zcode/v2/credentials.json'), '{}');
await writeFile(join(root, '.zcode/v2/config.json'), JSON.stringify({ provider: { 'builtin:zai-coding-plan': { options: { apiKey: 'desktop.secret' } } } }));
try {
  await cli('plugins', 'add', 'zcode', wrapper, '--runtime', process.execPath);
  const registry = join(env.USAGE_PLUGIN_DIR, 'plugins.json');
  const plugins = JSON.parse(await readFile(registry, 'utf8'));
  plugins[0].package = metadata.name; plugins[0].version = metadata.version;
  await writeFile(registry, JSON.stringify(plugins));
  const providers = await json('plugins', 'providers', 'zcode');
  assert.equal(providers[0].protocol, 'messages');
  assert.equal(providers[0].methods.length, 5);
  assert.equal(providers[0].capabilities.checkin, false);
  for (const [method, site, name] of [['0', 'zai', 'Zai'], ['1', 'bigmodel', 'BigModel'], ['2', 'zai', 'Team'], ['3', 'zai', 'Desktop'], ['4', 'zai', 'KeyZai'], ['4', 'bigmodel', 'KeyBigModel']]) {
    await cli('plugins', 'login', 'zcode', 'zcode', '--method', method, '--no-browser', '--name', name,
      '--input', `site=${site}`, ...(method === '4' ? ['--input', 'key=fixture.secret'] : []));
    const account = (await json('plugins', 'accounts')).find(a => a.name === name);
    assert.ok(account);
    const saved = JSON.parse(await readFile(join(env.USAGE_PLUGIN_DIR, 'accounts', account.id + '.json'), 'utf8'));
    if (method === '4') { assert.equal(saved.auth.metadata.site, site); assert.equal(saved.auth.metadata.key, undefined); }
    const models = await json('plugins', 'models', account.id);
    assert.deepEqual(models.map(m => m.id), ['GLM-5.3']);
    assert.equal(models[0].images, true);
    const usage = await json('plugins', 'usage', account.id);
    assert.equal(usage.windows[0].used, 25);
    await cli('plugins', 'bind', account.id, '--name', name, '--model', 'GLM-5.3');
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
with urllib.request.urlopen(urllib.request.Request(catalog,headers=h),timeout=10) as r: assert json.load(r)['models'][0]['slug']=='GLM-5.3'
for stream in [True,False]:
 body=json.dumps({'model':c['model'],'input':'hello','stream':stream,'tools':[{'type':'namespace','name':'functions','tools':[{'type':'custom','name':'exec'}]},{'type':'web_search'}]}).encode()
 with urllib.request.urlopen(urllib.request.Request(base+'/responses',data=body,headers=h),timeout=10) as r: raw=r.read().decode()
 result=next(json.loads(line[6:])['response'] for line in raw.splitlines() if line.startswith('data: ') and json.loads(line[6:]).get('type')=='response.completed') if stream else json.loads(raw)
 assert result['usage']['input_tokens']==12 and result['usage']['output_tokens']==5,result
 assert result['output'][1]['type']=='custom_tool_call' and result['output'][1]['input']=='ls',result
 assert result['output'][1]['namespace']=='functions',result
print('zcode-inference-ok')
`, { mode: 0o700 });
  for (const name of ['Zai', 'BigModel', 'Team', 'Desktop', 'KeyZai', 'KeyBigModel'])
    assert.match(await cli('codex', 'run', name, '--binary', mock), /zcode-inference-ok/);
  assert.equal(calls.filter(c => c.path.endsWith('/messages')).length, 12);
  assert.deepEqual(errors, []);
  console.log('PASS: original ZCode 0.1.27, Z.ai/BigModel browser and API logins, team/desktop login, model catalog, quota, Messages inference, streaming/JSON, custom tool and cache usage');
} catch (error) {
  console.error('Mock upstream failures:', errors);
  console.error('Mock requests:', calls);
  throw error;
} finally {
  try { const status = await json('gateway', 'status'); if (status.pid) process.kill(status.pid, 'SIGTERM'); } catch {}
  server.closeAllConnections(); await new Promise(r => server.close(r));
  await rm(root, { recursive: true, force: true });
}
