//! Private directories and atomic files shared by account and native-store persistence.
use std::fs::{self, OpenOptions};
use std::io::{Result, Write};
use std::path::Path;

pub(crate) fn create_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    private_write_impl(path, bytes, true)
}

/// Atomically initialize a file without replacing a concurrent CLI's mutable configuration.
pub(crate) fn create(path: &Path, bytes: &[u8]) -> Result<()> {
    private_write_impl(path, bytes, false)
}

fn private_write_impl(path: &Path, bytes: &[u8], overwrite: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("文件缺少父目录"))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if overwrite {
            fs::rename(&temp, path)?;
        } else {
            match fs::hard_link(&temp, path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}
