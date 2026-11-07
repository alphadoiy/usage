//! Bounded, conservative content evidence. Display-title heuristics are never deletion evidence.
use crate::{
    background::Cancellation,
    sessions::{Session, SessionKey},
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, Read},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_LINE: u64 = 1024 * 1024;
const MAX_FILE: u64 = 32 * 1024 * 1024;
const MAX_TOTAL: u64 = 128 * 1024 * 1024;
const MAX_ENTRIES: usize = 16_384;
const MAX_TIME: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Evidence {
    Empty,
    NonEmpty,
    Unknown(String),
}

impl Evidence {
    fn merge(&mut self, other: Self) {
        // A real input proves non-emptiness even if another segment is unreadable.
        if matches!(other, Self::NonEmpty) || matches!(self, Self::Empty) {
            *self = other;
        }
    }
}

struct Scan<'a> {
    cancel: &'a Cancellation,
    started: Instant,
    bytes: u64,
    entries: usize,
}
impl Scan<'_> {
    fn check(&self) -> Result<(), String> {
        if self.cancel.is_cancelled() {
            return Err("分析已取消".into());
        }
        if self.started.elapsed() > MAX_TIME
            || self.bytes >= MAX_TOTAL
            || self.entries >= MAX_ENTRIES
        {
            return Err("分析达到读取预算，已保留未确认会话".into());
        }
        Ok(())
    }
    fn entries(&mut self, dir: &Path) -> Result<Vec<PathBuf>, String> {
        self.check()?;
        if fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("会话目录含未知符号链接".into());
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.to_string()),
        };
        let mut paths = Vec::new();
        for entry in entries {
            self.check()?;
            self.entries += 1;
            let path = entry.map_err(|e| e.to_string())?.path();
            if fs::symlink_metadata(&path)
                .map_err(|e| e.to_string())?
                .file_type()
                .is_symlink()
            {
                return Err("会话存储含未知符号链接".into());
            }
            paths.push(path);
        }
        paths.sort();
        Ok(paths)
    }
    fn row(
        &mut self,
        input: &mut dyn BufRead,
        file_bytes: &mut u64,
    ) -> Result<Option<Value>, String> {
        self.check()?;
        let mut line = Vec::new();
        let n = input
            .take(MAX_LINE + 1)
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())? as u64;
        self.bytes += n;
        *file_bytes += n;
        if n > MAX_LINE || *file_bytes > MAX_FILE {
            return Err("会话日志超过读取上限".into());
        }
        if n == 0 {
            return Ok(None);
        }
        serde_json::from_slice(&line)
            .map(Some)
            .map_err(|e| format!("日志格式不完整：{e}"))
    }
    fn log(&mut self, path: &Path, provider: &str, id: &mut Option<String>) -> Evidence {
        let read = (|| {
            let (path, _) = crate::logfiles::resolve(path).map_err(|e| e.to_string())?;
            let mut input = crate::logfiles::open(&path).map_err(|e| e.to_string())?;
            let mut bytes = 0;
            let mut seen = false;
            while let Some(row) = self.row(&mut input, &mut bytes)? {
                let kind = match provider {
                    "grok" => row.pointer("/params/update/sessionUpdate"),
                    _ => row.get("type"),
                }
                .and_then(Value::as_str)
                .ok_or("未知日志结构，无法确认内容")?;
                let recognized = match provider {
                    "codex" => matches!(
                        kind,
                        "session_meta" | "turn_context" | "event_msg" | "response_item"
                    ),
                    "claude" => matches!(
                        kind,
                        "user"
                            | "assistant"
                            | "system"
                            | "summary"
                            | "progress"
                            | "file-history-snapshot"
                            | "queue-operation"
                            | "last-prompt"
                            | "ai-title"
                    ),
                    "grok" => matches!(
                        kind,
                        "user_message_chunk"
                            | "agent_message_chunk"
                            | "agent_thought_chunk"
                            | "tool_call"
                            | "tool_call_update"
                            | "plan"
                            | "available_commands_update"
                            | "current_mode_update"
                            | "session_info_update"
                            | "usage_update"
                    ),
                    _ => false,
                };
                if !recognized {
                    return Err("未知日志类型，无法确认内容".into());
                }
                seen = true;
                if provider == "codex" && row["type"] == "session_meta" {
                    let found = row
                        .pointer("/payload/id")
                        .and_then(Value::as_str)
                        .ok_or("会话缺少身份")?;
                    if id.as_ref().is_some_and(|id| id != found) {
                        return Err("日志会话身份不一致".into());
                    }
                    *id = Some(found.into());
                }
                if id.is_some() && has_input(provider, &row) {
                    return Ok(Evidence::NonEmpty);
                }
            }
            if !seen || id.is_none() {
                return Err("缺少完整会话记录".into());
            }
            Ok(Evidence::Empty)
        })();
        read.unwrap_or_else(Evidence::Unknown)
    }
}

// Preserve arbitrary user text and multimodal/unknown content blocks. Only explicit
// agent metadata and tool-result messages are excluded; false negatives are safer.
fn content_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(parts) => parts.iter().any(content_present),
        Value::Object(_) => match value["type"].as_str() {
            Some("tool_result") => false,
            Some("text" | "input_text") => content_present(&value["text"]),
            _ => true,
        },
        _ => true,
    }
}
fn has_input(provider: &str, row: &Value) -> bool {
    match provider {
        "codex" => {
            (row["type"] == "event_msg"
                && row["payload"]["type"] == "user_message"
                && (content_present(&row["payload"]["message"])
                    || content_present(&row["payload"]["images"])))
                || (row["type"] == "response_item"
                    && row["payload"]["role"] == "user"
                    && content_present(&row["payload"]["content"]))
        }
        "claude" => {
            row["type"] == "user"
                && row["isMeta"] != true
                && row["isSidechain"] != true
                && content_present(&row["message"]["content"])
        }
        "grok" => {
            row["params"]["update"]["sessionUpdate"] == "user_message_chunk"
                && content_present(&row["params"]["update"]["content"])
        }
        _ => false,
    }
}

pub(super) fn collect(
    sessions: &[Session],
    cancel: &Cancellation,
) -> HashMap<SessionKey, Evidence> {
    let mut groups = HashMap::<(String, PathBuf), Vec<&Session>>::new();
    for session in sessions {
        let home = super::storage_home(session.provider.id(), &session.home);
        groups
            .entry((session.provider.id().into(), home))
            .or_default()
            .push(session);
    }
    let mut scan = Scan {
        cancel,
        started: Instant::now(),
        bytes: 0,
        entries: 0,
    };
    let mut result = HashMap::new();
    // Stable order makes a bounded scan repeatable.
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    for ((provider, home), sessions) in groups {
        let mut found = HashMap::<String, Evidence>::new();
        let discovery = discover(&mut scan, &provider, &home, &sessions, &mut found);
        for session in sessions {
            let mut evidence = found
                .remove(&session.id)
                .unwrap_or_else(|| Evidence::Unknown("未找到会话日志".into()));
            if let Err(error) = &discovery {
                evidence.merge(Evidence::Unknown(error.clone()));
            }
            result.insert(session.key(), evidence.clone());
            // Shared Homes can have several configured instance identities.
            found.insert(session.id.clone(), evidence);
        }
    }
    result
}

fn discover(
    scan: &mut Scan<'_>,
    provider: &str,
    home: &Path,
    sessions: &[&Session],
    found: &mut HashMap<String, Evidence>,
) -> Result<(), String> {
    let wanted: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    let mut unknown = None;
    let mut add = |id: String, evidence: Evidence| {
        found
            .entry(id)
            .and_modify(|current| current.merge(evidence.clone()))
            .or_insert(evidence);
    };
    match provider {
        "codex" => {
            let mut dirs = vec![home.join("archived_sessions"), home.join("sessions")];
            while let Some(dir) = dirs.pop() {
                for path in scan.entries(&dir)? {
                    if path.is_dir() {
                        dirs.push(path);
                        continue;
                    }
                    let Some(logical) = crate::logfiles::discovered(&path) else {
                        continue;
                    };
                    let mut id = None;
                    let evidence = scan.log(&logical, provider, &mut id);
                    match id {
                        Some(id) if wanted.contains(id.as_str()) => add(id, evidence),
                        Some(_) => {}
                        None => unknown = Some("部分日志缺少身份，无法确认空会话".to_string()),
                    }
                }
            }
        }
        "claude" => {
            for project in scan.entries(&home.join("projects"))? {
                if !project.is_dir() {
                    continue;
                }
                for path in scan.entries(&project)? {
                    let Some(logical) = crate::logfiles::discovered(&path) else {
                        continue;
                    };
                    let Some(id) = logical.file_stem().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    if wanted.contains(id) {
                        add(
                            id.into(),
                            scan.log(&logical, provider, &mut Some(id.into())),
                        );
                    }
                }
            }
        }
        "grok" => {
            for project in scan.entries(&home.join("sessions"))? {
                if !project.is_dir() {
                    continue;
                }
                for dir in scan.entries(&project)? {
                    if !dir.is_dir() {
                        continue;
                    }
                    let summary = (|| {
                        scan.check()?;
                        let path = dir.join("summary.json");
                        if !fs::symlink_metadata(&path)
                            .map_err(|e| e.to_string())?
                            .is_file()
                        {
                            return Err("会话摘要不是普通文件".into());
                        }
                        let file = fs::File::open(path).map_err(|e| e.to_string())?;
                        let mut bytes = Vec::new();
                        file.take(MAX_LINE + 1)
                            .read_to_end(&mut bytes)
                            .map_err(|e| e.to_string())?;
                        scan.bytes += bytes.len() as u64;
                        if bytes.len() as u64 > MAX_LINE {
                            return Err("摘要超过读取上限".to_string());
                        }
                        serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string())
                    })();
                    let id = summary.ok().and_then(|s| {
                        s.pointer("/info/id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    });
                    if let Some(id) = id {
                        if wanted.contains(id.as_str()) {
                            add(
                                id.clone(),
                                scan.log(&dir.join("updates.jsonl"), provider, &mut Some(id)),
                            );
                        }
                    } else {
                        unknown = Some("部分会话摘要不可读，无法确认空会话".into());
                    }
                }
            }
        }
        _ => return Err("此 Agent 暂不支持智能清理".into()),
    }
    unknown.map_or(Ok(()), Err)
}
