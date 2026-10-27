//! Bounded, read-only snippets for the selected local conversation. No transcript is persisted.
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::Session;
use crate::background::Cancellation;
use serde_json::Value;

const TAIL_BYTES: u64 = 2 * 1024 * 1024;
const LINE_BYTES: u64 = 256 * 1024;
const DISCOVERY_LIMIT: usize = 4096;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Preview {
    pub request: Option<String>,
    pub response: Option<String>,
}

pub(crate) fn load(session: &Session, cancel: &Cancellation) -> Result<Preview, String> {
    if cancel.is_cancelled() {
        return Err("预览已取消".into());
    }
    if !session.provider.valid_session_id(&session.id) {
        return Err("会话 ID 无效".into());
    }
    let home = session.home.canonicalize().map_err(|_| "实例目录不可读")?;
    let mut paths = discover(session, &home, cancel)?;
    paths.sort_by_key(|p| (fs::metadata(p).and_then(|m| m.modified()).ok(), p.clone()));
    let mut preview = Preview::default();
    for path in paths
        .iter()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        if cancel.is_cancelled() {
            return Err("预览已取消".into());
        }
        let physical = path.canonicalize().map_err(|_| "会话日志不可读")?;
        if !physical.starts_with(&home) {
            continue;
        }
        let mut reader: Box<dyn BufRead + Send> = if crate::logfiles::compressed(&physical) {
            // Never label the beginning of a large archive as its latest conversation.
            let mut bytes = Vec::new();
            crate::logfiles::open(&physical)
                .map_err(|_| "压缩日志不可读")?
                .take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "压缩日志不可读")?;
            if bytes.len() > 8 * 1024 * 1024 {
                return Err("压缩日志较大，暂无法生成内容预览".into());
            }
            Box::new(std::io::Cursor::new(bytes))
        } else {
            let mut file = File::open(&physical).map_err(|_| "会话日志不可读")?;
            let offset = file
                .metadata()
                .map_err(|_| "会话日志不可读")?
                .len()
                .saturating_sub(TAIL_BYTES);
            file.seek(SeekFrom::Start(offset))
                .map_err(|_| "会话日志不可读")?;
            let mut reader = BufReader::new(file);
            if offset > 0 {
                let _ = bounded_line(&mut reader);
            }
            Box::new(reader)
        };
        let mut grok_kind = String::new();
        while !cancel.is_cancelled() {
            let Some(bytes) = bounded_line(&mut reader) else {
                break;
            };
            let Ok(row) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            match session.provider.id() {
                "codex" => {
                    if row["type"] == "event_msg" {
                        let kind = row.pointer("/payload/type").and_then(Value::as_str);
                        let text = row.pointer("/payload/message").and_then(Value::as_str);
                        if let Some(text) = text {
                            match kind {
                                Some("user_message") => set_request(&mut preview, text),
                                Some("agent_message") => preview.response = snippet(text),
                                _ => {}
                            }
                        }
                    } else if row["type"] == "response_item" && row["payload"]["type"] == "message"
                    {
                        let text = content_text(&row["payload"]["content"]);
                        match row["payload"]["role"].as_str() {
                            Some("user") => set_request(&mut preview, &text),
                            Some("assistant") => preview.response = snippet(&text),
                            _ => {}
                        }
                    }
                }
                "claude" if row["isSidechain"] != true && row["isMeta"] != true => {
                    let text = content_text(&row["message"]["content"]);
                    match row["type"].as_str() {
                        Some("user") if !has_tool_result(&row["message"]["content"]) => {
                            set_request(&mut preview, &text)
                        }
                        Some("assistant") if !text.is_empty() => preview.response = snippet(&text),
                        _ => {}
                    }
                }
                "grok" => {
                    let update = &row["params"]["update"];
                    let kind = update["sessionUpdate"].as_str().unwrap_or_default();
                    let text = update
                        .pointer("/content/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if kind == "user_message_chunk" && kind != grok_kind {
                        preview.response = None;
                    }
                    let target = match kind {
                        "user_message_chunk" => Some(&mut preview.request),
                        "agent_message_chunk" => Some(&mut preview.response),
                        _ => None,
                    };
                    if let Some(target) = target {
                        if kind != grok_kind {
                            *target = None;
                        }
                        let joined = format!("{}{text}", target.as_deref().unwrap_or_default());
                        *target = snippet(&joined);
                    }
                    if !kind.is_empty() {
                        grok_kind = kind.into();
                    }
                }
                _ => {}
            }
        }
    }
    Ok(preview)
}

fn snippet(text: &str) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text: String = text.chars().filter(|c| !c.is_control()).take(280).collect();
    (!text.is_empty()).then_some(text)
}

fn set_request(preview: &mut Preview, text: &str) {
    if super::user_prompt(text).is_some() {
        let request = snippet(text);
        preview.response = None;
        preview.request = request;
    }
}

fn has_tool_result(content: &Value) -> bool {
    content
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["type"] == "tool_result"))
}

fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.into();
    }
    content
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    matches!(
                        item["type"].as_str(),
                        Some("text" | "input_text" | "output_text")
                    )
                })
                .filter_map(|item| item["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn bounded_line(reader: &mut dyn BufRead) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let chunk = reader.fill_buf().ok()?;
        if chunk.is_empty() {
            return (!bytes.is_empty()).then_some(bytes);
        }
        let count = chunk
            .iter()
            .position(|b| *b == b'\n')
            .map_or(chunk.len(), |i| i + 1);
        let ended = chunk[count - 1] == b'\n';
        if bytes.len() + count > LINE_BYTES as usize {
            oversized = true;
            bytes.clear();
        }
        if !oversized {
            bytes.extend_from_slice(&chunk[..count]);
        }
        reader.consume(count);
        if ended {
            return Some(bytes);
        }
    }
}

fn entries(path: &Path) -> Vec<PathBuf> {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|m| !m.is_symlink()))
        .take(DISCOVERY_LIMIT)
        .map(|e| e.path())
        .collect()
}

fn codex_matches(path: &Path, id: &str) -> bool {
    let Ok((path, _)) = crate::logfiles::resolve(path) else {
        return false;
    };
    let Ok(reader) = crate::logfiles::open(&path) else {
        return false;
    };
    let mut reader = reader.take(64 * 1024);
    (0..8).any(|_| {
        bounded_line(&mut reader)
            .and_then(|line| serde_json::from_slice::<Value>(&line).ok())
            .is_some_and(|row| row["type"] == "session_meta" && row["payload"]["id"] == id)
    })
}

fn discover(session: &Session, home: &Path, cancel: &Cancellation) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    match session.provider.id() {
        "codex" => {
            if let Ok(Some(database)) = super::codex_index(home)
                && let Ok(db) = rusqlite::Connection::open_with_flags(
                    database,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
            {
                let _ = db.busy_timeout(std::time::Duration::from_millis(100));
                if let Ok(path) = db.query_row(
                    "SELECT rollout_path FROM threads WHERE id=?1",
                    [&session.id],
                    |r| r.get::<_, String>(0),
                ) {
                    let path = PathBuf::from(path);
                    let path = if path.is_absolute() {
                        path
                    } else {
                        home.join(path)
                    };
                    if path.canonicalize().is_ok_and(|p| p.starts_with(home))
                        && codex_matches(&path, &session.id)
                    {
                        let (path, _) =
                            crate::logfiles::resolve(&path).map_err(|_| "会话日志不可读")?;
                        return Ok(vec![path]);
                    }
                }
            }
            let mut dirs = vec![home.join("sessions")];
            let mut scanned = 0;
            let mut candidates = Vec::new();
            while let Some(dir) = dirs.pop() {
                for path in entries(&dir) {
                    scanned += 1;
                    if cancel.is_cancelled() || scanned > DISCOVERY_LIMIT {
                        break;
                    }
                    if path.is_dir() {
                        dirs.push(path);
                    } else if crate::logfiles::discovered(&path).is_some() {
                        candidates.push(path);
                    }
                }
                if cancel.is_cancelled() || scanned > DISCOVERY_LIMIT {
                    break;
                }
            }
            // Native rollouts carry the UUID in their name. Verify those first so ordinary
            // selection does not open every conversation in a large Home.
            for named_only in [true, false] {
                for path in &candidates {
                    if cancel.is_cancelled() {
                        return Err("预览已取消".into());
                    }
                    let named = path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(&session.id));
                    if named != named_only {
                        continue;
                    }
                    if codex_matches(path, &session.id) {
                        found.push(
                            crate::logfiles::resolve(path)
                                .map_err(|_| "会话日志不可读")?
                                .0,
                        );
                    }
                }
                if !found.is_empty() {
                    break;
                }
            }
        }
        "claude" => {
            for project in entries(&home.join("projects")) {
                if cancel.is_cancelled() {
                    break;
                }
                let path = project.join(format!("{}.jsonl", session.id));
                if path.is_file() {
                    found.push(path);
                }
            }
        }
        "grok" => {
            for project in entries(&home.join("sessions")) {
                for dir in entries(&project) {
                    if cancel.is_cancelled() {
                        return Ok(found);
                    }
                    let summary = File::open(dir.join("summary.json"))
                        .ok()
                        .and_then(|f| serde_json::from_reader::<_, Value>(f.take(LINE_BYTES)).ok());
                    if summary
                        .as_ref()
                        .is_some_and(|s| s["info"]["id"] == session.id)
                    {
                        let path = dir.join("updates.jsonl");
                        if path.is_file() {
                            found.push(path);
                        }
                    }
                }
            }
        }
        _ => return Err("此 Agent 暂不支持内容预览".into()),
    }
    Ok(found)
}
