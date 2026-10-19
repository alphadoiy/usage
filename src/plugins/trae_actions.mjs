// Read-only Trae CN check-in status through the original plugin's signed account fetch.
// Claims stay in auth.checkin; this adapter never submits a claim or reads credentials.
import { bodyOf, supportsAdapter, supportsAdapterStatus } from './action_support.mjs';
export const supports = (packageName, provider) => supportsAdapter('trae', packageName, provider);
export const supportsStatus = (packageName, provider) => supportsAdapterStatus('trae', packageName, provider);
export async function checkin({ hooks, provider, getAuth, phase, signal }) {
  if (phase !== 'preview') return { outcome: 'failed', reason: 'unsupported' };
  try {
    const options = await hooks.auth.loader(getAuth, provider);
    if (typeof options?.fetch !== 'function') return { outcome: 'failed', reason: 'unsupported' };
    const response = await options.fetch('https://api.trae.cn/trae/api/v2/ug/checkin_credits/status', {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: '{"req_source":1}',
      redirect: 'error', signal,
    });
    if (response.status === 401 || response.headers.get('x-magpie-sign-in') === 'expired') return { outcome: 'expired' };
    const value = await bodyOf(response);
    if (!value || typeof value !== 'object' || Array.isArray(value)) return { outcome: 'failed', reason: 'invalid_response' };
    const error = value.ResponseMetadata?.Error ?? (typeof value.error === 'object' ? value.error : value);
    const code = String(error?.code ?? error?.Code ?? value.code ?? 0);
    if (code === '1001' || code === '401') return { outcome: 'expired' };
    if (!response.ok || code !== '0') return { outcome: 'failed', reason: 'request_failed' };
    if (value.checked_in === true) return { outcome: 'done' };
    if (value.enable === false) return { outcome: 'inactive' };
    if (typeof value.enable !== 'boolean' || typeof value.checked_in !== 'boolean') return { outcome: 'failed', reason: 'invalid_response' };
    const reward = Number(value.credits) + Number(value.extra_credits ?? 0);
    return { outcome: 'ready', ...(value.credits !== null && value.credits !== '' && Number.isFinite(reward) && reward >= 0 ? { credit: reward } : {}) };
  } catch { return { outcome: 'failed', reason: signal?.aborted ? 'timeout' : 'request_failed' }; }
}
