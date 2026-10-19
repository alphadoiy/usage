// One declaration drives local Catalog discovery and the host's actual capabilities.
import { readFileSync } from 'node:fs';
const capabilities = JSON.parse(readFileSync(new URL('./action_capabilities.json', import.meta.url), 'utf8'));
export const builtin = (packageName, provider) => capabilities.find((c) => c.package === packageName && c.provider === provider);
export const supportsAdapter = (adapter, packageName, provider) => {
  const c = builtin(packageName, provider);
  return c?.checkin === true && c.adapter === adapter;
};
export const supportsAdapterStatus = (adapter, packageName, provider) => supportsAdapter(adapter, packageName, provider) && builtin(packageName, provider).checkin_status === true;

export async function bodyOf(response) {
  const reader = response.body?.getReader();
  if (!reader) throw new Error('invalid response');
  const decoder = new TextDecoder();
  let text = '', size = 0;
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
