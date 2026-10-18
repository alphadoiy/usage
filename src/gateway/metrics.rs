//! Bounded SSE observation. Bytes/chunks are timing evidence, never token counts.
use serde_json::Value;

const MAX_EVENT: usize = 1024 * 1024;

#[derive(Default)]
pub(super) struct Observer {
    pending: Vec<u8>,
    data: String,
    invalid: bool,
    pub model: String,
    pub response_id: Option<String>,
    pub input: Option<u64>,
    pub cache_read: Option<u64>,
    pub output: Option<u64>,
    pub reasoning: u64,
    pub first: Option<i64>,
    pub first_text: Option<i64>,
    pub first_reasoning: Option<i64>,
    all: Flow,
    text: Flow,
    reasoning_flow: Flow,
    pub complete: bool,
    pub completed_at: Option<i64>,
}
#[derive(Default)]
struct Flow {
    bytes: u64,
    points: Vec<(u64, i64)>,
}
impl Flow {
    fn add(&mut self, bytes: usize, ms: i64) {
        self.bytes += bytes as u64;
        if self.points.len() >= 128 {
            self.points = self.points.iter().step_by(2).copied().collect();
        }
        self.points.push((self.bytes, ms));
    }
    fn window(&self) -> Option<i64> {
        let at = |fraction: u64| {
            self.points
                .iter()
                .find(|(n, _)| *n * 10 >= self.bytes * fraction)
                .map(|(_, ms)| *ms)
        };
        Some((at(9)? - at(1)?) * 10 / 8)
    }
}
impl Observer {
    pub fn new(model: String) -> Self {
        Self {
            model,
            ..Default::default()
        }
    }
    pub fn feed(&mut self, bytes: &[u8], ms: i64) {
        if self.invalid {
            return;
        }
        for &byte in bytes {
            if self.pending.len() + self.data.len() >= MAX_EVENT {
                self.invalid = true;
                self.pending.clear();
                self.data.clear();
                return;
            }
            if byte == b'\n' {
                let line = String::from_utf8_lossy(&self.pending);
                let line = line.trim_end_matches('\r');
                if line.is_empty() {
                    let data = std::mem::take(&mut self.data);
                    self.event(&data, ms);
                } else if let Some(data) = line.strip_prefix("data:") {
                    if !self.data.is_empty() {
                        self.data.push('\n');
                    }
                    self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
                }
                self.pending.clear();
            } else {
                self.pending.push(byte);
            }
        }
    }
    fn event(&mut self, data: &str, ms: i64) {
        if data == "[DONE]" {
            self.complete = true;
            self.completed_at = Some(ms.max(1));
            return;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return;
        };
        self.value(&event, ms, true);
    }
    pub fn json(&mut self, bytes: &[u8]) {
        if let Ok(value) = serde_json::from_slice(bytes) {
            self.value(&value, 0, false);
        }
    }
    pub fn websocket(&mut self, event: &Value, ms: i64) {
        self.value(event, ms, true);
    }
    fn value(&mut self, event: &Value, ms: i64, streamed: bool) {
        let response = event.get("response").unwrap_or(event);
        if let Some(model) = response
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 256)
        {
            self.model = model.into();
        }
        if let Some(id) = response
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 256)
        {
            self.response_id = Some(id.into());
        }
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(kind, "response.completed") {
            self.complete = true;
            self.completed_at = Some(ms.max(1));
        }
        if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
            self.input = usage
                .get("input_tokens")
                .or_else(|| usage.get("prompt_tokens"))
                .and_then(Value::as_u64);
            self.cache_read = usage
                .pointer("/input_tokens_details/cached_tokens")
                .or_else(|| usage.pointer("/prompt_tokens_details/cached_tokens"))
                .and_then(Value::as_u64);
            self.output = usage
                .get("output_tokens")
                .or_else(|| usage.get("completion_tokens"))
                .and_then(Value::as_u64);
            self.reasoning = usage
                .pointer("/output_tokens_details/reasoning_tokens")
                .or_else(|| usage.pointer("/completion_tokens_details/reasoning_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if !streamed
                && !matches!(
                    response.get("status").and_then(Value::as_str),
                    Some("failed" | "incomplete" | "cancelled")
                )
                && !matches!(kind, "response.failed" | "response.incomplete")
            {
                self.complete = true;
            }
        }
        if !streamed {
            return;
        }
        let mut content = |text: &str, is_text: bool, reasoning: bool| {
            if text.is_empty() {
                return;
            }
            self.first.get_or_insert(ms.max(1));
            self.all.add(text.len(), ms.max(1));
            if is_text {
                self.first_text.get_or_insert(ms.max(1));
                self.text.add(text.len(), ms.max(1));
            }
            if reasoning {
                self.first_reasoning.get_or_insert(ms.max(1));
                self.reasoning_flow.add(text.len(), ms.max(1));
            }
        };
        if let Some(delta) = event.get("delta").and_then(Value::as_str) {
            match kind {
                "response.output_text.delta" => content(delta, true, false),
                "response.reasoning_text.delta" => content(delta, false, true),
                "response.reasoning_summary_text.delta"
                | "response.function_call_arguments.delta" => content(delta, false, false),
                _ => {}
            }
        }
        if let Some(choices) = event.get("choices").and_then(Value::as_array) {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    if let Some(text) = delta.get("content").and_then(Value::as_str) {
                        content(text, true, false);
                    }
                    if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
                        content(text, false, true);
                    }
                    if let Some(tools) = delta.get("tool_calls").and_then(Value::as_array) {
                        for tool in tools {
                            if let Some(text) =
                                tool.pointer("/function/arguments").and_then(Value::as_str)
                            {
                                content(text, false, false);
                            }
                        }
                    }
                }
            }
        }
    }
    pub fn flow(&self) -> Option<i64> {
        if self.reasoning == 0 {
            self.all.window()
        } else if self.first_text.is_some() {
            self.text.window()
        } else {
            self.reasoning_flow.window()
        }
    }
    pub fn speed_unavailable(&self, duration: i64) -> Option<String> {
        let reason = if self.invalid {
            "observation_limit"
        } else if !self.complete {
            "incomplete_response"
        } else if self.output.is_none() {
            "missing_token_usage"
        } else if self.first.is_none() {
            "no_stream_content_timing"
        } else if self.reasoning > 0 && self.first_text.is_none() && self.first_reasoning.is_none()
        {
            "reasoning_not_observed"
        } else if self.decode(duration).is_none() {
            "insufficient_or_buffered_stream"
        } else {
            return None;
        };
        Some(reason.into())
    }
    pub fn decode(&self, duration: i64) -> Option<(u64, i64)> {
        if self.invalid || !self.complete {
            return None;
        }
        let output = self.output?;
        if self.reasoning > output {
            return None;
        }
        let (tokens, start, flow) = if self.reasoning == 0 {
            (output, self.first?, &self.all)
        } else if let Some(start) = self.first_text {
            (output - self.reasoning, start, &self.text)
        } else {
            (output, self.first_reasoning?, &self.reasoning_flow)
        };
        let window = self.completed_at.unwrap_or(duration).min(duration) - start;
        if tokens == 0 || window < 100 || tokens.saturating_mul(1000) > 10_000 * window as u64 {
            return None;
        }
        if flow.window()? * 4 < window {
            return None;
        }
        if self.reasoning > 0
            && self.first_text.is_some()
            && i128::from(tokens) * i128::from(start - self.first?)
                > 20 * i128::from(self.reasoning) * i128::from(window)
        {
            return None;
        }
        Some((tokens, window))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn send(o: &mut Observer, value: Value, at: i64) {
        let bytes = format!("data: {value}\r\n\r\n");
        for chunk in bytes.as_bytes().chunks(3) {
            o.feed(chunk, at);
        }
    }
    #[test]
    fn split_frames_skip_metadata_and_measure_answer_after_reasoning() {
        let mut o = Observer::default();
        send(&mut o, serde_json::json!({"type":"response.created"}), 1);
        assert_eq!(o.first, None);
        send(
            &mut o,
            serde_json::json!({"type":"response.reasoning_summary_text.delta","delta":"summary"}),
            200,
        );
        for n in 0..=10 {
            send(
                &mut o,
                serde_json::json!({"type":"response.output_text.delta","delta":"abc"}),
                1000 + n * 100,
            );
        }
        send(
            &mut o,
            serde_json::json!({"type":"response.completed","response":{"id":"resp-x","model":"served","usage":{"output_tokens":400,"output_tokens_details":{"reasoning_tokens":200}}}}),
            2100,
        );
        assert_eq!(o.decode(2100), Some((200, 1100)));
        assert_eq!(o.first, Some(200));
        assert_eq!(o.model, "served");
    }
    #[test]
    fn missing_usage_partial_and_burst_have_no_speed() {
        let mut o = Observer::default();
        send(
            &mut o,
            serde_json::json!({"type":"response.output_text.delta","delta":"x"}),
            100,
        );
        assert_eq!(o.decode(2000), None);
        send(
            &mut o,
            serde_json::json!({"type":"response.output_text.delta","delta":"a".repeat(1000)}),
            1900,
        );
        send(
            &mut o,
            serde_json::json!({"type":"response.completed","response":{"usage":{"output_tokens":1000}}}),
            2000,
        );
        assert_eq!(o.decode(2000), None);
    }
    #[test]
    fn oversized_events_stop_observation_without_changing_transport() {
        let mut o = Observer::default();
        o.feed(&vec![b'x'; MAX_EVENT + 1], 1);
        assert!(o.invalid);
        assert!(o.pending.is_empty());
    }
}
