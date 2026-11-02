//! Per-conversation mutation protocol. A prepared journal precedes every write;
//! SQLite changes stay uncommitted until all transcript replacements succeed.
use super::*;
use rusqlite::TransactionBehavior;
use std::collections::HashSet;
use std::fs::{File, FileTimes, Metadata};

pub(super) enum Outcome {
    Applied { files: usize, index_rows: usize },
    Deferred(&'static str),
    Unchanged,
}

struct FileChange {
    path: PathBuf,
    original_provider: String,
    metadata: Metadata,
    replacement: Option<Vec<u8>>,
}

fn prepare(candidate: Candidate, target: &str) -> std::result::Result<FileChange, &'static str> {
    let metadata = fs::metadata(&candidate.path).map_err(|_| "unreadable")?;
    let replacement = if third_party(&candidate.header.provider, target) {
        if metadata.len() > MAX_ROLLOUT {
            return Err("too_large");
        }
        let mut bytes = Vec::new();
        reader(&candidate.path)
            .map_err(|_| "unreadable")?
            .take(MAX_ROLLOUT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "unreadable")?;
        if bytes.len() as u64 > MAX_ROLLOUT {
            return Err("too_large");
        }
        let mut current = header(&bytes).ok_or("conflict")?;
        if current.id != candidate.header.id || current.provider != candidate.header.provider {
            return Err("conflict");
        }
        current.value["payload"]["model_provider"] = Value::String(target.into());
        let mut next = bytes[..current.range.start].to_vec();
        next.extend(serde_json::to_vec(&current.value).map_err(|_| "unreadable")?);
        if bytes[current.range.clone()].ends_with(b"\r\n") {
            next.extend_from_slice(b"\r\n");
        } else if bytes[current.range.clone()].ends_with(b"\n") {
            next.push(b'\n');
        }
        next.extend_from_slice(&bytes[current.range.end..]);
        Some(if crate::logfiles::compressed(&candidate.path) {
            ruzstd::encoding::compress_to_vec(
                next.as_slice(),
                ruzstd::encoding::CompressionLevel::Fastest,
            )
        } else {
            next
        })
    } else {
        let current = read_header(&candidate.path)
            .map_err(|_| "unreadable")?
            .ok_or("conflict")?;
        if current.id != candidate.header.id || current.provider != candidate.header.provider {
            return Err("conflict");
        }
        None
    };
    Ok(FileChange {
        path: candidate.path,
        original_provider: candidate.header.provider,
        metadata,
        replacement,
    })
}

pub(super) struct Backup {
    path: PathBuf,
    store: PathBuf,
    target: String,
    created: bool,
    indexes: HashSet<PathBuf>,
}

impl Backup {
    pub(super) fn new(backups: &Path, store: &SessionStore, target: &str) -> Self {
        Self {
            path: backups.join(format!(
                "{}-{}",
                chrono::Utc::now().format("%Y%m%d-%H%M%S"),
                uuid::Uuid::new_v4()
            )),
            store: store.root().into(),
            target: target.into(),
            created: false,
            indexes: HashSet::new(),
        }
    }

    pub(super) fn created_path(&self) -> Option<PathBuf> {
        self.created.then(|| self.path.clone())
    }

    fn rollout(&self, path: &Path) -> Result<PathBuf> {
        let relative = path
            .strip_prefix(&self.store)
            .map_err(|_| "会话路径已改变")?;
        Ok(self.path.join("rollouts").join(relative))
    }

    fn record(
        &mut self,
        id: &str,
        files: &[FileChange],
        indexes: &[Index],
        providers: &[Option<String>],
    ) -> Result<Journal> {
        if !self.created {
            crate::private_fs::create_dir(&self.path).map_err(|_| "无法创建会话迁移备份")?;
            write_json(
                &self.path.join("manifest.json"),
                &serde_json::json!({
                    "store": self.store, "target_provider": self.target, "changes": "changes/"
                }),
            )?;
            self.created = true;
        }
        let mut entries = Vec::new();
        for file in files {
            crate::private_fs::write(
                &self.rollout(&file.path)?,
                &fs::read(&file.path).map_err(|_| "无法备份原会话")?,
            )
            .map_err(|_| "无法备份原会话")?;
            entries.push(serde_json::json!({
                "file": file.path.strip_prefix(&self.store).map_err(|_| "会话路径已改变")?,
                "original_provider": file.original_provider,
                "original_modified_at": file.metadata.modified().ok().map(chrono::DateTime::<chrono::Utc>::from)
            }));
        }
        let mut mappings = Vec::new();
        for (index, previous) in indexes.iter().zip(providers) {
            let Some(previous) = previous.as_deref().filter(|p| third_party(p, &self.target))
            else {
                continue;
            };
            if !self.indexes.contains(&index.path) {
                let path = self
                    .path
                    .join(index.path.file_name().ok_or("索引路径无效")?);
                crate::private_fs::create(&path, b"").map_err(|_| "无法创建索引备份")?;
                index
                    .conn
                    .backup("main", &path, None)
                    .map_err(|_| "无法备份原生会话索引")?;
                self.indexes.insert(index.path.clone());
            }
            mappings.push(serde_json::json!({"path": index.path, "original_provider": previous}));
        }
        let journal = Journal {
            path: self.path.join("changes").join(format!("{id}.json")),
            value: serde_json::json!({ "id": id, "phase": "prepared", "files": entries, "indexes": mappings }),
        };
        write_json(&journal.path, &journal.value)?;
        Ok(journal)
    }
}

struct Journal {
    path: PathBuf,
    value: Value,
}

impl Journal {
    fn complete(mut self) -> Result<()> {
        self.value["phase"] = "completed".into();
        write_json(&self.path, &self.value)
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    crate::private_fs::write(
        path,
        &serde_json::to_vec_pretty(value).map_err(|_| "无法生成迁移记录")?,
    )
    .map_err(|_| "无法保存迁移记录".into())
}

fn preserve_time(file: &FileChange) -> Result<()> {
    if let Ok(modified) = file.metadata.modified() {
        File::open(&file.path)
            .and_then(|f| f.set_times(FileTimes::new().set_modified(modified)))
            .map_err(|_| "无法保留会话时间")?;
    }
    Ok(())
}

fn write_files(files: &[FileChange], backup: &Backup) -> Result<usize> {
    // Validate the entire group before replacing any duplicate transcript.
    for file in files {
        let current = fs::symlink_metadata(&file.path).map_err(|_| "无法复核会话状态")?;
        if !current.is_file()
            || current.len() != file.metadata.len()
            || current.modified().ok() != file.metadata.modified().ok()
        {
            return Err("会话在迁移期间发生变化；索引事务将回滚，备份已保留".into());
        }
    }
    let mut written = Vec::new();
    let result = (|| {
        for file in files {
            if let Some(bytes) = &file.replacement {
                written.push(file);
                crate::private_fs::write(&file.path, bytes).map_err(|_| "无法更新会话分类")?;
                preserve_time(file)?;
            }
        }
        Ok(written.len())
    })();
    if result.is_err() {
        for file in written {
            let original = fs::read(backup.rollout(&file.path)?).map_err(|_| "无法读取恢复备份")?;
            crate::private_fs::write(&file.path, &original)
                .map_err(|_| "迁移失败且文件恢复未完成；索引事务将回滚，请根据保留的备份恢复")?;
            preserve_time(file)?;
        }
    }
    result
}

pub(super) fn session(
    store: &SessionStore,
    indexes: &mut [Index],
    session: SessionPlan,
    target: &str,
    backup: &mut Backup,
) -> Result<Outcome> {
    let mut guard = store.mutation_guard().map_err(|e| e.to_string())?;
    if guard
        .require_idle(std::slice::from_ref(&session.id))
        .is_err()
    {
        return Ok(Outcome::Deferred("busy"));
    }
    let mut files = Vec::new();
    for candidate in session.files {
        // Recheck paths after native compression/attachment could have run during planning.
        if indexed_path(store, &candidate.path)?.as_ref() != Some(&candidate.path) {
            return Ok(Outcome::Deferred("conflict"));
        }
        match prepare(candidate, target) {
            Ok(file) => files.push(file),
            Err(reason) => return Ok(Outcome::Deferred(reason)),
        }
    }
    let providers = indexes
        .iter()
        .map(|i| i.provider(&session.id))
        .collect::<Result<Vec<_>>>()?;
    if providers.iter().flatten().any(|p| p == "openai") {
        return Ok(Outcome::Unchanged);
    }
    if files.iter().all(|f| f.replacement.is_none())
        && !providers.iter().flatten().any(|p| third_party(p, target))
    {
        return Ok(Outcome::Unchanged);
    }
    // Online backups include WAL content and must run before opening write transactions.
    let journal = backup.record(&session.id, &files, indexes, &providers)?;
    let mut transactions = Vec::new();
    for index in indexes {
        transactions.push(
            index
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| "无法锁定原生会话索引；文件未改动，备份已保留")?,
        );
    }
    for (tx, expected) in transactions.iter().zip(&providers) {
        if provider(tx, &session.id)? != *expected {
            return Ok(Outcome::Deferred("conflict"));
        }
    }
    let mut index_rows = 0;
    for (tx, previous) in transactions.iter().zip(&providers) {
        if let Some(previous) = previous.as_deref().filter(|p| third_party(p, target)) {
            let changed = tx
                .execute(
                    "UPDATE threads SET model_provider=?1 WHERE id=?2 AND model_provider=?3",
                    params![target, session.id, previous],
                )
                .map_err(|_| "无法更新会话索引；事务将回滚，文件未改动，备份已保留")?;
            if changed != 1 || provider(tx, &session.id)?.as_deref() != Some(target) {
                return Err("会话索引分类发生冲突；事务将回滚，文件未改动，备份已保留".into());
            }
            index_rows += changed;
        }
    }
    let changed_files = write_files(&files, backup)?;
    // Multiple native schema versions cannot commit atomically together. A prepared
    // journal and idempotent re-planning make a partial commit explicit and recoverable.
    for tx in transactions {
        tx.commit()
            .map_err(|_| "会话文件已迁移，但索引提交未完成；备份已保留，重试迁移可继续")?;
    }
    journal.complete()?;
    Ok(Outcome::Applied {
        files: changed_files,
        index_rows,
    })
}
