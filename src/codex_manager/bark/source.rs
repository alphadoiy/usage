//! Legacy `notify` also fires for ephemeral TUI jobs such as thread titles.
//! Its payload has no thread source, so verify the managed home's local metadata.
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    User,
    Internal,
    Untracked,
    Unknown,
}

impl Origin {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Internal => "internal",
            Self::Untracked => "untracked",
            Self::Unknown => "unknown",
        }
    }

    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::User => "user_thread",
            Self::Internal => "internal_thread",
            Self::Untracked => "untracked_thread",
            Self::Unknown => "thread_source_unknown",
        }
    }
}

pub(crate) fn classify(home: Option<&Path>, thread: &str) -> Origin {
    let Some(home) = home else {
        lookup("context", "home_missing");
        return Origin::Unknown;
    };
    if uuid::Uuid::parse_str(thread).is_err() {
        lookup("context", "invalid_thread_id");
        return Origin::Unknown;
    }
    match from_index(home, thread) {
        Ok(Some(origin)) if origin != Origin::Unknown => return origin,
        Ok(Some(_)) => lookup("index", "source_unknown"),
        Ok(None) => lookup("index", "thread_not_found"),
        Err(reason) => lookup("index", reason),
    }
    // Old versions and a not-yet-updated SQLite index can still have a rollout.
    // Read only the matching session's first metadata line, never its transcript.
    match from_rollout(home, thread) {
        Ok(origin) => origin,
        Err(reason) => {
            lookup("rollout", reason);
            Origin::Unknown
        }
    }
}

fn lookup(basis: &str, result: &str) {
    tracing::info!(event = "bark_source_lookup", basis, result);
}

fn metadata(basis: &str, thread_source: Option<&str>, source: &Value) -> Origin {
    let origin = origin(thread_source, source);
    tracing::info!(
        event = "bark_source_metadata",
        basis,
        thread_source,
        session_source = %super::content::excerpt(&source.to_string(), 300),
        origin = origin.label()
    );
    origin
}

fn origin(thread_source: Option<&str>, source: &Value) -> Origin {
    if source.is_object() {
        return Origin::Internal;
    }
    match thread_source {
        Some("user") => Origin::User,
        Some(_) => Origin::Internal,
        None => match source.as_str() {
            Some("cli" | "exec" | "vscode" | "mcp") => Origin::User,
            _ => Origin::Unknown,
        },
    }
}

fn from_index(home: &Path, thread: &str) -> Result<Option<Origin>, &'static str> {
    let store = crate::codex_native::SessionStore::resolve(home);
    let database = store.index().map_err(|_| "home_unreadable")?;
    let Some((version, database)) = database else {
        lookup("index", "database_missing");
        return Ok(None);
    };
    tracing::info!(event = "bark_source_index", database_version = version);
    let conn = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| "database_open_failed")?;
    conn.busy_timeout(Duration::from_millis(100))
        .map_err(|_| "database_setup_failed")?;
    let mut statement = conn
        .prepare("PRAGMA table_info(threads)")
        .map_err(|_| "schema_read_failed")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| "schema_read_failed")?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "schema_read_failed")?;
    let sql = if columns.iter().any(|name| name == "thread_source") {
        "SELECT thread_source, source FROM threads WHERE id=?1"
    } else {
        "SELECT NULL, source FROM threads WHERE id=?1"
    };
    conn.query_row(sql, [thread], |row| {
        let thread_source: Option<String> = row.get(0)?;
        let source: String = row.get(1)?;
        let source = serde_json::from_str(&source).unwrap_or(Value::String(source));
        Ok(metadata("index", thread_source.as_deref(), &source))
    })
    .optional()
    .map_err(|_| "thread_query_failed")
}

fn from_rollout(home: &Path, thread: &str) -> Result<Origin, &'static str> {
    let suffix = format!("-{thread}.jsonl");
    let mut directories = vec![
        (home.join("sessions"), 0),
        (home.join("archived_sessions"), 0),
    ];
    while let Some((directory, depth)) = directories.pop() {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("directory_unreadable"),
        };
        for entry in entries {
            let entry = entry.map_err(|_| "directory_entry_unreadable")?;
            let kind = entry.file_type().map_err(|_| "file_type_unreadable")?;
            if kind.is_dir() && depth < 3 {
                directories.push((entry.path(), depth + 1));
            } else if kind.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
            {
                const MAX_META: u64 = 8 * 1024 * 1024;
                let file = File::open(entry.path()).map_err(|_| "metadata_open_failed")?;
                let mut line = String::new();
                BufReader::new(file.take(MAX_META + 1))
                    .read_line(&mut line)
                    .map_err(|_| "metadata_read_failed")?;
                if line.len() as u64 > MAX_META {
                    return Err("metadata_too_large");
                }
                let meta: Value =
                    serde_json::from_str(&line).map_err(|_| "metadata_invalid_json")?;
                let data = &meta["payload"];
                if meta["type"] != "session_meta" || data["id"] != thread {
                    return Err("metadata_identity_mismatch");
                }
                return Ok(metadata(
                    "rollout",
                    data["thread_source"].as_str(),
                    &data["source"],
                ));
            }
        }
    }
    // Title generation is ephemeral: neither the index nor a rollout contains
    // its thread. It must not be treated as another user turn in this run.
    lookup("rollout", "thread_not_found");
    Ok(Origin::Untracked)
}
