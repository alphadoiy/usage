import test from 'node:test';
import assert from 'node:assert/strict';
import {checkin,supports} from './workbuddy_actions.mjs';
import {accountAction,supportsCheckin,supportsCheckinStatus} from './actions.mjs';
function fixture(replies) {
  const calls=[]; const auth={uid:'synthetic-account'};
  const hooks={auth:{loader:async (getAuth)=>{
    assert.equal(await getAuth(),auth);
    return {baseURL:'http://127.0.0.1:9/v2',fetch:async (url,init)=>{
      assert.equal(init.method,'POST');assert.equal(init.body,'{}');assert.equal(init.redirect,'error');
      calls.push(new URL(url).pathname);
      const reply=replies.shift();assert.ok(reply,'unexpected request');
      if (reply instanceof Error)throw reply;
      return new Response(typeof reply.body==='string'?reply.body:JSON.stringify(reply.body??{code:0,data:reply.data}),{status:reply.status??200});
    }};
  }}};
  return {calls, run:(phase='preview')=>checkin({hooks,provider:{id:'workbuddy'},getAuth:async()=>auth,phase,signal:AbortSignal.timeout(1000)})};
}
test('fallback is limited to the known package and Chinese provider',()=>{
  assert.ok(supports('@magpie-community/opencode-workbuddy-auth','workbuddy'));
  assert.ok(!supports('unknown','workbuddy'));
  assert.ok(!supports('@magpie-community/opencode-workbuddy-auth','workbuddy-ai'));
});
test('preview never claims; execution rechecks status and reports reward',async()=>{
  const f=fixture([{data:{active:true,today_checked_in:false,daily_credit:'10',streak_days:2}},{data:{active:true,today_checked_in:false}},{data:{credit:'10',streak_days:3,credit_delayed:true}}]);
  assert.equal((await f.run()).outcome,'ready');
  assert.deepEqual(f.calls,['/v2/billing/meter/checkin-activity-status']);
  assert.deepEqual(await f.run('execute'),{outcome:'claimed',credit:10,streak:3,delayed:true});
  assert.deepEqual(f.calls,['/v2/billing/meter/checkin-activity-status','/v2/billing/meter/checkin-activity-status','/v2/billing/meter/daily-checkin']);
});
test('already checked in or inactive makes no claim, including after another client claims',async()=>{
  for(const [data,outcome] of [[{active:false},'inactive'],[{active:true,today_checked_in:true,today_credit:0},'done']]) {
    const f=fixture([{data}]);assert.equal((await f.run('execute')).outcome,outcome);assert.equal(f.calls.length,1);
  }
});
test('vendor outcome codes are business results with no raw message',async()=>{
  for(const [code,outcome] of [[1001,'done'],[1002,'ineligible'],[1003,'inactive']]) {
    const f=fixture([{status:400,body:{code,msg:'private-token-sentinel'}}]);
    const result=await f.run('execute');assert.equal(result.outcome,outcome);assert.ok(!JSON.stringify(result).includes('sentinel'));
  }
});
test('401 with a readable wallet asks for the own app; a refused wallet asks for login',async()=>{
  for(const [status,outcome] of [[200,'own-app'],[401,'expired']]) {
    const f=fixture([{status:401,body:'Unauthorized private-token-sentinel'},{status,data:{}}]);
    assert.equal((await f.run()).outcome,outcome);
    assert.equal(f.calls[1],'/v2/billing/meter/get-user-resource-summary');
  }
});
test('wallet outages and invalid responses do not imply expired credentials',async()=>{
  for(const reply of [
    ...[429,500,503].map(status=>({status,data:{}})),
    {body:'invalid'}, {body:{code:999,data:{}}}, {body:{code:0}},
    {body:{code:0,data:[]}}, new Error('unavailable'),
  ]) {
    const f=fixture([{status:401,body:'Unauthorized'},reply]);
    assert.equal((await f.run()).outcome,'failed');
    assert.equal(f.calls.length,2);
  }
});
test('unknown, oversized, malformed and failed replies never claim success',async()=>{
  for(const reply of [{data:{}},{body:'invalid private-token-sentinel'},{body:'x'.repeat(65537)},
    {body:{code:0,data:{active:true,today_checked_in:true,padding:'x'.repeat(70000)}}},
    new Error('private-token-sentinel')]) {
    const f=fixture([reply]);const result=await f.run('execute');assert.equal(result.outcome,'failed');assert.ok(!JSON.stringify(result).includes('sentinel'));
  }
});
test('native hooks take precedence, preview has no side effects, messages are sanitized',async()=>{
  let calls=0;
  const hooks={auth:{checkin:async()=>{calls++;return {outcome:'claimed',credit:12,streak:2,message:'private-token-sentinel'};}}};
  const input={packageName:'unknown',hooks,provider:{id:'other'},getAuth:async()=>({})};
  assert.ok(supportsCheckin('unknown',hooks,input.provider));
  assert.deepEqual(await accountAction({...input,phase:'preview'}),{outcome:'ready'});assert.equal(calls,0);
  const result=await accountAction({...input,phase:'execute'});assert.equal(result.outcome,'claimed');assert.equal(calls,1);assert.ok(!JSON.stringify(result).includes('sentinel'));
});

test('readonly status capability queries status without ever calling the claim hook',async()=>{
  let claims=0,reads=0;
  const hooks={auth:{checkinStatus:async()=>{reads++;return {outcome:'done'};},checkin:async()=>{claims++;return {outcome:'claimed'};}}};
  const input={packageName:'unknown',hooks,provider:{id:'other'},getAuth:async()=>({})};
  assert.ok(supportsCheckinStatus('unknown',hooks,input.provider));
  assert.equal((await accountAction({...input,phase:'preview'})).outcome,'done');
  assert.equal(reads,1);assert.equal(claims,0);
  assert.ok(!supportsCheckinStatus('unknown',{auth:{checkin:hooks.auth.checkin}},input.provider));
});

test('registered read-only adapter still checks status when a native claim hook is present',async()=>{
  let claims=0;
  const hooks={auth:{checkin:async()=>{claims++;return {outcome:'claimed'};},loader:async()=>({baseURL:'http://127.0.0.1:9/v2',fetch:async()=>new Response(JSON.stringify({code:0,data:{active:true,today_checked_in:true}}))})}};
  const result=await accountAction({packageName:'@magpie-community/opencode-workbuddy-auth',hooks,provider:{id:'workbuddy'},getAuth:async()=>({}),phase:'preview'});
  assert.equal(result.outcome,'done');assert.equal(claims,0);
});
