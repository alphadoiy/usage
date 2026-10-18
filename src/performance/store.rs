use super::*;
use rusqlite::{Connection, OpenFlags, params};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, ReadDir};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, Instant};

const MAX_LINE: usize = 1024 * 1024;
const BATCH_BYTES: u64 = 4 * 1024 * 1024;
const BATCH_FILES: usize = 64;
const DISCOVER_ENTRIES: usize = 256;
const PAGE: usize = 40;
const PARSER_VERSION: u32 = 2;
const MATCHED: &str =
    "scope=?1 AND at>=?2 AND at<=?3 AND model<>'' AND (?4 IS NULL OR upstream=?4)";

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Cursor {
    format: u32,
    offset: u64,
    len: u64,
    modified: u128,
    identity: String,
    fingerprint: Vec<u8>,
    parser: serde_json::Value,
    skip: bool,
    pending_line: bool,
}
struct Walker {
    stack: Vec<ReadDir>,
    finished: Option<Instant>,
}
impl Walker {
    fn new(home: &Path, source: RequestSource) -> Self {
        Self {
            stack: source
                .directories
                .iter()
                .filter_map(|d| fs::read_dir(home.join(d)).ok())
                .collect(),
            finished: None,
        }
    }
    fn next(&mut self, entries: &mut usize) -> Option<PathBuf> {
        while *entries > 0 {
            let Some(dir) = self.stack.last_mut() else {
                self.finished = Some(Instant::now());
                return None;
            };
            *entries -= 1;
            let Some(entry) = dir.next() else {
                self.stack.pop();
                continue;
            };
            let Ok(entry) = entry else { continue };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && self.stack.len() < 16 {
                if let Ok(dir) = fs::read_dir(entry.path()) {
                    self.stack.push(dir);
                }
            } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "jsonl") {
                return Some(entry.path());
            }
        }
        None
    }
}
#[derive(Default)]
struct Workers {
    walkers: HashMap<String, Walker>,
}
pub struct Service {
    path: PathBuf,
    workers: Mutex<Workers>,
    queries: Mutex<()>,
}
#[derive(Default, Debug)]
pub struct Batch {
    pub bytes: u64,
    pub files: u64,
    pub skipped: u64,
    pub more: bool,
}
impl Service {
    pub fn rediscover(&self, binding: &Binding) {
        if let Ok(mut workers) = self.workers.try_lock()
            && workers
                .walkers
                .get(&binding.scope())
                .is_some_and(|w| w.stack.is_empty())
        {
            workers.walkers.remove(&binding.scope());
        }
    }
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            workers: Mutex::new(Workers::default()),
            queries: Mutex::new(()),
        }
    }
    pub fn from_environment() -> Self {
        let base = std::env::var_os("XDG_DATA_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::config::home_dir().join(".local/share"));
        Self::at(base.join("usage/performance.sqlite3"))
    }
    pub(super) fn open(&self) -> Result<Connection, String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|_| "无法创建性能索引目录")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                    .map_err(|_| "无法设置性能目录权限")?;
            }
        }
        let db = Connection::open(&self.path).map_err(|_| "无法打开性能索引")?;
        db.busy_timeout(Duration::from_millis(250))
            .map_err(|_| "性能索引暂不可用")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))
                .map_err(|_| "无法设置性能索引权限")?;
        }
        db.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS perf_files(scope TEXT NOT NULL,path TEXT NOT NULL,cursor TEXT NOT NULL,checked INTEGER NOT NULL DEFAULT 0,mtime INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(scope,path));
            CREATE TABLE IF NOT EXISTS perf_calls(scope TEXT NOT NULL,id TEXT NOT NULL,session TEXT NOT NULL,turn TEXT,at INTEGER NOT NULL,model TEXT NOT NULL,upstream TEXT,output INTEGER NOT NULL,reasoning INTEGER NOT NULL,duration INTEGER,ttft INTEGER,native INTEGER NOT NULL, PRIMARY KEY(scope,id));
            CREATE INDEX IF NOT EXISTS perf_time ON perf_calls(scope,at,model);
            CREATE TABLE IF NOT EXISTS perf_gateway_runs(scope TEXT NOT NULL,run TEXT NOT NULL,owner TEXT NOT NULL,started INTEGER NOT NULL,ended INTEGER,PRIMARY KEY(scope,run));
            CREATE TABLE IF NOT EXISTS perf_gateway_sessions(scope TEXT NOT NULL,session TEXT NOT NULL,run TEXT NOT NULL,PRIMARY KEY(scope,session,run));
            CREATE TABLE IF NOT EXISTS perf_gateway_calls(scope TEXT NOT NULL,id TEXT NOT NULL,run TEXT NOT NULL,session TEXT NOT NULL,at INTEGER NOT NULL,model TEXT NOT NULL,upstream TEXT,output INTEGER NOT NULL,usage_known INTEGER NOT NULL,reasoning INTEGER NOT NULL,duration INTEGER,ttft INTEGER,decode_out INTEGER,decode_ms INTEGER,status INTEGER NOT NULL,metadata TEXT NOT NULL,PRIMARY KEY(scope,id));
            CREATE TABLE IF NOT EXISTS perf_gateway_native(scope TEXT NOT NULL,run TEXT NOT NULL,PRIMARY KEY(scope,run));
            CREATE INDEX IF NOT EXISTS perf_gateway_run_id ON perf_gateway_runs(run);
            CREATE INDEX IF NOT EXISTS perf_gateway_session_run ON perf_gateway_sessions(run,scope);
            CREATE INDEX IF NOT EXISTS perf_gateway_call_run ON perf_gateway_calls(run,session);
            CREATE INDEX IF NOT EXISTS perf_gateway_time ON perf_gateway_calls(scope,at,model);
            CREATE INDEX IF NOT EXISTS perf_files_round ON perf_files(scope,checked); CREATE INDEX IF NOT EXISTS perf_files_active ON perf_files(scope,mtime DESC);")
            .map_err(|_| "性能索引初始化失败")?;
        Ok(db)
    }

    pub(crate) fn register_gateway_run(
        &self,
        scope: &str,
        run: &str,
        owner: &str,
    ) -> Result<(), String> {
        self.open()?
            .execute(
                "INSERT OR IGNORE INTO perf_gateway_runs(scope,run,owner,started) VALUES(?1,?2,?3,?4)",
                params![scope, run, owner, chrono::Utc::now().timestamp_millis()],
            )
            .map_err(|_| "无法绑定网关运行")?;
        Ok(())
    }
    fn has_gateway_run(&self, run: &str) -> bool {
        if !self.path.exists() {
            return false;
        }
        let Ok(db) = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            return false;
        };
        let _ = db.busy_timeout(Duration::from_millis(100));
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM perf_gateway_runs WHERE run=?1)",
            [run],
            |r| r.get(0),
        )
        .unwrap_or(false)
    }
    pub(crate) fn finish_gateway_run(&self, run: &str) -> Result<(), String> {
        if !self.has_gateway_run(run) {
            return Ok(());
        }
        self.open()?
            .execute(
                "UPDATE perf_gateway_runs SET ended=?2 WHERE run=?1 AND ended IS NULL",
                params![run, chrono::Utc::now().timestamp_millis()],
            )
            .map_err(|_| "无法结束网关采集范围")?;
        Ok(())
    }
    pub(crate) fn recover_gateway_runs(&self, owner: &str) -> Result<(), String> {
        self.open()?.execute("UPDATE perf_gateway_runs SET ended=COALESCE((SELECT MAX(at+duration) FROM perf_gateway_calls AS c WHERE c.scope=perf_gateway_runs.scope AND c.run=perf_gateway_runs.run),started) WHERE ended IS NULL AND owner=?1", [owner]).map_err(|_| "无法恢复网关采集范围")?;
        Ok(())
    }
    pub(crate) fn link_gateway_session(&self, run: &str, session: &str) -> Result<(), String> {
        if !self.has_gateway_run(run) {
            return Ok(());
        }
        let mut db = self.open()?;
        let tx = db.transaction().map_err(|_| "无法关联网关会话")?;
        tx.execute("INSERT OR IGNORE INTO perf_gateway_sessions(scope,session,run) SELECT scope,?2,run FROM perf_gateway_runs WHERE run=?1", params![run,session]).map_err(|_| "无法关联网关会话")?;
        tx.execute(
            "UPDATE perf_gateway_calls SET session=?2 WHERE run=?1 AND session=''",
            params![run, session],
        )
        .map_err(|_| "无法补充网关会话")?;
        tx.commit().map_err(|_| "无法保存网关会话".into())
    }
    pub(crate) fn mark_gateway_cli_sources(
        &self,
        sources: &[(String, String)],
    ) -> Result<(), String> {
        let mut db = self.open()?;
        let tx = db.transaction().map_err(|_| "无法记录会话日志来源")?;
        for (scope, run) in sources {
            tx.execute(
                "INSERT OR IGNORE INTO perf_gateway_native(scope,run) VALUES(?1,?2)",
                params![scope, run],
            )
            .map_err(|_| "无法记录会话日志来源")?;
        }
        tx.commit().map_err(|_| "无法保存会话日志来源".into())
    }
    pub(crate) fn record_gateway(
        &self,
        scope: &str,
        upstream: &str,
        record: &crate::gateway::Record,
    ) -> Result<(), String> {
        if record.native_transport {
            return self.mark_gateway_cli_sources(&[(scope.to_owned(), record.run.clone())]);
        }
        let mut db = self.open()?;
        let tx = db.transaction().map_err(|_| "无法记录网关请求")?;
        let session = if record.session.is_empty() {
            // A launch may own several sessions: only infer when exactly one is known.
            tx.query_row("SELECT CASE WHEN COUNT(*)=1 THEN MIN(session) ELSE '' END FROM perf_gateway_sessions WHERE scope=?1 AND run=?2", params![scope,record.run], |r|r.get::<_,String>(0)).map_err(|_| "无法读取网关会话")?
        } else {
            record.session.clone()
        };
        if !session.is_empty() {
            tx.execute(
                "INSERT OR IGNORE INTO perf_gateway_sessions(scope,session,run) VALUES(?1,?2,?3)",
                params![scope, session, record.run],
            )
            .map_err(|_| "无法保存网关会话")?;
        }
        tx.execute("INSERT OR IGNORE INTO perf_gateway_calls(scope,id,run,session,at,model,upstream,output,reasoning,duration,ttft,decode_out,decode_ms,status,metadata,usage_known) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![scope,record.id,record.run,session,record.at,record.model,upstream,record.output.unwrap_or(0).min(1_000_000_000),record.reasoning.min(record.output.unwrap_or(0)),record.duration,record.ttft,record.decode.map(|v|v.0),record.decode.map(|v|v.1),record.status,serde_json::to_string(record).map_err(|_| "网关指标编码失败")?,record.output.is_some()]).map_err(|_| "无法保存网关指标")?;
        tx.commit().map_err(|_| "无法保存网关请求".into())
    }

    /// At most one bounded batch per service; never called on the terminal thread.
    pub fn refresh(
        &self,
        binding: &Binding,
        source: RequestSource,
        cancel: &AtomicBool,
    ) -> Result<Batch, String> {
        if cancelled(cancel) {
            return Ok(Batch::default());
        }
        let mut workers = match self.workers.try_lock() {
            Ok(workers) => workers,
            Err(TryLockError::WouldBlock) => {
                return Ok(Batch {
                    more: true,
                    ..Default::default()
                });
            }
            Err(TryLockError::Poisoned(_)) => return Err("性能采集状态不可用，请重启 Usage".into()),
        };
        let scope = binding.scope();
        // Bound open directory handles when users rapidly switch many providers.
        if !workers.walkers.contains_key(&scope) && workers.walkers.len() >= 4 {
            workers.walkers.clear();
        }
        let mut db = self.open()?;
        let tx = db.transaction().map_err(|_| "性能索引暂不可写")?;
        let walker = workers
            .walkers
            .entry(scope.clone())
            .or_insert_with(|| Walker::new(&binding.home, source));
        if walker
            .finished
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(30))
        {
            *walker = Walker::new(&binding.home, source);
        }
        let mut entries = DISCOVER_ENTRIES;
        while !cancelled(cancel) {
            let Some(path) = walker.next(&mut entries) else {
                break;
            };
            tx.execute(
                "INSERT OR IGNORE INTO perf_files(scope,path,cursor) VALUES(?1,?2,'{}')",
                params![scope, path.to_string_lossy()],
            )
            .map_err(|_| "无法保存日志索引")?;
        }
        let mut paths: Vec<(String, String)> = tx
            .prepare(
                "SELECT path,cursor FROM perf_files WHERE scope=?1 ORDER BY checked,path LIMIT ?2",
            )
            .and_then(|mut s| {
                s.query_map(params![scope, BATCH_FILES as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect()
            })
            .map_err(|_| "无法读取日志游标")?;
        let active: Vec<(String, String)> = tx
            .prepare(
                "SELECT path,cursor FROM perf_files WHERE scope=?1 ORDER BY mtime DESC LIMIT 8",
            )
            .and_then(|mut s| {
                s.query_map([&scope], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect()
            })
            .map_err(|_| "无法读取活跃日志游标")?;
        let mut seen = HashSet::new();
        paths = active
            .into_iter()
            .chain(paths)
            .filter(|(path, _)| seen.insert(path.clone()))
            .collect();
        let mut batch = Batch::default();
        let began = Instant::now();
        for (path, text) in paths {
            if cancelled(cancel)
                || batch.bytes >= BATCH_BYTES
                || began.elapsed() >= Duration::from_millis(100)
            {
                batch.more = true;
                break;
            }
            let path_ref = Path::new(&path);
            let meta = match fs::symlink_metadata(path_ref) {
                Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => meta,
                _ => {
                    tx.execute(
                        "DELETE FROM perf_files WHERE scope=?1 AND path=?2",
                        params![scope, path],
                    )
                    .map_err(|_| "无法更新日志目录索引")?;
                    continue;
                }
            };
            let mut cursor: Cursor = serde_json::from_str(&text).unwrap_or_default();
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |t| t.as_nanos());
            let identity = identity(&meta);
            let incompatible = cursor.format != PARSER_VERSION;
            let unchanged = !incompatible
                && cursor.len == meta.len()
                && cursor.modified == modified
                && cursor.identity == identity;
            batch.files += 1;
            if !(unchanged && (cursor.offset >= meta.len() || cursor.pending_line)) {
                if incompatible
                    || cursor.offset > meta.len()
                    || cursor.identity != identity
                    || (!unchanged
                        && fingerprint(path_ref, cursor.offset).ok().as_ref()
                            != Some(&cursor.fingerprint))
                {
                    cursor = Cursor::default();
                }
                cursor.format = PARSER_VERSION;
                let mut parser = (source.parser)(cursor.parser.clone());
                let mut file =
                    BufReader::new(File::open(path_ref).map_err(|_| "无法读取性能日志")?);
                file.seek(SeekFrom::Start(cursor.offset))
                    .map_err(|_| "无法定位日志增量")?;
                let mut line = Vec::new();
                let mut line_start = cursor.offset;
                cursor.pending_line = false;
                loop {
                    if cancelled(cancel)
                        || batch.bytes >= BATCH_BYTES
                        || began.elapsed() >= Duration::from_millis(100)
                    {
                        batch.more = true;
                        if !cursor.skip {
                            cursor.offset = line_start;
                        }
                        break;
                    }
                    let buf = file.fill_buf().map_err(|_| "读取日志增量失败")?;
                    if buf.is_empty() {
                        if !line.is_empty() && !cursor.skip {
                            cursor.offset = line_start;
                            cursor.pending_line = true;
                        }
                        break;
                    }
                    let n = buf
                        .iter()
                        .position(|b| *b == b'\n')
                        .map_or(buf.len(), |i| i + 1)
                        .min((BATCH_BYTES - batch.bytes) as usize);
                    let ended = buf[n - 1] == b'\n';
                    if !cursor.skip {
                        if line.len() + n > MAX_LINE {
                            cursor.skip = true;
                            line.clear();
                            batch.skipped += 1;
                        } else {
                            line.extend_from_slice(&buf[..n]);
                        }
                    }
                    file.consume(n);
                    cursor.offset += n as u64;
                    batch.bytes += n as u64;
                    if ended {
                        if !cursor.skip {
                            match parser.feed(&line) {
                                Ok(calls) => {
                                    for call in calls.into_iter().filter(Observation::valid) {
                                        save(&tx, &scope, &call)?;
                                    }
                                }
                                Err(_) => batch.skipped += 1,
                            }
                        }
                        line.clear();
                        cursor.skip = false;
                        line_start = cursor.offset;
                    }
                }
                cursor.parser = parser.checkpoint();
                cursor.fingerprint =
                    fingerprint(path_ref, cursor.offset).map_err(|_| "无法核对日志游标")?;
                cursor.len = meta.len();
                cursor.modified = modified;
                cursor.identity = identity;
                batch.more |= cursor.offset < meta.len() && !cursor.pending_line;
            }
            tx.execute(
                "UPDATE perf_files SET cursor=?3,checked=?4,mtime=?5 WHERE scope=?1 AND path=?2",
                params![
                    scope,
                    path,
                    serde_json::to_string(&cursor).map_err(|_| "无法编码日志游标")?,
                    chrono::Utc::now().timestamp_micros(),
                    (modified / 1_000_000) as i64
                ],
            )
            .map_err(|_| "保存日志游标失败")?;
        }
        batch.more |= !walker.stack.is_empty();
        // Continue a round-robin pass when a Home has more files than one batch.
        let left: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM perf_files WHERE scope=?1 AND checked=0)",
                [&scope],
                |r| r.get(0),
            )
            .unwrap_or(false);
        batch.more |= left;
        tx.commit().map_err(|_| "保存性能索引失败")?;
        Ok(batch)
    }

    /// Cheap homepage read: one aggregate query, no creation, discovery or log reads.
    pub fn summary(
        &self,
        binding: &Binding,
        period: Period,
        now: i64,
    ) -> Result<Option<Summary>, String> {
        if !self.path.try_exists().map_err(|_| "无法检查性能索引")? {
            return Ok(None);
        }
        let db = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| "无法读取性能索引")?;
        db.busy_timeout(Duration::from_millis(250))
            .map_err(|_| "性能索引暂不可用")?;
        summary(&db, binding, period, now).map(Some)
    }

    pub fn snapshot(
        &self,
        binding: &Binding,
        period: Period,
        model: Option<&str>,
        page: u32,
        now: i64,
    ) -> Result<Snapshot, String> {
        let _query = self
            .queries
            .try_lock()
            .map_err(|_| "性能统计处理中，请稍后刷新")?;
        let mut connection = self.open()?;
        // Keep counts, means and detail pages on the same WAL read snapshot.
        let db = connection.transaction().map_err(|_| "性能统计暂不可读")?;
        let scope = binding.scope();
        let since = now - period.days() * 86_400_000;
        let mut result = Snapshot {
            summary: summary(&db, binding, period, now)?,
            native_transport:db.query_row("SELECT EXISTS(SELECT 1 FROM perf_gateway_native n JOIN perf_gateway_runs r ON r.scope=n.scope AND r.run=n.run WHERE n.scope=?1 AND r.started<=?3 AND (r.ended IS NULL OR r.ended>=?2))",params![scope,since,now],|r|r.get(0)).map_err(|_| "无法读取原生传输来源")?,
            ..Default::default()
        };
        let mut models = HashMap::new();
        let measured = measured_calls(&binding.agent, &db);
        let sql = format!(
            "{measured} SELECT model,{TOTALS} FROM measured_calls GROUP BY model ORDER BY model LIMIT 501"
        );
        let mut statement = db.prepare(&sql).map_err(|_| "性能统计查询失败")?;
        let rows = statement
            .query_map(params![scope, since, now, binding.expected_upstream], |r| {
                Ok(ModelStats {
                    model: r.get(0)?,
                    calls: r.get(1)?,
                    mean_ttft_ms: r.get(2)?,
                    timed_samples: r.get(3)?,
                    speed_tokens: r.get(4)?,
                    speed_ms: r.get(5)?,
                    speed_samples: r.get(6)?,
                    floor_tokens: r.get(7)?,
                    floor_ms: r.get(8)?,
                    floor_samples: r.get(9)?,
                })
            })
            .map_err(|_| "性能统计查询失败")?;
        for row in rows {
            let row = row.map_err(|_| "性能统计数据无效")?;
            models.insert(row.model.clone(), row);
        }
        result.models = models.into_values().collect();
        result.models.sort_by(|a, b| a.model.cmp(&b.model));
        result.models_truncated = result.models.len() > 500;
        result.models.truncate(500);
        let unmatched = format!(
            "WITH source_calls AS ({}) SELECT COUNT(*) FROM source_calls WHERE scope=?1 AND at>=?2 AND at<=?3 AND (model='' OR (?4 IS NOT NULL AND (upstream IS NULL OR upstream<>?4)))",
            source_calls(&db)
        );
        result.unmatched = db
            .query_row(
                &unmatched,
                params![scope, since, now, binding.expected_upstream],
                |r| r.get(0),
            )
            .map_err(|_| "归属统计查询失败")?;
        result.unmatched+=db.query_row("SELECT COUNT(*) FROM perf_gateway_calls g WHERE scope=?1 AND at>=?2 AND at<=?3 AND session='' AND NOT EXISTS(SELECT 1 FROM perf_gateway_native n WHERE n.scope=g.scope AND n.run=g.run)",params![scope,since,now],|r|r.get::<_,u64>(0)).map_err(|_| "网关归属统计查询失败")?;
        let sql = format!(
            "{measured} SELECT id,session,turn,at,model,upstream,output,reasoning,duration,first_ms,native,decode_out,decode_ms,floor_out,floor_ms,output_unknown FROM measured_calls WHERE (?5 IS NULL OR model=?5) ORDER BY at DESC,id LIMIT ?6 OFFSET ?7"
        );
        let mut statement = db.prepare(&sql).map_err(|_| "请求明细查询失败")?;
        result.recent = statement
            .query_map(
                params![
                    scope,
                    since,
                    now,
                    binding.expected_upstream,
                    model,
                    PAGE as i64,
                    i64::from(page) * PAGE as i64
                ],
                |r| {
                    Ok(Observation {
                        id: r.get(0)?,
                        session: r.get(1)?,
                        turn: r.get(2)?,
                        at: r.get(3)?,
                        model: r.get(4)?,
                        upstream: r.get(5)?,
                        output: r.get(6)?,
                        output_unknown: r.get(15)?,
                        reasoning: r.get(7)?,
                        duration_ms: r.get(8)?,
                        ttft_ms: r.get(9)?,
                        native_timing: r.get::<_, i64>(10)? == 1,
                        gateway: r.get::<_, i64>(10)? == 2,
                        generated: rate_sample(r.get(11)?, r.get(12)?),
                        lower_bound: rate_sample(r.get(13)?, r.get(14)?),
                    })
                },
            )
            .map_err(|_| "请求明细查询失败")?
            .collect::<Result<_, _>>()
            .map_err(|_| "请求明细数据无效")?;
        Ok(result)
    }
}
// Magpie usage/usage.go DecodeWindow and usage/ledger.go Row.Decode/Row.Floor,
// CLI rules use session-log evidence; gateway rows carry bounded, directly observed decode evidence.
// SQL keeps large histories aggregated without loading every request into memory.
const TOTALS: &str = "COUNT(*),CAST(AVG(first_ms) AS INTEGER),COUNT(first_ms),COALESCE(SUM(decode_out),0),COALESCE(SUM(decode_ms),0),COUNT(decode_ms),COALESCE(SUM(floor_out),0),COALESCE(SUM(floor_ms),0),COUNT(floor_ms)";
fn has_gateway(db: &Connection) -> bool {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='perf_gateway_calls')", [], |r|r.get(0)).unwrap_or(false)
}
fn source_calls(db: &Connection) -> String {
    if !has_gateway(db) {
        return "SELECT *,NULL AS gw_tokens,NULL AS gw_ms,0 AS output_unknown FROM perf_calls"
            .into();
    }
    let native: bool=db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='perf_gateway_native')",[],|r|r.get(0)).unwrap_or(false);
    let cli_native = if native {
        "AND NOT EXISTS(SELECT 1 FROM perf_gateway_native n WHERE n.scope=lifetime.scope AND n.run=lifetime.run)"
    } else {
        ""
    };
    let gateway_native = if native {
        "AND NOT EXISTS(SELECT 1 FROM perf_gateway_native n WHERE n.scope=g.scope AND n.run=g.run)"
    } else {
        ""
    };
    format!("SELECT scope,id,session,turn,at,model,upstream,output,reasoning,duration,ttft,native,NULL AS gw_tokens,NULL AS gw_ms,0 AS output_unknown FROM perf_calls AS cli
         WHERE NOT EXISTS(SELECT 1 FROM perf_gateway_sessions AS own JOIN perf_gateway_runs AS lifetime ON lifetime.scope=own.scope AND lifetime.run=own.run WHERE EXISTS(SELECT 1 FROM perf_gateway_calls AS observed WHERE observed.scope=lifetime.scope AND observed.run=lifetime.run) {cli_native} AND own.scope=cli.scope AND own.session=cli.session AND cli.at>=lifetime.started AND (lifetime.ended IS NULL OR cli.at<=lifetime.ended))
         UNION ALL SELECT scope,id,session,NULL,at,model,upstream,output,reasoning,duration,ttft,2,decode_out,decode_ms,usage_known=0 FROM perf_gateway_calls g WHERE session<>'' {gateway_native}")
}
fn measured_calls(agent: &str, db: &Connection) -> String {
    let codex = i32::from(agent == "codex");
    let floors = i32::from(matches!(agent, "codex" | "claude" | "claude-desktop"));
    let rows = source_calls(db);
    format!("WITH source_calls AS ({rows}), base_calls AS (
        SELECT *,CASE WHEN native IN (1,2) AND ttft>0 AND ttft<=duration THEN ttft END AS first_ms
        FROM source_calls WHERE {MATCHED}
    ), windows AS (
        SELECT *,duration-first_ms AS window_ms,output-reasoning AS answer_tokens FROM base_calls
    ), eligible AS (
        SELECT *,
            (native=2 AND gw_tokens>0 AND gw_ms>=100) OR (native<>2 AND first_ms IS NOT NULL AND reasoning=0 AND output>0
                AND window_ms>=100 AND output*1000<=10000*window_ms
                AND ({codex}=0 OR (window_ms>=1000 AND output>=100))) AS decoded,
            {codex}=1 AND first_ms IS NOT NULL AND reasoning>0
                AND answer_tokens>=100 AND window_ms>=1000
                AND answer_tokens*1000<=10000*window_ms AS answer_floor,
            output>=1000 AND duration>=1000 AND output*1000<=10000*duration AS whole_floor
        FROM windows
    ), measured_calls AS (
        SELECT *,
            CASE WHEN decoded THEN CASE WHEN native=2 THEN gw_tokens ELSE output END END AS decode_out,
            CASE WHEN decoded THEN CASE WHEN native=2 THEN gw_ms ELSE window_ms END END AS decode_ms,
            CASE WHEN native<>2 AND {floors}=1 AND COALESCE(decoded,0)=0 THEN
                CASE WHEN answer_floor THEN answer_tokens WHEN whole_floor THEN output END END AS floor_out,
            CASE WHEN native<>2 AND {floors}=1 AND COALESCE(decoded,0)=0 THEN
                CASE WHEN answer_floor THEN window_ms WHEN whole_floor THEN duration END END AS floor_ms
        FROM eligible
    )")
}
fn rate_sample(tokens: Option<u64>, ms: Option<i64>) -> Option<RateSample> {
    Some(RateSample {
        tokens: tokens?,
        ms: ms?,
    })
}

fn summary(
    db: &Connection,
    binding: &Binding,
    period: Period,
    now: i64,
) -> Result<Summary, String> {
    let measured = measured_calls(&binding.agent, db);
    let sql = format!("{measured} SELECT {TOTALS} FROM measured_calls");
    db.query_row(
        &sql,
        params![
            binding.scope(),
            now - period.days() * 86_400_000,
            now,
            binding.expected_upstream
        ],
        |r| {
            Ok(Summary {
                calls: r.get(0)?,
                mean_ttft_ms: r.get(1)?,
                timed_samples: r.get(2)?,
                speed_tokens: r.get(3)?,
                speed_ms: r.get(4)?,
                speed_samples: r.get(5)?,
                floor_tokens: r.get(6)?,
                floor_ms: r.get(7)?,
                floor_samples: r.get(8)?,
            })
        },
    )
    .map_err(|_| "性能摘要查询失败".into())
}
pub(super) fn save(db: &Connection, scope: &str, c: &Observation) -> Result<(), String> {
    db.prepare_cached("INSERT INTO perf_calls(scope,id,session,turn,at,model,upstream,output,reasoning,duration,ttft,native) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
        ON CONFLICT(scope,id) DO UPDATE SET at=MAX(at,excluded.at),model=excluded.model,upstream=COALESCE(excluded.upstream,upstream),output=MAX(output,excluded.output),reasoning=MAX(reasoning,excluded.reasoning),duration=CASE WHEN excluded.native>=native THEN COALESCE(excluded.duration,duration) ELSE duration END,ttft=CASE WHEN excluded.native>=native THEN COALESCE(excluded.ttft,ttft) ELSE ttft END,native=MAX(native,excluded.native)").map_err(|_| "无法准备性能写入")?.execute(params![scope,c.id,c.session,c.turn,c.at,c.model,c.upstream,c.output,c.reasoning,c.duration_ms,c.ttft_ms,c.native_timing]).map_err(|_| "保存性能请求失败")?;
    Ok(())
}
fn identity(meta: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        format!("{:?}", meta.created().ok())
    }
}
fn fingerprint(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    if offset == 0 {
        return Ok(Vec::new());
    }
    let mut f = File::open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut f)
        .take(offset.min(1024))
        .read_to_end(&mut bytes)?;
    f.seek(SeekFrom::Start(offset.saturating_sub(1024)))?;
    f.take(offset.min(1024)).read_to_end(&mut bytes)?;
    // Persist only a digest: the boundary may contain conversation or credentials.
    Ok(Sha256::digest(bytes).to_vec())
}
