use super::{Observation, RequestParser};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

// Typed envelopes skip conversation/tool/image fields without allocating their contents.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Event {
    r#type: String,
    timestamp: String,
    uuid: String,
    #[serde(rename = "parentUuid", deserialize_with = "nullable_string")]
    parent: String,
    #[serde(rename = "sessionId")]
    session: String,
    #[serde(rename = "apiBlockIndex")]
    block: Option<u64>,
    #[serde(rename = "isApiErrorMessage")]
    api_error: bool,
    payload: Payload,
    message: Message,
}
fn nullable_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct Payload {
    r#type: String,
    role: String,
    id: String,
    turn_id: String,
    model: String,
    model_provider: Option<String>,
    info: Info,
    duration_ms: Option<i64>,
    time_to_first_token_ms: Option<i64>,
    forked_from_id: Option<String>,
    thread_spawn: Option<Value>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct Info {
    total_token_usage: Option<Usage>,
    last_token_usage: Option<Usage>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct Message {
    id: String,
    model: String,
    usage: Option<ClaudeUsage>,
    #[serde(deserialize_with = "content_kinds")]
    content: Vec<Content>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct ClaudeUsage {
    output_tokens: u64,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct Content {
    r#type: String,
}
fn time(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp_millis())
}
fn duration(start: Option<i64>, end: i64) -> Option<i64> {
    end.checked_sub(start?)
        .filter(|ms| (1..=7_200_000).contains(ms))
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Codex {
    session: String,
    model: String,
    upstream: Option<String>,
    turn: Option<String>,
    start: Option<i64>,
    asked: Option<i64>,
    answered: Option<i64>,
    answering_turn: Option<String>,
    last_end: Option<i64>,
    high: Usage,
    previous: String,
    calls: u64,
    first_call: Option<Observation>,
    fork_at: Option<i64>,
    invalid_fork: bool,
}
pub(super) fn codex(state: Value) -> Box<dyn RequestParser> {
    Box::new(serde_json::from_value::<Codex>(state).unwrap_or_default())
}
impl RequestParser for Codex {
    fn checkpoint(&self) -> Value {
        serde_json::to_value(self).unwrap_or_default()
    }
    fn feed(&mut self, line: &[u8]) -> Result<Vec<Observation>, String> {
        let e = decode(line)?;
        let at = time(&e.timestamp);
        let p = e.payload;
        if e.r#type == "session_meta" {
            if self.session != p.id {
                *self = Self::default();
            }
            self.session = p.id;
            self.upstream = p.model_provider;
            if p.forked_from_id.is_some() || p.thread_spawn.is_some() {
                self.fork_at = at;
                self.invalid_fork = at.is_none();
            }
        } else if e.r#type == "turn_context" {
            if !p.model.is_empty() {
                self.model = p.model;
            }
            if p.model_provider.is_some() {
                self.upstream = p.model_provider;
            }
            if !p.turn_id.is_empty() {
                self.turn = Some(p.turn_id);
            }
        } else if e.r#type == "response_item"
            && (p.role == "assistant"
                || matches!(
                    p.r#type.as_str(),
                    "function_call"
                        | "custom_tool_call"
                        | "local_shell_call"
                        | "reasoning"
                        | "web_search_call"
                        | "tool_search_call"
                        | "image_generation_call"
                ))
        {
            if self.answered.is_none() {
                self.asked = self.start;
                self.answering_turn = self.turn.clone();
            }
            self.answered = at;
        } else if e.r#type == "response_item"
            && (p.role == "user"
                || p.r#type == "function_call_output"
                || p.r#type == "custom_tool_call_output")
        {
            self.start = at;
        } else if e.r#type == "event_msg" {
            match p.r#type.as_str() {
                "task_started" => {
                    self.start = at;
                    self.asked = None;
                    self.answered = None;
                    self.answering_turn = None;
                    self.calls = 0;
                    self.first_call = None;
                    self.turn = (!p.turn_id.is_empty()).then_some(p.turn_id);
                }
                "token_count" => {
                    let Some(at) = at else { return Ok(Vec::new()) };
                    if self.invalid_fork {
                        return Ok(Vec::new());
                    }
                    let (total, last) = (p.info.total_token_usage, p.info.last_token_usage);
                    if total.is_none() && last.is_none() {
                        return Ok(Vec::new());
                    }
                    let signature = serde_json::to_string(&total).unwrap_or_default();
                    if total.is_some() && signature == self.previous {
                        return Ok(Vec::new());
                    }
                    let usage = last.unwrap_or_else(|| {
                        let total = total.as_ref().unwrap();
                        Usage {
                            input_tokens: total.input_tokens.saturating_sub(self.high.input_tokens),
                            output_tokens: total
                                .output_tokens
                                .saturating_sub(self.high.output_tokens),
                            reasoning_output_tokens: total
                                .reasoning_output_tokens
                                .saturating_sub(self.high.reasoning_output_tokens),
                        }
                    });
                    if let Some(total) = total {
                        self.high.input_tokens = self.high.input_tokens.max(total.input_tokens);
                        self.high.output_tokens = self.high.output_tokens.max(total.output_tokens);
                        self.high.reasoning_output_tokens = self
                            .high
                            .reasoning_output_tokens
                            .max(total.reasoning_output_tokens);
                        self.previous = signature.clone();
                    }
                    if self.fork_at.is_some_and(|cutoff| at < cutoff) {
                        return Ok(Vec::new());
                    }
                    let mut hash = Sha256::new();
                    hash.update(self.session.as_bytes());
                    hash.update([0]);
                    hash.update(e.timestamp.as_bytes());
                    hash.update([0]);
                    hash.update(signature.as_bytes());
                    hash.update(serde_json::to_vec(&usage).unwrap_or_default());
                    let call =
                        Observation {
                            id: format!("cx-{:x}", hash.finalize()),
                            session: self.session.clone(),
                            turn: self.turn.clone(),
                            at,
                            model: self.model.clone(),
                            upstream: self.upstream.clone(),
                            output: usage.output_tokens,
                            reasoning: usage.reasoning_output_tokens.min(usage.output_tokens),
                            duration_ms: {
                                // Older rollouts emit usage after their own tool output. Keep the
                                // input captured when answering began and exclude the tool's runtime.
                                let captured =
                                    self.answered.is_some() && self.answering_turn == self.turn;
                                let start = if captured { self.asked } else { self.start };
                                let start = start.map(|v| v.max(self.last_end.unwrap_or(v)));
                                let end = if captured
                                    && self.start.zip(self.answered).is_some_and(
                                        |(input, output)| input > output && output <= at,
                                    ) {
                                    self.answered.unwrap()
                                } else {
                                    at
                                };
                                duration(start, end)
                            },
                            ..Default::default()
                        };
                    self.last_end = Some(at);
                    self.asked = None;
                    self.answered = None;
                    self.answering_turn = None;
                    self.calls += 1;
                    if self.calls == 1 {
                        self.first_call = Some(call.clone());
                    }
                    return Ok(vec![call]);
                }
                "task_complete" => {
                    if self.calls == 1
                        && self.turn.as_deref() == Some(p.turn_id.as_str())
                        && let Some(mut call) = self.first_call.clone()
                        && let Some(ms) = p.duration_ms.filter(|ms| *ms > 0)
                    {
                        call.duration_ms = Some(ms);
                        call.ttft_ms = p.time_to_first_token_ms.filter(|t| *t > 0 && *t <= ms);
                        call.native_timing = true;
                        return Ok(vec![call]);
                    }
                }
                _ => {}
            }
        }
        Ok(Vec::new())
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Parent {
    id: String,
    parent: String,
    at: i64,
    attachment: bool,
    message: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct ClaudeCall {
    start: Option<i64>,
    call: Observation,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Claude {
    parents: VecDeque<Parent>,
    calls: VecDeque<ClaudeCall>,
}
pub(super) fn claude(state: Value) -> Box<dyn RequestParser> {
    Box::new(serde_json::from_value::<Claude>(state).unwrap_or_default())
}
impl RequestParser for Claude {
    fn checkpoint(&self) -> Value {
        serde_json::to_value(self).unwrap_or_default()
    }
    fn feed(&mut self, line: &[u8]) -> Result<Vec<Observation>, String> {
        let e = decode(line)?;
        let Some(at) = time(&e.timestamp) else {
            return Ok(Vec::new());
        };
        let mut parent = e.parent.as_str();
        let mut start = None;
        let id = if e.message.id.is_empty() {
            &e.uuid
        } else {
            &e.message.id
        };
        let first_block = e.block.is_none_or(|b| b == 0);
        for _ in 0..64 {
            let Some(node) = self.parents.iter().rev().find(|node| node.id == parent) else {
                break;
            };
            if !node.attachment {
                // A parent belonging to this same response is not a request start.
                if first_block && node.message.as_deref() != Some(id.as_str()) {
                    start = Some(node.at);
                }
                break;
            }
            parent = &node.parent;
        }
        if !e.uuid.is_empty() {
            self.parents.push_back(Parent {
                id: e.uuid.clone(),
                parent: e.parent,
                at,
                attachment: e.r#type == "attachment",
                message: (e.r#type == "assistant").then(|| id.clone()),
            });
            while self.parents.len() > 256 {
                self.parents.pop_front();
            }
        }
        if e.r#type != "assistant" || e.api_error || e.message.model == "<synthetic>" {
            return Ok(Vec::new());
        }
        let Some(usage) = e.message.usage else {
            return Ok(Vec::new());
        };
        let id = if e.message.id.is_empty() {
            e.uuid
        } else {
            e.message.id
        };
        if id.is_empty() {
            return Ok(Vec::new());
        }
        let index = self.calls.iter().position(|c| c.call.id == id);
        let mut state = index
            .and_then(|i| self.calls.remove(i))
            .unwrap_or_else(|| ClaudeCall {
                start,
                call: Observation {
                    id: id.clone(),
                    session: e.session,
                    at,
                    model: e.message.model,
                    ..Default::default()
                },
            });
        let call = &mut state.call;
        call.at = call.at.max(at);
        call.duration_ms = duration(state.start, call.at);
        call.output = call.output.max(usage.output_tokens);
        // Match Magpie's local Claude reader: block timestamps/thinkingDurationMs
        // do not establish when a network stream produced its first token.
        call.ttft_ms = None;
        let result = call.clone();
        self.calls.push_back(state);
        while self.calls.len() > 64 {
            self.calls.pop_front();
        }
        Ok(vec![result])
    }
}

fn decode(line: &[u8]) -> Result<Event, String> {
    let e: Event = serde_json::from_slice(line).map_err(|_| "日志事件格式无效")?;
    for value in [
        &e.uuid,
        &e.parent,
        &e.session,
        &e.payload.id,
        &e.payload.turn_id,
        &e.payload.model,
        &e.message.id,
        &e.message.model,
    ] {
        if value.len() > 256 || value.chars().any(char::is_control) {
            return Err("日志标识超出限制".into());
        }
    }
    Ok(e)
}
fn content_kinds<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Content>, D::Error> {
    struct Kinds;
    impl<'de> serde::de::Visitor<'de> for Kinds {
        type Value = Vec<Content>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("content list or text")
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let first = seq.next_element::<Content>()?;
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(first.into_iter().collect())
        }
    }
    d.deserialize_any(Kinds)
}
