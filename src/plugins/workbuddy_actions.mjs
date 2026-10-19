// WorkBuddy's host-side daily check-in, matching Magpie's fallback for this package.
// Credentials remain inside the plugin's loader/fetch. Never read or print opaque auth here.
import { readFileSync } from 'node:fs';
const capabilities = JSON.parse(readFileSync(new URL('./action_capabilities.json', import.meta.url), 'utf8'));
export function supports(packageName, provider) {
  return capabilities.some((c) => c.adapter === 'workbuddy' && c.package === packageName && c.provider === provider);
}
export function supportsStatus(packageName, provider) {
  return capabilities.some((c) => c.adapter === 'workbuddy' && c.package === packageName && c.provider === provider && c.checkin_status === true);
}
const failed = (reason) => ({ outcome: 'failed', reason });
const count = (value) => {
  if (value === null || value === undefined || value === '') return undefined;
  const n = Number(value);
  return Number.isFinite(n) && n >= 0 ? n : undefined;
};
async function bodyOf(response) {
  let text = '';
  const reader = response.body?.getReader();
  if (!reader) throw new Error('invalid response');
  const decoder = new TextDecoder();
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > 65536) throw new Error('response too large');
      text += decoder.decode(value, { stream: true });
    }
    return JSON.parse(text + decoder.decode());
  } finally { await reader.cancel().catch(() => {}); }
}
export async function checkin({ hooks, provider, getAuth, phase, signal }) {
  try {
    const options = await hooks.auth.loader(getAuth, provider);
    if (typeof options?.fetch !== 'function') return failed('unsupported');
    const base = new URL(options.baseURL ?? provider.api);
    if (base.protocol !== 'https:' && !(base.protocol === 'http:' && ['localhost', '127.0.0.1', '[::1]'].includes(base.hostname))) return failed('unsupported');
    const ask = async (path) => {
      const response = await options.fetch(new URL(`/v2/billing/meter/${path}`, base), {
        method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}',
        redirect: 'error', signal,
      });
      let envelope;
      try { envelope = await bodyOf(response); }
      catch { return { http: response.status }; }
      return { http: response.status, code: Number(envelope.code ?? 0), data: envelope.data };
    };
    const interpret = async (reply) => {
      if (reply.http === 401 || reply.code === 401) {
        // A valid wallet with a refused check-in requires WorkBuddy's device token.
        const wallet = await ask('get-user-resource-summary');
        return wallet.http >= 200 && wallet.http < 300 && wallet.code === 0
          && wallet.data && typeof wallet.data === 'object' && !Array.isArray(wallet.data)
          ? { outcome: 'own-app' }
          : wallet.http === 401 || wallet.code === 401
            ? { outcome: 'expired' } : failed('request_failed');
      }
      if (reply.code === 1001) return { outcome: 'done' };
      if (reply.code === 1002) return { outcome: 'ineligible' };
      if (reply.code === 1003) return { outcome: 'inactive' };
      if (!(reply.http >= 200 && reply.http < 300) || reply.code !== 0 || !reply.data || typeof reply.data !== 'object') return failed('invalid_response');
      return null;
    };
    const status = await ask('checkin-activity-status');
    const problem = await interpret(status);
    if (problem) return problem;
    const st = status.data;
    if (typeof st.active !== 'boolean') return failed('invalid_response');
    if (!st.active) return { outcome: 'inactive' };
    if (typeof st.today_checked_in !== 'boolean') return failed('invalid_response');
    if (st.today_checked_in) return { outcome: 'done', credit: count(st.today_credit), streak: count(st.streak_days) };
    if (phase === 'preview') return { outcome: 'ready', credit: count(st.daily_credit), streak: count(st.streak_days) };
    // Re-read status on execution; another Usage instance or WorkBuddy may have claimed it.
    const claim = await ask('daily-checkin');
    return await interpret(claim) ?? {
      outcome: 'claimed', credit: count(claim.data.credit), streak: count(claim.data.streak_days),
      delayed: claim.data.credit_delayed === true,
    };
  } catch { return failed(signal?.aborted ? 'timeout' : 'request_failed'); }
}
