import { test } from "node:test";
import assert from "node:assert/strict";
import { retryDelay, errorCategory, errorSummary, observeStream } from "./inference.mjs";

test("magpie retry budget: 429 pauses 1/2/4s, transient 5xx twice, quota never", () => {
  const headers = new Headers();
  assert.deepEqual([0, 1, 2, 3].map(i => retryDelay(429, headers, "too many requests", i)), [1000, 2000, 4000, null]);
  for (const status of [408, 500, 502, 503, 504, 529])
    assert.deepEqual([0, 1, 2].map(i => retryDelay(status, headers, "busy", i)), [1000, 2000, null]);
  for (const body of ["Insufficient balance", "quota exhausted", "1113", "1005", "额度不足", "context_length_exceeded"])
    assert.equal(retryDelay(429, headers, body, 0), null);
  for (const status of [200, 400, 401, 403]) assert.equal(retryDelay(status, headers, "", 0), null);
  assert.equal(retryDelay(502, headers, "expected streamed response", 0), null);
  assert.equal(errorCategory(429, "private-sentinel quota exhausted"), "quota");
});

test("Retry-After seconds/date are respected; waits beyond eight seconds return the refusal", () => {
  assert.equal(retryDelay(429, new Headers({ "retry-after": "7" }), "busy", 0), 7000);
  assert.equal(retryDelay(429, new Headers({ "retry-after": "9" }), "busy", 0), null);
  const now = Date.UTC(2026, 9, 25);
  assert.equal(retryDelay(503, new Headers({ "retry-after": new Date(now + 5000).toUTCString() }), "busy", 0, now), 5000);
});

test("reading a bounded error summary preserves the original refusal body", async () => {
  const res = new Response('{"error":{"code":1302,"message":"too many requests"}}', { status: 429 });
  assert.match(await errorSummary(res), /1302/);
  assert.match(await res.text(), /too many requests/);
});

test("raw stream diagnostics distinguish explicit completion and bare EOF without changing bytes", async () => {
  for (const terminal of ["", 'event: Done\ndata: {}\n\n', 'data: [DONE]\n\n', 'data: {"choices":[{"finish_reason":"stop"}]}\n\n']) {
    const raw = 'event: output\ndata: {"response":"private-sentinel"}\n\n' + terminal;
    const reports = [];
    const res = observeStream(new Response(raw, { headers: { "content-type": "text/event-stream" } }), x => reports.push(x));
    assert.equal(await res.text(), raw);
    assert.equal(reports.at(-1).outcome, terminal ? "complete" : "eof_without_terminal");
    assert.equal(JSON.stringify(reports).includes("private-sentinel"), false);
  }
});

test("raw stream cancellation releases the reader and records cancellation", async () => {
  let cancelled = false;
  const reports = [];
  const body = new ReadableStream({ cancel() { cancelled = true; } });
  const res = observeStream(new Response(body, { headers: { "content-type": "text/event-stream" } }), x => reports.push(x));
  await res.body.cancel();
  assert.equal(cancelled, true);
  assert.equal(reports.at(-1).outcome, "cancelled");
});
