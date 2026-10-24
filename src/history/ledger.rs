//! Incremental SQLite ledger and transactional migration of legacy file snapshots.
use super::*;

pub(super) fn read_file_row(
    db: &rusqlite::Connection,
    path: &Path,
) -> Result<Option<String>, String> {
    db.query_row(
        "SELECT data FROM history_files WHERE path=?1",
        [path.to_string_lossy()],
        |r| r.get(0),
    )
    .optional()
    .map_err(|e| e.to_string())
}

/// Parse and serialize before taking SQLite's write lock. Compare-and-swap prevents
/// a slower refresh from replacing a row another process advanced in the meantime.
pub(super) fn store_file(
    db: &rusqlite::Connection,
    path: &Path,
    expected: Option<&str>,
    entry: &CachedFile,
) -> Result<Option<CachedFile>, String> {
    let cursor = CachedFile {
        source: entry.source.clone(),
        compressed: entry.compressed,
        len: entry.len,
        modified: entry.modified,
        offset: entry.offset,
        fingerprint: entry.fingerprint.clone(),
        state: entry.state.clone(),
        records: Vec::new(),
        split: true,
        malformed: entry.malformed,
    };
    let data = serde_json::to_string(&cursor).map_err(|e| e.to_string())?;
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    let changed = tx.execute(
        "INSERT INTO history_files(path,data) VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET data=excluded.data WHERE history_files.data=?3",
        rusqlite::params![path.to_string_lossy(), data, expected],
    ).map_err(|e| e.to_string())?;
    if changed == 0 {
        return read_file_row(&tx, path)?
            .map(|data| serde_json::from_str(&data).map_err(|e| e.to_string()))
            .transpose();
    }
    if !entry.split {
        tx.execute(
            "DELETE FROM history_records WHERE path=?1",
            [path.to_string_lossy()],
        )
        .map_err(|e| e.to_string())?;
    }
    {
        let mut insert = tx
            .prepare("INSERT INTO history_records(path,data) VALUES(?1,?2)")
            .map_err(|e| e.to_string())?;
        for record in &entry.records {
            let data = serde_json::to_string(record).map_err(|e| e.to_string())?;
            insert
                .execute(rusqlite::params![path.to_string_lossy(), data])
                .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(None)
}

/// Stream persisted records: the cache retains cursors, never another full ledger.
pub(super) fn aggregate_database(
    db: &rusqlite::Connection,
    files: &HashMap<PathBuf, CachedFile>,
    roots: &[(String, String, PathBuf)],
    mut history: History,
    cancel: &crate::background::Cancellation,
) -> History {
    let mut unique = HashMap::new();
    let mut instances: HashMap<String, Option<String>> = HashMap::new();
    let mut corrupt = 0;
    let result = (|| -> Result<(), String> {
        let mut stmt = db
            .prepare("SELECT path,data FROM history_records ORDER BY sequence")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            if cancel.is_cancelled() {
                return Err("已取消".into());
            }
            let path: String = row.get(0).map_err(|e| e.to_string())?;
            let instance = instances.entry(path.clone()).or_insert_with(|| {
                let normalized = crate::config::resolved_data_path(Path::new(&path));
                let entry = files.get(&normalized)?;
                roots
                    .iter()
                    .find(|(source, _, root)| {
                        *source == entry.source && normalized.starts_with(root)
                    })
                    .map(|(_, instance, _)| instance.clone())
            });
            let Some(instance) = instance else {
                continue;
            };
            let data: String = row.get(1).map_err(|e| e.to_string())?;
            let mut record: Record = match serde_json::from_str(&data) {
                Ok(record) => record,
                Err(_) => {
                    corrupt += 1;
                    continue;
                }
            };
            record.instance.clone_from(instance);
            merge_record(&mut unique, record);
        }
        Ok(())
    })();
    if let Err(error) = result {
        history.warnings.push(error);
        history.synced_at = None;
    }
    if corrupt > 0 {
        history
            .warnings
            .push(format!("跳过 {corrupt} 条损坏的账本记录"));
    }
    let malformed = files
        .iter()
        .filter(|(path, entry)| {
            roots
                .iter()
                .any(|(source, _, root)| *source == entry.source && path.starts_with(root))
        })
        .map(|(_, entry)| entry.malformed)
        .sum();
    finish_aggregate(unique, roots, malformed, history)
}
pub(super) fn open_database(path: &Path) -> Result<rusqlite::Connection, String> {
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
        "CREATE TABLE IF NOT EXISTS history_files(path TEXT PRIMARY KEY, data TEXT NOT NULL); CREATE TABLE IF NOT EXISTS history_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE IF NOT EXISTS history_records(sequence INTEGER PRIMARY KEY, path TEXT NOT NULL, data TEXT NOT NULL); CREATE INDEX IF NOT EXISTS history_records_path ON history_records(path);",
    )
    .map_err(|e| e.to_string())?;
    Ok(db)
}
/// Normalize legacy aliases before matching cursors to record rows. Both sides move
/// in one transaction so an append/rewrite cannot keep an older alias's statistics.
fn normalize_database_paths(db: &rusqlite::Connection) -> Result<(), String> {
    let paths = db
        .prepare("SELECT path FROM history_files")
        .map_err(|e| e.to_string())?
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    for raw in paths {
        let normalized = crate::config::resolved_data_path(Path::new(&raw));
        if normalized == Path::new(&raw) {
            continue;
        }
        let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
        let Some(data) = read_file_row(&tx, Path::new(&raw))? else {
            continue;
        };
        let entry: CachedFile = match serde_json::from_str(&data) {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let existing = read_file_row(&tx, &normalized)?;
        let keep_existing = existing
            .as_ref()
            .and_then(|data| serde_json::from_str::<CachedFile>(data).ok())
            .is_some_and(|current| current.modified >= entry.modified);
        let target = normalized.to_string_lossy();
        if !keep_existing {
            if existing.is_some() {
                tx.execute("DELETE FROM history_records WHERE path=?1", [&target])
                    .map_err(|e| e.to_string())?;
            }
            tx.execute("INSERT INTO history_files(path,data) VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET data=excluded.data", rusqlite::params![target, data]).map_err(|e| e.to_string())?;
            tx.execute(
                "UPDATE history_records SET path=?1 WHERE path=?2",
                rusqlite::params![target, raw],
            )
            .map_err(|e| e.to_string())?;
        } else {
            tx.execute("DELETE FROM history_records WHERE path=?1", [&raw])
                .map_err(|e| e.to_string())?;
        }
        tx.execute("DELETE FROM history_files WHERE path=?1", [&raw])
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
    }
    Ok(())
}
pub(super) fn read_database(
    db: &rusqlite::Connection,
) -> Result<HashMap<PathBuf, CachedFile>, String> {
    normalize_database_paths(db)?;
    let mut stmt = db
        .prepare("SELECT path,data FROM history_files")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?;
    let mut files = HashMap::new();
    let mut rewrite = false;
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
        let needs_migration = !entry.split;
        let raw_fingerprint = entry.fingerprint.len() != FINGERPRINT_LEN;
        if raw_fingerprint {
            entry.fingerprint = Sha256::digest(&entry.fingerprint).to_vec();
        }
        if needs_migration || raw_fingerprint {
            // Migrate a single file atomically; never retain all legacy record arrays.
            if let Some(winner) = store_file(db, Path::new(&raw_path), Some(&data), &entry)? {
                entry = winner;
            }
            rewrite = true;
        }
        entry.records.clear();
        entry.split = true;
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
    // Freed pages would still hold the old bytes; compact once, outside any transaction.
    if rewrite && db.is_autocommit() {
        let _ = db.execute_batch("VACUUM");
    }
    Ok(files)
}
