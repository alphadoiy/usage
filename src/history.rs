//! Local token history. Quota percentages cannot reconstruct daily consumption.
mod ledger;
mod query;
mod sources;
use ledger::{aggregate_database, open_database, read_database, read_file_row, store_file};
use query::HistoryDatabase;
use sources::{append_file, collect_files, fresh_file, history_roots};

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};
use rusqlite::OptionalExtension;
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
    database: Option<HistoryDatabase>,
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
    #[serde(skip)]
    pub(crate) source_daily: BTreeMap<String, BTreeMap<String, Tokens>>,
    #[serde(skip)]
    pub(crate) source_monthly: BTreeMap<String, BTreeMap<String, Tokens>>,
}
impl HistoryReport {
    pub(crate) fn source_day(&self, source: &str, day: &str) -> Tokens {
        self.source_daily
            .get(source)
            .and_then(|days| days.get(day))
            .copied()
            .unwrap_or_default()
    }
    pub(crate) fn source_month(&self, source: &str, month: &str) -> Tokens {
        self.source_monthly
            .get(source)
            .and_then(|months| months.get(month))
            .copied()
            .unwrap_or_default()
    }
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionUsage {
    pub tokens: Tokens,
    pub models: Vec<String>,
}
impl History {
    pub fn is_database_backed(&self) -> bool {
        self.database.is_some()
    }

    pub fn report_query_controlled(
        &self,
        year: i32,
        sources: &[String],
        query: &HistoryQuery,
        cancel: &crate::background::Cancellation,
    ) -> HistoryReport {
        if let Some(database) = &self.database {
            return self.database_report(database, year, sources, query, Some(cancel));
        }
        self.report_query(year, sources, query)
    }

    fn database_report(
        &self,
        database: &HistoryDatabase,
        year: i32,
        sources: &[String],
        query: &HistoryQuery,
        cancel: Option<&crate::background::Cancellation>,
    ) -> HistoryReport {
        database
            .report(self, year, sources, query, cancel)
            .unwrap_or_else(|error| {
                let mut fallback = History {
                    warnings: self.warnings.clone(),
                    synced_at: self.synced_at,
                    scanned_files: self.scanned_files,
                    read_bytes: self.read_bytes,
                    ..History::default()
                };
                fallback.warnings.push(format!("历史查询失败：{error}"));
                fallback.report(year, sources)
            })
    }
    pub fn filtered(&self, query: &HistoryQuery) -> Self {
        if let Some(database) = &self.database {
            let mut result = Self {
                warnings: self.warnings.clone(),
                instances: self.instances.clone(),
                scanned_files: self.scanned_files,
                read_bytes: self.read_bytes,
                synced_at: self.synced_at,
                ..Self::default()
            };
            match database.records(query) {
                Ok(records) => {
                    for record in &records {
                        result
                            .days
                            .entry(record.time.with_timezone(&Local).date_naive())
                            .or_default()
                            .entry(record.source.clone())
                            .or_default()
                            .add(record.tokens);
                    }
                    result.records = records;
                }
                Err(error) => result.warnings.push(format!("历史查询失败：{error}")),
            }
            return result;
        }
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
        if let Some(database) = &self.database {
            return database
                .session_usage(sources, query, None)
                .unwrap_or_else(|error| {
                    tracing::warn!(event = "history_query_failed", %error);
                    HashMap::new()
                });
        }
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
    pub fn session_usage_controlled(
        &self,
        sources: &[String],
        query: &HistoryQuery,
        cancel: &crate::background::Cancellation,
    ) -> Result<HashMap<crate::sessions::SessionKey, SessionUsage>, String> {
        if let Some(database) = &self.database {
            database.session_usage(sources, query, Some(cancel))
        } else {
            Ok(self.session_usage(sources, query))
        }
    }
    pub fn report_query(
        &self,
        year: i32,
        sources: &[String],
        query: &HistoryQuery,
    ) -> HistoryReport {
        if let Some(database) = &self.database {
            return self.database_report(database, year, sources, query, None);
        }
        if query == &HistoryQuery::default() {
            return self.report(year, sources);
        }
        // Aggregate into day/account buckets directly. A report never needs its own
        // copy of the matching records (which can be the entire ledger).
        let mut filtered = Self {
            warnings: self.warnings.clone(),
            synced_at: self.synced_at,
            scanned_files: self.scanned_files,
            read_bytes: self.read_bytes,
            ..Self::default()
        };
        let from = query.from.or_else(|| NaiveDate::from_ymd_opt(year, 1, 1));
        let to = query.to.or_else(|| NaiveDate::from_ymd_opt(year, 12, 31));
        let mut by_instance = BTreeMap::<String, Tokens>::new();
        if self.records.is_empty()
            && query.project.is_none()
            && query.model.is_none()
            && query.instance.is_none()
        {
            filtered.days = self
                .days
                .iter()
                .filter(|(day, _)| query.contains_date(**day))
                .map(|(day, totals)| (*day, totals.clone()))
                .collect();
        }
        for record in self
            .records
            .iter()
            .filter(|record| sources.contains(&record.source) && query.matches(record))
        {
            let day = record.time.with_timezone(&Local).date_naive();
            if from.is_none_or(|from| day < from) || to.is_none_or(|to| day > to) {
                continue;
            }
            filtered
                .days
                .entry(day)
                .or_default()
                .entry(record.source.clone())
                .or_default()
                .add(record.tokens);
            by_instance
                .entry(record.instance.clone())
                .or_default()
                .add(record.tokens);
        }
        let mut report = filtered.report(year, sources);
        report.by_instance = by_instance;
        report.from = query.from;
        report.to = query.to;
        if query.from.is_some() || query.to.is_some() {
            let (Some(from), Some(to)) = (from, to) else {
                return report;
            };
            report.daily.clear();
            report.monthly.clear();
            report.source_daily.clear();
            report.source_monthly.clear();
            report.totals = Tokens::default();
            let mut date = from;
            while date <= to {
                let tokens = filtered.day(date, sources);
                report.daily.insert(date.to_string(), tokens);
                if let Some(day) = filtered.days.get(&date) {
                    for source in sources {
                        if let Some(source_tokens) = day.get(source) {
                            report
                                .source_daily
                                .entry(source.clone())
                                .or_default()
                                .insert(date.to_string(), *source_tokens);
                            report
                                .source_monthly
                                .entry(source.clone())
                                .or_default()
                                .entry(date.format("%Y-%m").to_string())
                                .or_default()
                                .add(*source_tokens);
                        }
                    }
                }
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
        if self.database.is_some() {
            return self
                .report_query(
                    date.year(),
                    sources,
                    &HistoryQuery {
                        from: Some(date),
                        to: Some(date),
                        ..HistoryQuery::default()
                    },
                )
                .totals;
        }
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
        if self.database.is_some() {
            return self.report_query(year, sources, &HistoryQuery::default());
        }
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
            source_daily: BTreeMap::new(),
            source_monthly: BTreeMap::new(),
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
            if let Some(day) = self.days.get(&date) {
                for source in sources {
                    if let Some(source_tokens) = day.get(source) {
                        report
                            .source_daily
                            .entry(source.clone())
                            .or_default()
                            .insert(date.to_string(), *source_tokens);
                        report
                            .source_monthly
                            .entry(source.clone())
                            .or_default()
                            .entry(date.format("%Y-%m").to_string())
                            .or_default()
                            .add(*source_tokens);
                    }
                }
            }
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

    pub fn dimension_values(&self, sources: &[String], field: &str) -> Vec<String> {
        if let Some(database) = &self.database {
            return database
                .distinct(sources, field, None)
                .unwrap_or_else(|error| {
                    tracing::warn!(event = "history_query_failed", %error);
                    Vec::new()
                });
        }
        let mut values: Vec<_> = self
            .records
            .iter()
            .filter(|r| sources.contains(&r.source))
            .filter_map(|r| match field {
                "project" => Some(&r.project),
                "model" => Some(&r.model),
                "instance" => Some(&r.instance),
                _ => None,
            })
            .filter(|value| !value.is_empty())
            .cloned()
            .collect();
        values.sort();
        values.dedup();
        values
    }

    pub fn dimension_values_controlled(
        &self,
        sources: &[String],
        field: &str,
        cancel: &crate::background::Cancellation,
    ) -> Result<Vec<String>, String> {
        if let Some(database) = &self.database {
            database.distinct(sources, field, Some(cancel))
        } else {
            Ok(self.dimension_values(sources, field))
        }
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
    compressed: bool,
    len: u64,
    modified: Option<SystemTime>,
    offset: u64,
    /// SHA-256 of the file's boundary bytes. Older ledgers stored the bytes themselves.
    fingerprint: Vec<u8>,
    state: ParserState,
    records: Vec<Record>,
    /// Records live in separate rows; `records` only contains the pending append.
    split: bool,
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
    pub fn load_compact(&mut self, config: &AppConfig) -> History {
        self.load_compact_with_cached_controlled(
            config,
            &crate::background::Cancellation::new(),
            |_| {},
        )
    }
    pub fn load_with_cached(
        &mut self,
        config: &AppConfig,
        cached: impl FnOnce(History),
    ) -> History {
        self.load_with_cached_controlled(config, &crate::background::Cancellation::new(), cached)
    }
    pub fn load_with_cached_controlled(
        &mut self,
        config: &AppConfig,
        cancel: &crate::background::Cancellation,
        cached: impl FnOnce(History),
    ) -> History {
        self.load_internal(config, cancel, cached, false)
    }
    pub fn load_compact_with_cached_controlled(
        &mut self,
        config: &AppConfig,
        cancel: &crate::background::Cancellation,
        cached: impl FnOnce(History),
    ) -> History {
        self.load_internal(config, cancel, cached, true)
    }
    fn load_internal(
        &mut self,
        config: &AppConfig,
        cancel: &crate::background::Cancellation,
        cached: impl FnOnce(History),
        compact: bool,
    ) -> History {
        let mut history = History::default();
        if cancel.is_cancelled() {
            return history;
        }
        let roots = history_roots(config);
        let database = self.database.as_ref().map(|path| open_database(path));
        if let Some(Ok(db)) = &database {
            match read_database(db) {
                Ok(files) => self.files = files,
                Err(e) => {
                    history.warnings.push(format!("读取历史账本失败：{e}"));
                    return history;
                }
            }
            let synced: Option<String> = db
                .query_row(
                    "SELECT value FROM history_meta WHERE key='synced_at'",
                    [],
                    |r| r.get(0),
                )
                .ok();
            let previous = History {
                synced_at: synced
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|t| t.with_timezone(&Utc)),
                ..History::default()
            };
            let previous = if compact {
                self.compact_snapshot(&roots, previous)
            } else {
                aggregate_database(db, &self.files, &roots, previous, cancel)
            };
            if previous.synced_at.is_some() {
                cached(previous);
            }
        }
        if let Some(Err(e)) = &database {
            history.warnings.push(format!("历史账本不可用：{e}"));
        }
        if matches!(database, Some(Err(_))) || cancel.is_cancelled() {
            return history;
        }
        // Return the persisted view before walking or decoding potentially large logs.
        let mut paths = Vec::new();
        for (source, id, root) in &roots {
            collect_files(
                root,
                source,
                sources::file_name(source),
                &mut paths,
                &mut history.warnings,
                cancel,
            );
            if !root.exists() && config.instances.iter().any(|i| i.id == *id) {
                history.warnings.push(format!("实例 {id}: 数据目录不存在"));
            }
        }
        for source in &config.providers {
            if !sources::supported(source) {
                history
                    .warnings
                    .push(format!("{source}: 没有历史日志读取器"));
            } else if !paths.iter().any(|(_, s)| s == source) {
                history
                    .warnings
                    .push(format!("{source}: 未找到本地历史日志"));
            }
        }
        paths.sort();
        paths.dedup();
        for (path, source) in paths {
            if cancel.is_cancelled() {
                return History::default();
            }
            history.scanned_files += 1;
            let (physical, metadata) = match crate::logfiles::resolve(&path) {
                Ok(file) => file,
                Err(_) => {
                    history.warnings.push(format!("{source}: 无法读取日志文件"));
                    continue;
                }
            };
            let compressed = crate::logfiles::compressed(&physical);
            if self.files.get(&path).is_some_and(|e| {
                e.compressed == compressed
                    && e.len == metadata.len()
                    && e.modified == metadata.modified().ok()
            }) {
                continue;
            }
            let fallback = self
                .files
                .remove(&path)
                .unwrap_or_else(|| fresh_file(&path, &source));
            let persisted = database
                .as_ref()
                .and_then(|db| db.as_ref().ok())
                .map(|db| read_file_row(db, &path));
            let expected = match &persisted {
                Some(Ok(data)) => data.as_deref(),
                Some(Err(e)) => {
                    history.warnings.push(format!("读取历史账本失败：{e}"));
                    None
                }
                None => None,
            };
            let mut entry = expected
                .and_then(|data| serde_json::from_str(data).ok())
                .unwrap_or_else(|| fallback.clone());
            let incremental = !compressed
                && !entry.compressed
                && entry.offset <= metadata.len()
                && fingerprint(&physical, entry.offset).is_ok_and(|f| f == entry.fingerprint)
                && metadata.len() > entry.len;
            if !incremental {
                entry = fresh_file(&path, &source);
            }
            entry.compressed = compressed;
            let start = entry.offset;
            match append_file(&physical, &source, &mut entry, cancel) {
                Ok(()) => {
                    history.read_bytes += entry.offset.saturating_sub(start);
                    entry.len = metadata.len();
                    entry.modified = metadata.modified().ok();
                    if let Some(Ok(db)) = &database
                        && matches!(persisted, Some(Ok(_)))
                    {
                        match store_file(db, &path, expected, &entry) {
                            Ok(Some(winner)) => entry = winner,
                            Ok(None) => {
                                entry.records.clear();
                                entry.split = true;
                            }
                            Err(e) => {
                                entry = fallback.clone();
                                history.warnings.push(format!("保存历史账本失败：{e}"));
                            }
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
        if cancel.is_cancelled() {
            return History::default();
        }
        history.synced_at = Some(Utc::now());
        if let Some(Ok(db)) = &database {
            let result = db.execute("INSERT INTO history_meta(key,value) VALUES('synced_at',?1) ON CONFLICT(key) DO UPDATE SET value=MAX(history_meta.value,excluded.value)", [history.synced_at.unwrap().to_rfc3339()]);
            if let Err(e) = result {
                history.warnings.push(format!("保存同步时间失败：{e}"));
            }
        }
        if let Some(Ok(db)) = &database {
            if compact {
                self.compact_snapshot(&roots, history)
            } else {
                aggregate_database(db, &self.files, &roots, history, cancel)
            }
        } else {
            aggregate(&self.files, &roots, history)
        }
    }

    fn compact_snapshot(
        &self,
        roots: &[(String, String, PathBuf)],
        mut history: History,
    ) -> History {
        let Some(path) = &self.database else {
            return history;
        };
        history.instances = roots.iter().map(|(_, id, _)| id.clone()).collect();
        history.instances.sort();
        history.instances.dedup();
        let mut malformed = 0;
        let mut scope = Vec::new();
        for (file, entry) in &self.files {
            if let Some((_, instance, _)) = roots
                .iter()
                .find(|(source, _, root)| *source == entry.source && file.starts_with(root))
            {
                malformed += entry.malformed;
                scope.push((file.to_string_lossy().into_owned(), instance.clone()));
            }
        }
        if malformed > 0 {
            history
                .warnings
                .push(format!("跳过 {malformed} 条损坏或无有效时间的用量记录"));
        }
        history.database = Some(HistoryDatabase {
            path: path.clone(),
            scope,
        });
        history
    }
}
fn merge_record(unique: &mut HashMap<(String, String, String), Record>, record: Record) {
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
        .or_insert(record);
}
// Missing logs remain in the ledger. Configured roots isolate data directories.
fn aggregate(
    files: &HashMap<PathBuf, CachedFile>,
    roots: &[(String, String, PathBuf)],
    history: History,
) -> History {
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
            merge_record(&mut unique, record);
        }
    }
    finish_aggregate(unique, roots, malformed, history)
}
fn finish_aggregate(
    unique: HashMap<(String, String, String), Record>,
    roots: &[(String, String, PathBuf)],
    malformed: usize,
    mut history: History,
) -> History {
    history.instances = roots.iter().map(|(_, id, _)| id.clone()).collect();
    history.instances.sort();
    history.instances.dedup();
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
    fn compressed_history_preserves_identity_cache_and_last_good_counts() {
        let f = Fixture::new();
        let config = f.config(&["codex"]);
        let database = f.0.join("history.sqlite3");
        let path = f.write(
            "codex/sessions/a.jsonl",
            &[
                meta("a"),
                codex(100, 80, 10, None, "codex", "2026-09-01T12:00:00Z"),
            ],
        );
        let archive = path.with_extension("jsonl.zst");
        let mut cache = HistoryCache::at(database.clone());
        assert_eq!(cache.load(&config).records.len(), 1);
        fs::write(&archive, b"unfinished compression").unwrap();
        let twin = cache.load(&config);
        assert_eq!(twin.scanned_files, 1);
        assert_eq!(twin.read_bytes, 0);
        assert!(twin.warnings.is_empty());
        let content = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let damaged = cache.load(&config);
        assert_eq!(damaged.report(2026, &config.providers).totals.total(), 110);
        assert!(damaged.warnings.iter().any(|w| w.contains("保留上次统计")));
        fs::write(
            &archive,
            ruzstd::encoding::compress_to_vec(
                content.as_slice(),
                ruzstd::encoding::CompressionLevel::Fastest,
            ),
        )
        .unwrap();
        let compressed = cache.load(&config);
        assert_eq!(compressed.read_bytes, content.len() as u64);
        assert_eq!(
            compressed.report(2026, &config.providers).totals.total(),
            110
        );
        assert_eq!(cache.files.len(), 1);
        assert_eq!(HistoryCache::at(database).load(&config).read_bytes, 0);
        let fresh = HistoryCache::memory().load(&config);
        assert_eq!(fresh.report(2026, &config.providers).totals.total(), 110);
    }
    #[test]
    fn compressed_claude_and_named_grok_logs_are_discovered() {
        let f = Fixture::new();
        let claude = json!({"type":"assistant","timestamp":"2026-09-01T12:00:00Z","message":{"id":"msg","usage":{"input_tokens":10,"output_tokens":5}}});
        let grok = json!({"timestamp":1788264000,"params":{"update":{"sessionUpdate":"turn_completed","prompt_id":"prompt","usage":{"inputTokens":20,"outputTokens":10}}}});
        for (name, record) in [
            ("claude/projects/p/a.jsonl", claude),
            ("grok/sessions/p/s/updates.jsonl", grok),
        ] {
            let path = f.write(name, &[record]);
            let content = fs::read(&path).unwrap();
            fs::write(
                path.with_extension("jsonl.zst"),
                ruzstd::encoding::compress_to_vec(
                    content.as_slice(),
                    ruzstd::encoding::CompressionLevel::Fastest,
                ),
            )
            .unwrap();
            fs::remove_file(path).unwrap();
        }
        let config = f.config(&["claude", "grok"]);
        let history = HistoryCache::memory().load(&config);
        assert_eq!(history.records.len(), 2);
        assert_eq!(history.report(2026, &config.providers).totals.total(), 45);
        assert!(history.warnings.is_empty());
    }
    #[test]
    fn slower_refresh_cannot_replace_a_concurrent_ledger_update() {
        let f = Fixture::new();
        let path = f.0.join("a.jsonl");
        let db = open_database(&f.0.join("history.sqlite3")).unwrap();
        let mut old = fresh_file(&path, "codex");
        old.len = 1;
        assert!(store_file(&db, &path, None, &old).unwrap().is_none());
        let expected = read_file_row(&db, &path).unwrap().unwrap();
        let other = open_database(&f.0.join("history.sqlite3")).unwrap();
        let mut newer = old.clone();
        newer.len = 20;
        assert!(
            store_file(&other, &path, Some(&expected), &newer)
                .unwrap()
                .is_none()
        );
        old.len = 10;
        assert_eq!(
            store_file(&db, &path, Some(&expected), &old)
                .unwrap()
                .unwrap()
                .len,
            20
        );
        assert_eq!(store_file(&db, &path, None, &old).unwrap().unwrap().len, 20);
        assert_eq!(
            read_database(&db).unwrap()[&crate::config::resolved_data_path(&path)].len,
            20
        );
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
        let expected = sources::time(&json!("2024-02-29T23:30:00Z"))
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
            value["split"] = json!(false);
            value["records"] = serde_json::Value::Array(
                db.prepare("SELECT data FROM history_records WHERE path=?1 ORDER BY sequence")
                    .unwrap()
                    .query_map([&path], |r| r.get::<_, String>(0))
                    .unwrap()
                    .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
                    .collect(),
            );
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
    #[test]
    fn large_ledger_keeps_only_cursors_and_appends_without_rewriting_old_records() {
        use std::io::Write;
        let fixture = Fixture::new();
        let config = fixture.config(&["claude"]);
        let row = |id| json!({"type":"assistant","sessionId":"large","timestamp":"2026-09-01T12:00:00Z","message":{"id":format!("message-{id}"),"usage":{"input_tokens":1,"output_tokens":1}}});
        let lines: Vec<_> = (0..10_000).map(row).collect();
        let path = fixture.write("claude/projects/p/large.jsonl", &lines);
        let database = fixture.0.join("ledger.sqlite3");
        let mut cache = HistoryCache::at(database.clone());
        let first = cache.load(&config);
        assert_eq!(first.records.len(), 10_000);
        assert!(cache.files.values().all(|entry| entry.records.is_empty()));
        let db = open_database(&database).unwrap();
        let cursor_bytes: i64 = db
            .query_row("SELECT SUM(length(data)) FROM history_files", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            cursor_bytes < 4096,
            "cursor should not grow with the ledger: {cursor_bytes}"
        );
        db.execute_batch("CREATE TRIGGER no_old_record_update BEFORE UPDATE ON history_records BEGIN SELECT RAISE(ABORT,'old row updated'); END;
        CREATE TRIGGER no_old_record_delete BEFORE DELETE ON history_records BEGIN SELECT RAISE(ABORT,'old row deleted'); END;").unwrap();
        writeln!(
            fs::OpenOptions::new().append(true).open(path).unwrap(),
            "{}",
            row(10_000)
        )
        .unwrap();
        let second = cache.load(&config);
        assert!(second.warnings.is_empty(), "{:?}", second.warnings);
        assert_eq!(second.records.len(), 10_001);
        assert_eq!(second.read_bytes, row(10_000).to_string().len() as u64 + 1);
        assert!(cache.files.values().all(|entry| entry.records.is_empty()));
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM history_records", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            10_001
        );
        let compact = HistoryCache::at(database).load_compact(&config);
        assert!(compact.records.is_empty());
        assert_eq!(
            compact.report(2026, &config.providers).totals.total(),
            20_002
        );
        let cancel = crate::background::Cancellation::new();
        cancel.cancel();
        assert!(
            compact
                .database
                .as_ref()
                .unwrap()
                .report(
                    &compact,
                    2026,
                    &config.providers,
                    &HistoryQuery::default(),
                    Some(&cancel)
                )
                .is_err(),
            "a cancelled SQL aggregate should release its worker promptly"
        );
    }
    #[test]
    fn failed_record_insert_rolls_back_cursor_and_rows_together() {
        let fixture = Fixture::new();
        let config = fixture.config(&["codex"]);
        let path = fixture.write(
            "codex/sessions/a.jsonl",
            &[
                meta("a"),
                codex(100, 80, 10, None, "codex", "2026-09-01T12:00:00Z"),
            ],
        );
        let database = fixture.0.join("ledger.sqlite3");
        let original = HistoryCache::at(database.clone()).load(&config);
        let db = open_database(&database).unwrap();
        let cursor: String = db
            .query_row("SELECT data FROM history_files", [], |r| r.get(0))
            .unwrap();
        db.execute_batch("CREATE TRIGGER reject_record BEFORE INSERT ON history_records BEGIN SELECT RAISE(ABORT,'synthetic write failure'); END;").unwrap();
        fixture.write(
            "codex/sessions/a.jsonl",
            &[
                meta("a"),
                codex(300, 100, 30, None, "codex", "2026-09-01T12:00:00Z"),
            ],
        );
        let mut failed_cache = HistoryCache::at(database.clone());
        let failed = failed_cache.load(&config);
        assert!(
            failed_cache
                .files
                .values()
                .all(|entry| entry.records.is_empty())
        );
        assert!(
            failed
                .warnings
                .iter()
                .any(|w| w.contains("保存历史账本失败"))
        );
        assert_eq!(
            failed.records[0].tokens.total(),
            original.records[0].tokens.total()
        );
        assert_eq!(
            read_file_row(&db, &crate::config::resolved_data_path(&path))
                .unwrap()
                .unwrap(),
            cursor
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM history_records", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        db.execute_batch("DROP TRIGGER reject_record").unwrap();
        let retried = HistoryCache::at(database).load(&config);
        assert_eq!(retried.records[0].tokens.total(), 330);
    }
    #[test]
    fn cancelled_history_load_does_not_create_a_ledger_or_publish_data() {
        let fixture = Fixture::new();
        let path = fixture.0.join("cancelled.sqlite3");
        let token = crate::background::Cancellation::new();
        token.cancel();
        let history = HistoryCache::at(path.clone()).load_with_cached_controlled(
            &fixture.config(&["codex"]),
            &token,
            |_| panic!("cancelled consumer"),
        );
        assert!(history.synced_at.is_none());
        assert!(!path.exists());
    }
    #[cfg(unix)]
    #[test]
    fn aliased_ledger_cursor_and_records_move_together_before_append_and_rewrite() {
        use std::io::Write;
        let fixture = Fixture::new();
        let config = fixture.config(&["codex"]);
        let path = fixture.write(
            "codex/sessions/a.jsonl",
            &[
                meta("a"),
                codex(100, 80, 10, None, "codex", "2026-09-01T12:00:00Z"),
            ],
        );
        let database = fixture.0.join("ledger.sqlite3");
        HistoryCache::at(database.clone()).load(&config);
        let alias = fixture.0.join("alias");
        std::os::unix::fs::symlink(fixture.0.join("codex"), &alias).unwrap();
        let db = open_database(&database).unwrap();
        let alias = alias.join("sessions/a.jsonl");
        for table in ["history_files", "history_records"] {
            db.execute(
                &format!("UPDATE {table} SET path=?1"),
                [alias.to_string_lossy()],
            )
            .unwrap();
        }
        writeln!(
            fs::OpenOptions::new().append(true).open(&path).unwrap(),
            "{}",
            codex(150, 90, 20, None, "codex", "2026-09-01T12:01:00Z")
        )
        .unwrap();
        let appended = HistoryCache::at(database.clone()).load(&config);
        assert_eq!(
            appended
                .records
                .iter()
                .map(|r| r.tokens.total())
                .sum::<u64>(),
            170
        );
        fixture.write(
            "codex/sessions/a.jsonl",
            &[
                meta("replacement"),
                codex(30, 0, 3, None, "codex", "2026-09-01T12:00:00Z"),
            ],
        );
        let rewritten = HistoryCache::at(database).load(&config);
        assert_eq!(rewritten.records.len(), 1);
        assert_eq!(rewritten.records[0].tokens.total(), 33);
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM history_files", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn report_aggregation_matches_materialized_filter_across_dimensions_and_sources() {
        let fixture = Fixture::new();
        let config = fixture.config(&["claude"]);
        let rows: Vec<_> = (0..600).map(|i| json!({"type":"assistant", "sessionId":"session", "cwd":if i%2==0 {"/project/A"} else {"/project/B"}, "timestamp":format!("{}-{:02}-15T12:00:00Z", 2025+i/300, 1+i%12), "message":{"id":format!("m{i}"),"model":if i%3==0 {"Model-A"} else {"Model-B"}, "usage":{"input_tokens":10,"output_tokens":2}}})).collect();
        fixture.write("claude/projects/p/s.jsonl", &rows);
        let history = HistoryCache::memory().load(&config);
        for sources in [vec!["claude".into()], vec!["codex".into()]] {
            for query in [
                HistoryQuery {
                    project: Some("PROJECT/a".into()),
                    ..Default::default()
                },
                HistoryQuery {
                    model: Some("model-A".into()),
                    ..Default::default()
                },
                HistoryQuery {
                    instance: Some("claude".into()),
                    ..Default::default()
                },
                HistoryQuery {
                    project: Some("missing".into()),
                    ..Default::default()
                },
            ] {
                let expected = history.filtered(&query).report(2026, &sources);
                let actual = history.report_query(2026, &sources, &query);
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
            }
        }
        // An open date bound defaults to the selected year's edge for both totals and accounts.
        let query = HistoryQuery {
            from: NaiveDate::from_ymd_opt(2025, 11, 1),
            ..Default::default()
        };
        let report = history.report_query(2025, &config.providers, &query);
        let expected = history
            .records
            .iter()
            .filter(|r| query.matches(r) && r.time.with_timezone(&Local).year() == 2025)
            .map(|r| r.tokens.total())
            .sum::<u64>();
        assert_eq!(report.totals.total(), expected);
        assert_eq!(
            report.by_instance.values().map(|t| t.total()).sum::<u64>(),
            expected
        );
    }

    #[test]
    fn compact_database_queries_match_materialized_history() {
        let fixture = Fixture::new();
        let config = fixture.config(&["claude"]);
        let rows: Vec<_> = (0..120)
            .map(|i| {
                json!({
                    "type":"assistant", "sessionId":format!("session-{}",i%3),
                    "cwd":if i%2==0 {"/Project/Ä"} else {"/project/B"},
                    "timestamp":format!("2026-{:02}-15T12:00:00Z",1+i%12),
                    "message":{"id":format!("m{i}"),"model":if i%3==0 {"Model-A"} else {"Model-B"},
                        "usage":{"input_tokens":10+i,"output_tokens":2}}
                })
            })
            .collect();
        fixture.write("claude/projects/p/s.jsonl", &rows);
        fixture.write("claude/projects/p/t.jsonl", &[json!({
            "type":"assistant", "sessionId":"other-session", "cwd":"/other",
            "timestamp":"2026-01-14T11:00:00Z",
            "message":{"id":"m0","model":"Model-C","usage":{"input_tokens":250,"output_tokens":9}}
        })]);
        let path = fixture.0.join("ledger.sqlite3");
        let materialized = HistoryCache::at(path.clone()).load(&config);
        let mut cached = None;
        let compact = HistoryCache::at(path).load_compact_with_cached_controlled(
            &config,
            &crate::background::Cancellation::new(),
            |history| cached = Some(history),
        );
        assert!(compact.records.is_empty());
        assert!(cached.is_some_and(|history| history.records.is_empty()));
        for query in [
            HistoryQuery::default(),
            HistoryQuery {
                project: Some("/project/ä".into()),
                ..Default::default()
            },
            HistoryQuery {
                model: Some("model-a".into()),
                ..Default::default()
            },
            HistoryQuery {
                from: NaiveDate::from_ymd_opt(2026, 3, 1),
                to: NaiveDate::from_ymd_opt(2026, 5, 31),
                ..Default::default()
            },
            HistoryQuery {
                instance: Some("claude".into()),
                ..Default::default()
            },
        ] {
            let expected = materialized.report_query(2026, &config.providers, &query);
            let actual = compact.report_query(2026, &config.providers, &query);
            assert_eq!(actual.totals, expected.totals);
            assert_eq!(actual.daily, expected.daily);
            assert_eq!(actual.monthly, expected.monthly);
            assert_eq!(actual.by_instance, expected.by_instance);
            assert_eq!(actual.source_daily, expected.source_daily);
            assert_eq!(actual.source_monthly, expected.source_monthly);
            assert_eq!((actual.from, actual.to), (expected.from, expected.to));
            assert_eq!(
                compact.filtered(&query).records.len(),
                materialized.filtered(&query).records.len()
            );
            assert_eq!(
                compact.session_usage(&config.providers, &query),
                materialized.session_usage(&config.providers, &query)
            );
        }
        for field in ["project", "model", "instance"] {
            assert_eq!(
                compact.dimension_values(&config.providers, field),
                materialized.dimension_values(&config.providers, field)
            );
        }
        assert_eq!(
            compact.day(
                NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                &config.providers
            ),
            materialized.day(
                NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                &config.providers
            )
        );
    }

    #[test]
    #[ignore = "manual history query profile"]
    fn profile_100k_database_report() {
        let fixture = Fixture::new();
        fs::create_dir_all(&fixture.0).unwrap();
        let database = fixture.0.join("profile.sqlite3");
        let path = fixture.0.join("claude/projects/profile.jsonl");
        let mut db = open_database(&database).unwrap();
        let tx = db.transaction().unwrap();
        {
            let mut insert = tx
                .prepare("INSERT INTO history_records(path,data) VALUES(?1,?2)")
                .unwrap();
            for index in 0..100_000 {
                let record = Record {
                    source: "claude".into(),
                    instance: "claude".into(),
                    id: format!("message-{index}"),
                    session: format!("session-{}", index / 100),
                    project: format!("/project/{}", index % 10),
                    model: "model-a".into(),
                    time: DateTime::parse_from_rfc3339("2026-09-01T12:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                    tokens: Tokens {
                        input: 10,
                        output: 2,
                        cached: 0,
                        calls: 1,
                    },
                };
                insert
                    .execute(rusqlite::params![
                        path.to_string_lossy(),
                        serde_json::to_string(&record).unwrap()
                    ])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
        let history = History {
            database: Some(HistoryDatabase {
                path: database,
                scope: vec![(path.to_string_lossy().into_owned(), "claude".into())],
            }),
            ..History::default()
        };
        let start = std::time::Instant::now();
        let report = history.report(2026, &["claude".into()]);
        eprintln!("100k history report: {:?}", start.elapsed());
        assert_eq!(report.totals.total(), 1_200_000);
    }
}
