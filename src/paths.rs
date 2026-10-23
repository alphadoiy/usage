//! Per-user data location shared by every store. Unit tests get a private, empty root, so no
//! test reads or writes the user's accounts, ledgers or indexes.
use std::path::PathBuf;

/// `$XDG_DATA_HOME/usage`, else `~/.local/share/usage`.
pub fn data_dir() -> PathBuf {
    #[cfg(test)]
    {
        static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| {
            std::env::temp_dir()
                .join(format!("usage-test-data-{}", uuid::Uuid::new_v4()))
                .join("usage")
        })
        .clone()
    }
    #[cfg(not(test))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| crate::config::home_dir().join(".local/share"))
            .join("usage")
    }
}
