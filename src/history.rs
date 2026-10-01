//! Local token history. Quota percentages cannot reconstruct daily consumption.
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::config::{AppConfig, home_dir};

pub const SOURCES: [&str; 3] = ["codex", "claude", "grok"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    /// Included in input, never added to total a second time.
    pub cached: u64,
    pub calls: u64,
}
impl Tokens {
    pub fn total(self) -> u64 {
        self.input.saturating_add(self.output)
    }
    pub fn add(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cached = self.cached.saturating_add(other.cached);
        self.calls = self.calls.saturating_add(other.calls);
    }
    fn merge(&mut self, other: Self) {
        self.input = self.input.max(other.input);
        self.output = self.output.max(other.output);
        self.cached = self.cached.max(other.cached);
        self.calls = self.calls.max(other.calls);
    }
}

#[derive(Clone)]
struct Record {
    source: String,
    id: String,
    time: DateTime<Utc>,
    tokens: Tokens,
}
#[derive(Clone, Debug, Default)]
pub struct History {
    pub days: BTreeMap<NaiveDate, BTreeMap<String, Tokens>>,
    pub warnings: Vec<String>,
}
#[derive(Serialize)]
pub struct HistoryReport {
    pub year: i32,
    pub providers: Vec<String>,
    pub timezone: String,
    pub totals: Tokens,
    pub daily: BTreeMap<String, Tokens>,
    pub monthly: BTreeMap<String, Tokens>,
    pub warnings: Vec<String>,
}
impl History {
    pub fn day(&self, date: NaiveDate, sources: &[String]) -> Tokens {
        let mut total = Tokens::default();
        if let Some(day) = self.days.get(&date) {
            for source in sources {
                if let Some(tokens) = day.get(source) {
                    total.add(*tokens);
                }
            }
        }
        total
    }
    pub fn report(&self, year: i32, sources: &[String]) -> HistoryReport {
        let mut report = HistoryReport {
            year,
            providers: sources.to_vec(),
            timezone: Local::now().format("%Z %:z").to_string(),
            totals: Tokens::default(),
            daily: BTreeMap::new(),
            monthly: BTreeMap::new(),
            warnings: self.warnings.clone(),
        };
        let Some(mut date) = NaiveDate::from_ymd_opt(year, 1, 1) else {
            return report;
        };
        while date.year() == year {
            let tokens = self.day(date, sources);
            report.totals.add(tokens);
            report.daily.insert(date.to_string(), tokens);
            report
                .monthly
                .entry(date.format("%Y-%m").to_string())
                .or_default()
                .add(tokens);
            let Some(next) = date.succ_opt() else {
                break;
            };
            date = next;
        }
        report
    }
}

struct CachedFile {
    len: u64,
    modified: Option<SystemTime>,
    records: Vec<Record>,
    malformed: usize,
}
#[derive(Default)]
pub struct HistoryCache {
    files: HashMap<PathBuf, CachedFile>,
}
impl HistoryCache {
    pub fn load(&mut self, config: &AppConfig) -> History {
        let home = home_dir();
        let mut history = History::default();
        let mut paths = Vec::new();
        for source in &config.providers {
            let (root, name) = match source.as_str() {
                "codex" => (
                    config
                        .codex_dir
                        .clone()
                        .unwrap_or_else(|| home.join(".codex")),
                    None,
                ),
                "claude" => (
                    config
                        .claude_dir
                        .clone()
                        .unwrap_or_else(|| home.join(".claude")),
                    None,
                ),
                "grok" => (
                    config
                        .grok_dir
                        .clone()
                        .unwrap_or_else(|| home.join(".grok")),
                    Some("updates.jsonl"),
                ),
                _ => {
                    history
                        .warnings
                        .push(format!("{source}: 没有历史日志读取器"));
                    continue;
                }
            };
            let roots = if source == "codex" {
                vec![root.join("sessions"), root.join("archived_sessions")]
            } else {
                vec![root.join(if source == "claude" {
                    "projects"
                } else {
                    "sessions"
                })]
            };
            let before = paths.len();
            for root in roots {
                collect_files(&root, source, name, &mut paths, &mut history.warnings);
            }
            if before == paths.len() {
                history
                    .warnings
                    .push(format!("{source}: 未找到本地历史日志"));
            }
        }
        let mut active = std::collections::HashSet::new();
        let mut unique: HashMap<(String, String), Record> = HashMap::new();
        let mut malformed = 0;
        for (path, source) in paths {
            active.insert(path.clone());
            let metadata = match fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => {
                    history.warnings.push(format!("{source}: 无法读取日志文件"));
                    continue;
                }
            };
            let unchanged = self.files.get(&path).is_some_and(|entry| {
                entry.len == metadata.len() && entry.modified == metadata.modified().ok()
            });
            if !unchanged {
                match parse_file(&path, &source) {
                    Ok((records, malformed)) => {
                        self.files.insert(
                            path.clone(),
                            CachedFile {
                                len: metadata.len(),
                                modified: metadata.modified().ok(),
                                records,
                                malformed,
                            },
                        );
                    }
                    Err(_) => {
                        self.files.remove(&path);
                        history.warnings.push(format!("{source}: 无法打开日志文件"));
                        continue;
                    }
                }
            }
            if let Some(entry) = self.files.get(&path) {
                malformed += entry.malformed;
                for record in &entry.records {
                    let key = (source.clone(), record.id.clone());
                    unique
                        .entry(key)
                        .and_modify(|r| {
                            r.tokens.merge(record.tokens);
                            r.time = r.time.min(record.time);
                        })
                        .or_insert_with(|| record.clone());
                }
            }
        }
        self.files.retain(|path, _| active.contains(path));
        if malformed > 0 {
            history
                .warnings
                .push(format!("跳过 {malformed} 条损坏或无有效时间的用量记录"));
        }
        for record in unique.into_values() {
            if record.tokens.total() == 0 {
                continue;
            }
            history
                .days
                .entry(record.time.with_timezone(&Local).date_naive())
                .or_default()
                .entry(record.source)
                .or_default()
                .add(record.tokens);
        }
        history
    }
}
fn collect_files(
    root: &Path,
    source: &str,
    name: Option<&str>,
    out: &mut Vec<(PathBuf, String)>,
    warnings: &mut Vec<String>,
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
        let Ok(entry) = entry else {
            warnings.push(format!("{source}: 无法读取目录项"));
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_files(&entry.path(), source, name, out, warnings);
        } else if kind.is_file()
            && entry.path().extension().is_some_and(|ext| ext == "jsonl")
            && name.is_none_or(|n| entry.file_name() == n)
        {
            out.push((entry.path(), source.into()));
        }
    }
}
fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}
fn time(value: &Value) -> Option<DateTime<Utc>> {
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
fn parse_file(path: &Path, source: &str) -> std::io::Result<(Vec<Record>, usize)> {
    let mut records = Vec::new();
    let mut malformed = 0;
    let mut high = Tokens::default();
    let mut signatures: HashMap<String, String> = HashMap::new();
    let mut previous = None;
    let mut session = path.to_string_lossy().into_owned();
    let mut fork_time = None;
    for (index, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // Avoid decoding large conversation/tool payloads unrelated to usage.
        if source == "codex"
            && !line.contains("\"token_count\"")
            && !line.contains("\"session_meta\"")
        {
            continue;
        }
        if source == "claude" && !line.contains("\"usage\"") {
            continue;
        }
        if source == "grok" && !line.contains("\"turn_completed\"") {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                malformed += 1;
                continue;
            }
        };
        if source == "codex" && v["type"] == "session_meta" {
            if let Some(id) = v["payload"]["id"].as_str() {
                session = id.into();
            }
            if v["payload"]
                .get("forked_from_id")
                .is_some_and(|p| !p.is_null())
                || v.pointer("/payload/thread_spawn/parent_thread_id")
                    .is_some()
            {
                fork_time = v.get("timestamp").and_then(time);
                if fork_time.is_none() {
                    malformed += 1;
                    return Ok((Vec::new(), malformed));
                }
            }
            continue;
        }
        let parsed = match source {
            "codex" if v["type"] == "event_msg" && v["payload"]["type"] == "token_count" => {
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
                    continue;
                }
                let signature = serde_json::to_string(&(
                    info.get("total_token_usage"),
                    info.get("last_token_usage"),
                ))
                .unwrap_or_default();
                let key = v
                    .pointer("/payload/rate_limits/limit_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let duplicate = total.is_some()
                    && (signatures.get(&key) == Some(&signature)
                        || previous.as_ref() == Some(&signature));
                if total.is_some() {
                    signatures.insert(key, signature.clone());
                }
                previous = Some(signature);
                let mut tokens = last.unwrap_or_else(|| {
                    let t = total.unwrap_or_default();
                    Tokens {
                        input: t.input.saturating_sub(high.input),
                        output: t.output.saturating_sub(high.output),
                        cached: t.cached.saturating_sub(high.cached),
                        calls: 1,
                    }
                });
                tokens.cached = tokens.cached.min(tokens.input);
                if let Some(t) = total {
                    high.merge(t);
                }
                if duplicate {
                    continue;
                }
                let timestamp = v.get("timestamp").and_then(time);
                if fork_time.is_some_and(|cutoff| timestamp.is_some_and(|t| t < cutoff)) {
                    continue;
                }
                // Same rollout copied into sessions/ and archived_sessions/ counts once.
                Some((format!("{session}:{index}"), timestamp, tokens))
            }
            "claude" if v["type"] == "assistant" && v.pointer("/message/usage").is_some() => {
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
                    .unwrap_or_else(|| format!("{session}:{index}"));
                Some((
                    id,
                    v.get("timestamp").and_then(time),
                    Tokens {
                        input,
                        output: number(u, "output_tokens"),
                        cached,
                        calls: 1,
                    },
                ))
            }
            "grok"
                if v.pointer("/params/update/sessionUpdate")
                    .and_then(Value::as_str)
                    == Some("turn_completed") =>
            {
                let update = &v["params"]["update"];
                let u = &update["usage"];
                if !u.is_object() {
                    continue;
                }
                // turn_completed usage is the aggregate for this prompt, not the session.
                let id = update["prompt_id"]
                    .as_str()
                    .or_else(|| v.pointer("/params/_meta/promptId").and_then(Value::as_str))
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{session}:{index}"));
                let input = number(u, "inputTokens");
                Some((
                    id,
                    v.get("timestamp").and_then(time),
                    Tokens {
                        input,
                        output: number(u, "outputTokens"),
                        cached: number(u, "cachedReadTokens").min(input),
                        calls: number(u, "modelCalls").max(1),
                    },
                ))
            }
            _ => None,
        };
        if let Some((id, timestamp, tokens)) = parsed {
            if let Some(time) = timestamp {
                records.push(Record {
                    source: source.into(),
                    id,
                    time,
                    tokens,
                });
            } else {
                malformed += 1;
            }
        }
    }
    Ok((records, malformed))
}

pub fn compact(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("usage-history-{}", uuid::Uuid::new_v4())))
        }
        fn write(&self, path: &str, lines: &[Value]) -> PathBuf {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                lines
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n",
            )
            .unwrap();
            path
        }
        fn config(&self, sources: &[&str]) -> AppConfig {
            AppConfig {
                providers: sources.iter().map(|s| s.to_string()).collect(),
                codex_dir: Some(self.0.join("codex")),
                claude_dir: Some(self.0.join("claude")),
                grok_dir: Some(self.0.join("grok")),
                ..AppConfig::default()
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn meta(id: &str) -> Value {
        json!({"type":"session_meta","timestamp":"2026-09-01T00:00:00Z","payload":{"id":id}})
    }
    fn codex(
        input: u64,
        cached: u64,
        output: u64,
        last: Option<(u64, u64, u64)>,
        source: &str,
        timestamp: &str,
    ) -> Value {
        let mut v = json!({"type":"event_msg","timestamp":timestamp,"payload":{"type":"token_count","rate_limits":{"limit_id":source},"info":{"total_token_usage":{"input_tokens":input,"cached_input_tokens":cached,"output_tokens":output}}}});
        if let Some((i, c, o)) = last {
            v["payload"]["info"]["last_token_usage"] =
                json!({"input_tokens":i,"cached_input_tokens":c,"output_tokens":o});
        }
        v
    }
    #[test]
    fn codex_last_usage_dedupes_per_limit_and_archive_copies() {
        let f = Fixture::new();
        let first = codex(
            1000,
            800,
            100,
            Some((100, 80, 10)),
            "codex",
            "2026-09-01T12:00:00Z",
        );
        let mut duplicate = first.clone();
        duplicate["payload"]["rate_limits"]["limit_id"] = json!("other");
        let second = codex(
            2000,
            1600,
            200,
            Some((300, 200, 30)),
            "other",
            "2026-09-01T12:01:00Z",
        );
        let lines = [meta("session"), first, duplicate, second];
        f.write("codex/sessions/a.jsonl", &lines);
        f.write("codex/archived_sessions/a.jsonl", &lines);
        let history = HistoryCache::default().load(&f.config(&["codex"]));
        let report = history.report(2026, &["codex".into()]);
        assert_eq!(
            report.totals,
            Tokens {
                input: 400,
                cached: 280,
                output: 40,
                calls: 2
            }
        );
        assert_eq!(report.monthly["2026-09"], report.totals);
    }
    #[test]
    fn cumulative_delta_uses_high_water_and_skips_fork_prefix() {
        let f = Fixture::new();
        let t = "2026-09-01T12:00:00Z";
        let a = codex(100, 80, 10, None, "codex", t);
        let b = codex(200, 160, 20, None, "codex", "2026-09-01T12:01:00Z");
        f.write(
            "codex/sessions/parent.jsonl",
            &[meta("parent"), a.clone(), a.clone(), b.clone()],
        );
        let mut fork = meta("child");
        fork["payload"]["forked_from_id"] = json!("parent");
        fork["timestamp"] = json!("2026-09-01T12:02:00Z");
        let c = codex(250, 190, 25, None, "codex", "2026-09-01T12:03:00Z");
        f.write("codex/sessions/child.jsonl", &[fork, a, b, c]);
        let report = HistoryCache::default()
            .load(&f.config(&["codex"]))
            .report(2026, &["codex".into()]);
        assert_eq!(
            report.totals,
            Tokens {
                input: 250,
                cached: 190,
                output: 25,
                calls: 3
            }
        );
    }
    #[test]
    fn claude_stream_fragments_and_grok_prompt_totals_count_once() {
        let f = Fixture::new();
        let a = json!({"type":"assistant","timestamp":"2026-09-01T12:00:00Z","message":{"id":"msg","usage":{"input_tokens":100,"cache_read_input_tokens":80,"cache_creation_input_tokens":20,"output_tokens":10}}});
        let mut b = a.clone();
        b["message"]["usage"]["output_tokens"] = json!(30);
        f.write("claude/projects/project/session.jsonl", &[a.clone(), b]);
        f.write("claude/projects/project/subagents/copy.jsonl", &[a]);
        let g = json!({"timestamp":1788264000,"params":{"update":{"sessionUpdate":"turn_completed","prompt_id":"prompt","usage":{"inputTokens":200,"cachedReadTokens":80,"outputTokens":40,"reasoningTokens":10,"modelCalls":2}}}});
        let mut interim = g.clone();
        interim["params"]["update"]["sessionUpdate"] = json!("agent_message_chunk");
        f.write(
            "grok/sessions/project/session/updates.jsonl",
            &[interim, g.clone(), g],
        );
        let history = HistoryCache::default().load(&f.config(&["claude", "grok"]));
        assert_eq!(
            history.report(2026, &["claude".into()]).totals,
            Tokens {
                input: 200,
                cached: 80,
                output: 30,
                calls: 1
            }
        );
        assert_eq!(
            history.report(2026, &["grok".into()]).totals,
            Tokens {
                input: 200,
                cached: 80,
                output: 40,
                calls: 2
            }
        );
        assert_eq!(
            history
                .report(2026, &["claude".into(), "grok".into()])
                .totals
                .total(),
            470
        );
    }
    #[test]
    fn refresh_replaces_changed_files_and_removes_deleted_files() {
        let f = Fixture::new();
        let config = f.config(&["codex"]);
        let mut cache = HistoryCache::default();
        let a = codex(100, 80, 10, None, "codex", "2026-09-01T12:00:00Z");
        let path = f.write("codex/sessions/a.jsonl", &[meta("a"), a.clone()]);
        let total = |h: History| h.report(2026, &["codex".into()]).totals.total();
        assert_eq!(total(cache.load(&config)), 110);
        assert_eq!(total(cache.load(&config)), 110);
        f.write(
            "codex/sessions/a.jsonl",
            &[
                meta("a"),
                a,
                codex(200, 160, 20, None, "codex", "2026-09-01T12:01:00Z"),
            ],
        );
        assert_eq!(total(cache.load(&config)), 220);
        fs::remove_file(path).unwrap();
        assert_eq!(total(cache.load(&config)), 0);
    }
    #[test]
    fn local_dates_leap_year_zero_days_and_bad_lines() {
        let f = Fixture::new();
        let a = codex(100, 80, 10, None, "codex", "2024-02-29T23:30:00Z");
        let path = f.write("codex/sessions/a.jsonl", &[meta("a"), a]);
        use std::io::Write;
        writeln!(
            fs::OpenOptions::new().append(true).open(path).unwrap(),
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\""
        )
        .unwrap();
        let history = HistoryCache::default().load(&f.config(&["codex"]));
        let expected = time(&json!("2024-02-29T23:30:00Z"))
            .unwrap()
            .with_timezone(&Local)
            .date_naive();
        assert_eq!(history.day(expected, &["codex".into()]).total(), 110);
        let report = history.report(2024, &["codex".into()]);
        assert_eq!(report.daily.len(), 366);
        assert_eq!(report.monthly.len(), 12);
        assert_eq!(report.daily.values().map(|t| t.total()).sum::<u64>(), 110);
        assert_eq!(report.monthly.values().map(|t| t.total()).sum::<u64>(), 110);
        assert_eq!(report.warnings.len(), 1);
    }
}
