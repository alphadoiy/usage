//! Plan native history classification separately from locked, backed-up mutations.
use super::SessionStore;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, Read};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

mod apply;
mod pending;
#[cfg(test)]
mod tests;

type Result<T> = std::result::Result<T, String>;
const MAX_ROLLOUT: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Scan {
    All,
    Indexed,
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct ProviderHistoryReport {
    pub dry_run: bool,
    pub files: usize,
    pub index_rows: usize,
    pub deferred: usize,
    pub deferred_reasons: BTreeMap<String, usize>,
    pub backup: Option<PathBuf>,
}

impl ProviderHistoryReport {
    fn defer(&mut self, reason: &str) {
        self.deferred += 1;
        *self.deferred_reasons.entry(reason.into()).or_default() += 1;
    }
}

struct Header {
    id: String,
    provider: String,
    range: Range<usize>,
    value: Value,
}

fn third_party(provider: &str, target: &str) -> bool {
    !provider.is_empty() && provider != "openai" && provider != target
}

fn header(bytes: &[u8]) -> Option<Header> {
    let mut offset = 0;
    for line in bytes.split_inclusive(|b| *b == b'\n').take(8) {
        let start = offset;
        offset += line.len();
        let value: Value = serde_json::from_slice(line).ok()?;
        if value["type"] != "session_meta" {
            continue;
        }
        let id = value["payload"]["id"].as_str()?;
        uuid::Uuid::parse_str(id).ok()?;
        return Some(Header {
            id: id.into(),
            provider: value["payload"]["model_provider"].as_str()?.into(),
            range: start..offset,
            value,
        });
    }
    None
}

fn reader(path: &Path) -> Result<Box<dyn BufRead + Send>> {
    crate::logfiles::open(path).map_err(|_| "无法读取会话文件，原文件已保留".into())
}

fn read_header(path: &Path) -> Result<Option<Header>> {
    let mut reader = reader(path)?.take(1024 * 1024);
    let mut prefix = Vec::new();
    for _ in 0..8 {
        if reader
            .read_until(b'\n', &mut prefix)
            .map_err(|_| "无法读取会话元数据")?
            == 0
        {
            break;
        }
        if let Some(header) = header(&prefix) {
            return Ok(Some(header));
        }
    }
    Ok(None)
}

fn rollouts(directory: &Path, out: &mut BTreeSet<PathBuf>) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory).map_err(|_| "无法扫描会话目录")? {
        let entry = entry.map_err(|_| "无法读取会话目录")?;
        let kind = entry.file_type().map_err(|_| "无法读取会话文件类型")?;
        if kind.is_dir() {
            rollouts(&entry.path(), out)?;
        } else if kind.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
        {
            out.insert(entry.path());
        }
    }
    Ok(())
}

struct Index {
    path: PathBuf,
    conn: Connection,
    has_paths: bool,
}

impl Index {
    fn provider(&self, id: &str) -> Result<Option<String>> {
        provider(&self.conn, id)
    }
}

fn provider(conn: &Connection, id: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT model_provider FROM threads WHERE id=?1",
        [id],
        |row| row.get(0),
    )
    .optional()
    .map_err(|_| "无法读取会话分类".into())
}

fn indexes(directory: &Path, dry_run: bool) -> Result<Vec<Index>> {
    let mut out = Vec::new();
    for (_, path) in super::index_paths(directory).map_err(|_| "无法读取会话索引目录")? {
        let flags = if dry_run {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let conn = Connection::open_with_flags(&path, flags).map_err(|_| "无法打开原生会话索引")?;
        conn.busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| "无法准备会话索引")?;
        let columns = conn
            .prepare("PRAGMA table_info(threads)")
            .map_err(|_| "无法读取会话索引结构")?
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|_| "无法读取会话索引结构")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|_| "无法读取会话索引结构")?;
        if columns.iter().any(|c| c == "model_provider") {
            out.push(Index {
                path,
                conn,
                has_paths: columns.iter().any(|c| c == "rollout_path"),
            });
        }
    }
    Ok(out)
}

/// Native paths are data, not permission to mutate files outside the shared store.
fn indexed_path(store: &SessionStore, path: &Path) -> Result<Option<PathBuf>> {
    let relative = path
        .strip_prefix(store.root())
        .map_err(|_| "原生会话索引指向共享目录之外，迁移已停止")?;
    if !matches!(relative.components().next(), Some(Component::Normal(name)) if name == "sessions" || name == "archived_sessions")
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("原生会话索引路径无效，迁移已停止".into());
    }
    let mut current = store.root().to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err("原生会话索引路径含符号链接，迁移已停止".into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err("无法检查原生会话路径".into()),
        }
    }
    match crate::logfiles::resolve(path) {
        Ok((physical, _)) => Ok(Some(physical)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("无法定位原生会话文件".into()),
    }
}

fn discover(
    store: &SessionStore,
    indexes: &[Index],
    target: &str,
    scan: Scan,
) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();
    if matches!(scan, Scan::Indexed) && !indexes.is_empty() && indexes.iter().all(|i| i.has_paths) {
        for index in indexes {
            let mut statement = index.conn.prepare(
                "SELECT rollout_path FROM threads WHERE model_provider NOT IN ('', 'openai', ?1)",
            ).map_err(|_| "无法查询待迁移会话")?;
            let rows = statement
                .query_map([target], |row| row.get::<_, String>(0))
                .map_err(|_| "无法查询待迁移会话")?;
            for row in rows {
                let path = row.map_err(|_| "无法读取待迁移会话路径")?;
                if let Some(path) = indexed_path(store, Path::new(&path))? {
                    paths.insert(path);
                }
            }
        }
    } else {
        for name in ["sessions", "archived_sessions"] {
            rollouts(&store.root().join(name), &mut paths)?;
        }
    }
    Ok(paths)
}

struct Candidate {
    path: PathBuf,
    header: Header,
}

struct SessionPlan {
    id: String,
    files: Vec<Candidate>,
}

struct Plan {
    sessions: Vec<SessionPlan>,
    report: ProviderHistoryReport,
    retry: BTreeSet<PathBuf>,
}

fn plan(
    store: &SessionStore,
    indexes: &[Index],
    target: &str,
    scan: Scan,
    dry_run: bool,
    pending: &pending::Pending,
) -> Result<Plan> {
    let mut report = ProviderHistoryReport {
        dry_run,
        ..Default::default()
    };
    let mut sessions: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    let mut paths = discover(store, indexes, target, scan)?;
    for path in pending.paths() {
        if let Some(path) = indexed_path(store, path)? {
            paths.insert(path);
        }
    }
    let mut retry = BTreeSet::new();
    for path in paths {
        match read_header(&path) {
            Ok(Some(header)) => sessions
                .entry(header.id.clone())
                .or_default()
                .push(Candidate { path, header }),
            Ok(None) => {}
            Err(_) => {
                report.defer("unreadable");
                retry.insert(path);
            }
        }
    }
    let mut pending = Vec::new();
    for (id, files) in sessions {
        let providers = indexes
            .iter()
            .map(|i| i.provider(&id))
            .collect::<Result<Vec<_>>>()?;
        // An official classification in either source fences the entire conversation,
        // including duplicate transcripts. Ownership is never inferred from a conflict.
        if files.iter().any(|f| f.header.provider == "openai")
            || providers.iter().flatten().any(|p| p == "openai")
        {
            continue;
        }
        let changed_files = files
            .iter()
            .filter(|f| third_party(&f.header.provider, target))
            .count();
        let changed_rows = providers
            .iter()
            .flatten()
            .filter(|p| third_party(p, target))
            .count();
        if changed_files == 0 && changed_rows == 0 {
            continue;
        }
        if dry_run {
            report.files += changed_files;
            report.index_rows += changed_rows;
        } else {
            pending.push(SessionPlan { id, files });
        }
    }
    Ok(Plan {
        sessions: pending,
        report,
        retry,
    })
}

pub(super) fn unify(
    store: &SessionStore,
    target: &str,
    backups: &Path,
    dry_run: bool,
    scan: Scan,
) -> Result<ProviderHistoryReport> {
    if !store.root().is_dir() {
        return Ok(ProviderHistoryReport {
            dry_run,
            ..Default::default()
        });
    }
    let pending = pending::Pending::snapshot(store, dry_run)?;
    let scan = pending.scan(scan, target);
    let mut indexes = indexes(store.index_dir(), dry_run)?;
    // Discovery and classification never hold the native writer coordination lock.
    let Plan {
        sessions,
        mut report,
        mut retry,
    } = plan(store, &indexes, target, scan, dry_run, &pending)?;
    let pending = if dry_run {
        pending
    } else {
        let paths = sessions
            .iter()
            .flat_map(|s| s.files.iter().map(|f| f.path.clone()));
        pending.stage(store, retry.iter().cloned().chain(paths).collect())?
    };
    let mut backup = apply::Backup::new(backups, store, target);
    for session in sessions {
        let paths: Vec<_> = session.files.iter().map(|f| f.path.clone()).collect();
        match apply::session(store, &mut indexes, session, target, &mut backup)? {
            apply::Outcome::Applied { files, index_rows } => {
                report.files += files;
                report.index_rows += index_rows;
            }
            apply::Outcome::Deferred(reason) => {
                report.defer(reason);
                retry.extend(paths);
            }
            apply::Outcome::Unchanged => {}
        }
    }
    if !dry_run {
        pending.finish(store, target, retry)?;
    }
    report.backup = backup.created_path();
    Ok(report)
}

/// The attachment protocol already holds native coordination while scheduling work.
pub(super) fn schedule_full_scan(store: &SessionStore) -> Result<()> {
    pending::require_scan(store)
}
