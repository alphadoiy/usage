//! Durable file compensation coordinated with the OAuth SQLite commit marker.
use super::*;

/// Durable undo journal. The account lease covers capture, commit and recovery.
/// SQLite's commit marker decides whether recovery keeps new files or restores old ones.
pub(super) struct SaveFiles {
    directory: PathBuf,
    files: Vec<SavedFile>,
    armed: bool,
    preserve: bool,
}
struct SavedFile {
    path: PathBuf,
    backup: Option<PathBuf>,
}
#[derive(Serialize, Deserialize)]
struct SaveManifest {
    version: u32,
    revision: i64,
    files: Vec<Option<String>>,
}
fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
pub(super) fn saved_names(revision: i64) -> [String; 4] {
    [
        "auth.json".into(),
        "config.toml".into(),
        "usage-binding.json".into(),
        format!("usage-r{revision}.config.toml"),
    ]
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
impl SaveFiles {
    pub(super) fn operation(&self) -> &str {
        self.directory.file_name().unwrap().to_str().unwrap()
    }
    pub(super) fn capture(home: &Path, revision: i64) -> Result<Self> {
        let directory = home.join(format!(".usage-oauth-recovery-v1-{}", uuid::Uuid::new_v4()));
        private_dir(&directory)?;
        let mut saved = Self {
            directory,
            files: Vec::new(),
            armed: false,
            preserve: false,
        };
        let mut manifest = SaveManifest {
            version: 1,
            revision,
            files: Vec::new(),
        };
        for name in saved_names(revision) {
            let path = home.join(&name);
            let backup = match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    let bytes = fs::read(&path)?;
                    let backup = saved.directory.join(&name);
                    private_write(&backup, &bytes)?;
                    manifest.files.push(Some(digest(&bytes)));
                    Some(backup)
                }
                Ok(_) => return Err(Error::message("OAuth 账号文件必须是普通文件")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    manifest.files.push(None);
                    None
                }
                Err(error) => return Err(error.into()),
            };
            saved.files.push(SavedFile { path, backup });
            #[cfg(test)]
            save_checkpoint("capturing");
        }
        private_write(
            &saved.directory.join("manifest.json"),
            &serde_json::to_vec(&manifest).map_err(|_| Error::message("无法保存恢复清单"))?,
        )?;
        // Persist the journal and newly-created account/home directory entries before mutations.
        for directory in [
            Some(home),
            home.parent(),
            home.parent().and_then(Path::parent),
            home.parent().and_then(Path::parent).and_then(Path::parent),
        ]
        .into_iter()
        .flatten()
        {
            sync_dir(directory)?;
        }
        saved.armed = true;
        Ok(saved)
    }
    fn restore(&mut self) -> Result<()> {
        // Keep backups intact: a second interruption during recovery can safely retry.
        for (index, file) in self.files.iter().enumerate() {
            match &file.backup {
                Some(backup) => private_write(&file.path, &fs::read(backup)?)?,
                None => match fs::remove_file(&file.path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    result => result?,
                },
            }
            if index == 0 {
                #[cfg(test)]
                save_checkpoint("restoring");
            }
        }
        sync_dir(self.directory.parent().unwrap())
    }
    pub(super) fn rollback(mut self) -> Result<()> {
        let result = self.restore();
        self.armed = false;
        self.preserve = result.is_err();
        result
    }
    pub(super) fn commit(mut self) -> bool {
        self.armed = false;
        self.preserve = true;
        match self.cleanup() {
            Ok(()) => true,
            Err(_) => {
                tracing::warn!(event = "oauth_cleanup_deferred");
                false
            }
        }
    }
    fn cleanup(&self) -> Result<()> {
        // Never leave a discoverable journal with partially deleted backups.
        let tombstone = self.directory.with_file_name(self.operation().replacen(
            ".usage-oauth-recovery-",
            ".usage-oauth-cleanup-",
            1,
        ));
        fs::rename(&self.directory, &tombstone)?;
        sync_dir(tombstone.parent().unwrap())?;
        #[cfg(test)]
        save_checkpoint("cleaning");
        fs::remove_dir_all(tombstone)?;
        Ok(())
    }
}
impl Drop for SaveFiles {
    fn drop(&mut self) {
        if self.armed && self.restore().is_err() {
            self.preserve = true;
            tracing::error!(event = "oauth_save_restore_failed", recovery = %self.directory.display());
        }
        if !self.preserve && self.cleanup().is_err() {
            tracing::warn!(event = "oauth_save_recovery_cleanup_failed", recovery = %self.directory.display());
        }
    }
}

pub(super) fn pending_journals(home: &Path) -> Result<Vec<PathBuf>> {
    recovery_directories(home, ".usage-oauth-recovery-")
}
pub(super) fn recovery_needed(home: &Path) -> Result<bool> {
    Ok(!pending_journals(home)?.is_empty()
        || !recovery_directories(home, ".usage-oauth-cleanup-")?.is_empty())
}
fn recovery_directories(home: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(home) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut journals = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            if !entry.file_type()?.is_dir() {
                return Err(Error::message("OAuth 恢复目录异常"));
            }
            journals.push(entry.path());
        }
    }
    Ok(journals)
}
pub(super) fn recover_home(conn: &rusqlite::Connection, home: &Path) -> Result<()> {
    logging::operation("codex.oauth_recovery", |op| {
        if let Some(id) = home
            .parent()
            .and_then(Path::file_name)
            .and_then(|id| id.to_str())
        {
            op.account(id, 0, "oauth");
        }
        op.begin("journal_discover");
        recover_home_inner(conn, home, op)
    })
}
fn recover_home_inner(
    conn: &rusqlite::Connection,
    home: &Path,
    op: &mut crate::logging::Operation,
) -> Result<()> {
    for directory in pending_journals(home)? {
        let operation = directory
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        op.begin("commit_decision");
        let committed = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM oauth_save_commits WHERE operation=?1)",
            [&operation],
            |row| row.get::<_, bool>(0),
        )?;
        let mut saved = SaveFiles {
            directory,
            files: Vec::new(),
            armed: false,
            preserve: true,
        };
        if !committed {
            op.begin("backup_validate");
            let manifest = saved.directory.join("manifest.json");
            let bytes = match fs::read(&manifest) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            // Only this journal version guarantees no writes before a durable manifest.
            // Earlier releases used the same base prefix without a commit protocol.
            if bytes.is_none() && !operation.starts_with(".usage-oauth-recovery-v1-") {
                return Err(Error::message(
                    "旧版 OAuth 恢复副本缺少提交信息，已保留副本；请先恢复账号文件",
                ));
            }
            // A new-format directory without a manifest is an interrupted capture.
            if let Some(bytes) = bytes {
                let manifest: SaveManifest = serde_json::from_slice(&bytes)
                    .map_err(|_| Error::message("OAuth 恢复清单损坏，已保留恢复副本"))?;
                if manifest.version != 1 || manifest.revision < 1 || manifest.files.len() != 4 {
                    return Err(Error::message("OAuth 恢复清单版本或内容无效"));
                }
                for (name, checksum) in saved_names(manifest.revision)
                    .into_iter()
                    .zip(manifest.files)
                {
                    let backup = if let Some(checksum) = checksum {
                        let backup = saved.directory.join(&name);
                        if !fs::symlink_metadata(&backup)?.is_file()
                            || digest(&fs::read(&backup)?) != checksum
                        {
                            return Err(Error::message("OAuth 恢复副本损坏，已停止恢复"));
                        }
                        Some(backup)
                    } else {
                        None
                    };
                    saved.files.push(SavedFile {
                        path: home.join(name),
                        backup,
                    });
                }
                op.begin("backup_restore");
                saved.restore()?;
                tracing::info!(event = "oauth_recovery_restored");
            }
        }
        if committed {
            tracing::info!(event = "oauth_recovery_committed");
        }
        op.begin("journal_cleanup");
        saved.cleanup()?;
        // The tombstone rename is durable before removing the commit decision.
        conn.execute(
            "DELETE FROM oauth_save_commits WHERE operation=?1",
            [&operation],
        )?;
    }
    op.begin("cleanup_resume");
    for tombstone in recovery_directories(home, ".usage-oauth-cleanup-")? {
        let operation = tombstone.file_name().unwrap().to_string_lossy().replacen(
            ".usage-oauth-cleanup-",
            ".usage-oauth-recovery-",
            1,
        );
        // A previous rename might have completed just before its directory sync failed.
        // Make it durable before discarding its commit decision or remaining backups.
        sync_dir(home)?;
        fs::remove_dir_all(tombstone)?;
        conn.execute(
            "DELETE FROM oauth_save_commits WHERE operation=?1",
            [&operation],
        )?;
    }
    Ok(())
}
