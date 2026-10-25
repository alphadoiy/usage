// OpenCode provider compatibility host. All credentials are brokered by Rust.
import http from "node:http";
import { AsyncLocalStorage } from "node:async_hooks";
import { pathToFileURL } from "node:url";
import { Readable } from "node:stream";
import { translateRequest, translateResponse } from "./protocol.mjs";
import { retryDelay, errorSummary, errorCategory, observeStream } from "./inference.mjs";
import { supportsCheckin, supportsCheckinStatus, accountAction } from "./actions.mjs";
const [entry, broker, token, packageName] = process.argv.slice(2);
const scope = new AsyncLocalStorage();
const pending = new Map();
const activeLogins = new Map();
const hooks = new Map();
const config = { provider: {} };
const rawFetch = globalThis.fetch;
const LOGIN_TTL_MS = 600_000;
class HostError extends Error {
  constructor(code) {
    super(code);
    this.code = code;
  }
}
const fail = (message) => {
  throw new HostError(message);
};
globalThis.fetch = async (input, init = {}) => {
  const current = scope.getStore();
  const signal = current?.signal;
  const response = await rawFetch(
    input,
    signal
      ? {
          ...init,
          signal: init.signal ? AbortSignal.any([init.signal, signal]) : signal,
        }
      : init,
  );
  if (current?.report && (!response.ok || response.headers.get("content-type")?.includes("event-stream"))) {
    const detail = response.ok ? "" : await errorSummary(response);
    let code;
    try { const d = JSON.parse(detail); code = String(d.error?.code ?? d.code ?? ""); } catch {}
    current.report({ stage: "upstream_response", outcome: "returned", status: response.status,
      attempt: current.attempt,
      upstream_request_id: response.headers.get("x-request-id") ?? response.headers.get("request-id"),
      vendor_code: /^\d{1,10}$/.test(code ?? "") ? code : undefined,
      ...(response.ok ? {} : { category: errorCategory(response.status, detail) }) });
  }
  const attempt = current?.attempt;
  return current?.report ? observeStream(response, fields => current.report({ ...fields, attempt })) : response;
};
console.log = console.info = console.warn = console.error = () => {}; // stdout is the startup protocol; plugin prose may contain secrets.
async function credentials(operation, values = {}) {
  const s = scope.getStore();
  if (!s?.account) return fail("account_scope_required");
  const res = await rawFetch(broker, {
    method: "POST",
    headers: {
      authorization: `Bearer ${token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({ operation, account: s.account, ...values }),
    signal: AbortSignal.timeout(10000),
  });
  if (!res.ok) return fail("credential_broker_failed");
  return res.json();
}
async function getAuth() {
  const c = await credentials("get");
  scope.getStore().generation = c.generation;
  return c.auth;
}
const client = {
  auth: {
    set: async ({ path, body }) => {
      const s = scope.getStore();
      if (!s || path?.id !== s.provider)
        return fail("credential_provider_mismatch");
      if (!s.generation) await getAuth();
      const next = await credentials("set", {
        generation: s.generation,
        auth: body,
      });
      s.generation = next.generation;
      return { data: body };
    },
  },
  config: { get: async () => ({ data: config }) },
  app: { log: async () => ({ data: null }) },
  tui: { showToast: async () => ({ data: null }) },
};
const unsupported = () =>
  new Proxy(() => fail("unsupported_client_api"), {
    get: (_, k) =>
      k === "then" ? undefined : unsupported(),
  });
function compatible(obj) {
  return new Proxy(obj, {
    get: (o, k) =>
      k in o
        ? o[k] && typeof o[k] === "object"
          ? compatible(o[k])
          : o[k]
        : k === "then"
          ? undefined
          : unsupported(),
  });
}
const module = await import(pathToFileURL(entry));
const loaded = new Set();
for (const [name, fn] of Object.entries(module)) {
  const factory = typeof fn === "function" ? fn : fn?.server;
  if (
    typeof factory !== "function" ||
    loaded.has(factory) ||
    !(
      name === "default" ||
      name === "server" ||
      name.endsWith("Plugin") ||
      fn?.server
    )
  )
    continue;
  loaded.add(factory);
  const h = await factory({
    client: compatible(client),
    directory: process.cwd(),
    worktree: process.cwd(),
    project: { id: "usage", worktree: process.cwd() },
    serverUrl: new URL("http://127.0.0.1"),
    $: globalThis.Bun?.$ ?? unsupported(),
  });
  if (h?.config) await h.config(config);
  if (h?.auth?.provider) {
    if (hooks.has(h.auth.provider)) fail("duplicate_provider");
    hooks.set(h.auth.provider, h);
  }
}
if (!hooks.size) fail("no_supported_provider_exports");
function provider(id) {
  const p = config.provider[id] ?? {};
  const npm = p.npm ?? "@ai-sdk/openai-compatible";
  const protocol =
    p.wire_api ??
    (npm.includes("anthropic")
      ? "messages"
      : npm === "@ai-sdk/openai"
        ? "responses"
        : npm.includes("openai-compatible")
          ? "chat"
          : null);
  return {
    ...p,
    id,
    name: p.name ?? id,
    npm,
    protocol,
    models: p.models ?? {},
  };
}
function descriptor(id, h) {
  const p = provider(id);
  return {
    id,
    name: p.name,
    protocol: p.protocol,
    methods: h.auth.methods.map((m, i) => ({
      id: String(i),
      type: m.type,
      label: m.label,
      prompts: (m.prompts ?? []).map(({ validate, condition, ...p }) => p),
    })),
    capabilities: {
      models: typeof h.provider?.models === "function",
      usage: typeof h.auth.usage === "function",
      refresh: typeof h.auth.refresh === "function",
      fetch: typeof h.auth.loader === "function",
      checkin: supportsCheckin(packageName, h, p),
      checkin_status: supportsCheckinStatus(packageName, h, p),
    },
    compatibility: "opencode-provider-v1+magpie-provider-extensions",
    runtime: globalThis.Bun ? "bun" : "node",
  };
}
async function fresh(h, p) {
  const a = await getAuth();
  if (
    typeof h.auth.refresh === "function" &&
    a?.type === "oauth" &&
    a.expires > 0 &&
    Date.now() >= a.expires - (h.auth.refreshLead ?? 300000)
  ) {
    try {
      const patch = await h.auth.refresh(structuredClone(a), p.id);
      if (patch)
        await client.auth.set({ path: { id: p.id }, body: { ...a, ...patch } });
    } catch {
      /* loader decides whether the remaining access token is usable */
    }
  }
}
async function models(h, p) {
  const result = h.provider?.models
    ? await h.provider.models(p, { auth: await getAuth() })
    : p.models;
  return Object.entries(result ?? {}).map(([id, m]) => ({
    id,
    name: m.name ?? id,
    context: m.limit?.context ?? m.context,
    output: m.limit?.output ?? m.output,
    images: m.modalities?.input?.includes("image") ?? m.capabilities?.input?.image ?? m.images ?? false,
    reasoning: m.variants ?? {},
    protocol: m.api?.wire_api ?? p.protocol,
  }));
}
async function rpc(x, signal) {
  if (x.operation === "providers")
    return [...hooks].map(([id, h]) => descriptor(id, h));
  const h = hooks.get(x.provider);
  if (!h) fail("provider_not_found");
  const p = provider(x.provider);
  if (x.operation === "login.begin") {
    const method = h.auth.methods[Number(x.method)];
    if (!method) fail("login_method_not_found");
    for (const prompt of method.prompts ?? []) {
      if (
        prompt.when &&
        (x.inputs?.[prompt.when.key] === prompt.when.value) !==
          (prompt.when.op === "eq")
      )
        continue;
      const error = prompt.validate?.(x.inputs?.[prompt.key] ?? "");
      if (error) fail("invalid_login_input");
    }
    const result =
      typeof method.authorize === "function"
        ? await method.authorize(x.inputs ?? {})
        : undefined;
    if (method.type === "api")
      return {
        complete: true,
        result: {
          ...(result ?? { type: "success", key: x.inputs?.key }),
          metadata: {
            ...Object.fromEntries(Object.entries(x.inputs ?? {}).filter(([k]) => k !== "key")),
            ...(result?.metadata ?? {}),
          },
        },
      };
    if (typeof result?.callback !== "function") fail("invalid_login_callback");
    const session = crypto.randomUUID();
    pending.set(session, {
      ...result,
      provider: x.provider,
      expires: Date.now() + LOGIN_TTL_MS,
    });
    const timer = setTimeout(() => pending.delete(session), LOGIN_TTL_MS);
    timer.unref();
    return {
      session,
      url: result.url,
      instructions: result.instructions,
      method: result.method,
    };
  }
  if (x.operation === "login.cancel") {
    pending.delete(x.session);
    activeLogins.get(x.session)?.abort();
    return { cancelled: true };
  }
  if (x.operation === "login.finish") {
    const f = pending.get(x.session);
    if (!f || f.provider !== x.provider || Date.now() > f.expires)
      fail("login_session_expired");
    pending.delete(x.session);
    return await f.callback(x.code);
  }
  if (x.operation === "prepare") {
    await fresh(h, p);
    return { ready: true };
  }
  if (x.operation === "models") return models(h, p);
  if (x.operation === "action") {
    if (x.action !== "checkin" || !["preview", "execute"].includes(x.phase) || !supportsCheckin(packageName, h, p)) fail("operation_unsupported");
    try { return await accountAction({ packageName, hooks: h, provider: p, getAuth, phase: x.phase, signal }); }
    catch { return { outcome: "failed", reason: "request_failed" }; }
  }
  if (x.operation === "usage") {
    if (!h.auth.usage) fail("usage_unsupported");
    return h.auth.usage(getAuth, p);
  }
  fail("operation_unsupported");
}
async function infer(x, signal) {
  const current = scope.getStore();
  const diagnostics = current.diagnostics = new Set();
  const report = current.report = (fields) => {
    const sent = rawFetch(broker, {
      method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify({ operation: "diagnostic", context: x.context, account: x.account, ...fields }),
      signal: AbortSignal.timeout(2000),
    }).then(r => r.body?.cancel()).catch(() => {}).finally(() => diagnostics.delete(sent));
    diagnostics.add(sent);
  };
  const h = hooks.get(x.provider);
  if (!h) fail("provider_not_found");
  const p = provider(x.provider);
  if (!p.protocol) fail("sdk_protocol_unsupported");
  const options = (await h.auth.loader?.(getAuth, p)) ?? {};
  const base = String(options.baseURL ?? p.api ?? "").replace(/\/$/, "");
  const url = new URL(base);
  if (
    url.protocol !== "https:" &&
    !(
      url.protocol === "http:" &&
      ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)
    )
  )
    fail("invalid_upstream");
  const headers = new Headers(options.headers);
  if (options.apiKey) headers.set("authorization", `Bearer ${options.apiKey}`);
  headers.set("content-type", "application/json");
  if (p.protocol === "messages") headers.set("anthropic-version", "2023-06-01");
  const native = p.models?.[x.body.model];
  const out = { headers: {} };
  await h["chat.headers"]?.(
    {
      sessionID: x.context?.session ?? "",
      agent: "build",
      model: {
        id: x.body.model,
        providerID: p.id,
        api: { id: x.body.model, npm: p.npm },
        ...native,
      },
      provider: { source: "custom", info: p, options },
    },
    out,
  );
  for (const [k, v] of Object.entries(out.headers)) headers.set(k, String(v));
  const levels = native?.variants && typeof native.variants === "object"
    ? Object.keys(native.variants)
    : undefined;
  const converted = translateRequest(x.protocol, p.protocol, x.body, levels);
  const suffix = {
    chat: "chat/completions",
    responses: "responses",
    messages: "messages",
  }[p.protocol];
  if (!suffix) fail("sdk_protocol_unsupported");
  const send = typeof options.fetch === "function" ? options.fetch : rawFetch;
  const body = JSON.stringify(converted);
  let response;
  for (let again = 0; ; again++) {
    current.attempt = again + 1;
    const began = Date.now();
    try {
      response = await send(`${base}/${suffix}`, { method: "POST", headers, body, signal });
    } catch (e) {
      const wait = signal.aborted ? null : retryDelay(502, new Headers(), "", again);
      report({ stage: "response", outcome: signal.aborted ? "cancelled" : "interrupted", attempt: again + 1, duration_ms: Date.now() - began });
      if (wait === null) throw e;
      report({ stage: "retry", outcome: "retrying", category: "upstream", attempt: again + 1, wait_ms: wait });
      await pause(wait, signal);
      continue;
    }
    const detail = response.ok ? "" : await errorSummary(response);
    const wait = retryDelay(response.status, response.headers, detail, again);
    let vendorCode;
    try { const d = JSON.parse(detail); vendorCode = String(d.error?.code ?? d.code ?? ""); } catch {}
    report({ stage: "response", outcome: "returned", status: response.status, attempt: again + 1,
      upstream_request_id: response.headers.get("x-request-id") ?? response.headers.get("request-id"),
      vendor_code: /^\d{1,10}$/.test(vendorCode ?? "") ? vendorCode : undefined,
      duration_ms: Date.now() - began, ...(response.ok ? {} : { category: errorCategory(response.status, detail) }) });
    if (wait === null) break;
    report({ stage: "retry", outcome: "retrying", status: response.status, attempt: again + 1, wait_ms: wait });
    await response.body?.cancel();
    await pause(wait, signal);
  }
  return translateResponse(x.protocol, p.protocol, response, x.body, report);
}
function pause(wait, signal) {
  return new Promise((resolve, reject) => {
    const cancel = () => { clearTimeout(timer); signal.removeEventListener("abort", cancel); reject(signal.reason); };
    const timer = setTimeout(() => { signal.removeEventListener("abort", cancel); resolve(); }, wait);
    signal.addEventListener("abort", cancel, { once: true });
    if (signal.aborted) cancel();
  });
}
const server = http.createServer(async (req, res) => {
  if (req.headers.authorization !== `Bearer ${token}`) {
    res.writeHead(401).end();
    return;
  }
  const ctl = new AbortController();
  res.on("close", () => {
    if (!res.writableFinished) ctl.abort();
  });
  try {
    const parts = [];
    let size = 0;
    for await (const part of req) {
      size += part.length;
      if (size > 32 * 1024 * 1024) fail("request_too_large");
      parts.push(part);
    }
    const x = JSON.parse(Buffer.concat(parts));
    if (x.operation === "login.finish") activeLogins.set(x.session, ctl);
    await scope.run(
      {
        account: x.account,
        provider: x.provider,
        generation: 0,
        signal: ctl.signal,
      },
      async () => {
        if (req.url === "/call") {
          let result;
          try {
            result = await rpc(x, ctl.signal);
          } finally {
            if (x.operation === "login.finish") activeLogins.delete(x.session);
          }
          res
            .writeHead(200, { "content-type": "application/json" })
            .end(JSON.stringify(result));
        } else if (req.url === "/fetch") {
          const upstream = await infer(x, ctl.signal);
          const headers = {};
          for (const [k, v] of upstream.headers)
            if (
              ![
                "content-length",
                "content-encoding",
                "connection",
                "transfer-encoding",
              ].includes(k)
            )
              headers[k] = v;
          res.writeHead(upstream.status, headers);
          res.flushHeaders();
          try {
            if (upstream.body)
              for await (const chunk of upstream.body) {
                if (ctl.signal.aborted) break;
                if (!res.write(chunk))
                  await new Promise((ok, bad) => {
                    const clean = () => {
                      res.off("drain", done);
                      res.off("close", closed);
                    };
                    const done = () => {
                      clean();
                      ok();
                    };
                    const closed = () => {
                      clean();
                      bad(new Error("cancelled"));
                    };
                    res.once("drain", done);
                    res.once("close", closed);
                  });
              }
            res.end();
            scope.getStore().report?.({ stage: "delivery", outcome: ctl.signal.aborted ? "cancelled" : "complete" });
          } catch (e) {
            scope.getStore().report?.({ stage: "delivery", outcome: ctl.signal.aborted ? "cancelled" : "interrupted" });
            throw e;
          } finally {
            await Promise.allSettled([...scope.getStore().diagnostics ?? []]);
          }
        } else {
          res.writeHead(404).end();
        }
      },
    );
  } catch (error) {
    const kind =
      error?.name === "ProtocolError"
        ? "protocol"
        : error?.name === "AbortError"
          ? "cancelled"
          : "hook";
    if (!res.headersSent)
      res
        .writeHead(kind === "protocol" ? 400 : 502, {
          "content-type": "application/json",
        })
        .end(
          JSON.stringify({
            error: {
              type: "plugin_error",
              message:
                kind === "protocol"
                  ? String(error.message).slice(0, 300)
                  : "Plugin operation failed; see Usage operation context",
              kind,
              code: error instanceof HostError ? error.code : kind,
            },
          }),
        );
    else res.destroy();
  }
});
server.listen(0, "127.0.0.1", () =>
  process.stdout.write(
    JSON.stringify({ port: server.address().port, protocol: 1 }) + "\n",
  ),
);
process.stdin.resume();
process.stdin.on("end", () => {
  server.closeAllConnections();
  server.close();
  process.exit(0);
});
