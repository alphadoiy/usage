//! Local token history. Quota percentages cannot reconstruct daily consumption.
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::AppConfig;

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
    #[serde(default)]
    pub instance: String,
    pub id: String,
    pub time: DateTime<Utc>,
    pub tokens: Tokens,
    pub session: String,
    pub project: String,
    pub model: String,
}
impl Record {
    pub fn session_key(&self) -> crate::sessions::SessionKey {
        crate::sessions::SessionKey {
            instance: self.instance.clone(),
            provider: self.source.clone(),
            id: self.session.clone(),
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct History {
    pub days: BTreeMap<NaiveDate, BTreeMap<String, Tokens>>,
    pub warnings: Vec<String>,
    pub instances: Vec<String>,
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
    pub by_instance: BTreeMap<String, Tokens>,
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
    pub instance: Option<String>,
}
impl HistoryQuery {
    pub fn contains_date(&self, date: NaiveDate) -> bool {
        self.from.is_none_or(|from| date >= from) && self.to.is_none_or(|to| date <= to)
    }
    pub fn matches(&self, record: &Record) -> bool {
        self.contains_date(record.time.with_timezone(&Local).date_naive())
            && self
                .instance
                .as_ref()
                .is_none_or(|id| *id == record.instance)
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
            instances: self.instances.clone(),
            scanned_files: self.scanned_files,
            read_bytes: self.read_bytes,
            synced_at: self.synced_at,
            ..Self::default()
        };
        if self.records.is_empty()
            && query.project.is_none()
            && query.model.is_none()
            && query.instance.is_none()
        {
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
    ) -> HashMap<crate::sessions::SessionKey, SessionUsage> {
        let mut sessions = HashMap::<crate::sessions::SessionKey, SessionUsage>::new();
        for record in self
            .records
            .iter()
            .filter(|r| sources.contains(&r.source) && query.matches(r))
        {
            let usage = sessions.entry(record.session_key()).or_default();
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
            report.by_instance.clear();
            for record in filtered
                .records
                .iter()
                .filter(|r| sources.contains(&r.source))
            {
                report
                    .by_instance
                    .entry(record.instance.clone())
                    .or_default()
                    .add(record.tokens);
            }
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
            by_instance: BTreeMap::new(),
            warnings: self.warnings.clone(),
            synced_at: self.synced_at,
            scanned_files: self.scanned_files,
            read_bytes: self.read_bytes,
            from: None,
            to: None,
        };
        for record in self
            .records
            .iter()
            .filter(|r| r.time.with_timezone(&Local).year() == year && sources.contains(&r.source))
        {
            report
                .by_instance
                .entry(record.instance.clone())
                .or_default()
                .add(record.tokens);
        }
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
#[serde(default)]
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
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct CachedFile {
    source: String,
    len: u64,
    modified: Option<SystemTime>,
    offset: u64,
    /// SHA-256 of the file's boundary bytes. Older ledgers stored the bytes themselves.
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
            Self::at(crate::paths::data_dir().join("history.sqlite3"))
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
        let mut history = History::default();
        let mut paths = Vec::new();
        let roots = history_roots(config);
        for (source, id, root) in &roots {
            collect_files(
                root,
                source,
                (source == "grok").then_some("updates.jsonl"),
                &mut paths,
                &mut history.warnings,
            );
            if !root.exists() && config.instances.iter().any(|i| i.id == *id) {
                history.warnings.push(format!("实例 {id}: 数据目录不存在"));
            }
        }
        for source in &config.providers {
            if !SOURCES.contains(&source.as_str()) {
                history
                    .warnings
                    .push(format!("{source}: 没有历史日志读取器"));
            } else if !paths.iter().any(|(_, s)| s == source) {
                history
                    .warnings
                    .push(format!("{source}: 未找到本地历史日志"));
            }
        }
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
    roots: &[(String, String, PathBuf)],
    mut history: History,
) -> History {
    history.instances = roots.iter().map(|(_, id, _)| id.clone()).collect();
    history.instances.sort();
    history.instances.dedup();
    let mut unique: HashMap<(String, String, String), Record> = HashMap::new();
    let mut malformed = 0;
    for (path, entry) in files {
        let Some((_, instance, _)) = roots
            .iter()
            .find(|(source, _, root)| *source == entry.source && path.starts_with(root))
        else {
            continue;
        };
        malformed += entry.malformed;
        for record in &entry.records {
            let mut record = record.clone();
            record.instance.clone_from(instance);
            unique
                .entry((
                    record.instance.clone(),
                    record.source.clone(),
                    record.id.clone(),
                ))
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
fn history_roots(config: &AppConfig) -> Vec<(String, String, PathBuf)> {
    let mut roots = Vec::new();
    for instance in config
        .data_instances()
        .into_iter()
        .filter(|i| config.providers.contains(&i.provider))
    {
        let suffixes: &[&str] = match instance.provider.as_str() {
            "codex" => &["sessions", "archived_sessions"],
            "claude" => &["projects"],
            "grok" => &["sessions"],
            _ => &[],
        };
        for suffix in suffixes {
            roots.push((
                instance.provider.clone(),
                instance.id.clone(),
                instance.path.join(suffix),
            ));
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
    let mut scrubbed = Vec::new();
    for row in rows {
        let (raw_path, data) = row.map_err(|e| e.to_string())?;
        // One undecodable row must not disable the whole ledger; it is left in place and skipped.
        let mut entry: CachedFile = match serde_json::from_str(&data) {
            Ok(entry) => entry,
            Err(_) => {
                tracing::warn!(event = "history_row_skipped", reason = "undecodable");
                continue;
            }
        };
        // Older ledgers kept raw log bytes, which may include conversation text. Their digest
        // equals what `fingerprint` now computes, so the file needs no reparse.
        if entry.fingerprint.len() != FINGERPRINT_LEN {
            entry.fingerprint = Sha256::digest(&entry.fingerprint).to_vec();
            scrubbed.push((raw_path.clone(), entry.clone()));
        }
        let path = crate::config::resolved_data_path(Path::new(&raw_path));
        files
            .entry(path)
            .and_modify(|existing: &mut CachedFile| {
                if entry.modified >= existing.modified {
                    *existing = entry.clone();
                }
            })
            .or_insert(entry);
    }
    drop(stmt);
    let rewrite = !scrubbed.is_empty();
    for (path, entry) in scrubbed {
        if let Ok(data) = serde_json::to_string(&entry) {
            // Best effort: a failed rewrite is retried on the next load.
            let _ = db.execute(
                "UPDATE history_files SET data=?1 WHERE path=?2",
                rusqlite::params![data, path],
            );
        }
    }
    // Freed pages would still hold the old bytes; compact once, outside any transaction.
    if rewrite && db.is_autocommit() {
        let _ = db.execute_batch("VACUUM");
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
const FINGERPRINT_LEN: usize = 32;
/// Identifies the indexed prefix by a digest of its first and last 2 KiB; the bytes themselves
/// can hold conversation text and are never stored.
fn fingerprint(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(offset.min(2048))
        .read_to_end(&mut bytes)?;
    file.seek(SeekFrom::Start(offset.saturating_sub(2048)))?;
    file.take(offset.min(2048)).read_to_end(&mut bytes)?;
    Ok(Sha256::digest(&bytes).to_vec())
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
                                instance: source.into(),
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
                    instance: source.into(),
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
    fn legacy_raw_fingerprints_are_scrubbed_and_a_bad_row_does_not_disable_the_ledger() {
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
        HistoryCache::at(db.clone()).load(&config);
        // Rewrite the row as an older ledger did: the boundary bytes themselves.
        let content = fs::read(&path).unwrap();
        let raw: Vec<u8> = content.iter().chain(content.iter()).copied().collect();
        let conn = rusqlite::Connection::open(&db).unwrap();
        let (key, data): (String, String) = conn
            .query_row("SELECT path,data FROM history_files", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let mut row: Value = serde_json::from_str(&data).unwrap();
        row["fingerprint"] = json!(raw);
        conn.execute(
            "UPDATE history_files SET data=?1 WHERE path=?2",
            rusqlite::params![row.to_string(), key],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO history_files VALUES('/gone/broken.jsonl','not json')",
            [],
        )
        .unwrap();
        drop(conn);
        let loaded = HistoryCache::at(db.clone()).load(&config);
        assert_eq!(loaded.report(2026, &config.providers).totals.total(), 110);
        assert_eq!(
            loaded.read_bytes, 0,
            "the legacy fingerprint still matches; no reparse"
        );
        let data: String = rusqlite::Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT data FROM history_files WHERE path=?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap();
        let stored: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(
            stored["fingerprint"].as_array().unwrap().len(),
            FINGERPRINT_LEN
        );
        assert!(!data.contains("session_meta"));
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
        assert_eq!(
            sessions[&crate::sessions::SessionKey {
                instance: "claude".into(),
                provider: "claude".into(),
                id: "s".into(),
            }]
                .tokens
                .total(),
            110
        );
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
    #[test]
    fn same_session_id_is_isolated_by_instance_and_legacy_cache_is_readable() {
        let f = Fixture::new();
        let mut config = f.config(&["codex"]);
        let a = codex(100, 80, 10, None, "codex", "2026-09-01T12:00:00Z");
        f.write("codex/sessions/a.jsonl", &[meta("same-session"), a]);
        f.write(
            "work/sessions/a.jsonl",
            &[
                meta("same-session"),
                codex(200, 80, 20, None, "codex", "2026-09-01T12:00:00Z"),
            ],
        );
        config.instances.push(crate::config::DataInstance {
            id: "work".into(),
            provider: "codex".into(),
            path: f.0.join("work"),
        });
        let dbpath = f.0.join("ledger.sqlite3");
        let h = HistoryCache::at(dbpath.clone()).load(&config);
        assert_eq!(h.report(2026, &config.providers).totals.total(), 330);
        assert_eq!(
            h.report(2026, &config.providers).by_instance["work"].total(),
            220
        );
        let usage = h.session_usage(&config.providers, &HistoryQuery::default());
        assert_eq!(usage.len(), 2);
        for (instance, total) in [("codex", 110), ("work", 220)] {
            assert_eq!(
                usage[&crate::sessions::SessionKey {
                    instance: instance.into(),
                    provider: "codex".into(),
                    id: "same-session".into(),
                }]
                    .tokens
                    .total(),
                total
            );
        }
        let query = HistoryQuery {
            instance: Some("work".into()),
            ..Default::default()
        };
        assert_eq!(
            h.report_query(2026, &config.providers, &query)
                .totals
                .total(),
            220
        );
        assert_eq!(h.session_usage(&config.providers, &query).len(), 1);
        // Recreate the previous ledger representation without an instance field.
        let db = open_database(&dbpath).unwrap();
        // Use the stored SQL keys, not normalized filesystem paths, and verify
        // that this fixture really replaces every row with the legacy format.
        let rows = db
            .prepare("SELECT path,data FROM history_files")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        for (path, data) in rows {
            let mut value: Value = serde_json::from_str(&data).unwrap();
            let records = value["records"].as_array_mut().unwrap();
            assert!(!records.is_empty());
            for record in records {
                assert!(record.as_object_mut().unwrap().remove("instance").is_some());
            }
            let changed = db
                .execute(
                    "UPDATE history_files SET data=?1 WHERE path=?2",
                    rusqlite::params![value.to_string(), path],
                )
                .unwrap();
            assert_eq!(changed, 1);
        }
        drop(db);
        // Deleted logs cannot conceal a decoding failure by being reparsed.
        fs::remove_dir_all(f.0.join("codex")).unwrap();
        fs::remove_dir_all(f.0.join("work")).unwrap();
        let h = HistoryCache::at(dbpath).load(&config);
        assert_eq!(h.read_bytes, 0);
        assert_eq!(h.report(2026, &config.providers).totals.total(), 330);
        assert!(!h.warnings.iter().any(|w| w.contains("读取历史账本失败")));
        assert_eq!(
            h.report_query(2026, &config.providers, &query)
                .totals
                .total(),
            220
        );
    }
    #[cfg(unix)]
    #[test]
    fn deleted_legacy_log_under_symlink_keeps_its_history() {
        let f = Fixture::new();
        let config = f.config(&["codex"]);
        let path = f.write(
            "codex/sessions/a.jsonl",
            &[
                meta("a"),
                codex(100, 80, 10, None, "codex", "2026-09-01T12:00:00Z"),
            ],
        );
        let alias = f.0.join("alias");
        std::os::unix::fs::symlink(f.0.join("codex"), &alias).unwrap();
        let dbpath = f.0.join("ledger.sqlite3");
        HistoryCache::at(dbpath.clone()).load(&config);
        let db = open_database(&dbpath).unwrap();
        db.execute(
            "UPDATE history_files SET path=?1",
            [alias.join("sessions/a.jsonl").to_string_lossy()],
        )
        .unwrap();
        drop(db);
        fs::remove_file(path).unwrap();
        let config = AppConfig {
            codex_dir: Some(alias),
            ..config
        };
        assert_eq!(
            HistoryCache::at(dbpath.clone())
                .load(&config)
                .report(2026, &config.providers)
                .totals
                .total(),
            110
        );
        fs::remove_dir_all(f.0.join("codex")).unwrap();
        assert_eq!(
            HistoryCache::at(dbpath)
                .load(&config)
                .report(2026, &config.providers)
                .totals
                .total(),
            110
        );
    }
}
