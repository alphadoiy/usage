//! Local session catalog and Ghostty launcher.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde_json::Value;

const CODEX_QUERY: &str = "SELECT id, cwd, COALESCE(NULLIF(name,''), NULLIF(title,''), NULLIF(preview,''), id) AS title, recency_at_ms FROM threads WHERE archived=0 AND cwd<>'' AND COALESCE(thread_source,'user')='user' AND source<>'exec' ORDER BY recency_at_ms DESC;";
const CODEX_METADATA_SCAN_LINES: usize = 8;
const CODEX_TITLE_SCAN_LINES: usize = 256;
const GHOSTTY_SCRIPT: &str = r#"
on run argv
    set projectDir to item 1 of argv
    set sessionId to item 2 of argv
    set agentCommand to item 3 of argv
    set homeKey to item 4 of argv
    set agentHome to item 5 of argv
    tell application "Ghostty"
        set currentTerm to focused terminal of selected tab of front window
        if currentTerm is missing value then error "找不到当前 Ghostty 终端"
        set cfg to new surface configuration
        set initial working directory of cfg to projectDir
        set environment variables of cfg to {homeKey & "=" & agentHome}
        set newTerm to split currentTerm direction right with configuration cfg
        input text agentCommand & " " & sessionId to newTerm
        send key "enter" to newTerm
        focus newTerm
    end tell
end run
"#;

/// Metadata needed to group, color, and resume one session provider.
#[derive(Debug, Clone, Copy)]
pub struct AgentKind {
    id: &'static str,
    name: &'static str,
    accent: Option<crate::theme::Rgb>,
    command: &'static str,
    home_env: &'static str,
    order: u16,
}

impl AgentKind {
    pub const CLAUDE: Self = Self::new(
        "claude",
        "Claude Code",
        None,
        "claude --resume",
        "CLAUDE_CONFIG_DIR",
        0,
    );
    pub const CODEX: Self = Self::new("codex", "Codex", None, "codex resume", "CODEX_HOME", 1);
    pub const GROK: Self = Self::new("grok", "Grok", None, "grok --resume", "GROK_HOME", 2);

    /// `accent = None` uses the shared theme; `order` controls group position.
    pub const fn new(
        id: &'static str,
        name: &'static str,
        accent: Option<crate::theme::Rgb>,
        command: &'static str,
        home_env: &'static str,
        order: u16,
    ) -> Self {
        Self {
            id,
            name,
            accent,
            command,
            home_env,
            order,
        }
    }

    pub fn id(self) -> &'static str {
        self.id
    }
    pub fn name(self) -> &'static str {
        self.name
    }
    pub fn accent(self) -> crate::theme::Rgb {
        self.accent.unwrap_or_else(|| crate::theme::accent(self.id))
    }
    pub fn order(self) -> u16 {
        self.order
    }
    fn command(self) -> &'static str {
        self.command
    }
    fn home_env(self) -> &'static str {
        self.home_env
    }
}

impl PartialEq for AgentKind {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for AgentKind {}

impl Hash for AgentKind {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub provider: AgentKind,
    pub id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub recency_at_ms: i64,
    pub home: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct SessionHomes(HashMap<String, PathBuf>);

impl SessionHomes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, provider_id: &str, home: impl Into<PathBuf>) -> Self {
        self.0.insert(provider_id.to_string(), home.into());
        self
    }

    pub fn get(&self, provider_id: &str) -> Option<&Path> {
        self.0.get(provider_id).map(PathBuf::as_path)
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
            let Some(home) = homes.get(source.kind.id()).filter(|home| home.is_dir()) else {
                continue;
            };
            match (source.load)(home) {
                Ok(records) => {
                    catalog
                        .sessions
                        .extend(
                            records
                                .into_iter()
                                .filter(is_local_record)
                                .map(|record| Session {
                                    provider: source.kind,
                                    id: record.id,
                                    cwd: record.cwd,
                                    title: record.title,
                                    recency_at_ms: record.recency_at_ms,
                                    home: home.to_path_buf(),
                                }),
                        )
                }
                Err(error) => catalog
                    .warnings
                    .push(format!("{}：{error}", source.kind.name())),
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

pub fn builtin_registry() -> SessionRegistry {
    let mut registry = SessionRegistry::new();
    for source in [
        SessionSource {
            kind: AgentKind::CLAUDE,
            load: load_claude,
        },
        SessionSource {
            kind: AgentKind::CODEX,
            load: load_codex,
        },
        SessionSource {
            kind: AgentKind::GROK,
            load: load_grok,
        },
    ] {
        registry.register(source).expect("内置会话来源 ID 唯一");
    }
    registry
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

fn load_codex(home: &Path) -> Result<Vec<SessionRecord>, String> {
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
        .map(|r| SessionRecord {
            id: r.id,
            cwd: r.cwd,
            title: r.title,
            recency_at_ms: r.recency_at_ms,
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
            {
                titles.insert(id.to_string(), title.to_string());
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
                if row.get("type").and_then(Value::as_str) != Some("response_item")
                    || row.pointer("/payload/role").and_then(Value::as_str) != Some("user")
                {
                    return None;
                }
                row.pointer("/payload/content")
                    .and_then(Value::as_array)?
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .map(str::trim)
                    .find(|text| {
                        !text.is_empty()
                            && !text.starts_with("<environment_context>")
                            && !text.starts_with("<system_reminder>")
                    })
                    .map(|text| text.chars().take(120).collect())
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

fn load_claude(home: &Path) -> Result<Vec<SessionRecord>, String> {
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
        if row.get("type").and_then(Value::as_str) != Some("user")
            || row.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let Some(content) = row.pointer("/message/content") else {
            continue;
        };
        let title = match content {
            Value::String(text) => text.as_str(),
            Value::Array(items) => {
                let Some(text) = items
                    .iter()
                    .find_map(|item| item.get("text").and_then(Value::as_str))
                else {
                    continue;
                };
                text
            }
            _ => continue,
        };
        if !title.trim().is_empty() {
            return Some((cwd?, title.into()));
        }
    }
    None
}

fn load_grok(home: &Path) -> Result<Vec<SessionRecord>, String> {
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
        for entry in fs::read_dir(project.path()).map_err(|e| e.to_string())? {
            let summary_path = entry
                .map_err(|e| e.to_string())?
                .path()
                .join("summary.json");
            let Ok(contents) = fs::read(&summary_path) else {
                continue;
            };
            let Ok(summary) = serde_json::from_slice::<Value>(&contents) else {
                continue;
            };
            let Some(id) = summary.pointer("/info/id").and_then(Value::as_str) else {
                continue;
            };
            let Some(cwd) = summary.pointer("/info/cwd").and_then(Value::as_str) else {
                continue;
            };
            let title = ["generated_title", "session_summary"]
                .iter()
                .filter_map(|key| summary.get(*key).and_then(Value::as_str))
                .find(|title| !title.trim().is_empty())
                .unwrap_or("未命名会话");
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
                title: title.into(),
                recency_at_ms,
            });
        }
    }
    Ok(sessions)
}

fn modified_ms(path: &Path) -> i64 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

pub fn resume_in_ghostty(session: &Session) -> Result<(), String> {
    if std::env::var_os("GHOSTTY_RESOURCES_DIR").is_none()
        && std::env::var("TERM_PROGRAM").as_deref() != Ok("ghostty")
    {
        return Err("请在 Ghostty 中打开 usage，再使用右侧分屏".into());
    }
    if uuid::Uuid::parse_str(&session.id).is_err() {
        return Err("无效的会话 ID".into());
    }
    if !session.cwd.is_dir() {
        return Err(format!("项目目录不存在：{}", session.cwd.display()));
    }
    if !session.home.is_dir() {
        return Err(format!("会话目录不存在：{}", session.home.display()));
    }
    let output = Command::new("osascript")
        .arg("-e")
        .arg(GHOSTTY_SCRIPT)
        .arg(&session.cwd)
        .arg(&session.id)
        .arg(session.provider.command())
        .arg(session.provider.home_env())
        .arg(&session.home)
        .output()
        .map_err(|e| format!("无法调用 Ghostty AppleScript：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "Ghostty 分屏失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
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
        assert_eq!(catalog.sessions[0].home, root);
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
}
