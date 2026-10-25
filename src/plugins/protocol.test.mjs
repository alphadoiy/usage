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
test("Unsupported protocols, server-side references and unknown built-in tools fail explicitly", () => {
  for (const body of [
    { previous_response_id: "x" },
    { tools: [{ type: "code_interpreter" }] },
    { input: [{ type: "unknown" }] },
  ])
    assert.throws(
      () => translateRequest("responses", "chat", body),
      ProtocolError,
    );
  assert.throws(
    () => translateRequest("responses", "unknown", {}),
    ProtocolError,
  );
});
test("Codex hosted web search is omitted for Chat-only providers", () => {
  const request = {
    model: "m",
    tools: [
      { type: "web_search", external_web_access: true },
      { type: "function", name: "read", parameters: { type: "object" } },
    ],
    input: [
      { type: "message", role: "user", content: "Earlier search" },
      { type: "web_search_call", action: { type: "search", query: "example" } },
      { type: "message", role: "assistant", content: [{ type: "output_text", text: "Found a cited page." }] },
    ],
    tool_choice: "auto",
  };
  const before = structuredClone(request);
  const converted = translateRequest("responses", "chat", request);
  assert.deepEqual(converted.tools.map((t) => t.function.name), ["read"]);
  assert.equal(converted.messages.length, 2);
  assert.equal(converted.messages[1].content[0].text, "Found a cited page.");
  assert.deepEqual(request, before);
  const onlySearch = translateRequest("responses", "chat", {
    tools: [{ type: "web_search_preview" }],
    tool_choice: { type: "allowed_tools", mode: "auto", tools: [{ type: "web_search_preview" }] },
  });
  assert.deepEqual(onlySearch.tools, []);
  assert.equal(onlySearch.tool_choice, "none");
  assert.throws(() => translateRequest("responses", "chat", {
    tools: [{ type: "web_search" }], tool_choice: { type: "web_search" },
  }), /web search is unavailable/);
  assert.throws(() => translateRequest("responses", "chat", {
    tools: [{ type: "web_search" }],
    tool_choice: { type: "allowed_tools", mode: "required", tools: [{ type: "web_search" }] },
  }), /required web search is unavailable/);
});
test("Namespaced function and custom tools round-trip through Chat requests", () => {
  const body = {
    model: "m",
    tools: [{
      type: "namespace",
      name: "functions",
      tools: [
        { type: "function", name: "read", parameters: { type: "object", properties: { path: { type: "string" } } } },
        { type: "custom", name: "exec", format: { type: "grammar", syntax: "lark", definition: "start: WORD" } },
      ],
    }],
    tool_choice: { type: "custom", namespace: "functions", name: "exec" },
    input: [
      { type: "function_call", call_id: "a", namespace: "functions", name: "read", arguments: '{"path":"a"}' },
      { type: "function_call_output", call_id: "a", output: "contents" },
      { type: "custom_tool_call", call_id: "b", namespace: "functions", name: "exec", input: "print(1)" },
      { type: "custom_tool_call_output", call_id: "b", output: "1" },
    ],
  };
  const before = structuredClone(body);
  const out = translateRequest("responses", "chat", body);
  assert.deepEqual(out.tools.map((t) => t.function.name), ["functions__read", "functions__exec"]);
  assert.deepEqual(out.tools[1].function.parameters.required, ["input"]);
  assert.match(out.tools[1].function.description, /start: WORD/);
  assert.deepEqual(out.tool_choice, { type: "function", function: { name: "functions__exec" } });
  assert.equal(out.messages[0].tool_calls[0].function.name, "functions__read");
  assert.deepEqual(out.messages[2].tool_calls[0].function, {
    name: "functions__exec",
    arguments: '{"input":"print(1)"}',
  });
  assert.deepEqual(body, before);
});
test("Allowed tool choices filter flattened names and collisions fail explicitly", () => {
  const tools = [{ type: "namespace", name: "functions", tools: [
    { type: "function", name: "read" },
    { type: "custom", name: "exec" },
  ] }];
  const out = translateRequest("responses", "chat", {
    tools,
    tool_choice: { type: "allowed_tools", mode: "required", tools: [
      { type: "custom", namespace: "functions", name: "exec" },
    ] },
  });
  assert.deepEqual(out.tools.map((t) => t.function.name), ["functions__exec"]);
  assert.equal(out.tool_choice, "required");
  assert.throws(() => translateRequest("responses", "chat", {
    tools: [...tools, { type: "function", name: "functions__exec" }],
  }), /tool name collision/);
});
test("Client tool search and discovered tools survive Chat history", async () => {
  const request = {
    model: "m", stream: true,
    tools: [{ type: "tool_search", execution: "client", description: "Find tools", parameters: { type: "object", properties: { query: { type: "string" } } } }],
    input: [
      { type: "tool_search_call", call_id: "search1", execution: "client", arguments: { query: "files" } },
      { type: "tool_search_output", call_id: "search1", tools: [
        { type: "namespace", name: "files", tools: [{ type: "function", name: "read", parameters: { type: "object" } }] },
      ] },
      { type: "function_call", call_id: "read1", namespace: "files", name: "read", arguments: "{}" },
      { type: "function_call_output", call_id: "read1", output: "ok" },
    ],
  };
  const converted = translateRequest("responses", "chat", request);
  assert.deepEqual(converted.tools.map((t) => t.function.name), ["tool_search", "files__read"]);
  assert.equal(converted.messages[0].tool_calls[0].function.name, "tool_search");
  assert.equal(converted.messages[1].tool_call_id, "search1");
  assert.match(converted.messages[1].content, /files__read/);
  assert.equal(converted.messages[2].tool_calls[0].function.name, "files__read");
  const later = translateRequest("responses", "chat", { ...request, tools: [] });
  assert.deepEqual(later.tools.map((t) => t.function.name), ["files__read"]);
  assert.equal(later.messages[0].tool_calls[0].function.name, "tool_search");
  const stream = event(chat({ tool_calls: [{ index: 0, id: "search2", function: { name: "tool_search", arguments: '{"query":"more"}' } }] }, "tool_calls")) + "data: [DONE]\n\n";
  const result = await translateResponse("responses", "chat", source(stream), request);
  const item = decode(await result.text()).at(-1).response.output[0];
  assert.deepEqual({ type: item.type, execution: item.execution, arguments: item.arguments }, {
    type: "tool_search_call", execution: "client", arguments: { query: "more" },
  });
  assert.equal(item.call_id, "search2");
  assert.throws(() => translateRequest("responses", "chat", {
    tools: [{ type: "tool_search", execution: "server" }],
  }), /only client-executed tool_search/);
});
test("Fragmented names restore namespaced function and custom Responses calls", async () => {
  const request = {
    model: "m", stream: true,
    tools: [{ type: "namespace", name: "functions", tools: [
      { type: "function", name: "read" }, { type: "custom", name: "exec" },
    ] }],
  };
  const stream =
    event(chat({ tool_calls: [{ index: 0, id: "a", function: { name: "functions__", arguments: '{"path":' } }] })) +
    event(chat({ tool_calls: [{ index: 0, function: { name: "read", arguments: '"a"}' } }] })) +
    event(chat({ tool_calls: [{ index: 1, id: "b", function: { name: "functions__ex", arguments: '{"input":"pri' } }] })) +
    event(chat({ tool_calls: [{ index: 1, function: { name: "ec", arguments: 'nt(1)"}' } }] }, "tool_calls")) +
    "data: [DONE]\n\n";
  const result = await translateResponse("responses", "chat", source(stream, 3), request);
  const events = decode(await result.text());
  const output = events.at(-1).response.output;
  assert.deepEqual(output.map((x) => [x.type, x.namespace, x.name]), [
    ["function_call", "functions", "read"],
    ["custom_tool_call", "functions", "exec"],
  ]);
  assert.equal(output[0].arguments, '{"path":"a"}');
  assert.equal(output[1].input, "print(1)");
  assert.equal(events.filter((x) => x.type === "response.custom_tool_call_input.done")[0].input, "print(1)");
  assert.deepEqual(events.map((x) => x.sequence_number), events.map((_, i) => i));
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

test("Messages requests preserve images and namespaced tools with round-trip history", () => {
  const out = translateRequest("responses", "messages", {
    model: "glm", instructions: "system", reasoning: { effort: "low" },
    max_output_tokens: 4096, tool_choice: "required", parallel_tool_calls: false,
    tools: [{ type: "namespace", name: "functions", tools: [{ type: "custom", name: "exec" }] }],
    input: [
      { role: "developer", content: "developer" },
      { role: "user", content: [{ type: "input_image", image_url: "data:image/png;base64,YQ==" }] },
      { type: "custom_tool_call", namespace: "functions", name: "exec", call_id: "c1", input: "ls" },
      { type: "custom_tool_call_output", call_id: "c1", output: "ok" },
    ],
  });
  assert.deepEqual(out.system.map(x => x.text), ["system", "developer"]);
  assert.deepEqual(out.messages[0].content[0].source, { type: "base64", media_type: "image/png", data: "YQ==" });
  assert.deepEqual(out.messages[1].content[0], { type: "tool_use", id: "c1", name: "functions__exec", input: { input: "ls" } });
  assert.equal(out.messages[2].content[0].tool_use_id, "c1");
  assert.equal(out.tools[0].name, "functions__exec");
  assert.deepEqual(out.tool_choice, { type: "any", disable_parallel_tool_use: true });
  assert.equal(out.thinking.budget_tokens, 3072);
  assert.equal(out.max_tokens, 4096);
});
const messagesEvents = (reason = "tool_use") => [
  { type: "message_start", message: { model: "glm", usage: { input_tokens: 10, cache_read_input_tokens: 4, cache_creation_input_tokens: 2 } } },
  { type: "content_block_start", index: 0, content_block: { type: "thinking", thinking: "" } },
  { type: "content_block_delta", index: 0, delta: { type: "thinking_delta", thinking: "想" } },
  { type: "content_block_delta", index: 0, delta: { type: "signature_delta", signature: "opaque" } },
  { type: "content_block_stop", index: 0 },
  { type: "content_block_start", index: 1, content_block: { type: "text", text: "" } },
  { type: "content_block_delta", index: 1, delta: { type: "text_delta", text: "你好" } },
  { type: "content_block_stop", index: 1 },
  { type: "content_block_start", index: 2, content_block: { type: "tool_use", id: "call1", name: "functions__exec", input: {} } },
  { type: "content_block_delta", index: 2, delta: { type: "input_json_delta", partial_json: '{"input":' } },
  { type: "content_block_delta", index: 2, delta: { type: "input_json_delta", partial_json: '"ls"}' } },
  { type: "content_block_stop", index: 2 },
  { type: "message_delta", delta: { stop_reason: reason }, usage: { output_tokens: 5 } },
  { type: "message_stop" },
];
test("Messages SSE handles fragmented UTF-8, reasoning, custom tools and actual cache usage", async () => {
  for (const stream of [false, true]) {
    const request = { stream, tools: [{ type: "namespace", name: "functions", tools: [{ type: "custom", name: "exec" }] }] };
    const res = await translateResponse("responses", "messages", source(messagesEvents().map(event).join(""), 1), request);
    const result = stream ? decode(await res.text()).find(x => x.type === "response.completed").response : await res.json();
    assert.equal(result.output[0].summary[0].text, "想");
    assert.equal(result.output[1].content[0].text, "你好");
    assert.equal(result.output[2].namespace, "functions");
    assert.equal(result.output[2].type, "custom_tool_call");
    assert.equal(result.output[2].input, "ls");
    assert.equal(result.usage.input_tokens, 16);
    assert.equal(result.usage.input_tokens_details.cached_tokens, 4);
    assert.equal(result.usage.total_tokens, 21);
  }
});
test("Messages truncation, errors and missing stop cannot produce successful completion", async () => {
  for (const values of [messagesEvents().slice(0, -1), [{ type: "error", error: { message: "secret" } }]]) {
    const res = await translateResponse("responses", "messages", source(values.map(event).join("")), { stream: true });
    await assert.rejects(res.text(), ProtocolError);
  }
  const res = await translateResponse("responses", "messages", source(messagesEvents("max_tokens").map(event).join("")), { stream: false });
  assert.equal((await res.json()).status, "incomplete");
});
test("Messages cancellation releases upstream", async () => {
  let cancelled = false;
  const values = messagesEvents().map(event);
  const res = new Response(new ReadableStream({ pull(c) { c.enqueue(new TextEncoder().encode(values.shift() ?? ': ping\n\n')); }, cancel() { cancelled = true; } }), { headers: { 'content-type': 'text/event-stream' } });
  const result = await translateResponse('responses', 'messages', res, { stream: true });
  const reader = result.body.getReader();
  await reader.read(); await reader.cancel();
  assert.ok(cancelled);
});

test("Messages tools with no parameters retain an empty JSON object", async () => {
  const values = [
    { type: "message_start", message: {} },
    { type: "content_block_start", index: 0, content_block: { type: "tool_use", id: "c", name: "read", input: {} } },
    { type: "content_block_stop", index: 0 },
    { type: "message_delta", delta: { stop_reason: "tool_use" } },
    { type: "message_stop" },
  ];
  const res = await translateResponse("responses", "messages", source(values.map(event).join("")), { stream: false });
  assert.equal((await res.json()).output[0].arguments, "{}");
});

test("magpie text-call repair restores offered Hermes, DSML and GLM calls across chunks", async () => {
  const request = { model: "m", stream: true, tools: [{ type: "namespace", name: "functions", tools: [{ type: "function", name: "read" }] }] };
  const bodies = [
    '<tool_call>{"name":"functions__read","arguments":{"path":"a"}}</tool_call>',
    '<｜DSML｜function_calls><｜DSML｜invoke name="functions__read"><｜DSML｜parameter name="path" string="true">a</｜DSML｜parameter></｜DSML｜invoke></｜DSML｜function_calls>',
    '<tool_call>functions__read<arg_key>path</arg_key><arg_value>a</arg_value></tool_call>',
  ];
  for (const body of bodies) {
    const reports = [];
    const wire = [...`Checking. ${body}`].map(content => event(chat({ content }))).join("") + event(chat({}, "stop")) + 'data: [DONE]\n\n';
    const res = await translateResponse("responses", "chat", source(wire, 1), request, x => reports.push(x));
    const events = decode(await res.text()), output = events.at(-1).response.output;
    assert.equal(output[0].phase, "commentary");
    assert.equal(output[0].content[0].text.trim(), "Checking.");
    assert.equal(output[1].type, "function_call");
    assert.equal(output[1].namespace, "functions");
    assert.equal(output[1].name, "read");
    assert.deepEqual(JSON.parse(output[1].arguments), { path: "a" });
    assert.equal(reports.at(-1).recovered_calls, 1);
    assert.equal(JSON.stringify(reports).includes("Checking"), false);
  }
});

test("unknown or malformed text calls remain text, truncation never marks a final answer", async () => {
  for (const [body, reason] of [
    ['<tool_call>{"name":"unknown","arguments":{}}</tool_call>', "stop"],
    ['<tool_call>{"name":"read","arguments":', "length"],
    ['a literal <tool_', "stop"],
  ]) {
    const res = await translateResponse("responses", "chat", source(event(chat({ content: body }, reason)) + 'data: [DONE]\n\n'), {
      model: "m", stream: true, tools: [{ type: "function", name: "read" }],
    });
    const last = decode(await res.text()).at(-1);
    assert.equal(last.response.output.length, 1);
    assert.equal(last.response.output[0].content[0].text, body);
    assert.equal(last.response.output[0].phase, reason === "length" ? undefined : "final_answer");
  }
});

test("native calls give progress text commentary phase and plain replies final_answer", async () => {
  for (const calls of [false, true]) {
    const wire = event(chat({ content: "hello" })) + (calls ? event(chat({ tool_calls: [{ index: 0, id: "c", function: { name: "read", arguments: "{}" } }] }, "tool_calls")) : event(chat({}, "stop"))) + 'data: [DONE]\n\n';
    const res = await translateResponse("responses", "chat", source(wire), { model: "m", stream: false });
    assert.equal((await res.json()).output[0].phase, calls ? "commentary" : "final_answer");
  }
});

test("Reasoning effort matches magpie aliases, budgets and model levels", () => {
  const none = translateRequest("responses", "chat", { model: "m", reasoning: { effort: "none" } });
  assert.equal(none.reasoning_effort, "low");
  const off = translateRequest("responses", "messages", {
    model: "glm", reasoning: { effort: "off" }, max_output_tokens: 4096,
  });
  assert.deepEqual(off.thinking, { type: "disabled" });
  assert.equal(off.output_config, undefined);

  const fitted = translateRequest("responses", "chat", {
    model: "m", reasoning: { effort: "xhigh" },
  }, ["low", "high", "max"]);
  assert.equal(fitted.reasoning_effort, "max");

  const ultra = translateRequest("responses", "chat", { model: "m", reasoning: { effort: "ultra" } });
  assert.equal(ultra.reasoning_effort, "max");

  const minimal = translateRequest("responses", "messages", {
    model: "glm", reasoning: { effort: "minimal" }, max_output_tokens: 8192,
  });
  assert.equal(minimal.thinking.budget_tokens, 4096);

  const adaptive = translateRequest("responses", "messages", {
    model: "claude-opus-4-6", reasoning: { effort: "xhigh" }, max_output_tokens: 8192,
  });
  assert.deepEqual(adaptive.thinking, { type: "adaptive", display: "summarized" });
  assert.equal(adaptive.output_config.effort, "max");
  assert.equal(adaptive.temperature, undefined);

  const later = translateRequest("responses", "messages", {
    model: "claude-sonnet-5", reasoning: { effort: "xhigh" },
  });
  assert.equal(later.output_config.effort, "xhigh");
  assert.equal(later.max_tokens, 16384);

  const stopped = translateRequest("responses", "chat", {
    model: "m", reasoning: { effort: "none" },
  }, ["none", "low", "high"]);
  assert.equal(stopped.reasoning_effort, "none");
});

test("ultra stays an alias when model levels include ultra", () => {
  for (const [levels, expected] of [
    [["low", "high", "ultra"], "high"],
    [["low", "high", "max", "ultra"], "max"],
    [["ultra"], "max"],
  ]) {
    const out = translateRequest("responses", "chat", {
      model: "m", reasoning: { effort: " ULTRA " },
    }, levels);
    assert.equal(out.reasoning_effort, expected);
  }
});

test("magpie normalization and empty reasoning remain consistent across conversions", () => {
  for (const [raw, expected] of [["minimal", "low"], ["off", "low"], ["persistent", undefined]]) {
    const out = translateRequest("responses", "chat", {
      model: "m", reasoning: { effort: raw },
    }, ["minimal", "low", "high"]);
    assert.equal(out.reasoning_effort, expected);
  }
  for (const reasoning of [{}, { summary: "auto" }]) {
    const out = translateRequest("responses", "messages", {
      model: "claude-sonnet-4-5", reasoning, max_output_tokens: 8192, temperature: 0.2,
    });
    assert.deepEqual(out.thinking, { type: "enabled", budget_tokens: 7168 });
    assert.equal(out.temperature, undefined);
  }
  const plain = translateRequest("responses", "messages", {
    model: "claude-sonnet-4-5", max_output_tokens: 8192, temperature: 0.2,
  });
  assert.equal(plain.thinking, undefined);
  assert.equal(plain.temperature, 0.2);
});
