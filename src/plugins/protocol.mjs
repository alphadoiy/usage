// Protocol conversion belongs to the host, independently of vendor plugins.
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
export function translateRequest(from, to, b) {
  if (from === to) return b;
  if (from !== "responses" || to !== "chat")
    return unsupported(`unsupported protocol conversion: ${from} to ${to}`);
  if (b.previous_response_id)
    return unsupported(
      "previous_response_id requires server-side conversation storage; send full input",
    );
  const messages = [];
  if (b.instructions)
    messages.push({ role: "system", content: b.instructions });
  const input =
    typeof b.input === "string"
      ? [{ type: "message", role: "user", content: b.input }]
      : (b.input ?? []);
  for (const item of input) {
    if (item.type === "function_call") {
      const call = {
        id: item.call_id,
        type: "function",
        function: { name: item.name, arguments: item.arguments ?? "" },
      };
      const previous = messages.at(-1);
      if (previous?.role === "assistant" && previous.tool_calls)
        previous.tool_calls.push(call);
      else
        messages.push({ role: "assistant", content: null, tool_calls: [call] });
    } else if (item.type === "function_call_output") {
      messages.push({
        role: "tool",
        tool_call_id: item.call_id,
        content: content(item.output),
      });
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
  const tools = b.tools?.map((t) => {
    if (t.type !== "function")
      return unsupported(`unsupported built-in tool: ${t.type}`);
    return {
      type: "function",
      function: {
        name: t.name,
        description: t.description,
        parameters: t.parameters ?? { type: "object", properties: {} },
        strict: t.strict,
      },
    };
  });
  let tool_choice = b.tool_choice;
  if (tool_choice?.type === "function")
    tool_choice = { type: "function", function: { name: tool_choice.name } };
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
export async function translateResponse(from, to, res, request) {
  if (from === to || !res.ok) return res;
  if (from !== "responses" || to !== "chat")
    return unsupported("unsupported response conversion");
  if (!res.headers.get("content-type")?.includes("text/event-stream"))
    throw new ProtocolError("expected streamed chat response");
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
  const tools = new Map();
  const state = { reader: null };
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
        for (let index = 0; index < output.length; index++) {
          const item = output[index];
          if (item.type === "message") {
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
        textItem.content[0].text += d.content;
        total += d.content.length;
        yield emit("response.output_text.delta", {
          item_id: textItem.id,
          output_index: output.indexOf(textItem),
          content_index: 0,
          delta: d.content,
        });
      }
      for (const call of d.tool_calls ?? []) {
        const key = call.index ?? 0;
        let item = tools.get(key);
        if (!item) {
          item = {
            id: `fc_${crypto.randomUUID()}`,
            type: "function_call",
            status: "in_progress",
            call_id: call.id ?? `call_${crypto.randomUUID()}`,
            name: call.function?.name ?? "",
            arguments: "",
          };
          tools.set(key, item);
          output.push(item);
          yield emit("response.output_item.added", {
            output_index: output.length - 1,
            item: structuredClone(item),
          });
        } else if (call.function?.name) item.name += call.function.name;
        if (call.function?.arguments) {
          item.arguments += call.function.arguments;
          total += call.function.arguments.length;
          yield emit("response.function_call_arguments.delta", {
            item_id: item.id,
            output_index: output.indexOf(item),
            delta: call.function.arguments,
          });
        }
      }
      if (total > 16 * 1024 * 1024)
        throw new ProtocolError("converted response too large");
    }
    throw new ProtocolError("upstream stream interrupted before completion");
  }
  if (!request.stream) {
    let last;
    for await (const v of converted())
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
  const iterator = converted();
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
