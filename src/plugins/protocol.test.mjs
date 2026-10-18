import { test } from "node:test";
import assert from "node:assert/strict";
import {
  translateRequest,
  translateResponse,
  ProtocolError,
} from "./protocol.mjs";
const event = (v) => `data: ${JSON.stringify(v)}\n\n`;
const chat = (delta, finish_reason = null) => ({
  model: "model",
  choices: [{ index: 0, delta, finish_reason }],
});
function source(text, step = 7) {
  const bytes = new TextEncoder().encode(text);
  let at = 0;
  return new Response(
    new ReadableStream({
      pull(c) {
        if (at >= bytes.length) return c.close();
        c.enqueue(bytes.slice(at, at + step));
        at += step;
      },
    }),
    { headers: { "content-type": "text/event-stream" } },
  );
}
const decode = (text) =>
  text
    .split("\n")
    .filter((l) => l.startsWith("data: "))
    .map((l) => JSON.parse(l.slice(6)));
test("Responses input preserves tool calls/results, images, instructions and token caps", () => {
  const input = {
    model: "m",
    instructions: "system",
    max_output_tokens: 100,
    reasoning: { effort: "low" },
    tools: [{ type: "function", name: "f", parameters: { type: "object" } }],
    input: [
      {
        type: "message",
        role: "user",
        content: [
          { type: "input_text", text: "hello" },
          { type: "input_image", image_url: "data:image/png;base64,fixture" },
        ],
      },
      { type: "function_call", call_id: "call1", name: "f", arguments: "{}" },
      { type: "function_call_output", call_id: "call1", output: "ok" },
    ],
  };
  const copy = structuredClone(input),
    out = translateRequest("responses", "chat", input);
  assert.equal(out.messages[0].role, "system");
  assert.equal(out.messages[2].tool_calls[0].id, "call1");
  assert.equal(out.messages[3].tool_call_id, "call1");
  assert.equal(out.max_tokens, 100);
  assert.equal(out.reasoning_effort, "low");
  assert.equal(out.stream, true);
  assert.deepEqual(input, copy);
});
test("Unsupported protocols, server-side references and built-in tools fail explicitly", () => {
  for (const body of [
    { previous_response_id: "x" },
    { tools: [{ type: "web_search" }] },
    { input: [{ type: "unknown" }] },
  ])
    assert.throws(
      () => translateRequest("responses", "chat", body),
      ProtocolError,
    );
  assert.throws(
    () => translateRequest("responses", "messages", {}),
    ProtocolError,
  );
});
test("Fragmented UTF8 SSE becomes ordered Responses reasoning, text, tools and real usage", async () => {
  const text =
    event(chat({ reasoning_content: "想一想" })) +
    event(chat({ content: "你好" })) +
    event(
      chat({
        tool_calls: [
          { index: 0, id: "c", function: { name: "f", arguments: '{"x":' } },
        ],
      }),
    ) +
    event({
      ...chat(
        { tool_calls: [{ index: 0, function: { arguments: "1}" } }] },
        "tool_calls",
      ),
      usage: {
        prompt_tokens: 20,
        completion_tokens: 5,
        total_tokens: 25,
        prompt_tokens_details: { cached_tokens: 3 },
      },
    }) +
    "data: [DONE]\n\n";
  const result = await translateResponse("responses", "chat", source(text, 1), {
    model: "m",
    stream: true,
  });
  const events = decode(await result.text());
  assert.deepEqual(
    events.map((e) => e.sequence_number),
    events.map((_, i) => i),
  );
  const final = events.at(-1);
  assert.equal(final.type, "response.completed");
  assert.equal(final.response.output[0].summary[0].text, "想一想");
  assert.equal(final.response.output[1].content[0].text, "你好");
  assert.equal(final.response.output[2].arguments, '{"x":1}');
  assert.equal(final.response.usage.input_tokens_details.cached_tokens, 3);
});
test("Nonstream clients get a completed JSON response from streamed upstream", async () => {
  const res = await translateResponse(
    "responses",
    "chat",
    source(event(chat({ content: "ok" }, "stop")) + "data: [DONE]\n\n"),
    { model: "m", stream: false },
  );
  const json = await res.json();
  assert.equal(json.status, "completed");
  assert.equal(json.output[0].content[0].text, "ok");
  assert.equal(json.usage, undefined);
});
test("Interrupted stream cannot claim completion", async () => {
  const res = await translateResponse(
    "responses",
    "chat",
    source(event(chat({ content: "partial" }))),
    { model: "m", stream: true },
  );
  await assert.rejects(res.text());
});
test("Token limits produce incomplete events rather than successful completion", async () => {
  const res = await translateResponse(
    "responses",
    "chat",
    source(event(chat({ content: "partial" }, "length")) + "data: [DONE]\n\n"),
    { model: "m", stream: true },
  );
  const events = decode(await res.text());
  assert.equal(events.at(-1).type, "response.incomplete");
  assert.equal(
    events.at(-1).response.incomplete_details.reason,
    "max_output_tokens",
  );
});
test("Cancellation releases an upstream reader that is waiting for data", async () => {
  let cancel = false;
  const upstream = new Response(
    new ReadableStream({
      cancel() {
        cancel = true;
      },
    }),
    { headers: { "content-type": "text/event-stream" } },
  );
  const res = await translateResponse("responses", "chat", upstream, {
    model: "m",
    stream: true,
  });
  const r = res.body.getReader();
  await r.read();
  await r.read();
  const next = r.read();
  await new Promise((ok) => setTimeout(ok, 10));
  await r.cancel();
  await next;
  assert.equal(cancel, true);
});
test("Same protocol and upstream failures preserve the original response", async () => {
  const response = new Response('{"error":"fixture"}', { status: 429 });
  assert.equal(
    await translateResponse("responses", "chat", response, {}),
    response,
  );
  const success = Response.json({ id: "native" });
  assert.equal(
    await translateResponse("responses", "responses", success, {}),
    success,
  );
});

test('parallel tool results follow a single assistant tool-call batch',()=>{
 const b=translateRequest('responses','chat',{model:'fixture',input:[
  {type:'function_call',call_id:'a',name:'first',arguments:'{}'},
  {type:'function_call',call_id:'b',name:'second',arguments:'{}'},
  {type:'function_call_output',call_id:'a',output:'A'},
  {type:'function_call_output',call_id:'b',output:'B'}
 ]});
 const waiting=new Set();
 for(const m of b.messages){
  if(m.role==='assistant') {
   assert.equal(waiting.size,0,'another assistant message arrived before all tool results');
   for(const t of m.tool_calls??[]) waiting.add(t.id);
  } else if(m.role==='tool') assert.ok(waiting.delete(m.tool_call_id),'tool output must answer the pending assistant batch');
 }
 assert.equal(waiting.size,0);
});
