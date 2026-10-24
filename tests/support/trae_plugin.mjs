// Explicit compatibility smoke test against the pinned, original npm package.
// node tests/support/trae_plugin.mjs /tmp/package/index.mjs target/debug/usage
// Only synthetic credentials and a loopback upstream; no vendor requests or browser opening.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { pathToFileURL } from 'node:url';
import { spawn, execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { once } from 'node:events';

const entry = resolve(process.argv[2] ?? 'missing-trae-package');
const program = resolve(process.argv[3] ?? 'target/debug/usage');
const metadata = JSON.parse(await readFile(join(dirname(entry), 'package.json'), 'utf8'));
assert.equal(metadata.name, '@magpie-community/opencode-trae-auth');
assert.equal(metadata.version, '0.2.17');
const root = await mkdtemp(join(tmpdir(), 'usage-trae-smoke-'));
const env = { ...process.env, HOME: root, XDG_DATA_HOME: join(root, 'data'), XDG_CONFIG_HOME: join(root, 'config'),
  USAGE_CODEX_DIR: join(root, 'manager'), USAGE_PLUGIN_DIR: join(root, 'plugins'),
  USAGE_CONFIG: join(root, 'config.toml'), USAGE_LOG_DIR: join(root, 'logs'), USAGE_LOG: 'debug',
  USAGE_GATEWAY: 'on', NO_PROXY: '127.0.0.1,localhost', no_proxy: '127.0.0.1,localhost' };
delete env.USAGE_GATEWAY_TRANSPORT;
const exec = promisify(execFile);
const cli = async (...args) => (await exec(program, args, { env, cwd: root, timeout: 20000 })).stdout;
const json = async (...args) => JSON.parse(await cli(...args));
const calls = [], errors = [];
let claimed = false;
const until = Math.floor(Date.now() / 1000) + 86400;
const server = createServer(async (req, res) => {
  try {
    const parts = []; for await (const part of req) parts.push(part);
    const body = JSON.parse(Buffer.concat(parts).toString() || '{}');
    const host = req.headers['x-test-origin'];
    const path = new URL(req.url, 'http://localhost').pathname;
    const realm = host === 'coreva-normal.trae.ai' || host === 'api-us-east.trae.ai' ? 'us'
      : host?.endsWith('.trae.cn') || host?.endsWith('.mchost.guru') ? 'cn' : 'sg';
    calls.push({ path, realm });
    let answer;
    if (path.endsWith('/oauth/ExchangeToken')) {
      assert.ok(body.DeviceInfo?.DeviceID); assert.ok(body.DeviceProof?.Signature);
      answer = { Result: { Token: 'synthetic-cn', RefreshToken: 'synthetic-refresh-cn', TokenExpireAt: until } };
    } else {
      assert.equal(req.headers.authorization, `Cloud-IDE-JWT synthetic-${realm}`);
      if (path.endsWith('/batch_get_detail_param')) {
        answer = { function_configs: [{ function: 'chat_v3', config_info_list: [{
          config_name: `model-${realm}`, display_name: `Live ${realm}`, usage: 'chat_completion',
          context_window_tokens: { dev: 32000 }, model_detail_list: [{ model_name: `model-${realm}__dev`, max_tokens: 4096 }],
        }] }] };
      } else if (path.endsWith('/ide_user_ent_usage')) {
        answer = { is_credits_billing: true, user_entitlement_pack_list: [{
          entitlement_base_info: { quota: { credits_limit: 1000 }, end_time: until }, usage: { credits_amount: 125.25 },
        }] };
      } else if (path.endsWith('/user_current_entitlement_list')) {
        answer = { user_entitlement_pack_list: [{ display_desc: 'Pro',
          entitlement_base_info: { quota: { basic_usage_limit: 20, bonus_usage_limit: 5 }, end_time: until },
          usage: { basic_usage_amount: 3, bonus_usage_amount: 0.25 },
        }] };
      } else if (path.endsWith('/checkin_credits/status')) {
        assert.equal(body.req_source, 1);
        answer = { enable: true, checked_in: claimed, credits: 100, extra_credits: 10 };
      } else if (path.endsWith('/checkin_credits/claim')) {
        assert.equal(body.req_source, 1); assert.ok(!claimed); claimed = true;
        answer = { credits: 100, extra_credits: 10 };
      } else if (path.endsWith('/llm_utils_chat')) {
        assert.equal(body.model, `model-${realm}`);
        assert.deepEqual(body.tools.map(t => t.function.name), ['functions__read', 'functions__exec']);
        res.writeHead(200, { 'content-type': 'text/event-stream' });
        const response = realm === 'cn'
          ? '<tool_call>{"name":"functions__exec","arguments":{"input":"print(1)"}}</tool_call>'
          : realm === 'sg'
            ? '<tool_call>{"name":"functions__read","arguments":{"path":"a"}}</tool_call>'
            : `hello-${realm}`;
        return res.end(`event: output\ndata: ${JSON.stringify({ response })}\n\nevent: token_usage\ndata: {"prompt_tokens":10,"completion_tokens":5}\n\nevent: done\ndata: {"finish_reason":"stop"}\n\n`);
      } else throw new Error(`unexpected path ${path}`);
    }
    res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(answer));
  } catch (error) { errors.push(error); res.writeHead(500).end('{}'); }
});
server.listen(0, '127.0.0.1'); await once(server, 'listening');
const origin = `http://127.0.0.1:${server.address().port}`;
const wrapper = join(root, 'trae.mjs');
await writeFile(wrapper, `
import { TraeCNAuthPlugin, TraeGlobalAuthPlugin, _internal } from ${JSON.stringify(pathToFileURL(entry).href)};
_internal.ownClient.paths = () => []; // never inspect this machine's desktop account
const upstream = globalThis.fetch;
globalThis.fetch = (input, init = {}) => {
  const url = new URL(input instanceof Request ? input.url : String(input));
  if (!/\\.(trae\\.cn|trae\\.ai|mchost\\.guru)$/.test(url.hostname)) throw new Error('external network disabled');
  const headers = new Headers(init.headers); headers.set('x-test-origin', url.hostname);
  return upstream(${JSON.stringify(origin)} + url.pathname + url.search, { ...init, headers });
};
export { TraeCNAuthPlugin, TraeGlobalAuthPlugin };
`);
await writeFile(env.USAGE_CONFIG, '');
try {
  await cli('plugins', 'add', 'trae', wrapper, '--runtime', process.execPath);
  const registry = join(env.USAGE_PLUGIN_DIR, 'plugins.json');
  const plugins = JSON.parse(await readFile(registry, 'utf8'));
  plugins[0].package = metadata.name; plugins[0].version = metadata.version;
  await writeFile(registry, JSON.stringify(plugins));
  const providers = await json('plugins', 'providers', 'trae');
  assert.deepEqual(providers.map(p => p.id), ['trae-cn', 'trae-global']);
  assert.equal(providers[0].capabilities.checkin_status, true);
  assert.equal(providers[1].capabilities.checkin, false);
  const accounts = [];
  for (const realm of ['cn', 'sg', 'us', 'cn']) {
    const provider = realm === 'cn' ? 'trae-cn' : 'trae-global';
    const child = spawn(program, ['plugins', 'login', 'trae', provider, '--no-browser'], { env, cwd: root });
    let output = '', stderr = '', callback;
    child.stderr.on('data', b => { stderr += b; });
    child.stdout.on('data', b => {
      output += b;
      const address = output.match(/https:\/\/www\.trae\.(?:cn|ai)\/authorization\?[^\s]+/);
      if (callback || !address) return;
      callback = (async () => {
        const auth = new URL(address[0]);
        assert.equal(auth.hostname, realm === 'cn' ? 'www.trae.cn' : 'www.trae.ai');
        const url = new URL(auth.searchParams.get('auth_callback_url'));
        assert.equal(url.hostname, '127.0.0.1');
        url.searchParams.set('userJwt', JSON.stringify({ Token: `synthetic-${realm}`, RefreshToken: `synthetic-refresh-${realm}`, TokenExpireAt: until }));
        url.searchParams.set('userInfo', JSON.stringify({ UserID: `uid-${realm}`, ScreenName: `Account-${realm}`, AIRegion: realm === 'us' ? 'US' : 'SG' }));
        assert.equal((await fetch(url)).status, 200);
      })();
      callback.catch(() => child.kill());
    });
    const timer = setTimeout(() => child.kill(), 15000);
    const [code] = await once(child, 'exit'); clearTimeout(timer);
    assert.ok(callback, 'browser authorization URL was not returned'); await callback;
    assert.equal(code, 0, stderr);
    const account = (await json('plugins', 'accounts')).find(a => a.name === `Account-${realm}`);
    assert.ok(account); assert.equal(account.provider, provider);
    const existing = accounts.find(a => a.name === account.name);
    if (existing) assert.equal(existing.id, account.id, 'same identity must reuse the existing account');
    else accounts.push(account);
    const models = await json('plugins', 'models', account.id);
    assert.ok(models.some(m => m.id === `model-${realm}`), 'must use the live account model list');
    const usage = await json('plugins', 'usage', account.id);
    assert.equal(usage.windows[0].unit, realm === 'cn' ? 'credits' : 'usd');
    assert.equal(usage.windows[0].amount, realm === 'cn' ? 125.25 : 3.25);
    assert.equal(usage.windows[0].limit, realm === 'cn' ? 1000 : 25);
    if (existing) {
      await assert.rejects(cli('plugins', 'bind', account.id, '--name', 'Repeated', '--model', `model-${realm}`),
        error => error.stderr.includes(`Provider 配置已存在：Trae-${realm}`));
      assert.equal((await json('plugins', 'accounts')).length, 3);
      assert.equal((await json('codex', 'list', '--json')).length, 3);
    } else {
      await cli('plugins', 'bind', account.id, '--name', `Trae-${realm}`, '--model', `model-${realm}`);
    }
  }
  const cn = accounts[0];
  for (let i = 0; i < 2; i++) assert.equal((await json('plugins', 'checkin', cn.id)).outcome, 'ready');
  assert.ok(!claimed, 'read-only status must not claim');
  const reward = await json('plugins', 'checkin', cn.id, '--confirm');
  assert.equal(reward.outcome, 'claimed'); assert.equal(reward.credit, 110);
  assert.equal((await json('plugins', 'checkin', cn.id, '--confirm')).outcome, 'done');
  assert.equal(calls.filter(c => c.path.endsWith('/claim')).length, 1);
  const mock = join(root, 'codex');
  await writeFile(mock, `#!/usr/bin/env python3
import os, sys, json, pathlib, tomllib, urllib.request, urllib.error
if '--help' in sys.argv: print('--no-daemon --profile'); sys.exit(0)
if '--version' in sys.argv: print('codex-cli 0.159.3'); sys.exit(0)
c=tomllib.loads((pathlib.Path(os.environ['CODEX_HOME'])/'config.toml').read_text())
p=c['model_provider']; base=c['model_providers'][p]['base_url']
catalog=None
for i,arg in enumerate(sys.argv[:-1]):
 if arg=='-c' and '.base_url=' in sys.argv[i+1]: base=tomllib.loads(sys.argv[i+1])['model_providers'][p]['base_url']
 if arg=='-c' and '.model_catalog_url=' in sys.argv[i+1]: catalog=tomllib.loads(sys.argv[i+1])['model_providers'][p]['model_catalog_url']
assert catalog and catalog.startswith(base+'/codex/models'), catalog
try:
 urllib.request.urlopen(catalog,timeout=10)
 raise AssertionError('catalog must reject requests without the run token')
except urllib.error.HTTPError as error: assert error.code==401
catalog_request=urllib.request.Request(catalog,headers={'authorization':'Bearer '+os.environ['USAGE_CODEX_API_KEY']})
with urllib.request.urlopen(catalog_request,timeout=10) as r: listed=json.load(r)['models']
assert [x['slug'] for x in listed]==[c['model']], listed
assert listed[0]['visibility']=='list' and listed[0]['base_instructions'], listed
body=json.dumps({'model':c['model'],'input':'hello','stream':True,'tools':[{'type':'namespace','name':'functions','tools':[{'type':'function','name':'read','parameters':{'type':'object','properties':{'path':{'type':'string'}}}},{'type':'custom','name':'exec'}]},{'type':'web_search','external_web_access':True}]}).encode()
req=urllib.request.Request(base+'/responses',data=body,headers={'authorization':'Bearer '+os.environ['USAGE_CODEX_API_KEY'],'content-type':'application/json'})
with urllib.request.urlopen(req,timeout=10) as r: result=r.read().decode()
completed=[json.loads(line[6:])['response'] for line in result.splitlines() if line.startswith('data: ') and json.loads(line[6:]).get('type')=='response.completed']
assert len(completed)==1, result
items=completed[0]['output']
if c['model'].endswith('-cn'): assert any(x.get('type')=='custom_tool_call' and x.get('namespace')=='functions' and x.get('name')=='exec' and x.get('input')=='print(1)' for x in items), result
elif c['model'].endswith('-sg'): assert any(x.get('type')=='function_call' and x.get('namespace')=='functions' and x.get('name')=='read' and json.loads(x.get('arguments','{}')).get('path')=='a' for x in items), result
else: assert any(x.get('type')=='message' and 'hello-us' in str(x.get('content')) for x in items), result
print('trae-inference-ok')
`, { mode: 0o700 });
  for (const realm of ['cn', 'sg', 'us']) {
    assert.match(await cli('codex', 'run', `Trae-${realm}`, '--binary', mock), /trae-inference-ok/);
  }
  assert.equal(calls.filter(c => c.path.endsWith('/llm_utils_chat')).length, 3);
  assert.deepEqual(errors, []);
  console.log('PASS: original Trae 0.2.17, CN/SG/US browser callbacks, account/binding deduplication, live models and Codex catalog, Credits/USD, explicit check-in, namespaced function/custom tool round-trip beside Codex web_search');
} finally {
  try { const status = await json('gateway', 'status'); if (status.pid) process.kill(status.pid, 'SIGTERM'); } catch {}
  server.closeAllConnections(); await new Promise(r => server.close(r));
  await rm(root, { recursive: true, force: true });
}
