// Gateway policy mirrors magpie's passing(): retry only before an answer starts.
export function retryDelay(status, headers, body, again, now = Date.now()) {
  if (body === "[unreadable_error_body]") return null;
  let wait = 1000 * 2 ** again;
  const after = headers.get("retry-after");
  if (after) {
    const n = Number(after);
    const said = Number.isFinite(n) ? n * 1000 : Date.parse(after) - now;
    if (said > 0) wait = said;
  }
  if (wait > 8000) return null;
  if ([408, 500, 502, 503, 504, 529].includes(status))
    return again < 2 && !/did not stream|expected streamed/i.test(body) ? wait : null;
  // Quota, credit and context errors cannot recover in a few seconds.
  if (status === 429 && again < 3 && !/quota|balance|credit|billing|budget|usage.limit|insufficient|余额|额度|欠费|套餐|用量|context.length|too.long|1113|1005/i.test(body))
    return wait;
  return null;
}

export async function errorSummary(res) {
  const reader = res.clone().body?.getReader();
  if (!reader) return "";
  const chunks = [];
  let size = 0;
  let timer;
  const timeout = new Promise((_, reject) => { timer = setTimeout(() => reject(new Error("timeout")), 2000); });
  try {
    while (size <= 65536) {
      const { done, value } = await Promise.race([reader.read(), timeout]);
      if (done) return new TextDecoder().decode(Buffer.concat(chunks));
      size += value.length;
      chunks.push(value);
    }
    return "[unreadable_error_body]";
  } catch { return "[unreadable_error_body]"; } // Unknown/unfinished error bodies are not replayed.
  finally { clearTimeout(timer); reader.cancel().catch(() => {}); }
}

export function errorCategory(status, body) {
  if (body === "[unreadable_error_body]") return "unknown";
  if (/context.length|too.long|maximum.context/i.test(body)) return "context";
  if (/quota|balance|credit|billing|budget|insufficient|余额|额度|欠费|1113|1005/i.test(body)) return "quota";
  if (status === 429) return "rate";
  return status >= 500 ? "upstream" : "request";
}

// Observe raw upstream events before a plugin normalizes them. No message text,
// tool names/arguments, URLs or credentials are retained in diagnostics.
export function observeStream(res, report) {
  if (!res.body || !res.headers.get("content-type")?.includes("event-stream")) return res;
  const reader = res.body.getReader(), decoder = new TextDecoder();
  let buffer = "", terminal = false, event = "", count = 0, ended = false;
  function line(s) {
    if (s.startsWith("event:")) event = s.slice(6).trim();
    if (s.startsWith("data:")) {
      count++;
      const raw = s.slice(5).trim();
      if (raw === "[DONE]") terminal = true;
      else try {
        const d = JSON.parse(raw);
        const kind = String(event || d.type || d.event || "").replace(/([a-z])([A-Z])/g, "$1_$2").replace(/-/g, "_").toLowerCase();
        if (["done", "response_done", "stream_done", "message_stop", "response.completed", "response.failed", "response.incomplete", "error"].includes(kind) ||
            d.finish_reason || d.stop_reason || d.finishReason || d.delta?.stop_reason || d.choices?.some(c => c.finish_reason)) terminal = true;
      } catch {}
    }
    if (!s) event = "";
  }
  const finish = outcome => {
    if (ended) return;
    ended = true;
    report({ stage: "upstream_stream", outcome, terminal, events: count });
  };
  return new Response(new ReadableStream({
    async pull(c) {
      try {
        const { done, value } = await reader.read();
        if (done) {
          buffer += decoder.decode();
          if (buffer) line(buffer);
          finish(terminal ? "complete" : "eof_without_terminal");
          c.close(); return;
        }
        buffer += decoder.decode(value, { stream: true });
        let at;
        while ((at = buffer.indexOf("\n")) >= 0) {
          line(buffer.slice(0, at).replace(/\r$/, ""));
          buffer = buffer.slice(at + 1);
        }
        if (buffer.length > 1048576) buffer = "";
        c.enqueue(value);
      } catch (e) { finish("interrupted"); c.error(e); }
    },
    cancel(reason) { finish("cancelled"); return reader.cancel(reason); },
  }), { status: res.status, statusText: res.statusText, headers: res.headers });
}

// As in magpie/textcalls.go, only repair complete calls to offered tools.
// Unreadable/unknown calls remain text; never guess a command or its arguments.
export class TextCalls {
  pending = "";
  held = "";
  holding = false;
  push(text) {
    if (this.holding) { this.held += text; return ""; }
    const s = this.pending + text;
    this.pending = "";
    const at = s.search(/<tool_call>|<\s*[|｜][\s|｜]*DSML/);
    if (at >= 0) {
      this.holding = true; this.held = s.slice(at); return s.slice(0, at);
    }
    // Hold a possible opener across arbitrary stream chunk boundaries.
    const tail = s.lastIndexOf("<");
    if (tail >= 0 && (/^<\s*[|｜][\s|｜]*(?:D(?:S(?:M(?:L)?)?)?)?$/.test(s.slice(tail)) || "<tool_call>".startsWith(s.slice(tail)))) {
      this.pending = s.slice(tail); return s.slice(0, tail);
    }
    return s;
  }
  finish(names) {
    const original = this.pending + this.held;
    if (!this.holding) return { text: original, calls: [] };
    const calls = [];
    let valid = true;
    const add = (name, args) => {
      if (!names.has(name) || !args || typeof args !== "object" || Array.isArray(args)) { valid = false; return; }
      calls.push({ name, arguments: JSON.stringify(args) });
    };
    const dsml = "<\\s*/?\\s*[|｜][\\s|｜]*DSML[\\s|｜]*";
    let rest = original.replace(new RegExp(dsml + 'invoke\\s+name="([^"]+)"\\s*>([\\s\\S]*?)' + dsml + 'invoke\\s*>', 'g'), (_, name, body) => {
      const args = {};
      const residue = body.replace(new RegExp(dsml + 'parameter\\s+name="([^"]+)"([^>]*)>([\\s\\S]*?)' + dsml + 'parameter\\s*>', 'g'), (_, key, attrs, value) => {
        if (/string\s*=\s*"true"/.test(attrs)) Object.defineProperty(args, key, { value, enumerable: true });
        else try { Object.defineProperty(args, key, { value: JSON.parse(value), enumerable: true }); } catch { valid = false; }
        return "";
      });
      if (residue.trim()) valid = false;
      add(name, args); return "";
    });
    rest = rest.replace(/<tool_call>([\s\S]*?)<\/tool_call>/g, (_, body) => {
      if (!body.trim()) return "";
      try {
        const v = JSON.parse(body), f = v.function ?? v;
        add(f.name, typeof f.arguments === "string" ? JSON.parse(f.arguments) : f.arguments ?? f.parameters ?? {});
      } catch {
        const at = body.indexOf("<arg_key>");
        if (at < 0) { valid = false; return ""; }
        const name = body.slice(0, at).trim(), args = {};
        const residue = body.slice(at).replace(/<arg_key>([\s\S]*?)<\/arg_key>\s*<arg_value>([\s\S]*?)<\/arg_value>/g, (_, k, v) => {
          let value; try { value = JSON.parse(v); } catch { value = v; }
          Object.defineProperty(args, k.trim(), { value, enumerable: true }); return "";
        });
        if (residue.trim()) valid = false;
        add(name, args);
      }
      return "";
    });
    // Only leftover closing wrappers may be discarded, never an unknown invoke.
    rest = rest.replace(new RegExp(dsml + '(?:function_calls|invoke|parameter)\\s*>', 'g'), "").replace(/<\/?tool_calls>|<\/tool_call>/g, "");
    if (!valid || /<\s*[|｜].*DSML|<\/?(?:tool_call|arg_key|arg_value)/.test(rest)) return { text: original, calls: [] };
    return calls.length ? { text: rest, calls } : { text: original, calls: [] };
  }
}
