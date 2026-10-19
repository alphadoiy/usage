import test from 'node:test';
import assert from 'node:assert/strict';
import { accountAction, supportsCheckin, supportsCheckinStatus } from './actions.mjs';
const packageName = '@magpie-community/opencode-trae-auth';
function fixture(reply = {}, id = 'trae-cn') {
  const calls = []; let claims = 0;
  const auth = { type: 'oauth', access: 'synthetic' };
  const hooks = { auth: {
    loader: async (getAuth) => {
      assert.equal(await getAuth(), auth);
      return { fetch: async (url, init) => {
        calls.push(String(url));
        assert.equal(String(url), 'https://api.trae.cn/trae/api/v2/ug/checkin_credits/status');
        assert.equal(init.method, 'POST'); assert.deepEqual(JSON.parse(init.body), { req_source: 1 });
        assert.equal(init.redirect, 'error'); assert.ok(init.signal);
        if (reply instanceof Error) throw reply;
        return new Response(typeof reply.body === 'string' ? reply.body : JSON.stringify(reply.body ?? reply), {
          status: reply.status ?? 200, headers: reply.headers,
        });
      } };
    },
    checkin: async () => { claims++; return { outcome: 'claimed', credit: 100, message: 'private-sentinel' }; },
  } };
  return { calls, hooks, claims: () => claims, run: (phase = 'preview') => accountAction({
    packageName, hooks, provider: { id }, getAuth: async () => auth, phase,
  }) };
}
test('Trae capabilities disable the international inactive claim hook', async () => {
  const cn = fixture(); const global = fixture({}, 'trae-global');
  assert.ok(supportsCheckin(packageName, cn.hooks, { id: 'trae-cn' }));
  assert.ok(supportsCheckinStatus(packageName, cn.hooks, { id: 'trae-cn' }));
  assert.ok(!supportsCheckin(packageName, global.hooks, { id: 'trae-global' }));
  assert.ok(!supportsCheckinStatus(packageName, global.hooks, { id: 'trae-global' }));
  assert.equal((await global.run('execute')).reason, 'unsupported');
  assert.equal(global.claims(), 0); assert.equal(global.calls.length, 0);
  assert.ok(!supportsCheckinStatus('unrelated-package', cn.hooks, { id: 'trae-cn' }));
});
test('Trae previews only read status and explicit execution uses the original hook', async () => {
  for (const [body, outcome] of [
    [{ enable: true, checked_in: false, credits: 100, extra_credits: 10 }, 'ready'],
    [{ checked_in: true }, 'done'], [{ enable: false }, 'inactive'],
  ]) {
    const f = fixture(body); const result = await f.run();
    assert.equal(result.outcome, outcome);
    if (outcome === 'ready') assert.equal(result.credit, 110);
    assert.equal(f.claims(), 0); assert.equal(f.calls.length, 1);
  }
  const f = fixture({ enable: true, checked_in: false });
  assert.equal((await f.run()).credit, undefined);
  const result = await f.run('execute');
  assert.equal(result.outcome, 'claimed'); assert.equal(f.claims(), 1);
  assert.equal(f.calls.length, 1); assert.ok(!JSON.stringify(result).includes('sentinel'));
});
test('Trae status distinguishes authentication failures and rejects unknown responses', async () => {
  for (const reply of [
    { status: 401 }, { code: 1001 }, { error: { code: '401' } },
    { ResponseMetadata: { Error: { Code: '1001' } } },
    { headers: { 'x-magpie-sign-in': 'expired' } },
  ]) assert.equal((await fixture(reply).run()).outcome, 'expired');
  for (const reply of [
    { status: 429 }, { status: 503 }, { code: 9074 }, { code: 9095 }, {},
    { enable: 'true', checked_in: false }, { enable: true, checked_in: 'false' },
    { body: 'invalid private-sentinel' }, { body: [] }, new Error('private-sentinel'),
    { enable: true, checked_in: true, padding: 'x'.repeat(70000) },
  ]) {
    const f = fixture(reply); const result = await f.run();
    assert.equal(result.outcome, 'failed'); assert.equal(f.claims(), 0);
    assert.ok(!JSON.stringify(result).includes('sentinel'));
  }
});
