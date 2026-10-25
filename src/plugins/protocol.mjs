// Protocol conversion belongs to the host, independently of vendor plugins.
import { TextCalls } from "./inference.mjs";
export class ProtocolError extends Error {
  constructor(message) {
    super(message);
    this.name = "ProtocolError";
  }
}
function unsupported(message) {
  throw new ProtocolError(message);
}
function content(parts) {
  if (typeof parts === "string") return parts;
  if (!Array.isArray(parts)) return "";
  return parts.map((p) => {
    if (["input_text", "output_text", "text"].includes(p.type))
      return { type: "text", text: p.text ?? "" };
    if (p.type === "input_image")
      return {
        type: "image_url",
        image_url: { url: p.image_url, detail: p.detail ?? "auto" },
      };
    if (p.type === "image_url") return p;
    return unsupported(`unsupported content: ${p.type}`);
  });
}
const customSchema = {
  type: "object",
  properties: {
    input: {
      type: "string",
      description: "The complete free-form tool input, passed through as written.",
    },
  },
  required: ["input"],
  additionalProperties: false,
};
const flatName = (namespace, name) =>
  namespace ? `${namespace}__${name}` : name;
const isWebSearch = (tool) =>
  typeof tool?.type === "string" && tool.type.startsWith("web_search");
function customDescription(tool) {
  let description = tool.description?.trim() ?? "";
  if (description) description += "\n\n";
  description += "Put the complete free-form tool input in the `input` string. It is passed through as written.";
  if (tool.format?.type === "grammar" && tool.format.definition) {
    description += `\n\nThe input follows this ${tool.format.syntax || "specified"} grammar:\n${tool.format.definition.trim()}`;
  }
  return description;
}
function customInput(argumentsText) {
  try {
    const value = JSON.parse(argumentsText);
    if (typeof value?.input === "string") return value.input;
    if (typeof value === "string") return value;
  } catch {}
  return argumentsText;
}
function discoveredTools(input) {
  return input.flatMap((item) =>
    item.type === "tool_search_output" && Array.isArray(item.tools)
      ? item.tools
      : [],
  );
}
function toolRegistry(declarations = [], discovered = []) {
  const byName = new Map();
  const chatTools = [];
  function add(tool, namespace = "", alreadyOffered = false) {
    if (tool?.type === "tool_search") {
      if (tool.execution !== "client")
        unsupported("only client-executed tool_search can be converted");
      tool = { ...tool, name: "tool_search" };
    }
    if (!tool?.name || !["function", "custom", "tool_search"].includes(tool.type))
      return unsupported(`unsupported built-in tool: ${tool?.type}`);
    const name = flatName(namespace, tool.name);
    const previous = byName.get(name);
    if (alreadyOffered && previous?.name === tool.name &&
        previous.namespace === namespace && previous.type === tool.type)
      return;
    if (previous)
      return unsupported(`tool name collision after namespace conversion: ${name}`);
    byName.set(name, { name: tool.name, namespace, type: tool.type });
    chatTools.push({
      type: "function",
      function: {
        name,
        description: tool.type === "custom" ? customDescription(tool) : tool.description,
        parameters: tool.type === "custom"
          ? customSchema
          : (tool.parameters ?? { type: "object", properties: {} }),
        ...(tool.type === "function" && tool.strict !== undefined
          ? { strict: tool.strict }
          : {}),
      },
    });
  }
  function offer(tool, alreadyOffered) {
    if (!tool || typeof tool !== "object")
      unsupported("invalid tool declaration");
    // Codex offers its hosted search on ordinary turns. Chat-only plugins
    // cannot execute it, so do not advertise it as a client function.
    if (isWebSearch(tool)) return;
    if (tool.type === "namespace") {
      if (!tool.name || !Array.isArray(tool.tools))
        unsupported("invalid namespace tool declaration");
      for (const nested of tool.tools) add(nested, tool.name, alreadyOffered);
    } else add(tool, "", alreadyOffered);
  }
  for (const tool of declarations) offer(tool, false);
  for (const tool of discovered) offer(tool, true);
  return { byName, chatTools };
}
export function translateRequest(from, to, b) {
  if (from === to) return b;
  if (from === "responses" && to === "messages")
    return messagesRequest(translateRequest(from, "chat", b));
  if (from !== "responses" || to !== "chat")
    return unsupported(`unsupported protocol conversion: ${from} to ${to}`);
  if (b.previous_response_id)
    return unsupported(
      "previous_response_id requires server-side conversation storage; send full input",
    );
  const input =
    typeof b.input === "string"
      ? [{ type: "message", role: "user", content: b.input }]
      : (b.input ?? []);
  const found = discoveredTools(input);
  const registry = toolRegistry(b.tools, found);
  const messages = [];
  if (b.instructions)
    messages.push({ role: "system", content: b.instructions });
  for (const item of input) {
    if (["function_call", "custom_tool_call", "tool_search_call"].includes(item.type)) {
      const call = {
        id: item.call_id,
        type: "function",
        function: {
          name: item.type === "tool_search_call"
            ? "tool_search"
            : flatName(item.namespace, item.name),
          arguments: item.type === "tool_search_call"
            ? JSON.stringify(item.arguments ?? {})
            : item.type === "custom_tool_call"
              ? JSON.stringify({ input: item.input ?? "" })
              : (item.arguments ?? ""),
        },
      };
      const previous = messages.at(-1);
      if (previous?.role === "assistant" && previous.tool_calls)
        previous.tool_calls.push(call);
      else
        messages.push({ role: "assistant", content: null, tool_calls: [call] });
    } else if (item.type === "function_call_output" || item.type === "custom_tool_call_output") {
      messages.push({
        role: "tool",
        tool_call_id: item.call_id,
        content: content(item.output),
      });
    } else if (item.type === "tool_search_output") {
      const names = toolRegistry([], item.tools ?? []).chatTools.map((t) => t.function.name);
      messages.push({
        role: "tool",
        tool_call_id: item.call_id,
        content: names.length
          ? `These tools are now available to call: ${names.join(", ")}`
          : "No tools matched the search.",
      });
    } else if (item.type === "web_search_call") {
      // A hosted search in prior Responses history has no Chat tool result.
      // The following assistant message carries the answer and citations.
    } else if (item.type === "reasoning") {
      // Responses reasoning items are history metadata; chat carries actual messages separately.
      if (item.summary?.length)
        messages.push({
          role: "assistant",
          content: "",
          reasoning_content: item.summary.map((x) => x.text ?? "").join(""),
        });
    } else if (item.type === "message" || !item.type) {
      if (!["user", "assistant", "system", "developer"].includes(item.role))
        unsupported("unsupported message role");
      messages.push({ role: item.role, content: content(item.content) });
    } else unsupported(`unsupported input item: ${item.type}`);
  }
  let tools = b.tools === undefined && !found.length
    ? undefined
    : registry.chatTools;
  let tool_choice = b.tool_choice;
  if (["function", "custom"].includes(tool_choice?.type)) {
    const name = flatName(tool_choice.namespace, tool_choice.name);
    if (!registry.byName.has(name)) unsupported(`unknown tool choice: ${name}`);
    tool_choice = { type: "function", function: { name } };
  } else if (tool_choice?.type === "allowed_tools") {
    if (!["auto", "required"].includes(tool_choice.mode))
      unsupported(`unsupported allowed_tools mode: ${tool_choice.mode}`);
    const allowed = new Set(tool_choice.tools
      ?.filter((tool) => !isWebSearch(tool))
      .map((t) => flatName(t.namespace, t.name)));
    for (const name of allowed)
      if (!registry.byName.has(name)) unsupported(`unknown allowed tool: ${name}`);
    tools = registry.chatTools.filter((t) => allowed.has(t.function.name));
    if (tool_choice.mode === "required" && !tools.length)
      unsupported(tool_choice.tools?.some(isWebSearch)
        ? "required web search is unavailable for this provider"
        : "required tool choice has no available tools");
    tool_choice = tools.length ? tool_choice.mode : "none";
  } else if (isWebSearch(tool_choice)) {
    unsupported("web search is unavailable for this provider");
  } else if (tool_choice && typeof tool_choice !== "string")
    unsupported(`unsupported tool choice: ${tool_choice.type}`);
  if (tool_choice === "required" && !tools?.length)
    unsupported("required tool choice has no available tools");
  const out = {
    model: b.model,
    messages,
    stream: true,
    stream_options: { include_usage: true },
    tools,
    tool_choice,
    max_tokens: b.max_output_tokens,
    reasoning_effort: b.reasoning?.effort,
    temperature: b.temperature,
    top_p: b.top_p,
    parallel_tool_calls: b.parallel_tool_calls,
  };
  if (b.text?.format) {
    const f = b.text.format;
    out.response_format =
      f.type === "json_schema"
        ? {
            type: "json_schema",
            json_schema: { name: f.name, schema: f.schema, strict: f.strict },
          }
        : f;
  }
  return out;
}
function messagesRequest(chat) {
  const system = [], messages = [];
  function blocks(value) {
    if (typeof value === "string") return value ? [{ type: "text", text: value }] : [];
    return (value ?? []).map((part) => {
      if (part.type === "text") return part;
      if (part.type !== "image_url") unsupported(`unsupported Messages content: ${part.type}`);
      const url = part.image_url.url;
      const data = /^data:([^;,]+);base64,(.+)$/s.exec(url);
      return { type: "image", source: data
        ? { type: "base64", media_type: data[1], data: data[2] }
        : { type: "url", url } };
    });
  }
  for (const m of chat.messages) {
    if (["system", "developer"].includes(m.role)) { system.push(...blocks(m.content)); continue; }
    const role = m.role === "assistant" ? "assistant" : "user";
    const content = m.role === "tool"
      ? [{ type: "tool_result", tool_use_id: m.tool_call_id, content: blocks(m.content) }]
      : blocks(m.content);
    // Responses reasoning summaries do not carry Anthropic thinking signatures.
    for (const call of m.tool_calls ?? []) {
      let input;
      try { input = JSON.parse(call.function.arguments || "{}"); }
      catch { unsupported("tool arguments must be JSON for Messages"); }
      if (!input || typeof input !== "object" || Array.isArray(input))
        unsupported("tool arguments must be an object for Messages");
      content.push({ type: "tool_use", id: call.id, name: call.function.name, input });
    }
    if (!content.length) continue;
    if (messages.at(-1)?.role === role) messages.at(-1).content.push(...content);
    else messages.push({ role, content });
  }
  let tool_choice = chat.tool_choice;
  if (tool_choice?.type === "function") tool_choice = { type: "tool", name: tool_choice.function.name };
  else if (tool_choice) tool_choice = { type: tool_choice === "required" ? "any" : tool_choice };
  if (tool_choice && chat.parallel_tool_calls === false) tool_choice.disable_parallel_tool_use = true;
  if (chat.response_format && chat.response_format.type !== "text")
    unsupported("structured output is unavailable for Messages providers");
  const max_tokens = chat.max_tokens ?? 16384;
  const effort = chat.reasoning_effort;
  const budget = { minimal: 1024, low: 2048, medium: 8192, high: 16000, xhigh: 32000, max: 32000 }[effort];
  return {
    model: chat.model, messages, system: system.length ? system : undefined,
    max_tokens, stream: true, temperature: chat.temperature, top_p: chat.top_p,
    tools: chat.tools?.map(({ function: f }) => ({ name: f.name, description: f.description, input_schema: f.parameters })),
    tool_choice,
    ...(effort === "none" ? { thinking: { type: "disabled" } }
      : budget && max_tokens > 1024 ? { thinking: { type: "enabled", budget_tokens: Math.min(budget, max_tokens - 1) } } : {}),
  };
}
async function* events(body, state) {
  const decoder = new TextDecoder();
  let pending = "",
    data = [],
    size = 0;
  const reader = body.getReader();
  state.reader = reader;
  try {
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) break;
      pending += decoder.decode(chunk.value, { stream: true });
      let i;
      while ((i = pending.indexOf("\n")) >= 0) {
        const line = pending.slice(0, i).replace(/\r$/, "");
        pending = pending.slice(i + 1);
        if (line === "") {
          if (data.length) yield data.join("\n");
          data = [];
          size = 0;
        } else if (line.startsWith("data:")) {
          const text = line.slice(5).trimStart();
          size += text.length;
          if (size > 1024 * 1024)
            throw new ProtocolError("upstream SSE event too large");
          data.push(text);
        }
      }
      if (pending.length + size > 1024 * 1024)
        throw new ProtocolError("upstream SSE event too large");
    }
    pending += decoder.decode();
    if (pending.trim() || data.length)
      throw new ProtocolError("incomplete upstream SSE event");
  } finally {
    await reader.cancel().catch(() => {});
    reader.releaseLock();
    state.reader = null;
  }
}
// Normalize Messages SSE into the existing Chat event converter. Both stages
// pull on demand and cancellation propagates to the original upstream reader.
function messagesResponse(res) {
  if (!res.headers.get("content-type")?.includes("text/event-stream"))
    unsupported("expected streamed Messages response");
  const state = { reader: null };
  async function* chunks() {
    let started = false, stopped = false, finish = null, model;
    let input = 0, cached = 0, output = 0;
    const blocks = new Map();
    const chunk = (delta = {}, finish_reason = null, usage) => ({
      model, choices: [{ index: 0, delta, finish_reason }], ...(usage ? { usage } : {}),
    });
    for await (const raw of events(res.body, state)) {
      const v = JSON.parse(raw);
      if (v.type === "error") unsupported("upstream Messages stream returned an error");
      if (v.type === "ping") continue;
      if (v.type === "message_start") {
        if (started) unsupported("duplicate Messages start");
        started = true;
        model = v.message?.model;
        const u = v.message?.usage ?? {};
        cached = u.cache_read_input_tokens ?? 0;
        input = (u.input_tokens ?? 0) + cached + (u.cache_creation_input_tokens ?? 0);
        output = u.output_tokens ?? 0;
      } else if (!started) unsupported("Messages stream missing message_start");
      else if (v.type === "content_block_start") {
        const b = v.content_block;
        if (!b || blocks.has(v.index)) unsupported("invalid Messages content block");
        blocks.set(v.index, { type: b.type, hasInput: !!b.input && Object.keys(b.input).length > 0 });
        if (b.type === "tool_use") {
          yield chunk({ tool_calls: [{ index: v.index, id: b.id, function: { name: b.name,
            arguments: b.input && Object.keys(b.input).length ? JSON.stringify(b.input) : "" } }] });
        } else if (b.type === "text" && b.text) yield chunk({ content: b.text });
        else if (b.type === "thinking" && b.thinking) yield chunk({ reasoning_content: b.thinking });
        else if (!["text", "thinking", "redacted_thinking"].includes(b.type)) unsupported(`unsupported Messages block: ${b.type}`);
      } else if (v.type === "content_block_delta") {
        const d = v.delta;
        const block = blocks.get(v.index);
        if (!block) unsupported("Messages delta without block");
        if (d?.type === "text_delta" && block.type === "text") yield chunk({ content: d.text });
        else if (d?.type === "thinking_delta" && block.type === "thinking") yield chunk({ reasoning_content: d.thinking });
        else if (d?.type === "input_json_delta" && block.type === "tool_use") {
          block.hasInput ||= !!d.partial_json;
          yield chunk({ tool_calls: [{ index: v.index, function: { arguments: d.partial_json } }] });
        }
        else if (d?.type !== "signature_delta") unsupported(`unsupported Messages delta: ${d?.type}`);
      } else if (v.type === "content_block_stop") {
        const block = blocks.get(v.index);
        if (block?.type === "tool_use" && !block.hasInput)
          yield chunk({ tool_calls: [{ index: v.index, function: { arguments: "{}" } }] });
        if (!blocks.delete(v.index)) unsupported("Messages stop without block");
      } else if (v.type === "message_delta") {
        const reason = v.delta?.stop_reason;
        if (reason) {
          finish = { end_turn: "stop", stop_sequence: "stop", tool_use: "tool_calls", max_tokens: "length", refusal: "content_filter" }[reason];
          if (!finish) unsupported(`unsupported Messages stop reason: ${reason}`);
        }
        if (v.usage?.output_tokens !== undefined) output = v.usage.output_tokens;
      } else if (v.type === "message_stop") {
        if (!finish || blocks.size) unsupported("incomplete Messages response");
        stopped = true;
        yield chunk({}, finish, { prompt_tokens: input, completion_tokens: output,
          total_tokens: input + output, prompt_tokens_details: { cached_tokens: cached } });
        yield "[DONE]";
        return;
      } else unsupported(`unsupported Messages event: ${v.type}`);
    }
    if (!stopped) unsupported("upstream Messages stream interrupted before completion");
  }
  const iterator = chunks(), encoder = new TextEncoder();
  return new Response(new ReadableStream({
    async pull(controller) {
      try {
        const next = await iterator.next();
        if (next.done) controller.close();
        else controller.enqueue(encoder.encode(`data: ${typeof next.value === "string" ? next.value : JSON.stringify(next.value)}\n\n`));
      } catch (error) { controller.error(error); }
    },
    async cancel() {
      if (state.reader) await state.reader.cancel().catch(() => {});
      else await res.body?.cancel().catch(() => {});
      await iterator.return?.();
    },
  }), { status: res.status, headers: { "content-type": "text/event-stream" } });
}
export async function translateResponse(from, to, res, request, report = () => {}) {
  if (from === to || !res.ok) return res;
  if (from === "responses" && to === "messages")
    return translateResponse(from, "chat", messagesResponse(res), request, report);
  if (from !== "responses" || to !== "chat")
    return unsupported("unsupported response conversion");
  if (!res.headers.get("content-type")?.includes("text/event-stream"))
    throw new ProtocolError("expected streamed chat response");
  const requestInput = Array.isArray(request.input) ? request.input : [];
  const declaredTools = toolRegistry(request.tools, discoveredTools(requestInput)).byName;
  const id = `resp_${crypto.randomUUID().replaceAll("-", "")}`;
  const created_at = Math.floor(Date.now() / 1000),
    output = [];
  let sequence = 0,
    finish = null,
    usage = null,
    model = request.model,
    total = 0;
  const enc = new TextEncoder();
  const emit = (type, fields = {}) => ({
    type,
    ...fields,
    sequence_number: sequence++,
  });
  const response = (status) => ({
    id,
    object: "response",
    created_at,
    status,
    model,
    output,
    ...(usage ? { usage } : {}),
  });
  let textItem = null,
    reasoningItem = null;
  const toolCalls = new Map();
  const written = new TextCalls();
  let recovered = 0, reasoningChars = 0, textChars = 0;
  const state = { reader: null };
  function* textDelta(text) {
    if (text) {
      if (!textItem) {
        textItem = {
          id: `msg_${crypto.randomUUID()}`,
          type: "message",
          role: "assistant",
          status: "in_progress",
          content: [{ type: "output_text", text: "", annotations: [] }],
        };
        output.push(textItem);
        yield emit("response.output_item.added", {
          output_index: output.length - 1,
          item: { ...textItem, content: [] },
        });
        yield emit("response.content_part.added", {
          item_id: textItem.id,
          output_index: output.length - 1,
          content_index: 0,
          part: structuredClone(textItem.content[0]),
        });
      }
      textItem.content[0].text += text;
      total += text.length;
      yield emit("response.output_text.delta", {
        item_id: textItem.id,
        output_index: output.indexOf(textItem),
        content_index: 0,
        delta: text,
      });
    }
  }
  async function* converted() {
    yield emit("response.created", {
      response: { ...response("in_progress"), output: [] },
    });
    yield emit("response.in_progress", {
      response: { ...response("in_progress"), output: [] },
    });
    for await (const raw of events(res.body, state)) {
      if (raw === "[DONE]") {
        if (!finish)
          throw new ProtocolError("upstream ended without finish reason");
        if (declaredTools.size) {
          const repaired = written.finish(declaredTools);
          yield* textDelta(repaired.text);
          for (const call of repaired.calls) {
            toolCalls.set(`written-${recovered++}`, call);
          }
        }
        const outcome = finish === "length" ? "incomplete" : finish === "content_filter" ? "failed" : "completed";
        // Chat can split a function name across chunks. Wait for the full name
        // before emitting an item so Codex sees the original namespace and type.
        for (const call of toolCalls.values()) {
          if (!call.name) throw new ProtocolError("upstream tool call has no name");
          const declared = declaredTools.get(call.name);
          const custom = declared?.type === "custom";
          const search = declared?.type === "tool_search";
          let searchArguments;
          if (search) {
            try {
              searchArguments = JSON.parse(call.arguments || "{}");
            } catch {
              throw new ProtocolError("upstream tool_search arguments are not JSON");
            }
            if (!searchArguments || typeof searchArguments !== "object" || Array.isArray(searchArguments))
              throw new ProtocolError("upstream tool_search arguments must be an object");
          }
          const item = {
            id: `${search ? "tsc" : custom ? "ctc" : "fc"}_${crypto.randomUUID()}`,
            type: search ? "tool_search_call" : custom ? "custom_tool_call" : "function_call",
            status: "in_progress",
            call_id: call.call_id ?? `call_${crypto.randomUUID()}`,
            ...(!search ? { name: declared?.name ?? call.name } : {}),
            ...(declared?.namespace ? { namespace: declared.namespace } : {}),
            ...(search
              ? { execution: "client", arguments: searchArguments }
              : custom
              ? { input: customInput(call.arguments) }
              : { arguments: call.arguments }),
          };
          output.push(item);
          const output_index = output.length - 1;
          yield emit("response.output_item.added", {
            output_index,
            item: search ? { ...item } : { ...item, [custom ? "input" : "arguments"]: "" },
          });
          if (custom && item.input)
            yield emit("response.custom_tool_call_input.delta", {
              item_id: item.id,
              output_index,
              delta: item.input,
            });
          else if (!custom && item.arguments)
            yield emit("response.function_call_arguments.delta", {
              item_id: item.id,
              output_index,
              delta: item.arguments,
            });
        }
        for (let index = 0; index < output.length; index++) {
          const item = output[index];
          if (item.type === "message") {
            // Match magpie/phase.go: a message before calls is commentary;
            // only the final message of a completed reply is a final answer.
            if (toolCalls.size) item.phase = "commentary";
            else if (outcome === "completed") item.phase = "final_answer";
            yield emit("response.output_text.done", {
              item_id: item.id,
              output_index: index,
              content_index: 0,
              text: item.content[0].text,
            });
            yield emit("response.content_part.done", {
              item_id: item.id,
              output_index: index,
              content_index: 0,
              part: item.content[0],
            });
          } else if (item.type === "function_call")
            yield emit("response.function_call_arguments.done", {
              item_id: item.id,
              output_index: index,
              arguments: item.arguments,
            });
          else if (item.type === "custom_tool_call")
            yield emit("response.custom_tool_call_input.done", {
              item_id: item.id,
              output_index: index,
              input: item.input,
            });
          else if (item.type === "tool_search_call") {
            // Tool search has no argument delta event; the completed item carries it.
          }
          else
            yield emit("response.reasoning_summary_text.done", {
              item_id: item.id,
              output_index: index,
              summary_index: 0,
              text: item.summary[0].text,
            });
          item.status = "completed";
          yield emit("response.output_item.done", {
            output_index: index,
            item,
          });
        }
        report({ stage: "converted", outcome, finish, tool_calls: toolCalls.size, recovered_calls: recovered, text_chars: textChars, reasoning_chars: reasoningChars });
        if (finish === "length") {
          yield emit("response.incomplete", {
            response: {
              ...response("incomplete"),
              incomplete_details: { reason: "max_output_tokens" },
            },
          });
        } else if (finish === "content_filter") {
          yield emit("response.failed", {
            response: {
              ...response("failed"),
              error: {
                code: "content_filter",
                message: "Upstream filtered the response",
              },
            },
          });
        } else
          yield emit("response.completed", { response: response("completed") });
        return;
      }
      const v = JSON.parse(raw);
      if (v.error) throw new ProtocolError("upstream stream returned an error");
      if (v.model) model = v.model;
      if (v.usage) {
        const u = v.usage;
        usage = {
          input_tokens: u.prompt_tokens,
          output_tokens: u.completion_tokens,
          total_tokens: u.total_tokens,
          input_tokens_details: {
            cached_tokens: u.prompt_tokens_details?.cached_tokens ?? 0,
          },
          output_tokens_details: {
            reasoning_tokens:
              u.completion_tokens_details?.reasoning_tokens ?? 0,
          },
        };
      }
      const c = v.choices?.find((x) => x.index === 0) ?? v.choices?.[0];
      if (!c) continue;
      if (c.finish_reason) finish = c.finish_reason;
      const d = c.delta ?? {};
      if (d.reasoning_content) {
        reasoningChars += d.reasoning_content.length;
        if (!reasoningItem) {
          reasoningItem = {
            id: `rs_${crypto.randomUUID()}`,
            type: "reasoning",
            status: "in_progress",
            summary: [{ type: "summary_text", text: "" }],
          };
          output.push(reasoningItem);
          yield emit("response.output_item.added", {
            output_index: output.length - 1,
            item: structuredClone(reasoningItem),
          });
          yield emit("response.reasoning_summary_part.added", {
            item_id: reasoningItem.id,
            output_index: output.length - 1,
            summary_index: 0,
            part: { type: "summary_text", text: "" },
          });
        }
        reasoningItem.summary[0].text += d.reasoning_content;
        total += d.reasoning_content.length;
        yield emit("response.reasoning_summary_text.delta", {
          item_id: reasoningItem.id,
          output_index: output.indexOf(reasoningItem),
          summary_index: 0,
          delta: d.reasoning_content,
        });
      }
      if (d.content) {
        textChars += d.content.length;
        yield* textDelta(declaredTools.size ? written.push(d.content) : d.content);
      }
      for (const call of d.tool_calls ?? []) {
        const key = call.index ?? 0;
        let item = toolCalls.get(key);
        if (!item) {
          item = {
            call_id: call.id ?? null,
            name: "",
            arguments: "",
          };
          toolCalls.set(key, item);
        }
        if (call.id) item.call_id = call.id;
        if (call.function?.name) {
          item.name += call.function.name;
          total += call.function.name.length;
        }
        if (call.function?.arguments) {
          item.arguments += call.function.arguments;
          total += call.function.arguments.length;
        }
      }
      if (total > 16 * 1024 * 1024 || textChars + reasoningChars > 16 * 1024 * 1024)
        throw new ProtocolError("converted response too large");
    }
    throw new ProtocolError("upstream stream interrupted before completion");
  }
  async function* diagnosed() {
    try { yield* converted(); }
    catch (e) {
      const reason = /without finish reason/.test(e.message) ? "missing_finish"
        : /before completion/.test(e.message) ? "missing_terminal"
        : /stream returned an error/.test(e.message) ? "upstream_error"
        : /too large/.test(e.message) ? "size_limit"
        : /tool/.test(e.message) ? "invalid_tool_call" : "other";
      report({ stage: "converted", outcome: "interrupted", reason, tool_calls: toolCalls.size, recovered_calls: recovered, text_chars: textChars, reasoning_chars: reasoningChars });
      throw e;
    }
  }
  if (!request.stream) {
    let last;
    for await (const v of diagnosed())
      if (
        [
          "response.completed",
          "response.incomplete",
          "response.failed",
        ].includes(v.type)
      )
        last = v.response;
    return Response.json(last);
  }
  const iterator = diagnosed();
  const stream = new ReadableStream({
    async pull(controller) {
      try {
        const next = await iterator.next();
        if (next.done) controller.close();
        else
          controller.enqueue(
            enc.encode(
              `event: ${next.value.type}\ndata: ${JSON.stringify(next.value)}\n\n`,
            ),
          );
      } catch (e) {
        controller.error(e);
      }
    },
    async cancel() {
      if (state.reader) await state.reader.cancel().catch(() => {});
      else await res.body?.cancel().catch(() => {});
      await iterator.return?.();
    },
  });
  return new Response(stream, {
    status: res.status,
    headers: {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
    },
  });
}
