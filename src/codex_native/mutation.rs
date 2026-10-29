//! Native Codex writer coordination, shared by cleanup and migration.
use super::SessionStore;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::Result;
use std::path::{Path, PathBuf};

pub(super) fn lock_coordination(file: &File) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::other("会话目录正在使用，请稍后重试"));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Cooperate with native writers, compression and migration for the entire mutation.
/// Native writers hold their thread lock while idle too; modification time is not ownership.
pub(crate) struct SessionMutation {
    directory: PathBuf,
    writers: Vec<File>,
    _coordination: File,
}

fn native_lock(path: &Path, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() || fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::other("会话锁不是普通文件，操作已停止"));
    }
    Ok(file)
}

impl SessionMutation {
    pub(super) fn acquire(store: &Path) -> Result<Self> {
        if store.canonicalize()? != store {
            return Err(std::io::Error::other("会话存储位置已改变，请刷新后重试"));
        }
        let directory = store.join("thread-writer-locks");
        fs::create_dir_all(&directory)?;
        if fs::symlink_metadata(&directory)?.file_type().is_symlink() {
            return Err(std::io::Error::other("会话锁目录含符号链接，请刷新后重试"));
        }
        let path = directory.join(".coordination.lock");
        let coordination = native_lock(&path, true)?;
        lock_coordination(&coordination)?;
        // Migration can unlink the old coordination file while we wait on it. Never
        // proceed with a lock that no longer guards the namespace being mutated.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let opened = coordination.metadata()?;
            let current = fs::metadata(&path)?;
            if opened.dev() != current.dev() || opened.ino() != current.ino() {
                return Err(std::io::Error::other("会话存储位置已改变，请刷新后重试"));
            }
        }
        if directory.canonicalize()? != directory || SessionStore::resolve(store).root() != store {
            return Err(std::io::Error::other("会话存储位置已改变，请刷新后重试"));
        }
        Ok(Self {
            directory,
            _coordination: coordination,
            writers: Vec::new(),
        })
    }

    pub(crate) fn require_idle(&mut self, ids: &[String]) -> Result<()> {
        for id in ids {
            let id =
                uuid::Uuid::parse_str(id).map_err(|_| std::io::Error::other("会话 ID 无效"))?;
            let path = self.directory.join(format!("{id}.lock"));
            // Holding coordination prevents creation of a new writer or lock file.
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
                Ok(_) => {}
            }
            let file = native_lock(&path, false)?;
            file.try_lock_exclusive()
                .map_err(|_| std::io::Error::other("会话或其子任务仍有原生写入锁，请退出后重试"))?;
            self.writers.push(file);
        }
        Ok(())
    }
}
