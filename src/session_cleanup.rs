//! Reversible local session cleanup. Native shared indexes are never rewritten.
//! Manifests precede moves so an interrupted operation can be restored after restart.
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::sessions::Session;

type Result<T> = std::result::Result<T, String>;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum Phase {
    Moving,
    Trashed,
    Restoring,
    Purged,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub provider: String,
    pub instance: String,
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub deleted_at: i64,
    pub bytes: u64,
    paths: Vec<PathBuf>,
    /// Includes child threads; older manifests recover these from their payloads on restore.
    #[serde(default)]
    session_ids: Vec<String>,
    phase: Phase,
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

impl Default for Store {
    fn default() -> Self {
        Self::at(crate::paths::data_dir().join("trash/sessions"))
    }
}

impl Store {
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    fn lock(&self) -> Result<File> {
        private_dir(&self.root)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(".lock"))
            .map_err(err)?;
        file.lock_exclusive().map_err(err)?;
        Ok(file)
    }

    fn read_lock(&self) -> Result<Option<File>> {
        let file = match File::open(self.root.join(".lock")) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(err(e)),
        };
        FileExt::lock_shared(&file).map_err(err)?;
        Ok(Some(file))
    }

    fn dir(&self, key: &str) -> Result<PathBuf> {
        if uuid::Uuid::parse_str(key).is_err() || key.contains(['/', '\\']) {
            return Err("回收站条目无效".into());
        }
        let path = self.root.join(key);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("回收站目录不能是符号链接".into());
        }
        Ok(path)
    }

    fn write(&self, entry: &Entry) -> Result<()> {
        let dir = self.dir(&entry.key)?;
        let temp = dir.join("entry.tmp");
        let mut file = File::create(&temp).map_err(err)?;
        serde_json::to_writer(&mut file, entry).map_err(err)?;
        file.sync_all().map_err(err)?;
        fs::rename(temp, dir.join("entry.json")).map_err(err)
    }

    fn read(&self, key: &str) -> Result<Entry> {
        let dir = self.dir(key)?;
        let mut entry: Entry =
            serde_json::from_reader(File::open(dir.join("entry.json")).map_err(err)?)
                .map_err(err)?;
        if entry.key != key || (entry.paths.is_empty() && entry.phase != Phase::Purged) {
            return Err("回收站清单无效".into());
        }
        for path in &entry.paths {
            validate_relative(path)?;
        }
        // Resolve only Usage's validated migration alias, not arbitrary symlinks.
        // This also keeps old tombstones attached to their store after migration.
        entry.home = storage_home(&entry.provider, &entry.home);
        Ok(entry)
    }

    fn entries(&self) -> Result<Vec<Entry>> {
        let dirs = match fs::read_dir(&self.root) {
            Ok(dirs) => dirs,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(err(e)),
        };
        let mut entries = Vec::new();
        for dir in dirs {
            let dir = dir.map_err(err)?;
            let key = dir.file_name().to_string_lossy().into_owned();
            if uuid::Uuid::parse_str(&key).is_ok() {
                // An interruption before the first manifest cannot have moved any files.
                if !dir.path().join("entry.json").try_exists().map_err(err)? {
                    continue;
                }
                entries.push(self.read(&key)?);
            }
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.deleted_at));
        Ok(entries)
    }

    pub fn list(&self) -> Result<Vec<Entry>> {
        let _lock = self.read_lock()?;
        Ok(self
            .entries()?
            .into_iter()
            .filter(|e| e.phase != Phase::Purged)
            .collect())
    }

    /// Tombstones also suppress stale native index rows after the files are purged.
    /// Physical Home identity deliberately covers aliases of the same configured Home.
    pub fn hidden(&self, provider: &str, home: &Path) -> Result<HashSet<String>> {
        let _lock = self.read_lock()?;
        let home = storage_home(provider, home);
        Ok(self
            .entries()?
            .into_iter()
            .filter(|e| e.provider == provider && e.home == home)
            .flat_map(|e| std::iter::once(e.id).chain(e.session_ids))
            .collect())
    }

    pub fn delete(&self, session: &Session) -> Result<Entry> {
        let _lock = self.lock()?;
        if !session.provider.valid_session_id(&session.id) {
            return Err("会话 ID 无效".into());
        }
        let home = storage_home(session.provider.id(), &session.home)
            .canonicalize()
            .map_err(err)?;
        let mut mutation = mutation_guard(session.provider.id(), &home)?;
        if self
            .entries()?
            .iter()
            .any(|e| e.provider == session.provider.id() && e.home == home && e.id == session.id)
        {
            return Err("会话已在回收站中".into());
        }
        let (paths, session_ids) = session_paths(session, &home)?;
        if paths.is_empty() {
            return Err("未找到独立会话文件，无法清理此会话".into());
        }
        if let Some(guard) = &mut mutation {
            guard.require_idle(&session_ids).map_err(err)?;
        }
        let now = SystemTime::now();
        let mut bytes = 0;
        for path in &paths {
            secure_path(&home, path)?;
            bytes += inspect(&home.join(path), Some(now))?;
        }
        let mut entry = Entry {
            key: uuid::Uuid::new_v4().to_string(),
            provider: session.provider.id().into(),
            instance: session.instance.clone(),
            id: session.id.clone(),
            title: session.title.clone(),
            cwd: session.cwd.clone(),
            home,
            deleted_at: chrono::Utc::now().timestamp_millis(),
            bytes,
            paths,
            session_ids,
            phase: Phase::Moving,
        };
        let dir = self.dir(&entry.key)?;
        private_dir(&dir.join("files"))?;
        self.write(&entry)?;
        for (index, path) in entry.paths.iter().enumerate() {
            // Recheck immediately before each move as agents may still write externally.
            let moved = secure_path(&entry.home, path)
                .and_then(|_| inspect(&entry.home.join(path), Some(SystemTime::now())))
                .and_then(|_| {
                    move_exclusive(
                        entry.home.join(path),
                        dir.join("files").join(index.to_string()),
                    )
                });
            if let Err(error) = moved {
                let rollback = self.restore_entry(&entry);
                return Err(match rollback {
                    Ok(()) => format!("清理失败，已恢复原文件：{error}"),
                    Err(restore) => format!("清理未完成：{error}；请从回收站恢复：{restore}"),
                });
            }
        }
        entry.phase = Phase::Trashed;
        self.write(&entry)?;
        Ok(entry)
    }

    pub fn restore(&self, key: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut entry = self.read(key)?;
        if entry.phase == Phase::Purged {
            return Err("此会话已永久删除".into());
        }
        let _mutation = self.lock_entry(&mut entry)?;
        // Preflight every destination before changing anything. Never overwrite a new session.
        for (index, path) in entry.paths.iter().enumerate() {
            let payload = self.dir(key)?.join("files").join(index.to_string());
            secure_path(&entry.home, path)?;
            if payload.try_exists().map_err(err)?
                && entry.home.join(path).try_exists().map_err(err)?
            {
                return Err("原位置已有文件，恢复已停止，未覆盖任何文件".into());
            }
        }
        entry.phase = Phase::Restoring;
        self.write(&entry)?;
        self.restore_entry(&entry)
    }

    fn restore_entry(&self, entry: &Entry) -> Result<()> {
        let dir = self.dir(&entry.key)?;
        for (index, path) in entry.paths.iter().enumerate() {
            secure_path(&entry.home, path)?;
            let payload = dir.join("files").join(index.to_string());
            let target = entry.home.join(path);
            if payload.try_exists().map_err(err)? {
                if target.try_exists().map_err(err)? {
                    return Err("原位置已有文件，无法恢复".into());
                }
                inspect(&payload, None)?;
                fs::create_dir_all(target.parent().ok_or("会话目录无效")?).map_err(err)?;
                move_exclusive(payload, &target)?;
            } else if !target.try_exists().map_err(err)? {
                return Err("会话文件缺失，保留回收站清单供检查".into());
            }
        }
        fs::remove_dir_all(dir).map_err(err)
    }

    fn lock_entry(
        &self,
        entry: &mut Entry,
    ) -> Result<Option<crate::codex_native::SessionMutation>> {
        let mut guard = mutation_guard(&entry.provider, &entry.home)?;
        if let Some(guard) = &mut guard {
            self.recover_session_ids(entry)?;
            guard.require_idle(&entry.session_ids).map_err(err)?;
        }
        Ok(guard)
    }

    fn recover_session_ids(&self, entry: &mut Entry) -> Result<()> {
        if entry.provider != "codex" || !entry.session_ids.is_empty() {
            return Ok(());
        }
        let mut ids = HashSet::from([entry.id.clone()]);
        let files = self.dir(&entry.key)?.join("files");
        for (index, original) in entry.paths.iter().enumerate() {
            // An incomplete archive may coexist with its valid plain twin.
            if crate::logfiles::compressed(original)
                && entry.paths.contains(&original.with_extension(""))
            {
                continue;
            }
            let payload = files.join(index.to_string());
            let path = if payload.try_exists().map_err(err)? {
                payload
            } else {
                secure_path(&entry.home, original)?;
                entry.home.join(original)
            };
            inspect(&path, None)?;
            let metadata = codex_metadata(&path, crate::logfiles::compressed(original))?
                .ok_or("回收站会话日志无法识别，操作已停止")?;
            let id = metadata["id"].as_str().ok_or("回收站会话 ID 无效")?;
            ids.insert(id.to_string());
        }
        for id in &ids {
            uuid::Uuid::parse_str(id).map_err(|_| "回收站会话 ID 无效")?;
        }
        entry.session_ids = ids.into_iter().collect();
        entry.session_ids.sort();
        Ok(())
    }

    pub fn purge(&self, key: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut entry = self.read(key)?;
        if entry.phase == Phase::Purged {
            return Ok(());
        }
        if entry.phase != Phase::Trashed {
            return Err("清理曾被中断，请先恢复该会话".into());
        }
        let _mutation = if entry.provider == "codex" {
            match fs::symlink_metadata(&entry.home) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Purging touches only trash payloads. An unmounted/deleted Home must
                    // neither prevent reclamation nor be recreated just to take a lock.
                    self.recover_session_ids(&mut entry)?;
                    None
                }
                Err(error) => return Err(err(error)),
                Ok(_) => self.lock_entry(&mut entry)?,
            }
        } else {
            None
        };
        let files = self.dir(key)?.join("files");
        if files.try_exists().map_err(err)? {
            inspect(&files, None)?;
            fs::remove_dir_all(files).map_err(err)?;
        }
        entry.phase = Phase::Purged;
        // Keep only identity needed to suppress a stale native index row, not conversation metadata.
        entry.title.clear();
        entry.cwd.clear();
        entry.paths.clear();
        entry.bytes = 0;
        self.write(&entry)
    }
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn storage_home(provider: &str, home: &Path) -> PathBuf {
    if provider == "codex" {
        crate::codex_native::SessionStore::resolve(home)
            .root()
            .to_path_buf()
    } else {
        home.to_path_buf()
    }
}

fn mutation_guard(
    provider: &str,
    home: &Path,
) -> Result<Option<crate::codex_native::SessionMutation>> {
    (provider == "codex")
        .then(|| {
            crate::codex_native::SessionStore::resolve(home)
                .mutation_guard()
                .map_err(err)
        })
        .transpose()
}

fn codex_metadata(path: &Path, compressed: bool) -> Result<Option<serde_json::Value>> {
    let file = File::open(path).map_err(err)?;
    let input: Box<dyn Read> = if compressed {
        Box::new(ruzstd::decoding::StreamingDecoder::new(file).map_err(err)?)
    } else {
        Box::new(file)
    };
    Ok(BufReader::new(input.take(1024 * 1024))
        .lines()
        .take(8)
        .find_map(|line| {
            let row: serde_json::Value = serde_json::from_str(&line.ok()?).ok()?;
            (row["type"] == "session_meta").then(|| row["payload"].clone())
        }))
}

/// Atomic no-clobber moves also close the preflight/rename race on restoration.
/// Across volumes we fail without copying or removing a partially copied conversation.
fn move_exclusive(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    let from = from.as_ref();
    let to = to.as_ref();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(from.as_os_str().as_bytes()).map_err(err)?;
        let to = std::ffi::CString::new(to.as_os_str().as_bytes()).map_err(err)?;
        // SAFETY: both paths are NUL-terminated and live for the duration of the call.
        let result = unsafe {
            #[cfg(target_os = "macos")]
            {
                libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL)
            }
            #[cfg(target_os = "linux")]
            {
                libc::renameat2(
                    libc::AT_FDCWD,
                    from.as_ptr(),
                    libc::AT_FDCWD,
                    to.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            }
        };
        if result != 0 {
            return Err(err(std::io::Error::last_os_error()));
        }
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        fs::rename(from, to).map_err(err)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (from, to);
        Err("此平台尚不支持安全移动会话文件".into())
    }
}

fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(err)?;
    if fs::symlink_metadata(path)
        .map_err(err)?
        .file_type()
        .is_symlink()
    {
        return Err("回收站目录不能是符号链接".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(err)?;
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("会话文件路径无效".into());
    }
    if ![
        "sessions",
        "projects",
        "file-history",
        "session-env",
        "todos",
    ]
    .iter()
    .any(|root| path.starts_with(root))
    {
        return Err("会话文件不在支持的存储目录中".into());
    }
    Ok(())
}

fn secure_path(home: &Path, relative: &Path) -> Result<()> {
    validate_relative(relative)?;
    if home.canonicalize().map_err(err)? != home {
        return Err("实例目录已改变，操作已停止".into());
    }
    let mut path = home.to_path_buf();
    for component in relative.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err("会话路径含符号链接，操作已停止".into());
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(err(e)),
        }
    }
    Ok(())
}

/// Never follow symlinks, including inside artifact directories.
fn inspect(path: &Path, now: Option<SystemTime>) -> Result<u64> {
    let meta = fs::symlink_metadata(path).map_err(err)?;
    if meta.file_type().is_symlink() || (!meta.is_dir() && !meta.is_file()) {
        return Err("会话文件含符号链接或特殊文件，操作已停止".into());
    }
    if let Some(now) = now
        && now
            .duration_since(meta.modified().map_err(err)?)
            .unwrap_or_default()
            < Duration::from_secs(60)
    {
        return Err("会话文件在最近一分钟内有写入，可能仍在运行，请稍后重试".into());
    }
    if meta.is_dir() {
        let mut size = 0;
        for child in fs::read_dir(path).map_err(err)? {
            size += inspect(&child.map_err(err)?.path(), now)?;
        }
        Ok(size)
    } else {
        Ok(meta.len())
    }
}

fn children(path: &Path) -> Result<Vec<PathBuf>> {
    match fs::read_dir(path) {
        Ok(entries) => entries
            .map(|entry| entry.map(|e| e.path()).map_err(err))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(err(e)),
    }
}

fn session_paths(session: &Session, home: &Path) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let mut paths = Vec::new();
    let mut session_ids = vec![session.id.clone()];
    match session.provider.id() {
        "claude" => {
            for project in children(&home.join("projects"))? {
                let relative = project.strip_prefix(home).map_err(err)?;
                secure_path(home, relative)?;
                if !project.is_dir() {
                    continue;
                }
                let log = project.join(format!("{}.jsonl", session.id));
                if log.try_exists().map_err(err)? {
                    paths.push(log);
                    let artifacts = project.join(&session.id);
                    if artifacts.try_exists().map_err(err)? {
                        paths.push(artifacts);
                    }
                }
            }
            if !paths.is_empty() {
                for dir in ["file-history", "session-env"] {
                    let path = home.join(dir).join(&session.id);
                    if path.try_exists().map_err(err)? {
                        paths.push(path);
                    }
                }
                for path in children(&home.join("todos"))? {
                    if path.file_name().is_some_and(|name| {
                        name.to_string_lossy()
                            .starts_with(&format!("{}-", session.id))
                    }) {
                        paths.push(path);
                    }
                }
            }
        }
        "codex" => {
            let mut directories = vec![home.join("sessions")];
            let mut logs = Vec::new();
            while let Some(dir) = directories.pop() {
                secure_path(home, dir.strip_prefix(home).map_err(err)?)?;
                for path in children(&dir)? {
                    let meta = fs::symlink_metadata(&path).map_err(err)?;
                    if meta.file_type().is_symlink() {
                        return Err("会话存储含符号链接，操作已停止".into());
                    }
                    if meta.is_dir() {
                        directories.push(path);
                        continue;
                    }
                    if path.extension().is_none_or(|e| e != "jsonl")
                        && !crate::logfiles::compressed(&path)
                    {
                        continue;
                    }
                    // Read physical files: both a plain rollout and its compressed twin are moved.
                    let (read_path, _) = crate::logfiles::resolve(&path).map_err(err)?;
                    let metadata =
                        codex_metadata(&read_path, crate::logfiles::compressed(&read_path))?;
                    if let Some(metadata) = metadata {
                        let id = metadata["id"].as_str().unwrap_or_default().to_string();
                        let parent = metadata
                            .pointer("/source/subagent/thread_spawn/parent_thread_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        logs.push((path, id, parent));
                    } else if path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(&session.id))
                    {
                        return Err("会话日志无法识别，操作已停止".into());
                    }
                }
            }
            let mut ids = HashSet::from([session.id.clone()]);
            loop {
                let count = ids.len();
                for (_, id, parent) in &logs {
                    if parent.as_ref().is_some_and(|parent| ids.contains(parent)) {
                        ids.insert(id.clone());
                    }
                }
                if ids.len() == count {
                    break;
                }
            }
            paths.extend(
                logs.into_iter()
                    .filter(|(_, id, _)| ids.contains(id))
                    .map(|(path, _, _)| path),
            );
            session_ids = ids.into_iter().collect();
            session_ids.sort();
        }
        "grok" => {
            for project in children(&home.join("sessions"))? {
                secure_path(home, project.strip_prefix(home).map_err(err)?)?;
                if !project.is_dir() {
                    continue;
                }
                let mut sessions = Vec::new();
                for dir in children(&project)? {
                    secure_path(home, dir.strip_prefix(home).map_err(err)?)?;
                    if !dir.is_dir() {
                        continue;
                    }
                    let summary_path = dir.join("summary.json");
                    secure_path(home, summary_path.strip_prefix(home).map_err(err)?)?;
                    let summary: serde_json::Value = match fs::read(&summary_path) {
                        Ok(text) => serde_json::from_slice(&text).map_err(err)?,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => return Err(err(e)),
                    };
                    let Some(id) = summary.pointer("/info/id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let markers = dir.join("subagents");
                    secure_path(home, markers.strip_prefix(home).map_err(err)?)?;
                    let subagents: Vec<String> = children(&markers)?
                        .iter()
                        .filter_map(|path| path.file_stem()?.to_str().map(str::to_string))
                        .collect();
                    sessions.push((id.to_string(), dir, subagents));
                }
                let mut ids = HashSet::from([session.id.clone()]);
                loop {
                    let count = ids.len();
                    for (id, _, subagents) in &sessions {
                        if ids.contains(id) {
                            ids.extend(subagents.iter().cloned());
                        }
                    }
                    if ids.len() == count {
                        break;
                    }
                }
                paths.extend(
                    sessions
                        .into_iter()
                        .filter(|(id, _, _)| ids.contains(id))
                        .map(|(_, path, _)| path),
                );
            }
        }
        // No generic file deletion for adapters whose on-disk layout is unknown.
        _ => return Err("此 Agent 暂不支持会话清理".into()),
    }
    let paths = paths
        .into_iter()
        .map(|path| path.strip_prefix(home).map(Path::to_path_buf).map_err(err))
        .collect::<Result<_>>()?;
    Ok((paths, session_ids))
}
