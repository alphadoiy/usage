//! Local token history. Quota percentages cannot reconstruct daily consumption.
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{AppConfig, home_dir};

pub const SOURCES: [&str; 3] = ["codex", "claude", "grok"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub source: String,
    pub id: String,
    pub time: DateTime<Utc>,
    pub tokens: Tokens,
    pub session: String,
    pub project: String,
    pub model: String,
}
#[derive(Clone, Debug, Default)]
pub struct History {
    pub days: BTreeMap<NaiveDate, BTreeMap<String, Tokens>>,
    pub warnings: Vec<String>,
    pub records: Vec<Record>,
    pub scanned_files: usize,
    pub read_bytes: u64,
    pub synced_at: Option<DateTime<Utc>>,
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
    pub synced_at: Option<DateTime<Utc>>,
    pub scanned_files: usize,
    pub read_bytes: u64,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryQuery {
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub project: Option<String>,
    pub model: Option<String>,
}
impl HistoryQuery {
    pub fn contains_date(&self, date: NaiveDate) -> bool {
        self.from.is_none_or(|from| date >= from) && self.to.is_none_or(|to| date <= to)
    }
    pub fn matches(&self, record: &Record) -> bool {
        self.contains_date(record.time.with_timezone(&Local).date_naive())
            && self
                .project
                .as_ref()
                .is_none_or(|p| record.project.to_lowercase().contains(&p.to_lowercase()))
            && self
                .model
                .as_ref()
                .is_none_or(|m| record.model.to_lowercase().contains(&m.to_lowercase()))
    }
}
#[derive(Clone, Debug, Default)]
pub struct SessionUsage {
    pub tokens: Tokens,
    pub models: Vec<String>,
}
impl History {
    pub fn filtered(&self, query: &HistoryQuery) -> Self {
        let mut result = Self {
            warnings: self.warnings.clone(),
            scanned_files: self.scanned_files,
            read_bytes: self.read_bytes,
            synced_at: self.synced_at,
            ..Self::default()
        };
        if self.records.is_empty() && query.project.is_none() && query.model.is_none() {
            result.days = self
                .days
                .iter()
                .filter(|(d, _)| query.contains_date(**d))
                .map(|(d, t)| (*d, t.clone()))
                .collect();
        }
        for record in self.records.iter().filter(|r| query.matches(r)) {
            result.records.push(record.clone());
            result
                .days
                .entry(record.time.with_timezone(&Local).date_naive())
                .or_default()
                .entry(record.source.clone())
                .or_default()
                .add(record.tokens);
        }
        result
    }
    pub fn session_usage(
        &self,
        sources: &[String],
        query: &HistoryQuery,
    ) -> HashMap<(String, String), SessionUsage> {
        let mut sessions = HashMap::<(String, String), SessionUsage>::new();
        for record in self
            .records
            .iter()
            .filter(|r| sources.contains(&r.source) && query.matches(r))
        {
            let usage = sessions
                .entry((record.source.clone(), record.session.clone()))
                .or_default();
            usage.tokens.add(record.tokens);
            if !record.model.is_empty() && !usage.models.contains(&record.model) {
                usage.models.push(record.model.clone());
            }
        }
        for usage in sessions.values_mut() {
            usage.models.sort();
        }
        sessions
    }
    pub fn report_query(
        &self,
        year: i32,
        sources: &[String],
        query: &HistoryQuery,
    ) -> HistoryReport {
        if query == &HistoryQuery::default() {
            return self.report(year, sources);
        }
        let filtered = self.filtered(query);
        let mut report = filtered.report(year, sources);
        report.from = query.from;
        report.to = query.to;
        if query.from.is_some() || query.to.is_some() {
            let from = query
                .from
                .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 1, 1).unwrap());
            let to = query
                .to
                .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 12, 31).unwrap());
            report.daily.clear();
            report.monthly.clear();
            report.totals = Tokens::default();
            let mut date = from;
            while date <= to {
                let tokens = filtered.day(date, sources);
                report.daily.insert(date.to_string(), tokens);
                report
                    .monthly
                    .entry(date.format("%Y-%m").to_string())
                    .or_default()
                    .add(tokens);
                report.totals.add(tokens);
                let Some(next) = date.succ_opt() else {
                    break;
                };
                date = next;
            }
        }
        report
    }
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
            synced_at: self.synced_at,
            scanned_files: self.scanned_files,
            read_bytes: self.read_bytes,
            from: None,
            to: None,
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

#[derive(Clone, Default, Serialize, Deserialize)]
struct ParserState {
    high: Tokens,
    signatures: HashMap<String, String>,
    previous: Option<String>,
    session: String,
    project: String,
    model: String,
    fork_time: Option<DateTime<Utc>>,
    invalid_fork: bool,
    line: usize,
}
#[derive(Clone, Serialize, Deserialize)]
struct CachedFile {
    source: String,
    len: u64,
    modified: Option<SystemTime>,
    offset: u64,
    fingerprint: Vec<u8>,
    state: ParserState,
    records: Vec<Record>,
    malformed: usize,
}
pub struct HistoryCache {
    files: HashMap<PathBuf, CachedFile>,
    database: Option<PathBuf>,
}
impl Default for HistoryCache {
    fn default() -> Self {
        #[cfg(test)]
        {
            Self::memory()
        }
        #[cfg(not(test))]
        {
            let base = std::env::var_os("XDG_DATA_HOME")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home_dir().join(".local/share"));
            Self::at(base.join("usage/history.sqlite3"))
        }
    }
}
impl HistoryCache {
    pub fn at(path: PathBuf) -> Self {
        Self {
            files: HashMap::new(),
            database: Some(path),
        }
    }
    #[cfg(test)]
    fn memory() -> Self {
        Self {
            files: HashMap::new(),
            database: None,
        }
    }

    pub fn load(&mut self, config: &AppConfig) -> History {
        self.load_with_cached(config, |_| {})
    }
    pub fn load_with_cached(
        &mut self,
        config: &AppConfig,
        cached: impl FnOnce(History),
    ) -> History {
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
        let roots = history_roots(config);
        let mut database = self.database.as_ref().map(|path| open_database(path));
        if let Some(Ok(db)) = &database {
            match read_database(db) {
                Ok(files) => self.files = files,
                Err(e) => history.warnings.push(format!("读取历史账本失败：{e}")),
            }
            let synced: Option<String> = db
                .query_row(
                    "SELECT value FROM history_meta WHERE key='synced_at'",
                    [],
                    |r| r.get(0),
                )
                .ok();
            let mut previous = aggregate(&self.files, &roots, History::default());
            previous.synced_at = synced
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|t| t.with_timezone(&Utc));
            if previous.synced_at.is_some() {
                cached(previous);
            }
        }
        let mut transaction = None;
        if let Some(Err(e)) = &database {
            history
                .warnings
                .push(format!("历史账本不可用，本次使用内存：{e}"));
        }
        if let Some(Ok(db)) = database.as_mut() {
            match db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate) {
                Ok(tx) => transaction = Some(tx),
                Err(e) => history.warnings.push(format!("历史账本暂不可写：{e}")),
            }
        }

        if let Some(tx) = &transaction {
            match read_database(tx) {
                Ok(files) => self.files = files,
                Err(e) => history.warnings.push(format!("读取历史账本失败：{e}")),
            }
        }
        paths.sort();
        for (path, source) in paths {
            history.scanned_files += 1;
            let metadata = match fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => {
                    history.warnings.push(format!("{source}: 无法读取日志文件"));
                    continue;
                }
            };
            if self
                .files
                .get(&path)
                .is_some_and(|e| e.len == metadata.len() && e.modified == metadata.modified().ok())
            {
                continue;
            }
            let mut entry = self
                .files
                .remove(&path)
                .unwrap_or_else(|| fresh_file(&path, &source));
            let fallback = entry.clone();
            let incremental = entry.offset <= metadata.len()
                && fingerprint(&path, entry.offset).is_ok_and(|f| f == entry.fingerprint)
                && metadata.len() > entry.len;
            if !incremental {
                entry = fresh_file(&path, &source);
            }
            let start = entry.offset;
            match append_file(&path, &source, &mut entry) {
                Ok(()) => {
                    history.read_bytes += entry.offset.saturating_sub(start);
                    entry.len = metadata.len();
                    entry.modified = metadata.modified().ok();
                    if let Some(tx) = &transaction {
                        let result = serde_json::to_string(&entry).map_err(|e| e.to_string()).and_then(|data| {
                            tx.execute("INSERT INTO history_files(path,data) VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET data=excluded.data", rusqlite::params![path.to_string_lossy(), data]).map(|_| ()).map_err(|e| e.to_string())
                        });
                        if let Err(e) = result {
                            history.warnings.push(format!("保存历史账本失败：{e}"));
                        }
                    }
                }
                Err(_) => {
                    entry = fallback;
                    history
                        .warnings
                        .push(format!("{source}: 无法完整读取日志，保留上次统计"));
                }
            }
            self.files.insert(path, entry);
        }
        history.synced_at = Some(Utc::now());
        if let Some(tx) = transaction {
            let result = tx.execute("INSERT INTO history_meta(key,value) VALUES('synced_at',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [history.synced_at.unwrap().to_rfc3339()]);
            if let Err(e) = result {
                history.warnings.push(format!("保存同步时间失败：{e}"));
            }
            if let Err(e) = tx.commit() {
                history.warnings.push(format!("提交历史账本失败：{e}"));
            }
        }
        aggregate(&self.files, &roots, history)
    }
}
// Missing logs remain in the ledger. Configured roots isolate data directories.
fn aggregate(
    files: &HashMap<PathBuf, CachedFile>,
    roots: &[(String, PathBuf)],
    mut history: History,
) -> History {
    let mut unique: HashMap<(String, String), Record> = HashMap::new();
    let mut malformed = 0;
    for (path, entry) in files {
        if !roots
            .iter()
            .any(|(source, root)| *source == entry.source && path.starts_with(root))
        {
            continue;
        }
        malformed += entry.malformed;
        for record in &entry.records {
            unique
                .entry((record.source.clone(), record.id.clone()))
                .and_modify(|r| {
                    r.tokens.merge(record.tokens);
                    r.time = r.time.min(record.time);
                    if r.project.is_empty() {
                        r.project.clone_from(&record.project);
                    }
                    if r.model.is_empty() {
                        r.model.clone_from(&record.model);
                    }
                })
                .or_insert_with(|| record.clone());
        }
    }
    if malformed > 0 {
        history
            .warnings
            .push(format!("跳过 {malformed} 条损坏或无有效时间的用量记录"));
    }
    history.records = unique
        .into_values()
        .filter(|r| r.tokens.total() > 0)
        .collect();
    history
        .records
        .sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.id.cmp(&b.id)));
    for record in &history.records {
        history
            .days
            .entry(record.time.with_timezone(&Local).date_naive())
            .or_default()
            .entry(record.source.clone())
            .or_default()
            .add(record.tokens);
    }
    history
}
fn history_roots(config: &AppConfig) -> Vec<(String, PathBuf)> {
    let home = home_dir();
    let mut roots = Vec::new();
    for source in &config.providers {
        match source.as_str() {
            "codex" => {
                let root = config
                    .codex_dir
                    .clone()
                    .unwrap_or_else(|| home.join(".codex"));
                roots.push((source.clone(), root.join("sessions")));
                roots.push((source.clone(), root.join("archived_sessions")));
            }
            "claude" => roots.push((
                source.clone(),
                config
                    .claude_dir
                    .clone()
                    .unwrap_or_else(|| home.join(".claude"))
                    .join("projects"),
            )),
            "grok" => roots.push((
                source.clone(),
                config
                    .grok_dir
                    .clone()
                    .unwrap_or_else(|| home.join(".grok"))
                    .join("sessions"),
            )),
            _ => {}
        }
    }
    roots
}
fn open_database(path: &Path) -> Result<rusqlite::Connection, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let db = rusqlite::Connection::open(path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    db.busy_timeout(std::time::Duration::from_secs(20))
        .map_err(|e| e.to_string())?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS history_files(path TEXT PRIMARY KEY, data TEXT NOT NULL); CREATE TABLE IF NOT EXISTS history_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);",
    )
    .map_err(|e| e.to_string())?;
    Ok(db)
}
fn read_database(db: &rusqlite::Connection) -> Result<HashMap<PathBuf, CachedFile>, String> {
    let mut stmt = db
        .prepare("SELECT path,data FROM history_files")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?;
    let mut files = HashMap::new();
    for row in rows {
        let (path, data) = row.map_err(|e| e.to_string())?;
        let entry = serde_json::from_str(&data).map_err(|e| format!("无效的历史缓存：{e}"))?;
        files.insert(path.into(), entry);
    }
    Ok(files)
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
fn grok_tokens(u: &Value) -> Tokens {
    let input = number(u, "inputTokens");
    Tokens {
        input,
        output: number(u, "outputTokens"),
        cached: number(u, "cachedReadTokens").min(input),
        calls: number(u, "modelCalls").max(1),
    }
}
fn fresh_file(path: &Path, source: &str) -> CachedFile {
    let session = if source == "grok" {
        path.parent().and_then(Path::file_name)
    } else {
        path.file_stem()
    }
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_default();
    let mut state = ParserState {
        session,
        ..ParserState::default()
    };
    if source == "grok"
        && let Ok(text) = fs::read_to_string(path.with_file_name("summary.json"))
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
    CachedFile {
        source: source.into(),
        len: 0,
        modified: None,
        offset: 0,
        fingerprint: Vec::new(),
        state,
        records: Vec::new(),
        malformed: 0,
    }
}
fn fingerprint(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(offset.min(2048))
        .read_to_end(&mut bytes)?;
    file.seek(SeekFrom::Start(offset.saturating_sub(2048)))?;
    file.take(offset.min(2048)).read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn append_file(path: &Path, source: &str, entry: &mut CachedFile) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(entry.offset))?;
    let mut reader = BufReader::new(file);
    let ParserState {
        high,
        signatures,
        previous,
        session,
        project,
        model,
        fork_time,
        invalid_fork,
        line: line_index,
    } = &mut entry.state;
    let records = &mut entry.records;
    let malformed = &mut entry.malformed;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 || !line.ends_with('\n') {
            break;
        }
        let index = *line_index;
        *line_index += 1;
        entry.offset += bytes as u64;
        if line.trim().is_empty() {
            continue;
        }
        // Avoid decoding large conversation/tool payloads unrelated to usage.
        if source == "codex"
            && !line.contains("\"token_count\"")
            && !line.contains("\"session_meta\"")
            && !line.contains("\"turn_context\"")
        {
            continue;
        }
        if source == "claude" && !line.contains("\"usage\"") && !line.contains("\"cwd\"") {
            continue;
        }
        if source == "grok" && !line.contains("\"turn_completed\"") {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                *malformed += 1;
                continue;
            }
        };
        if source == "claude" {
            if let Some(cwd) = v["cwd"].as_str() {
                *project = cwd.into();
            }
            if let Some(id) = v["sessionId"].as_str() {
                *session = id.into();
            }
            if let Some(name) = v.pointer("/message/model").and_then(Value::as_str) {
                *model = name.into();
            }
        }
        if source == "codex" && v["type"] == "turn_context" {
            if let Some(name) = v.pointer("/payload/model").and_then(Value::as_str) {
                *model = name.into();
            }
            if let Some(cwd) = v.pointer("/payload/cwd").and_then(Value::as_str) {
                *project = cwd.into();
            }
            continue;
        }
        if source == "grok"
            && let Some(name) = v.pointer("/params/update/model").and_then(Value::as_str)
        {
            *model = name.into();
        }
        if source == "codex" && v["type"] == "session_meta" {
            if let Some(id) = v["payload"]["id"].as_str() {
                *session = id.into();
            }
            if let Some(cwd) = v.pointer("/payload/cwd").and_then(Value::as_str) {
                *project = cwd.into();
            }
            if v["payload"]
                .get("forked_from_id")
                .is_some_and(|p| !p.is_null())
                || v.pointer("/payload/thread_spawn/parent_thread_id")
                    .is_some()
            {
                *fork_time = v.get("timestamp").and_then(time);
                if fork_time.is_none() {
                    *malformed += 1;
                    *invalid_fork = true;
                }
            }
            continue;
        }
        if *invalid_fork {
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
                *previous = Some(signature);
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
                    format!("{session}:{id}"),
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
                if source == "grok"
                    && let Some(by_model) = v
                        .pointer("/params/update/usage/modelUsage")
                        .and_then(Value::as_object)
                        .filter(|m| !m.is_empty())
                {
                    let mut total = Tokens::default();
                    for counters in by_model.values() {
                        total.add(grok_tokens(counters));
                    }
                    // Split only complete breakdowns; partial model maps must not inflate totals.
                    if total == tokens || tokens.total() == 0 {
                        for (name, counters) in by_model {
                            records.push(Record {
                                source: source.into(),
                                id: format!("{id}:{name}"),
                                time,
                                tokens: grok_tokens(counters),
                                session: session.clone(),
                                project: project.clone(),
                                model: name.clone(),
                            });
                        }
                        continue;
                    }
                }
                records.push(Record {
                    source: source.into(),
                    id,
                    time,
                    tokens,
                    session: session.clone(),
                    project: project.clone(),
                    model: model.clone(),
                });
            } else {
                *malformed += 1;
            }
        }
    }
    entry.fingerprint = fingerprint(path, entry.offset)?;
    Ok(())
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
        let history = HistoryCache::memory().load(&f.config(&["codex"]));
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
        let report = HistoryCache::memory()
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
        let history = HistoryCache::memory().load(&f.config(&["claude", "grok"]));
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
    fn refresh_updates_changed_files_and_retains_deleted_history() {
        let f = Fixture::new();
        let config = f.config(&["codex"]);
        let mut cache = HistoryCache::memory();
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
        assert_eq!(total(cache.load(&config)), 220);
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
        let history = HistoryCache::memory().load(&f.config(&["codex"]));
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
    #[test]
    fn ledger_survives_restart_and_deleted_logs_and_emits_cached_result() {
        let f = Fixture::new();
        let config = f.config(&["codex"]);
        let db = f.0.join("ledger.sqlite3");
        let path = f.write(
            "codex/sessions/a.jsonl",
            &[
                meta("a"),
                codex(100, 80, 10, None, "codex", "2026-09-01T12:00:00Z"),
            ],
        );
        let first = HistoryCache::at(db.clone()).load(&config);
        assert_eq!(first.report(2026, &config.providers).totals.total(), 110);
        assert!(first.read_bytes > 0);
        let mut emitted = None;
        let second = HistoryCache::at(db.clone()).load_with_cached(&config, |h| emitted = Some(h));
        assert_eq!(
            emitted
                .unwrap()
                .report(2026, &config.providers)
                .totals
                .total(),
            110
        );
        assert_eq!(second.read_bytes, 0);
        fs::remove_file(path).unwrap();
        assert_eq!(
            HistoryCache::at(db)
                .load(&config)
                .report(2026, &config.providers)
                .totals
                .total(),
            110
        );
    }
    #[test]
    fn append_cursor_preserves_model_and_dedup_state_across_restart() {
        use std::io::Write;
        let f = Fixture::new();
        let config = f.config(&["codex"]);
        let db = f.0.join("ledger.sqlite3");
        let mut metadata = meta("a");
        metadata["payload"]["cwd"] = json!("/tmp/project");
        let a = codex(100, 80, 10, None, "codex", "2026-09-01T12:00:00Z");
        let path = f.write(
            "codex/sessions/a.jsonl",
            &[
                metadata,
                json!({"type":"turn_context","payload":{"model":"gpt-test"}}),
                a.clone(),
            ],
        );
        HistoryCache::at(db.clone()).load(&config);
        let b = codex(150, 90, 20, None, "codex", "2026-09-01T12:01:00Z");
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{a}\n{b}").unwrap();
        let history = HistoryCache::at(db).load(&config);
        assert_eq!(history.report(2026, &config.providers).totals.total(), 170);
        assert_eq!(history.records.len(), 2);
        assert_eq!(history.records[1].tokens.input, 50);
        assert_eq!(history.records[1].project, "/tmp/project");
        assert_eq!(history.records[1].model, "gpt-test");
        assert_eq!(
            history.read_bytes,
            (a.to_string().len() + b.to_string().len() + 2) as u64
        );
    }
    #[test]
    fn incomplete_tail_is_deferred_until_newline_without_losing_stream_fragments() {
        use std::io::Write;
        let f = Fixture::new();
        let config = f.config(&["claude"]);
        let a = json!({"type":"assistant","cwd":"/tmp/p","sessionId":"s","timestamp":"2026-09-01T12:00:00Z","message":{"id":"m","model":"claude-test","usage":{"input_tokens":100,"output_tokens":10}}});
        let path = f.write("claude/projects/p/s.jsonl", std::slice::from_ref(&a));
        let mut cache = HistoryCache::at(f.0.join("ledger.sqlite3"));
        assert_eq!(
            cache
                .load(&config)
                .report(2026, &config.providers)
                .totals
                .total(),
            110
        );
        let mut b = a;
        b["message"]["usage"]["output_tokens"] = json!(30);
        let text = b.to_string();
        let middle = text.len() / 2;
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{}", &text[..middle]).unwrap();
        let partial = cache.load(&config);
        assert_eq!(partial.report(2026, &config.providers).totals.total(), 110);
        assert!(partial.warnings.is_empty());
        writeln!(file, "{}", &text[middle..]).unwrap();
        let completed = HistoryCache::at(f.0.join("ledger.sqlite3")).load(&config);
        assert_eq!(
            completed.report(2026, &config.providers).totals.total(),
            130
        );
        assert_eq!(completed.records[0].session, "s");
    }
    #[test]
    fn rewritten_growing_file_is_rebuilt_and_other_roots_are_isolated() {
        let f = Fixture::new();
        let config = f.config(&["codex"]);
        let db = f.0.join("ledger.sqlite3");
        let a = codex(100, 80, 10, None, "codex", "2026-09-01T12:00:00Z");
        f.write("codex/sessions/a.jsonl", &[meta("a"), a]);
        HistoryCache::at(db.clone()).load(&config);
        let b = codex(200, 80, 10, None, "codex", "2026-09-01T12:00:00Z");
        f.write(
            "codex/sessions/a.jsonl",
            &[
                meta("changed"),
                b,
                json!({"type":"turn_context","payload":{"model":"different"}}),
            ],
        );
        let mut cache = HistoryCache::at(db);
        assert_eq!(
            cache
                .load(&config)
                .report(2026, &config.providers)
                .totals
                .total(),
            210
        );
        let config = AppConfig {
            codex_dir: Some(f.0.join("different-root")),
            ..config
        };
        assert_eq!(
            cache
                .load(&config)
                .report(2026, &config.providers)
                .totals
                .total(),
            0
        );
    }
    #[test]
    fn filtered_report_spans_year_boundary_and_session_summary_uses_same_filter() {
        let f = Fixture::new();
        let config = f.config(&["claude"]);
        let line = |id: &str, timestamp: &str, model: &str| json!({"type":"assistant","cwd":"/tmp/project","sessionId":"s","timestamp":timestamp,"message":{"id":id,"model":model,"usage":{"input_tokens":100,"output_tokens":10}}});
        f.write(
            "claude/projects/p/s.jsonl",
            &[
                line("a", "2025-12-31T12:00:00Z", "model-a"),
                line("b", "2026-01-01T12:00:00Z", "model-b"),
            ],
        );
        let h = HistoryCache::memory().load(&config);
        let query = HistoryQuery {
            from: NaiveDate::from_ymd_opt(2025, 12, 30),
            to: NaiveDate::from_ymd_opt(2026, 1, 3),
            project: Some("PROJECT".into()),
            ..HistoryQuery::default()
        };
        let report = h.report_query(2026, &config.providers, &query);
        assert_eq!(report.totals.total(), 220);
        assert_eq!(report.daily.len(), 5);
        assert_eq!(report.monthly["2025-12"].total(), 110);
        let query = HistoryQuery {
            model: Some("model-b".into()),
            ..query
        };
        assert_eq!(
            h.report_query(2026, &config.providers, &query)
                .totals
                .total(),
            110
        );
        let sessions = h.session_usage(&config.providers, &query);
        assert_eq!(sessions[&("claude".into(), "s".into())].tokens.total(), 110);
    }
    #[test]
    fn grok_model_usage_is_split_once_and_prompt_ids_are_scoped_by_session() {
        let f = Fixture::new();
        let config = f.config(&["grok"]);
        let g = json!({"timestamp":1788264000,"params":{"update":{"sessionUpdate":"turn_completed","prompt_id":"1","usage":{"inputTokens":300,"outputTokens":30,"modelCalls":2,"modelUsage":{"grok-a":{"inputTokens":100,"outputTokens":10,"modelCalls":1},"grok-b":{"inputTokens":200,"outputTokens":20,"modelCalls":1}}}}}});
        f.write("grok/sessions/p/a/updates.jsonl", &[g.clone(), g.clone()]);
        f.write("grok/sessions/p/b/updates.jsonl", &[g]);
        let h = HistoryCache::memory().load(&config);
        assert_eq!(h.report(2026, &config.providers).totals.total(), 660);
        assert_eq!(h.records.len(), 4);
        assert_eq!(
            h.report_query(
                2026,
                &config.providers,
                &HistoryQuery {
                    model: Some("grok-a".into()),
                    ..Default::default()
                }
            )
            .totals
            .total(),
            220
        );
    }
}
