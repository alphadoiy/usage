//! Attach an explicitly selected legacy Home to a native session store.
use super::mutation::lock_coordination;
use super::{ALIAS_MARKER as MARKER, SHARED_INDEX_MARKER, SessionStore};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::Result;
use std::path::{Path, PathBuf};

fn link_directory(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, destination)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(source, destination)?;
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
            (Ok(a), Ok(b)) => {
                a.is_file() && b.is_file() && a.dev() == b.dev() && a.ino() == b.ino()
            }
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        false
    }
}

/// Never replace a conflicting file or follow an unknown symlink during migration.
fn files(source: &Path, destination: &Path, moves: &mut Vec<(PathBuf, PathBuf)>) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(std::io::Error::other("旧会话目录含符号链接，无法自动合并"));
        }
        if kind.is_dir() {
            if fs::symlink_metadata(&to).is_ok_and(|m| !m.is_dir() || m.file_type().is_symlink()) {
                return Err(std::io::Error::other("共享会话目录冲突，原文件已保留"));
            }
            files(&from, &to, moves)?;
        } else if kind.is_file() {
            if fs::symlink_metadata(&to).is_ok() && !same_file(&from, &to) {
                return Err(std::io::Error::other(
                    "共享会话中存在同名文件，原文件已保留",
                ));
            }
            moves.push((from, to));
        } else {
            return Err(std::io::Error::other("旧会话目录含不支持的文件类型"));
        }
    }
    Ok(())
}

fn remove_empty_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_empty_tree(&entry.path())?;
        }
    }
    fs::remove_dir(path)?;
    Ok(())
}

pub(super) fn attach(account_home: &Path, store: &SessionStore) -> Result<()> {
    let base = store.root();
    if base.starts_with(account_home) {
        return Err(std::io::Error::other("共享会话目录不能指向账号目录"));
    }
    crate::private_fs::create_dir(base)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(base.join(".usage-session-migration.lock"))?;
    lock.lock_exclusive()?;
    let directories = ["sessions", "archived_sessions", "thread-writer-locks"];
    // Validate aliases before opening any coordination file through them.
    for name in directories {
        let source = account_home.join(name);
        if source.is_symlink() && source.canonicalize()? != base.join(name).canonicalize()? {
            return Err(std::io::Error::other(
                "账号会话目录已链接到其他位置，请保留原目录后再调整",
            ));
        }
    }
    let mut old = Vec::new();
    let mut moves = Vec::new();
    // Keep both native coordination locks held until the old namespace has been linked.
    let mut locks = Vec::<File>::new();
    for dir in [
        account_home.join("thread-writer-locks"),
        base.join("thread-writer-locks"),
    ] {
        fs::create_dir_all(&dir)?;
        let canonical = dir.canonicalize()?;
        if account_home.join("thread-writer-locks").is_symlink() && dir.starts_with(account_home) {
            continue;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(canonical.join(".coordination.lock"))?;
        lock_coordination(&file)?;
        locks.push(file);
    }
    let local_locks = account_home.join("thread-writer-locks");
    if !local_locks.is_symlink() {
        for entry in fs::read_dir(&local_locks)? {
            let entry = entry?;
            if entry.file_name() == ".coordination.lock" {
                continue;
            }
            if !entry.file_type()?.is_file() {
                return Err(std::io::Error::other(
                    "旧会话锁目录含不支持的文件类型，原文件已保留",
                ));
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(entry.path())?;
            file.try_lock_exclusive().map_err(|_| {
                std::io::Error::other("该账号仍有运行中的旧会话，请退出后再共享会话目录")
            })?;
            locks.push(file);
        }
    }
    for name in directories {
        let source = account_home.join(name);
        let destination = base.join(name);
        fs::create_dir_all(&destination)?;
        match fs::symlink_metadata(&source) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if source.canonicalize()? != destination.canonicalize()? {
                    return Err(std::io::Error::other(
                        "账号会话目录已链接到其他位置，请保留原目录后再调整",
                    ));
                }
            }
            Ok(metadata) if metadata.is_dir() => {
                if name != "thread-writer-locks" {
                    files(&source, &destination, &mut moves)?;
                }
                old.push(source);
            }
            Ok(_) => return Err(std::io::Error::other("账号会话目录类型无效")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    // Link then unlink preserves file identity without ever replacing a destination. A retry
    // also handles interruption between the two operations. Cross-device moves fail safely.
    for (from, to) in moves {
        fs::create_dir_all(to.parent().unwrap())?;
        if !same_file(&from, &to) {
            fs::hard_link(&from, &to)?;
        }
        fs::remove_file(from)?;
    }
    for source in old {
        if source == local_locks {
            for entry in fs::read_dir(&source)? {
                fs::remove_file(entry?.path())?;
            }
        }
        remove_empty_tree(&source)?;
    }
    for name in directories {
        let destination = account_home.join(name);
        if fs::symlink_metadata(&destination).is_err() {
            link_directory(&base.join(name), &destination)?;
        }
    }
    crate::private_fs::write(
        &account_home.join(MARKER),
        &serde_json::to_vec(&base).map_err(|_| std::io::Error::other("无法保存共享目录"))?,
    )?;
    let state = crate::config::resolved_data_path(store.index_dir());
    crate::private_fs::write(
        &base.join(SHARED_INDEX_MARKER),
        &serde_json::to_vec(&state).map_err(|_| std::io::Error::other("无法保存共享索引目录"))?,
    )?;
    Ok(())
}
