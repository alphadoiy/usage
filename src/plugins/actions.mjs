// Optional account actions: native plugin hooks take precedence over registered host adapters.
import * as workbuddy from './workbuddy_actions.mjs';
const extensions = [workbuddy];
const outcomes = new Set(['ready', 'claimed', 'done', 'ineligible', 'inactive', 'captcha', 'own-app', 'expired', 'failed']);
export function supportsCheckin(packageName, hooks, provider) {
  return typeof hooks.auth.checkin === 'function' || extensions.some((e) => e.supports(packageName, provider.id));
}
export function supportsCheckinStatus(packageName, hooks, provider) {
  return typeof hooks.auth.checkinStatus === 'function' || extensions.some((e) => e.supportsStatus(packageName, provider.id));
}
export async function accountAction({ packageName, hooks, provider, getAuth, phase, signal }) {
  const deadline = AbortSignal.timeout(20000);
  signal = signal ? AbortSignal.any([signal, deadline]) : deadline;
  let result;
  const extension = extensions.find((e) => e.supports(packageName, provider.id));
  if (phase === 'preview' && typeof hooks.auth.checkinStatus === 'function') {
    result = await hooks.auth.checkinStatus(getAuth, provider);
  } else if (phase === 'preview' && extension) {
    result = await extension.checkin({ hooks, provider, getAuth, phase, signal });
  } else if (typeof hooks.auth.checkin === 'function') {
    if (phase === 'preview') return { outcome: 'ready' };
    result = await hooks.auth.checkin(getAuth, provider);
  } else {
    if (!extension) return { outcome: 'failed', reason: 'unsupported' };
    result = await extension.checkin({ hooks, provider, getAuth, phase, signal });
  }
  // Only structured values cross the host boundary; raw vendor/plugin messages can contain secrets.
  if (!outcomes.has(result?.outcome) || (phase === 'execute' && result.outcome === 'ready') || (phase === 'preview' && result.outcome === 'claimed')) return { outcome: 'failed', reason: 'invalid_response' };
  const number = (v) => v !== null && v !== '' && v !== undefined && Number.isFinite(Number(v)) && Number(v) >= 0 ? Number(v) : undefined;
  return {
    outcome: result.outcome, credit: number(result.credit), streak: Number.isInteger(number(result.streak)) && Number(result.streak) <= 4294967295 ? Number(result.streak) : undefined, delayed: result.delayed === true,
    reason: ['unsupported', 'invalid_response', 'request_failed', 'timeout'].includes(result.reason) ? result.reason : undefined,
  };
}
