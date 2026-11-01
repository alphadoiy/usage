// node tests/support/subscription_plugins.mjs <codearts|raccoon|minimax> /tmp/package/index.mjs target/debug/usage
// Original pinned packages; synthetic credentials, mock browser and loopback upstream only.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { pathToFileURL } from 'node:url';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { once } from 'node:events';
const [kind, input, binary = 'target/debug/usage'] = process.argv.slice(2);
const entry = resolve(input), program = resolve(binary);
const versions = { codearts: '0.3.0', raccoon: '0.1.5', minimax: '0.1.7' };
const metadata = JSON.parse(await readFile(join(dirname(entry), 'package.json'), 'utf8'));
assert.equal(metadata.name, `@magpie-community/opencode-${kind}-auth`);
assert.equal(metadata.version, versions[kind]);
const root = await mkdtemp(join(tmpdir(), `usage-${kind}-smoke-`));
const env = { ...process.env, HOME: root, XDG_CONFIG_HOME: join(root, 'config'), XDG_DATA_HOME: join(root, 'data'),
  USAGE_CODEX_DIR: join(root, 'manager'), USAGE_PLUGIN_DIR: join(root, 'plugins'), USAGE_CONFIG: join(root, 'config.toml'),
  USAGE_LOG_DIR: join(root, 'logs'), USAGE_LOG: 'debug', USAGE_GATEWAY: 'on', NO_PROXY: '127.0.0.1,localhost', no_proxy: '127.0.0.1,localhost' };
delete env.USAGE_GATEWAY_TRANSPORT;
const exec = promisify(execFile);
const cli = async (...args) => (await exec(program, args, { env, cwd: root, timeout: 25000 })).stdout;
const json = async (...args) => JSON.parse(await cli(...args));
const jwt = claims => `e30.${Buffer.from(JSON.stringify({ exp: Math.floor(Date.now()/1000)+7200, ...claims })).toString('base64url')}.fixture`;
const raccoonToken = jwt({ sub: 'Raccoon-pu-fixture', user_id: 'Raccoon-pu-fixture', name: 'Fixture' });
const refreshToken = jwt({ user_profile: { principal_urn: 'iam::fixture:user:fixture', user_name: 'Fixture' } });
const model = kind === 'minimax' ? 'MiniMax-M2.7' : 'fixture-model';
const calls = [], errors = [], claimed = new Set();
let refreshes = 0, failedClaims = false;
const today = () => new Date(Date.now()+8*3600000).toISOString().slice(0,10);
const server = createServer(async (req, res) => {
  try {
    const chunks = []; for await (const c of req) chunks.push(c);
    const raw = Buffer.concat(chunks).toString();
    const url = new URL(req.url, 'http://localhost'), path = url.pathname, host = req.headers['x-test-origin'];
    const key = host;
    calls.push({ path, host, method: req.method });
    const reply = (data, status=200) => res.writeHead(status, {'content-type':'application/json'}).end(JSON.stringify(data));
    if (path.endsWith('/messages') || path.endsWith('/chat/completions')) {
      const body = JSON.parse(raw);
      assert.equal(body.model, model); assert.ok(req.headers.authorization);
      if (kind === 'codearts') assert.match(req.headers.authorization, /^SDK-HMAC-SHA256 /);
      assert.ok(JSON.stringify(body).includes('hello'));
      const messages = kind === 'minimax';
      assert.deepEqual(body.tools.map(t => messages ? t.name : t.function.name), ['functions__exec']);
      const events = messages ? [
        {type:'message_start',message:{model,usage:{input_tokens:10,cache_read_input_tokens:2}}},
        {type:'content_block_start',index:0,content_block:{type:'text',text:''}},
        {type:'content_block_delta',index:0,delta:{type:'text_delta',text:'你好'}},
        {type:'content_block_stop',index:0},
        {type:'content_block_start',index:1,content_block:{type:'tool_use',id:'tool1',name:'functions__exec',input:{}}},
        {type:'content_block_delta',index:1,delta:{type:'input_json_delta',partial_json:'{"input":"ls"}'}},
        {type:'content_block_stop',index:1},
        {type:'message_delta',delta:{stop_reason:'tool_use'},usage:{output_tokens:5}}, {type:'message_stop'}
      ] : [
        {choices:[{delta:{content:'你好'}}]},
        {choices:[{delta:{tool_calls:[{index:0,id:'tool1',type:'function',function:{name:'functions__exec',arguments:'{"input":"ls"}'}}]},finish_reason:'tool_calls'}]},
        {choices:[],usage:{prompt_tokens:12,completion_tokens:5,prompt_tokens_details:{cached_tokens:2}}}
      ];
      return res.writeHead(200, {'content-type':'text/event-stream'}).end(events.map(v => `${messages?'event: '+v.type+'\n':''}data: ${JSON.stringify(v)}\n\n`).join('')+(messages?'':'data: [DONE]\n\n'));
    }
    if (kind === 'minimax') {
      if (path === '/oauth2/device/code') return reply({device_code:'device',user_code:'CODE',verification_uri:'https://account.minimax.cn/device',interval:0.01,expires_in:600});
      if (path === '/oauth2/token') { if (new URLSearchParams(raw).get('grant_type') === 'refresh_token') refreshes++;
        return reply({access_token:jwt({sub:key}),refresh_token:`refresh-${key}-${refreshes}`,expires_in:7200}); }
      if (path === '/v1/api/user/info') return reply({data:{userInfo:{realUserID:key,userEmail:`${key}@example.test`}}});
      if (path.endsWith('/get_user_extra_info')) return reply({data:{workspaces:[{workspace_type:0,workspace_id:'personal'}]}});
      if (path.endsWith('/get_membership_info')) return reply({data:{opcredit_balance:'1000',has_token_plan:false}});
      if (path.endsWith('/credit/details')) return reply({data:{}});
      if (path.endsWith('/models')) return reply({providers:[{providerId:'minimax',config:{models:{[model]:{name:model,context_window:200000,max_tokens:128000}}}}]});
      if (path.endsWith('/signin/status')) return reply({data:{days:Array.from({length:7},(_,i)=>({day_no:i+1,is_today:i===0,status:i===0?(claimed.has(key)?3:2):1,points:100}))}});
      if (path.endsWith('/signin/claim')) { if(failedClaims) return reply({},503); claimed.add(key); return reply({data:{claim_result:1,points:100}}); }
    }
    if (kind === 'raccoon') {
      if (path.endsWith('/token') || path.endsWith('/refresh_token') || path.includes('/login/') || path.endsWith('/login_with_authorization_code') || path.endsWith('/refresh')) { refreshes++; return reply({code:0,data:{access_token:raccoonToken,refresh_token:'refresh-'+refreshes}}); }
      if (path.endsWith('/user_info')) return reply({code:0,data:{phone:'13800138000'}});
      if (path.endsWith('/model_catalog')) return reply({data:{categories:[{models:[{name:model,context_window:200000,max_tokens:64000,tags:['vision']}]}]}});
      if (path.endsWith('/balance')) return reply({data:{available_points:900}});
      if (path.endsWith('/entitlement_info')) return reply({data:{office:{pro_enable:false}}});
      if (path.endsWith('/bills')) return reply({data:{items:claimed.has(key)?[{bill_id:'daily',points:300,biz_type:'daily_grant',created_at:today()+'T12:00:00+08:00'}]:[]}});
      if (path.endsWith('/setting_info')) { if(failedClaims) return reply({},503); claimed.add(key); return reply({data:{}}); }
      if (path.endsWith('/points/grant')) return reply({data:{points:0}});
    }
    if (kind === 'codearts') {
      if (path.endsWith('/oauth2/tokens')) {
        const form = new URLSearchParams(raw); if(form.get('grant_type')==='refresh_token') refreshes++;
        assert.ok(req.headers.dpop);
        return reply({credentials:{access_key_id:'fixture-ak-'+refreshes,secret_access_key:'fixture-sk',security_token:'fixture-session',expiration:new Date(Date.now()+7200000).toISOString()},refresh_token:refreshToken});
      }
      if (path.includes('gateway/config')) return reply({result:{models:[]}});
      if (path.endsWith('/builtin')) return reply({builtinModels:[{model_id:model,model_name:'Fixture',context_window:200000,max_tokens:64000}]});
      if (path.endsWith('/statistics/plugin')) return reply({code:0,data:{package:{package_name_cn:'Pro'},metrics:[{name:'usageTotalPackageCredit',package_credit_amount:1000,package_credit_remain:900}]}});
      if (path.endsWith('/ops/delivery')) return reply({code:0,data:{items:[{campaignId:'daily',type:'USER_LOGIN',benefitAmount:100,claimable:!claimed.has(key),status:claimed.has(key)?'CLAIMED':'UNCLAIMED'}]}});
      if (path.endsWith('/ops/claim')) { if(failedClaims) return reply({},503); claimed.add(key); return reply({code:0,data:{id:'claim',benefitAmount:100}}); }
      if (path.endsWith('/ops/confirm')) return reply({code:0,data:{}});
      // Optional phone/MaaS/ledger/agent detail discovery may be unavailable.
      return reply({code:404,message:'optional fixture endpoint unavailable'},404);
    }
    throw new Error(`unhandled ${host}${path}`);
  } catch(e) { errors.push(e); res.writeHead(500).end('{}'); }
});
server.listen(0,'127.0.0.1'); await once(server,'listening');
const origin = `http://127.0.0.1:${server.address().port}`;
const wrapper = join(root,'plugin.mjs');
await writeFile(wrapper, `
import * as original from ${JSON.stringify(pathToFileURL(entry).href)};
const upstream = globalThis.fetch;
globalThis.fetch = (input, init={}) => {
 const url = new URL(input instanceof Request ? input.url : String(input));
 const allowed = ['account.minimax.cn','account.minimax.io','agent.minimax.cn','agent.minimaxi.com','agent.minimax.io','platform.minimax.io','www.minimaxi.com','xiaohuanxiong.com','sts.cn-north-4.myhuaweicloud.com','snap-access.cn-north-4.myhuaweicloud.com','opengw.developer.huaweicloud.com','iam.myhuaweicloud.com'];
 if (!allowed.includes(url.hostname)) throw new Error('external network disabled: '+url.hostname);
 const headers = new Headers(init.headers); headers.set('x-test-origin',url.hostname);
 return upstream(${JSON.stringify(origin)}+url.pathname+url.search,{...init,headers});
};
async function wrap(factory, input) {
 const hooks = await factory(input);
 if (${JSON.stringify(kind)} !== 'minimax') {
  const method = hooks.auth.methods[0], authorize = method.authorize;
  method.authorize = async (...args) => {
   const flow = await authorize(...args), url = new URL(flow.url);
   const callback = ${JSON.stringify(kind)} === 'codearts'
    ? new URL('http://127.0.0.1:'+url.searchParams.get('port')+'/oauth/callback')
    : new URL(url.searchParams.get('redirect'));
   callback.searchParams.set('code','synthetic-code');
   if (url.searchParams.has('state')) callback.searchParams.set('state',url.searchParams.get('state'));
   setTimeout(()=>upstream(callback,{redirect:'manual'}).then(r=>r.body?.cancel()).catch(()=>{}),50);
   return flow;
  };
 }
 return hooks;
}
${kind==='minimax' ? `export { MiniMaxCodeAuthPlugin, MiniMaxCodeGlobalAuthPlugin } from ${JSON.stringify(pathToFileURL(entry).href)};` : kind==='codearts' ? 'export const CodeArtsAuthPlugin = input => wrap(original.CodeArtsAuthPlugin,input); export const OfficeAceAuthPlugin = original.OfficeAceAuthPlugin;' : 'export const RaccoonAuthPlugin = input => wrap(original.RaccoonAuthPlugin,input);'}
`);
await writeFile(env.USAGE_CONFIG,'');
try {
 await cli('plugins','add',kind,wrapper,'--runtime',process.execPath);
 const registry = join(env.USAGE_PLUGIN_DIR,'plugins.json');
 const plugins = JSON.parse(await readFile(registry,'utf8')); plugins[0].package=metadata.name; plugins[0].version=metadata.version;
 await writeFile(registry,JSON.stringify(plugins));
 const descriptors = await json('plugins','providers',kind);
 const providers = kind==='minimax' ? ['minimax-code','minimax-code-global'] : [kind];
 for (const provider of providers) {
  assert.equal(descriptors.find(p=>p.id===provider).capabilities.checkin,true);
  assert.equal(descriptors.find(p=>p.id===provider).capabilities.checkin_status,false);
  await cli('plugins','login',kind,provider,'--no-browser','--name',provider);
  const a = (await json('plugins','accounts')).find(a=>a.provider===provider); assert.ok(a);
  const file=join(env.USAGE_PLUGIN_DIR,'accounts',a.id+'.json'), saved=JSON.parse(await readFile(file,'utf8'));
  saved.auth.expires=1;
  if(kind==='codearts') { const cred=JSON.parse(saved.auth.access); cred.expires_at=new Date(1).toISOString(); saved.auth.access=JSON.stringify(cred); }
  await writeFile(file,JSON.stringify(saved));
  const models=await json('plugins','models',a.id); assert.ok(models.some(m=>m.id===model),JSON.stringify(models));
  assert.ok(JSON.parse(await readFile(file,'utf8')).generation>saved.generation,'refresh must be persisted');
  const usage=await json('plugins','usage',a.id); assert.equal(usage.error,undefined,JSON.stringify(usage));
  assert.ok(usage.windows.length || usage.balance,JSON.stringify(usage));
  const before=claimed.size;
  assert.equal((await json('plugins','checkin',a.id)).outcome,'ready'); assert.equal(claimed.size,before);
  const checkin=await json('plugins','checkin',a.id,'--confirm'); assert.equal(checkin.outcome,'claimed',JSON.stringify(checkin));
  assert.equal((await json('plugins','checkin',a.id,'--confirm')).outcome,'done');
  claimed.clear(); failedClaims=true;
  assert.equal((await json('plugins','checkin',a.id,'--confirm')).outcome,'failed');
  assert.equal(claimed.size,0,'a refused claim must not become a success'); failedClaims=false;
  await cli('plugins','bind',a.id,'--name',provider,'--model',model);
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
with urllib.request.urlopen(urllib.request.Request(catalog,headers=h),timeout=10) as r: assert json.load(r)['models'][0]['slug']==c['model']
for stream in [True,False]:
 body=json.dumps({'model':c['model'],'input':'hello','stream':stream,'tools':[{'type':'namespace','name':'functions','tools':[{'type':'custom','name':'exec'}]},{'type':'web_search'}]}).encode()
 with urllib.request.urlopen(urllib.request.Request(base+'/responses',data=body,headers=h),timeout=10) as r: raw=r.read().decode()
 result=next(json.loads(line[6:])['response'] for line in raw.splitlines() if line.startswith('data: ') and json.loads(line[6:]).get('type')=='response.completed') if stream else json.loads(raw)
 assert result['usage']['input_tokens']==12 and result['usage']['output_tokens']==5,result
 assert result['output'][1]['type']=='custom_tool_call' and result['output'][1]['input']=='ls',result
 assert result['output'][1]['namespace']=='functions',result
print('subscription-inference-ok')
`, { mode: 0o700 });
  assert.match(await cli('codex','run',provider,'--binary',mock),/subscription-inference-ok/);
 }
 assert.ok(refreshes>0); assert.deepEqual(errors,[]);
 console.log(`PASS: original ${kind} ${metadata.version}, browser login, account refresh, models, Credits, confirmed check-in and repeat status, Codex Responses/SSE, tools and cache usage`);
} catch(e) { console.error('Mock failures:',errors); console.error('Mock requests:',calls); throw e; }
finally {
 try { const status=await json('gateway','status'); if(status.pid) process.kill(status.pid,'SIGTERM'); } catch {}
 server.closeAllConnections(); await new Promise(r=>server.close(r)); await rm(root,{recursive:true,force:true});
}
