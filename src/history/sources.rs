//! Source-specific history discovery and parsing. Add a reader here to support a new log format.
use super::*;

trait HistorySource: Sync {
    fn id(&self) -> &'static str;
    fn roots(&self) -> &'static [&'static str];
    fn file_name(&self) -> Option<&'static str> {
        None
    }
    fn initial_state(&self, path: &Path) -> ParserState {
        ParserState {
            session: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ..ParserState::default()
        }
    }
    fn relevant(&self, line: &str) -> bool;
    fn consume(
        &self,
        value: &Value,
        index: usize,
        state: &mut ParserState,
        records: &mut Vec<Record>,
        malformed: &mut usize,
    );
}

struct Codex;
struct Claude;
struct Grok;
static CODEX: Codex = Codex;
static CLAUDE: Claude = Claude;
static GROK: Grok = Grok;
static READERS: [&dyn HistorySource; 3] = [&CODEX, &CLAUDE, &GROK];

fn source(id: &str) -> Option<&'static dyn HistorySource> {
    READERS.iter().copied().find(|reader| reader.id() == id)
}

pub(super) fn supported(id: &str) -> bool {
    source(id).is_some()
}

pub(super) fn file_name(id: &str) -> Option<&'static str> {
    source(id).and_then(HistorySource::file_name)
}

pub(super) fn history_roots(config: &AppConfig) -> Vec<(String, String, PathBuf)> {
    let mut roots = Vec::new();
    for instance in config
        .data_instances()
        .into_iter()
        .filter(|i| config.providers.contains(&i.provider))
    {
        if let Some(reader) = source(&instance.provider) {
            for suffix in reader.roots() {
                roots.push((
                    instance.provider.clone(),
                    instance.id.clone(),
                    instance.path.join(suffix),
                ));
            }
        }
    }
    roots
}

pub(super) fn collect_files(
    root: &Path,
    source: &str,
    name: Option<&str>,
    out: &mut Vec<(PathBuf, String)>,
    warnings: &mut Vec<String>,
    cancel: &crate::background::Cancellation,
) {
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            warnings.push(format!("{source}: 无法读取日志目录"));
            return;
        }
    };
    for entry in entries {
        if cancel.is_cancelled() {
            return;
        }
        let Ok(entry) = entry else {
            warnings.push(format!("{source}: 无法读取目录项"));
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_files(&entry.path(), source, name, out, warnings, cancel);
        } else if kind.is_file()
            && let Some(path) = crate::logfiles::discovered(&entry.path())
            && name.is_none_or(|n| path.file_name().is_some_and(|file| file == n))
        {
            out.push((path, source.into()));
        }
    }
}

fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}
pub(super) fn time(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(text) = value.as_str() {
        DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    } else {
        value.as_i64().and_then(|n| {
            if n.unsigned_abs() > 100_000_000_000 {
                DateTime::from_timestamp_millis(n)
            } else {
                DateTime::from_timestamp(n, 0)
            }
        })
    }
}
fn codex_tokens(v: &Value) -> Tokens {
    let input = number(v, "input_tokens");
    Tokens {
        input,
        output: number(v, "output_tokens"),
        cached: number(v, "cached_input_tokens").min(input),
        calls: 1,
    }
}
fn grok_tokens(u: &Value) -> Tokens {
    let input = number(u, "inputTokens");
    Tokens {
        input,
        output: number(u, "outputTokens"),
        cached: number(u, "cachedReadTokens").min(input),
        calls: number(u, "modelCalls").max(1),
    }
}
fn emit(
    source: &str,
    id: String,
    timestamp: Option<DateTime<Utc>>,
    tokens: Tokens,
    state: &ParserState,
    records: &mut Vec<Record>,
    malformed: &mut usize,
) {
    if let Some(time) = timestamp {
        records.push(Record {
            source: source.into(),
            instance: source.into(),
            id,
            time,
            tokens,
            session: state.session.clone(),
            project: state.project.clone(),
            model: state.model.clone(),
        });
    } else {
        *malformed += 1;
    }
}

impl HistorySource for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn roots(&self) -> &'static [&'static str] {
        &["sessions", "archived_sessions"]
    }
    fn relevant(&self, line: &str) -> bool {
        line.contains("\"token_count\"")
            || line.contains("\"session_meta\"")
            || line.contains("\"turn_context\"")
    }
    fn consume(
        &self,
        v: &Value,
        index: usize,
        state: &mut ParserState,
        records: &mut Vec<Record>,
        malformed: &mut usize,
    ) {
        if v["type"] == "turn_context" {
            if let Some(name) = v.pointer("/payload/model").and_then(Value::as_str) {
                state.model = name.into();
            }
            if let Some(cwd) = v.pointer("/payload/cwd").and_then(Value::as_str) {
                state.project = cwd.into();
            }
            return;
        }
        if v["type"] == "session_meta" {
            if let Some(id) = v["payload"]["id"].as_str() {
                state.session = id.into();
            }
            if let Some(cwd) = v.pointer("/payload/cwd").and_then(Value::as_str) {
                state.project = cwd.into();
            }
            if v["payload"]
                .get("forked_from_id")
                .is_some_and(|p| !p.is_null())
                || v.pointer("/payload/thread_spawn/parent_thread_id")
                    .is_some()
            {
                state.fork_time = v.get("timestamp").and_then(time);
                if state.fork_time.is_none() {
                    *malformed += 1;
                    state.invalid_fork = true;
                }
            }
            return;
        }
        if state.invalid_fork || v["type"] != "event_msg" || v["payload"]["type"] != "token_count" {
            return;
        }
        let info = &v["payload"]["info"];
        let total = info
            .get("total_token_usage")
            .filter(|v| v.is_object())
            .map(codex_tokens);
        let last = info
            .get("last_token_usage")
            .filter(|v| v.is_object())
            .map(codex_tokens);
        if total.is_none() && last.is_none() {
            return;
        }
        let signature =
            serde_json::to_string(&(info.get("total_token_usage"), info.get("last_token_usage")))
                .unwrap_or_default();
        let key = v
            .pointer("/payload/rate_limits/limit_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let duplicate = total.is_some()
            && (state.signatures.get(&key) == Some(&signature)
                || state.previous.as_ref() == Some(&signature));
        if total.is_some() {
            state.signatures.insert(key, signature.clone());
        }
        state.previous = Some(signature);
        let mut tokens = last.unwrap_or_else(|| {
            let t = total.unwrap_or_default();
            Tokens {
                input: t.input.saturating_sub(state.high.input),
                output: t.output.saturating_sub(state.high.output),
                cached: t.cached.saturating_sub(state.high.cached),
                calls: 1,
            }
        });
        tokens.cached = tokens.cached.min(tokens.input);
        if let Some(t) = total {
            state.high.merge(t);
        }
        if duplicate {
            return;
        }
        let timestamp = v.get("timestamp").and_then(time);
        if state
            .fork_time
            .is_some_and(|cutoff| timestamp.is_some_and(|t| t < cutoff))
        {
            return;
        }
        // The same rollout can appear in sessions/ and archived_sessions/.
        emit(
            self.id(),
            format!("{}:{index}", state.session),
            timestamp,
            tokens,
            state,
            records,
            malformed,
        );
    }
}

impl HistorySource for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn roots(&self) -> &'static [&'static str] {
        &["projects"]
    }
    fn relevant(&self, line: &str) -> bool {
        line.contains("\"usage\"") || line.contains("\"cwd\"")
    }
    fn consume(
        &self,
        v: &Value,
        index: usize,
        state: &mut ParserState,
        records: &mut Vec<Record>,
        malformed: &mut usize,
    ) {
        if let Some(cwd) = v["cwd"].as_str() {
            state.project = cwd.into();
        }
        if let Some(id) = v["sessionId"].as_str() {
            state.session = id.into();
        }
        if let Some(name) = v.pointer("/message/model").and_then(Value::as_str) {
            state.model = name.into();
        }
        if v["type"] != "assistant" || v.pointer("/message/usage").is_none() {
            return;
        }
        let u = &v["message"]["usage"];
        let cached = number(u, "cache_read_input_tokens");
        let input = number(u, "input_tokens")
            .saturating_add(cached)
            .saturating_add(number(u, "cache_creation_input_tokens"));
        let id = v
            .pointer("/message/id")
            .and_then(Value::as_str)
            .or_else(|| v["uuid"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}:{index}", state.session));
        emit(
            self.id(),
            id,
            v.get("timestamp").and_then(time),
            Tokens {
                input,
                output: number(u, "output_tokens"),
                cached,
                calls: 1,
            },
            state,
            records,
            malformed,
        );
    }
}

impl HistorySource for Grok {
    fn id(&self) -> &'static str {
        "grok"
    }
    fn roots(&self) -> &'static [&'static str] {
        &["sessions"]
    }
    fn file_name(&self) -> Option<&'static str> {
        Some("updates.jsonl")
    }
    fn initial_state(&self, path: &Path) -> ParserState {
        let mut state = ParserState {
            session: path
                .parent()
                .and_then(Path::file_name)
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ..ParserState::default()
        };
        if let Ok(text) = fs::read_to_string(path.with_file_name("summary.json"))
            && let Ok(v) = serde_json::from_str::<Value>(&text)
        {
            state.project = v
                .pointer("/info/cwd")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into();
            state.model = v
                .pointer("/info/model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into();
        }
        state
    }
    fn relevant(&self, line: &str) -> bool {
        line.contains("\"turn_completed\"")
    }
    fn consume(
        &self,
        v: &Value,
        index: usize,
        state: &mut ParserState,
        records: &mut Vec<Record>,
        malformed: &mut usize,
    ) {
        if let Some(name) = v.pointer("/params/update/model").and_then(Value::as_str) {
            state.model = name.into();
        }
        if v.pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            != Some("turn_completed")
        {
            return;
        }
        let update = &v["params"]["update"];
        let u = &update["usage"];
        if !u.is_object() {
            return;
        }
        let id = update["prompt_id"]
            .as_str()
            .or_else(|| v.pointer("/params/_meta/promptId").and_then(Value::as_str))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}:{index}", state.session));
        let id = format!("{}:{id}", state.session);
        let input = number(u, "inputTokens");
        let tokens = Tokens {
            input,
            output: number(u, "outputTokens"),
            cached: number(u, "cachedReadTokens").min(input),
            calls: number(u, "modelCalls").max(1),
        };
        let timestamp = v.get("timestamp").and_then(time);
        if let Some(time) = timestamp
            && let Some(by_model) = u
                .get("modelUsage")
                .and_then(Value::as_object)
                .filter(|m| !m.is_empty())
        {
            let mut total = Tokens::default();
            for counters in by_model.values() {
                total.add(grok_tokens(counters));
            }
            // Split only complete breakdowns; partial maps must not inflate totals.
            if total == tokens || tokens.total() == 0 {
                for (name, counters) in by_model {
                    records.push(Record {
                        source: self.id().into(),
                        instance: self.id().into(),
                        id: format!("{id}:{name}"),
                        time,
                        tokens: grok_tokens(counters),
                        session: state.session.clone(),
                        project: state.project.clone(),
                        model: name.clone(),
                    });
                }
                return;
            }
        }
        emit(self.id(), id, timestamp, tokens, state, records, malformed);
    }
}

pub(super) fn fresh_file(path: &Path, source_id: &str) -> CachedFile {
    let state = source(source_id)
        .map(|reader| reader.initial_state(path))
        .unwrap_or_default();
    CachedFile {
        source: source_id.into(),
        compressed: false,
        len: 0,
        modified: None,
        offset: 0,
        fingerprint: Vec::new(),
        state,
        records: Vec::new(),
        split: false,
        malformed: 0,
    }
}

pub(super) fn append_file(
    path: &Path,
    source_id: &str,
    entry: &mut CachedFile,
    cancel: &crate::background::Cancellation,
) -> std::io::Result<()> {
    let reader_source = source(source_id).ok_or(std::io::ErrorKind::InvalidInput)?;
    let mut reader = crate::logfiles::open_at(path, entry.offset)?;
    loop {
        if cancel.is_cancelled() {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 || !line.ends_with('\n') {
            break;
        }
        let index = entry.state.line;
        entry.state.line += 1;
        entry.offset += bytes as u64;
        if line.trim().is_empty() || !reader_source.relevant(&line) {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(value) => reader_source.consume(
                &value,
                index,
                &mut entry.state,
                &mut entry.records,
                &mut entry.malformed,
            ),
            Err(_) => entry.malformed += 1,
        }
    }
    entry.fingerprint = if entry.compressed {
        Sha256::digest([]).to_vec()
    } else {
        fingerprint(path, entry.offset)?
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_sources_match_parser_registry() {
        let names: Vec<_> = READERS.iter().map(|reader| reader.id()).collect();
        assert_eq!(names.as_slice(), super::super::SOURCES.as_slice());
        assert_eq!(file_name("grok"), Some("updates.jsonl"));
        assert_eq!(file_name("claude"), None);
        assert!(!supported("unknown"));
    }
}
