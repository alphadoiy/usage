//! Local session catalog and Ghostty launcher.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde_json::Value;

const CODEX_QUERY: &str = "SELECT id, cwd, COALESCE(NULLIF(name,''), NULLIF(title,''), NULLIF(preview,''), id) AS title, recency_at_ms FROM threads WHERE archived=0 AND cwd<>'' AND COALESCE(thread_source,'user')='user' AND source<>'exec' ORDER BY recency_at_ms DESC;";
const CODEX_METADATA_SCAN_LINES: usize = 8;
const CODEX_TITLE_SCAN_LINES: usize = 256;
const GROK_TITLE_SCAN_LINES: usize = 256;
const TITLE_MAX_CHARS: usize = 120;
pub use crate::agents::AgentKind;

/// A session ID is unique only within its provider and configured instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub instance: String,
    pub provider: String,
    pub id: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    /// Adapter selected when discovering this configured instance. None for legacy source-only records.
    pub agent: Option<Arc<dyn crate::agents::AgentAdapter>>,
    pub provider: AgentKind,
    pub instance: String,
    pub id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub recency_at_ms: i64,
    pub home: PathBuf,
}

impl Session {
    pub fn key(&self) -> SessionKey {
        SessionKey {
            instance: self.instance.clone(),
            provider: self.provider.id().into(),
            id: self.id.clone(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionHomes {
    primary: HashMap<String, PathBuf>,
    extra: Vec<(String, String, PathBuf)>,
}

impl SessionHomes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, provider_id: &str, home: impl Into<PathBuf>) -> Self {
        self.primary.insert(provider_id.to_string(), home.into());
        self
    }

    pub fn with_instance(mut self, id: &str, provider: &str, home: impl Into<PathBuf>) -> Self {
        self.extra.retain(|(existing, _, _)| existing != id);
        self.extra.push((id.into(), provider.into(), home.into()));
        self
    }
    fn all(&self, provider: &str) -> Vec<(&str, &Path)> {
        let mut homes = Vec::new();
        if let Some((id, path)) = self.primary.get_key_value(provider) {
            homes.push((id.as_str(), path.as_path()));
        }
        homes.extend(
            self.extra
                .iter()
                .filter(|(_, p, _)| p == provider)
                .map(|(id, _, path)| (id.as_str(), path.as_path())),
        );
        homes
    }
    pub fn get(&self, provider_id: &str) -> Option<&Path> {
        self.primary.get(provider_id).map(PathBuf::as_path)
    }
}

#[derive(Default)]
pub struct SessionCatalog {
    pub sessions: Vec<Session>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub recency_at_ms: i64,
}

/// A loader produces provider-independent records from one configured home.
#[derive(Clone, Copy)]
pub struct SessionSource {
    pub kind: AgentKind,
    pub load: fn(&Path) -> Result<Vec<SessionRecord>, String>,
}

/// Registry of local session sources.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    sources: Vec<SessionSource>,
    agents: Option<Arc<crate::agents::AgentRegistry>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, source: SessionSource) -> Result<(), String> {
        if source.kind.id().trim().is_empty() {
            return Err("会话来源 ID 不能为空".into());
        }
        if self
            .agents
            .as_ref()
            .is_some_and(|agents| agents.contains(source.kind.id()))
            || self
                .sources
                .iter()
                .any(|existing| existing.kind.id() == source.kind.id())
        {
            return Err(format!("会话来源已注册：{}", source.kind.id()));
        }
        self.sources.push(source);
        Ok(())
    }

    pub fn load_all(&self, homes: &SessionHomes) -> SessionCatalog {
        let mut catalog = SessionCatalog::default();
        for source in &self.sources {
            for (instance, home) in homes.all(source.kind.id()) {
                load_source(&mut catalog, *source, instance, home, None);
            }
        }
        if let Some(agents) = &self.agents {
            for id in agents.ids() {
                for (instance, home) in homes.all(&id) {
                    if !home.is_dir() {
                        continue;
                    }
                    match agents.create(
                        &id,
                        crate::agents::AgentContext::Local {
                            home: home.to_path_buf(),
                        },
                    ) {
                        Ok(agent) if agent.kind().id() != id => catalog
                            .warnings
                            .push(format!("{id} / {instance}：Agent 工厂返回的类型不匹配")),
                        Ok(agent) => {
                            if let Some(source) = agent.session_source() {
                                load_source(
                                    &mut catalog,
                                    source,
                                    instance,
                                    home,
                                    Some(agent.clone()),
                                );
                            }
                        }
                        Err(error) => catalog.warnings.push(format!("{id} / {instance}：{error}")),
                    }
                }
            }
        }
        catalog.sessions.sort_by(|a, b| {
            b.recency_at_ms
                .cmp(&a.recency_at_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        catalog
    }
}

impl SessionRegistry {
    pub fn from_agents(agents: Arc<crate::agents::AgentRegistry>) -> Self {
        Self {
            sources: Vec::new(),
            agents: Some(agents),
        }
    }
}

fn load_source(
    catalog: &mut SessionCatalog,
    source: SessionSource,
    instance: &str,
    home: &Path,
    agent: Option<Arc<dyn crate::agents::AgentAdapter>>,
) {
    if !home.is_dir() {
        return;
    }
    if agent
        .as_ref()
        .is_some_and(|agent| agent.kind() != source.kind)
    {
        catalog.warnings.push(format!(
            "{} / {instance}：会话来源与 Agent 不匹配",
            source.kind.id()
        ));
        return;
    }
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    match (source.load)(&home) {
        Ok(records) => catalog.sessions.extend(
            records
                .into_iter()
                .filter(|record| {
                    source.kind.valid_session_id(&record.id)
                        && record.cwd.is_absolute()
                        && record.cwd.is_dir()
                })
                .map(|record| Session {
                    agent: agent.clone(),
                    provider: source.kind,
                    instance: instance.into(),
                    id: record.id,
                    cwd: record.cwd,
                    title: record.title,
                    recency_at_ms: record.recency_at_ms,
                    home: home.clone(),
                }),
        ),
        Err(error) => catalog
            .warnings
            .push(format!("{} / {instance}：{error}", source.kind.name())),
    }
}

pub fn builtin_registry() -> SessionRegistry {
    SessionRegistry::from_agents(Arc::new(crate::agents::builtin_registry()))
}

pub fn load_all(homes: &SessionHomes) -> SessionCatalog {
    builtin_registry().load_all(homes)
}

#[derive(Deserialize)]
struct CodexRow {
    id: String,
    cwd: PathBuf,
    title: String,
    recency_at_ms: i64,
}

pub(crate) fn load_codex(home: &Path) -> Result<Vec<SessionRecord>, String> {
    match load_codex_index(home) {
        Ok(sessions) => Ok(sessions),
        Err(index_error) => match load_codex_rollouts(home) {
            Ok(sessions) if sessions.iter().any(is_local_record) => Ok(sessions),
            _ => Err(index_error),
        },
    }
}

fn is_local_record(session: &SessionRecord) -> bool {
    uuid::Uuid::parse_str(&session.id).is_ok() && session.cwd.is_absolute() && session.cwd.is_dir()
}

fn display_title(raw: &str) -> Option<String> {
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = one_line.trim();
    if title.is_empty() {
        return None;
    }
    let mut chars = title.chars();
    let short: String = chars.by_ref().take(TITLE_MAX_CHARS).collect();
    Some(if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    })
}

fn tagged_text<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    text.split_once(&start)?
        .1
        .split_once(&end)
        .map(|(value, _)| value.trim())
}

fn user_prompt(text: &str) -> Option<String> {
    let text = text.trim();
    if let Some(command) = tagged_text(text, "command-name") {
        let args = tagged_text(text, "command-args").unwrap_or("");
        return display_title(&format!("{command} {args}"));
    }
    if text.starts_with('<')
        || text.starts_with("[Request interrupted")
        || text.starts_with("Caveat:")
        || text.starts_with("# AGENTS.md")
    {
        return None;
    }
    display_title(text)
}

fn load_codex_index(home: &Path) -> Result<Vec<SessionRecord>, String> {
    let database = fs::read_dir(home)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let version = name
                .to_str()?
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse::<u32>()
                .ok()?;
            Some((version, entry.path()))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, path)| path)
        .ok_or_else(|| "未找到 Codex 会话索引".to_string())?;
    let output = Command::new("sqlite3")
        .args(["-readonly", "-json"])
        .arg(database)
        .arg(CODEX_QUERY)
        .output()
        .map_err(|e| format!("无法读取索引（需要 sqlite3）：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "读取索引失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let rows: Vec<CodexRow> =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("索引格式不兼容：{e}"))?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let title = (r.title != r.id)
                .then(|| user_prompt(&r.title))
                .flatten()
                .unwrap_or_else(|| "未命名会话".into());
            SessionRecord {
                id: r.id,
                cwd: r.cwd,
                title,
                recency_at_ms: r.recency_at_ms,
            }
        })
        .collect())
}

fn load_codex_rollouts(home: &Path) -> Result<Vec<SessionRecord>, String> {
    let root = home.join("sessions");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut titles = HashMap::new();
    if let Ok(file) = File::open(home.join("session_index.jsonl")) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(row) = serde_json::from_str::<Value>(&line)
                && let (Some(id), Some(title)) = (
                    row.get("id").and_then(Value::as_str),
                    row.get("thread_name").and_then(Value::as_str),
                )
                && let Some(title) = user_prompt(title)
            {
                titles.insert(id.to_string(), title);
            }
        }
    }
    let mut directories = vec![root];
    let archived = archived_codex_ids(home);
    let mut sessions = HashMap::<String, SessionRecord>::new();
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if path.is_dir() {
                directories.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Some(session) = codex_rollout_metadata(&path, &titles) else {
                continue;
            };
            if archived.contains(&session.id) {
                continue;
            }
            let key = session.id.clone();
            if sessions
                .get(&key)
                .is_none_or(|old| session.recency_at_ms > old.recency_at_ms)
            {
                sessions.insert(key, session);
            }
        }
    }
    Ok(sessions.into_values().collect())
}

fn archived_codex_ids(home: &Path) -> HashSet<String> {
    let Ok(entries) = fs::read_dir(home.join("archived_sessions")) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().is_some_and(|ext| ext == "jsonl"))
                .then_some(path)
                .and_then(|path| File::open(path).ok())
                .and_then(|file| {
                    let mut lines = BufReader::new(file).lines().map_while(Result::ok);
                    codex_metadata(&mut lines)
                })
                .and_then(|meta| meta.get("id").and_then(Value::as_str).map(str::to_string))
        })
        .collect()
}

fn codex_metadata(lines: &mut impl Iterator<Item = String>) -> Option<Value> {
    lines.take(CODEX_METADATA_SCAN_LINES).find_map(|line| {
        let row = serde_json::from_str::<Value>(&line).ok()?;
        (row.get("type").and_then(Value::as_str) == Some("session_meta"))
            .then(|| row.get("payload").cloned())
            .flatten()
    })
}

fn codex_rollout_metadata(path: &Path, titles: &HashMap<String, String>) -> Option<SessionRecord> {
    let file = File::open(path).ok()?;
    let mut lines = BufReader::new(file).lines().map_while(Result::ok);
    let metadata = codex_metadata(&mut lines)?;
    if metadata
        .get("thread_source")
        .is_some_and(|source| source.as_str() != Some("user"))
        || metadata.get("source").and_then(Value::as_str) == Some("exec")
    {
        return None;
    }
    let id = metadata.get("id").and_then(Value::as_str)?;
    let cwd = metadata.get("cwd").and_then(Value::as_str)?;
    let title = titles.get(id).cloned().unwrap_or_else(|| {
        lines
            .take(CODEX_TITLE_SCAN_LINES)
            .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
            .find_map(|row| {
                if row.get("type").and_then(Value::as_str) == Some("event_msg")
                    && row.pointer("/payload/type").and_then(Value::as_str) == Some("user_message")
                {
                    return row
                        .pointer("/payload/message")
                        .and_then(Value::as_str)
                        .and_then(user_prompt);
                }
                if row.get("type").and_then(Value::as_str) != Some("response_item")
                    || row.pointer("/payload/role").and_then(Value::as_str) != Some("user")
                {
                    return None;
                }
                row.pointer("/payload/content")
                    .and_then(Value::as_array)?
                    .iter()
                    .filter(|part| {
                        matches!(
                            part.get("type").and_then(Value::as_str),
                            Some("input_text" | "text")
                        )
                    })
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .find_map(user_prompt)
            })
            .unwrap_or_else(|| "未命名会话".into())
    });
    Some(SessionRecord {
        id: id.to_string(),
        cwd: PathBuf::from(cwd),
        title,
        recency_at_ms: modified_ms(path),
    })
}

pub(crate) fn load_claude(home: &Path) -> Result<Vec<SessionRecord>, String> {
    let root = home.join("projects");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for project in fs::read_dir(root).map_err(|e| e.to_string())? {
        let project = project.map_err(|e| e.to_string())?;
        if !project.path().is_dir() {
            continue;
        }
        for file in fs::read_dir(project.path()).map_err(|e| e.to_string())? {
            let path = file.map_err(|e| e.to_string())?.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if uuid::Uuid::parse_str(id).is_err() {
                continue;
            }
            if let Some((cwd, title)) = claude_metadata(&path) {
                sessions.push(SessionRecord {
                    id: id.into(),
                    cwd,
                    title,
                    recency_at_ms: modified_ms(&path),
                });
            }
        }
    }
    Ok(sessions)
}

fn claude_metadata(path: &Path) -> Option<(PathBuf, String)> {
    let mut cwd = None;
    let mut named = None;
    for line in BufReader::new(File::open(path).ok()?)
        .lines()
        .map_while(Result::ok)
    {
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if cwd.is_none() {
            cwd = row.get("cwd").and_then(Value::as_str).map(PathBuf::from);
        }
        if row.get("type").and_then(Value::as_str) == Some("ai-title") {
            named = row
                .get("aiTitle")
                .and_then(Value::as_str)
                .and_then(display_title);
        } else if row.get("type").and_then(Value::as_str) == Some("summary") && named.is_none() {
            named = row
                .get("summary")
                .and_then(Value::as_str)
                .and_then(display_title);
        }
        if row.get("type").and_then(Value::as_str) != Some("user")
            || row.get("isSidechain").and_then(Value::as_bool) == Some(true)
            || row.get("isMeta").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let Some(content) = row.pointer("/message/content") else {
            continue;
        };
        let text = match content {
            Value::String(text) => text.clone(),
            Value::Array(items) => {
                if items
                    .iter()
                    .any(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
                {
                    continue;
                }
                items
                    .iter()
                    .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            _ => continue,
        };
        if let Some(title) = user_prompt(&text) {
            return Some((cwd?, title));
        }
    }
    Some((cwd?, named?))
}

pub(crate) fn load_grok(home: &Path) -> Result<Vec<SessionRecord>, String> {
    let root = home.join("sessions");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for project in fs::read_dir(root).map_err(|e| e.to_string())? {
        let project = project.map_err(|e| e.to_string())?;
        if !project.path().is_dir() {
            continue;
        }
        let subagent_ids = grok_subagent_ids(&project.path());
        for entry in fs::read_dir(project.path()).map_err(|e| e.to_string())? {
            let session_dir = entry.map_err(|e| e.to_string())?.path();
            let summary_path = session_dir.join("summary.json");
            let Ok(contents) = fs::read(&summary_path) else {
                continue;
            };
            let Ok(summary) = serde_json::from_slice::<Value>(&contents) else {
                continue;
            };
            let Some(id) = summary.pointer("/info/id").and_then(Value::as_str) else {
                continue;
            };
            if subagent_ids.contains(id)
                || summary
                    .get("session_kind")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind.starts_with("subagent"))
            {
                continue;
            }
            let Some(cwd) = summary.pointer("/info/cwd").and_then(Value::as_str) else {
                continue;
            };
            let forked = summary
                .get("parent_session_id")
                .and_then(Value::as_str)
                .is_some_and(|parent| !parent.is_empty());
            let fork_created_at = summary
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(crate::timeutil::parse_time)
                .map(|created| created.timestamp());
            let prompt_title = if forked && fork_created_at.is_none() {
                None
            } else {
                grok_prompt_title(
                    &session_dir.join("updates.jsonl"),
                    if forked { fork_created_at } else { None },
                )
            };
            let title = prompt_title
                .or_else(|| {
                    ["generated_title", "session_summary"]
                        .iter()
                        .filter_map(|key| summary.get(*key).and_then(Value::as_str))
                        .find_map(display_title)
                })
                .unwrap_or_else(|| "未命名会话".into());
            let recency_at_ms = ["last_active_at", "updated_at"]
                .iter()
                .filter_map(|key| summary.get(*key).and_then(Value::as_str))
                .find_map(crate::timeutil::parse_time)
                .map_or_else(
                    || modified_ms(&summary_path),
                    |time| time.timestamp_millis(),
                );
            sessions.push(SessionRecord {
                id: id.into(),
                cwd: PathBuf::from(cwd),
                title,
                recency_at_ms,
            });
        }
    }
    Ok(sessions)
}

fn grok_subagent_ids(project_dir: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let Ok(sessions) = fs::read_dir(project_dir) else {
        return ids;
    };
    for session in sessions.flatten() {
        let Ok(markers) = fs::read_dir(session.path().join("subagents")) else {
            continue;
        };
        for marker in markers.flatten() {
            if let Some(id) = marker.path().file_stem().and_then(|name| name.to_str()) {
                ids.insert(id.to_string());
            }
        }
    }
    ids
}

fn grok_prompt_title(path: &Path, since_secs: Option<i64>) -> Option<String> {
    let mut prompt = String::new();
    let lines = BufReader::new(File::open(path).ok()?).lines();
    for line in lines.map_while(Result::ok).take(GROK_TITLE_SCAN_LINES) {
        if !line.contains("user_message_chunk") {
            if !prompt.is_empty() {
                break;
            }
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(since) = since_secs {
            let Some(timestamp) = row.get("timestamp").and_then(Value::as_i64) else {
                continue;
            };
            let seconds = if timestamp > 1_000_000_000_000 {
                timestamp / 1000
            } else {
                timestamp
            };
            if seconds < since {
                continue;
            }
        }
        if row
            .pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("user_message_chunk")
            && row
                .pointer("/params/update/content/type")
                .and_then(Value::as_str)
                == Some("text")
            && let Some(chunk) = row
                .pointer("/params/update/content/text")
                .and_then(Value::as_str)
        {
            prompt.push_str(chunk);
        }
    }
    user_prompt(&prompt)
}

fn modified_ms(path: &Path) -> i64 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

pub fn uses_ghostty(mode: crate::config::ResumeMode) -> bool {
    match mode {
        crate::config::ResumeMode::Ghostty => true,
        crate::config::ResumeMode::Current => false,
        crate::config::ResumeMode::Auto => {
            cfg!(target_os = "macos")
                && (std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
                    || std::env::var("TERM_PROGRAM").as_deref() == Ok("ghostty"))
        }
    }
}
fn validate_resume(session: &Session) -> Result<(), String> {
    if session
        .agent
        .as_ref()
        .is_some_and(|agent| agent.kind() != session.provider)
    {
        return Err("会话来源与 Agent 不匹配".into());
    }
    if !session.provider.valid_session_id(&session.id) {
        return Err("无效的会话 ID".into());
    }
    if !session.cwd.is_absolute() || !session.cwd.is_dir() {
        return Err(format!("项目目录不存在：{}", session.cwd.display()));
    }
    if !session.home.is_dir() {
        return Err(format!("会话目录不存在：{}", session.home.display()));
    }
    let key = session.provider.home_env();
    if key.is_empty()
        || key
            .bytes()
            .enumerate()
            .any(|(i, c)| !(c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())))
    {
        return Err("无效的实例环境变量".into());
    }
    Ok(())
}
/// Direct arguments and env avoid a shell. The TUI suspends while this command owns the terminal.
pub fn resume_in_current(session: &Session) -> Result<std::process::ExitStatus, String> {
    validate_resume(session)?;
    let agent = match &session.agent {
        Some(agent) => Some(Arc::clone(agent)),
        None if session.provider == AgentKind::CODEX => Some(
            crate::agents::builtin_registry()
                .create(
                    "codex",
                    crate::agents::AgentContext::Local {
                        home: session.home.clone(),
                    },
                )
                .map_err(|error| error.to_string())?,
        ),
        None => None,
    };
    if let Some(agent) = agent {
        let program = session
            .provider
            .command()
            .split_whitespace()
            .next()
            .ok_or("恢复命令为空")?;
        return agent
            .launcher()
            .ok_or("此 Agent 不支持恢复会话")?
            .spawn(&crate::agents::SpawnRequest {
                cwd: session.cwd.clone(),
                program: program.into(),
                args: Vec::new(),
                resume: Some(session.id.clone()),
                usage_config: None,
            })
            .map(|outcome| outcome.status);
    }
    let mut parts = session.provider.command().split_whitespace();
    let program = parts.next().ok_or("恢复命令为空")?;
    Command::new(program)
        .args(parts)
        .arg(&session.id)
        .current_dir(&session.cwd)
        .env(session.provider.home_env(), &session.home)
        .status()
        .map_err(|e| format!("无法运行 {}：{e}", session.provider.name()))
}
pub fn resume_shell_command(session: &Session) -> Result<String, String> {
    validate_resume(session)?;
    match &session.agent {
        Some(agent) => agent
            .launcher()
            .ok_or("此 Agent 不支持恢复会话")?
            .resume_shell_command(session),
        None => local_resume_shell_command(session),
    }
}

pub(crate) fn local_resume_shell_command(session: &Session) -> Result<String, String> {
    validate_resume(session)?;
    if session.provider == AgentKind::CODEX
        && let Some((profile, revision)) =
            crate::codex_manager::binding_for_home(&session.home).map_err(|e| e.to_string())?
    {
        return crate::codex_manager::launch_shell_command(
            &profile,
            &session.cwd,
            Some(revision),
            Some(&session.id),
        )
        .map_err(|e| e.to_string());
    }
    if session.provider == AgentKind::CODEX {
        return crate::codex_manager::local_resume_shell_command(
            &session.home,
            &session.cwd,
            &session.id,
        )
        .map_err(|error| error.to_string());
    }
    #[cfg(not(target_os = "windows"))]
    {
        let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let command = session
            .provider
            .command()
            .split_whitespace()
            .chain(std::iter::once(session.id.as_str()))
            .map(quote)
            .collect::<Vec<_>>()
            .join(" ");
        Ok(format!(
            "cd -- {} && env {} {}",
            quote(&session.cwd.to_string_lossy()),
            quote(&format!(
                "{}={}",
                session.provider.home_env(),
                session.home.display()
            )),
            command
        ))
    }
    #[cfg(target_os = "windows")]
    {
        let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let command = session
            .provider
            .command()
            .split_whitespace()
            .chain(std::iter::once(session.id.as_str()))
            .map(quote)
            .collect::<Vec<_>>()
            .join(" ");
        Ok(format!(
            "Set-Location -LiteralPath {}; $env:{}={}; & {}",
            quote(&session.cwd.to_string_lossy()),
            session.provider.home_env(),
            quote(&session.home.to_string_lossy()),
            command
        ))
    }
}
pub fn copy_resume_command(session: &Session) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;
    let text = resume_shell_command(session)?;
    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pbcopy", &[])];
    #[cfg(target_os = "windows")]
    let candidates: &[(&str, &[&str])] = &[("clip.exe", &[])];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (program, args) in candidates {
        let Ok(mut child) = Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let write = child
            .stdin
            .take()
            .ok_or_else(|| "剪贴板未提供输入".to_string())
            .and_then(|mut input| input.write_all(text.as_bytes()).map_err(|e| e.to_string()));
        if write.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            continue;
        }
        let started = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(_)) => break,
                Ok(None) if started.elapsed() < std::time::Duration::from_secs(3) => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
    }
    Err("没有可用的系统剪贴板，请使用预览命令在终端恢复".into())
}

pub fn resume_in_ghostty(session: &Session) -> Result<(), String> {
    command_in_ghostty(&session.cwd, &resume_shell_command(session)?)
}

/// Open a launcher command containing identifiers, never credentials, in a fresh Ghostty split.
pub fn command_in_ghostty(cwd: &Path, command: &str) -> Result<(), String> {
    if !uses_ghostty(crate::config::ResumeMode::Auto) {
        return Err("请在 macOS Ghostty 中使用 --split".into());
    }
    let cwd = cwd
        .canonicalize()
        .map_err(|_| "项目目录不存在".to_string())?;
    if !cwd.is_dir() {
        return Err("项目路径不是目录".into());
    }
    let script = r#"on run argv
        tell application "Ghostty"
            set currentTerm to focused terminal of selected tab of front window
            if currentTerm is missing value then error "找不到当前 Ghostty 终端"
            set cfg to new surface configuration
            set initial working directory of cfg to item 1 of argv
            set newTerm to split currentTerm direction right with configuration cfg
            input text (item 2 of argv) to newTerm
            send key "enter" to newTerm
            focus newTerm
        end tell
    end run"#;
    let output = Command::new("osascript")
        .args(["-e", script])
        .arg(cwd)
        .arg(command)
        .output()
        .map_err(|e| format!("无法打开 Ghostty：{e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Ghostty 分屏失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom_source(home: &Path) -> Result<Vec<SessionRecord>, String> {
        Ok(vec![SessionRecord {
            id: "123e4567-e89b-12d3-a456-426614174003".into(),
            cwd: home.join("project"),
            title: "Custom session".into(),
            recency_at_ms: 42,
        }])
    }

    #[test]
    fn registry_adds_a_provider_without_browser_changes() {
        let root = std::env::temp_dir().join(format!("usage-custom-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("project")).unwrap();
        let custom = AgentKind::new(
            "custom",
            "Custom",
            Some((180, 120, 210)),
            "custom resume",
            "CUSTOM_HOME",
            3,
        );
        let source = SessionSource {
            kind: custom,
            load: custom_source,
        };
        let mut registry = builtin_registry();
        registry.register(source).unwrap();
        assert!(registry.register(source).is_err());
        let catalog = registry.load_all(&SessionHomes::new().with("custom", &root));
        assert_eq!(catalog.sessions.len(), 1);
        assert_eq!(catalog.sessions[0].provider, custom);
        assert_eq!(catalog.sessions[0].home, root.canonicalize().unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovers_claude_and_grok() {
        let root = std::env::temp_dir().join(format!("usage-session-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("project");
        let claude = root.join("claude");
        let grok = root.join("grok");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(claude.join("projects/project")).unwrap();
        fs::create_dir_all(grok.join("sessions/project/123e4567-e89b-12d3-a456-426614174002"))
            .unwrap();
        fs::write(
            claude.join("projects/project/123e4567-e89b-12d3-a456-426614174001.jsonl"),
            serde_json::json!({"type":"user","cwd":cwd,"message":{"content":"Fix the TUI"}})
                .to_string(),
        )
        .unwrap();
        fs::write(grok.join("sessions/project/123e4567-e89b-12d3-a456-426614174002/summary.json"),
            serde_json::json!({"info":{"id":"123e4567-e89b-12d3-a456-426614174002","cwd":cwd},"generated_title":"Improve UI","updated_at":"2026-09-29T12:00:00Z"}).to_string()).unwrap();
        let catalog = load_all(
            &SessionHomes::new()
                .with("claude", claude)
                .with("codex", root.join("missing-codex"))
                .with("grok", grok),
        );
        assert_eq!(catalog.sessions.len(), 2);
        assert!(
            catalog
                .sessions
                .iter()
                .any(|s| s.provider == AgentKind::CLAUDE && s.title == "Fix the TUI")
        );
        assert!(
            catalog
                .sessions
                .iter()
                .any(|s| s.provider == AgentKind::GROK && s.title == "Improve UI")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn claude_title_skips_metadata_tool_results_and_uses_prompt() {
        let root =
            std::env::temp_dir().join(format!("usage-claude-title-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let lines = [
            serde_json::json!({"type":"user","cwd":"/tmp","isMeta":true,"message":{"content":"Caveat: generated context"}}),
            serde_json::json!({"type":"user","cwd":"/tmp","message":{"content":[{"type":"tool_result","content":"tool output"}]}}),
            serde_json::json!({"type":"user","cwd":"/tmp","message":{"content":"<environment_context>injected</environment_context>"}}),
            serde_json::json!({"type":"user","cwd":"/tmp","message":{"content":[{"type":"text","text":"Fix the"},{"type":"text","text":"browser\nlayout"}]}}),
        ];
        fs::write(
            &path,
            lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        assert_eq!(
            claude_metadata(&path),
            Some((PathBuf::from("/tmp"), "Fix the browser layout".into()))
        );
        fs::write(&path, [
            serde_json::json!({"type":"user","cwd":"/tmp","message":{"content":"<command-name>/review</command-name><command-args>42</command-args>"}}).to_string(),
        ].join("\n")).unwrap();
        assert_eq!(claude_metadata(&path).unwrap().1, "/review 42");
        fs::write(
            &path,
            [
                serde_json::json!({"type":"user","cwd":"/tmp","isMeta":true,"message":{"content":"generated"}}),
                serde_json::json!({"type":"ai-title","aiTitle":"Named   session"}),
            ]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();
        assert_eq!(claude_metadata(&path).unwrap().1, "Named session");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn grok_skips_subagents_but_keeps_forks_and_prefers_first_prompt() {
        let root =
            std::env::temp_dir().join(format!("usage-grok-subagents-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("project");
        let base = root.join("grok/sessions/project");
        fs::create_dir_all(&cwd).unwrap();
        let main = "123e4567-e89b-12d3-a456-426614174010";
        let sub = "123e4567-e89b-12d3-a456-426614174011";
        let fork = "123e4567-e89b-12d3-a456-426614174012";
        let marked = "123e4567-e89b-12d3-a456-426614174013";
        for (id, kind, parent, generated) in [
            (main, "main", "", "Generated title"),
            (sub, "subagent:explore", main, "Subagent title"),
            (fork, "main", main, "Fork title"),
            (marked, "main", main, "Marked subagent"),
        ] {
            fs::create_dir_all(base.join(id)).unwrap();
            fs::write(
                base.join(id).join("summary.json"),
                serde_json::json!({
                    "info":{"id":id,"cwd":cwd},
                    "session_kind":kind,
                    "parent_session_id":parent,
                    "generated_title":generated,
                    "created_at":"2026-09-27T08:00:00Z",
                })
                .to_string(),
            )
            .unwrap();
        }
        fs::create_dir_all(base.join(main).join("subagents")).unwrap();
        fs::write(
            base.join(main)
                .join("subagents")
                .join(format!("{marked}.json")),
            "{}",
        )
        .unwrap();
        fs::write(base.join(main).join("updates.jsonl"), [
            serde_json::json!({"params":{"update":{"sessionUpdate":"hook_execution"}}}),
            serde_json::json!({"params":{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Port the "}}}}),
            serde_json::json!({"params":{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"parser"}}}}),
            serde_json::json!({"params":{"update":{"sessionUpdate":"agent_message_chunk"}}}),
        ].iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")).unwrap();
        fs::write(
            base.join(fork).join("updates.jsonl"),
            [
                serde_json::json!({"timestamp":1790495000,"params":{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Copied source prompt"}}}}),
                serde_json::json!({"timestamp":1790496010,"params":{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"New direction"}}}}),
                serde_json::json!({"timestamp":1790496020,"params":{"update":{"sessionUpdate":"agent_message_chunk"}}}),
            ]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();
        let records = load_grok(&root.join("grok")).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records.iter().find(|s| s.id == main).unwrap().title,
            "Port the parser"
        );
        assert_eq!(
            records.iter().find(|s| s.id == fork).unwrap().title,
            "New direction"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn loads_codex_index_and_skips_subagents() {
        let root = std::env::temp_dir().join(format!("usage-codex-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("project");
        let codex = root.join("codex");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&codex).unwrap();
        let escaped = cwd.to_string_lossy().replace('\'', "''");
        let sql = format!(
            "CREATE TABLE threads (id TEXT, cwd TEXT, name TEXT, title TEXT, preview TEXT, recency_at_ms INTEGER, archived INTEGER, thread_source TEXT, source TEXT); \
             INSERT INTO threads VALUES ('123e4567-e89b-12d3-a456-426614174000','{escaped}','Test session','','',100,0,'user','cli'), \
             ('123e4567-e89b-12d3-a456-426614174001','{escaped}','Subagent','','',90,0,'subagent','cli'), \
             ('123e4567-e89b-12d3-a456-426614174002','{escaped}','Archived','','',80,1,'user','cli');"
        );
        assert!(
            Command::new("sqlite3")
                .arg(codex.join("state_5.sqlite"))
                .arg(sql)
                .status()
                .unwrap()
                .success()
        );
        let catalog = load_all(
            &SessionHomes::new()
                .with("claude", root.join("missing-claude"))
                .with("codex", codex)
                .with("grok", root.join("missing-grok")),
        );
        assert_eq!(catalog.sessions.len(), 1);
        assert_eq!(catalog.sessions[0].provider, AgentKind::CODEX);
        assert_eq!(catalog.sessions[0].title, "Test session");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn falls_back_to_codex_rollouts_when_database_cannot_open() {
        let root = std::env::temp_dir().join(format!("usage-rollouts-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("project");
        let codex = root.join("codex");
        let rollouts = codex.join("sessions/2026/09/29");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&rollouts).unwrap();
        fs::create_dir_all(codex.join("archived_sessions")).unwrap();
        fs::write(codex.join("state_5.sqlite"), "not a database").unwrap();
        fs::write(
            codex.join("session_index.jsonl"),
            serde_json::json!({"id":"123e4567-e89b-12d3-a456-426614174000","thread_name":"Indexed title"}).to_string(),
        ).unwrap();
        for (id, source) in [
            ("123e4567-e89b-12d3-a456-426614174000", "user"),
            ("123e4567-e89b-12d3-a456-426614174001", "subagent"),
            ("123e4567-e89b-12d3-a456-426614174002", "user"),
        ] {
            fs::write(
                rollouts.join(format!("rollout-{id}.jsonl")),
                format!(
                    "{}\n{}\n",
                    serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":cwd,"thread_source":source,"source":"cli"}}),
                    serde_json::json!({"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"Fallback title"}]}}),
                ),
            ).unwrap();
        }
        fs::write(
            codex.join("archived_sessions/rollout-archived.jsonl"),
            serde_json::json!({"type":"session_meta","payload":{"id":"123e4567-e89b-12d3-a456-426614174002","cwd":cwd,"thread_source":"user","source":"cli"}}).to_string(),
        )
        .unwrap();
        let catalog = load_all(
            &SessionHomes::new()
                .with("claude", root.join("missing-claude"))
                .with("codex", codex)
                .with("grok", root.join("missing-grok")),
        );
        assert!(catalog.warnings.is_empty(), "{:?}", catalog.warnings);
        assert_eq!(catalog.sessions.len(), 1);
        assert_eq!(catalog.sessions[0].title, "Indexed title");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn codex_rollout_title_skips_environment_context() {
        let root = std::env::temp_dir().join(format!("usage-title-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout.jsonl");
        fs::write(
            &path,
            [
                serde_json::json!({"type":"session_meta","payload":{"id":"123e4567-e89b-12d3-a456-426614174000","cwd":"/tmp","thread_source":"user"}}),
                serde_json::json!({"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"<environment_context>system data</environment_context>"}]}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"# AGENTS.md project instructions"}}),
                serde_json::json!({"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"Build the browser"}]}}),
            ]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();
        let session = codex_rollout_metadata(&path, &HashMap::new()).unwrap();
        assert_eq!(session.title, "Build the browser");
        fs::write(
            &path,
            [
                serde_json::json!({"type":"session_meta","payload":{"id":"123e4567-e89b-12d3-a456-426614174000","cwd":"/tmp","thread_source":"user"}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"Review\n the layout"}}),
            ]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();
        assert_eq!(
            codex_rollout_metadata(&path, &HashMap::new())
                .unwrap()
                .title,
            "Review the layout"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fallback_does_not_hide_index_error_when_rollouts_are_not_resumable() {
        let root = std::env::temp_dir().join(format!("usage-invalid-{}", uuid::Uuid::new_v4()));
        let rollouts = root.join("sessions/2026/09/29");
        fs::create_dir_all(&rollouts).unwrap();
        fs::write(root.join("state_5.sqlite"), "not a database").unwrap();
        fs::write(
            rollouts.join("rollout.jsonl"),
            serde_json::json!({"type":"session_meta","payload":{"id":"123e4567-e89b-12d3-a456-426614174000","cwd":"/path/that/does/not/exist","thread_source":"user"}}).to_string(),
        )
        .unwrap();
        assert!(load_codex(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn registry_keeps_same_session_in_two_instances_and_correct_homes() {
        let root =
            std::env::temp_dir().join(format!("usage-instance-sessions-{}", uuid::Uuid::new_v4()));
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let id = "123e4567-e89b-12d3-a456-426614174001";
        for name in ["primary", "work"] {
            let logs = root.join(name).join("projects/p");
            fs::create_dir_all(&logs).unwrap();
            fs::write(logs.join(format!("{id}.jsonl")),format!("{}\n",serde_json::json!({"type":"user","cwd":project,"sessionId":id,"message":{"content":"Task"}}))).unwrap();
        }
        let homes = SessionHomes::new()
            .with("claude", root.join("primary"))
            .with_instance("work", "claude", root.join("work"));
        let catalog = builtin_registry().load_all(&homes);
        assert_eq!(catalog.sessions.len(), 2);
        assert!(
            catalog.sessions.iter().any(
                |s| s.instance == "work" && s.home == root.join("work").canonicalize().unwrap()
            )
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn current_terminal_and_copied_command_keep_quoted_paths_and_instance_env() {
        let root = std::env::temp_dir().join(format!("usage-resume-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("project ' with $ punctuation");
        let home = root.join("home ' instance");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        let script = root.join("agent.sh");
        fs::write(
            &script,
            r#"{ pwd -P; printf '%s\n' "$CUSTOM_HOME" "$1"; } > "$CUSTOM_HOME/result.txt"
"#,
        )
        .unwrap();
        let command: &'static str =
            Box::leak(format!("/bin/sh {}", script.display()).into_boxed_str());
        let session = Session {
            agent: None,
            provider: AgentKind::new("custom", "Custom", None, command, "CUSTOM_HOME", 0),
            instance: "work".into(),
            id: "123e4567-e89b-12d3-a456-426614174001".into(),
            cwd: cwd.clone(),
            home: home.clone(),
            title: "task".into(),
            recency_at_ms: 0,
        };
        assert!(resume_in_current(&session).unwrap().success());
        let first = fs::read_to_string(home.join("result.txt")).unwrap();
        assert!(first.contains(&cwd.canonicalize().unwrap().to_string_lossy().to_string()));
        assert!(first.contains(&home.to_string_lossy().to_string()));
        assert!(first.contains(&session.id));
        let copied = resume_shell_command(&session).unwrap();
        assert!(
            Command::new("/bin/sh")
                .arg("-c")
                .arg(copied)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(fs::read_to_string(home.join("result.txt")).unwrap(), first);
        let mut invalid = session;
        invalid.id = "$(touch bad)".into();
        assert!(resume_shell_command(&invalid).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
