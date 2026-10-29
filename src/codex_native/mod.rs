//! Native Codex session locations and storage coordination.
//! Account selection, migration timing and credential ownership stay with the caller.
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

mod migration;
mod mutation;
pub(crate) use mutation::SessionMutation;

#[cfg(test)]
mod tests;

const ALIAS_MARKER: &str = "usage-shared-home.json";
const SHARED_INDEX_MARKER: &str = ".usage-shared-sessions";

/// A read-only location snapshot. Resolve it again after a possible migration.
/// The session root is independent of the account's authentication Home.
#[derive(Clone, Debug)]
pub(crate) struct SessionStore {
    root: PathBuf,
    index_dir: PathBuf,
    merge_rollouts: bool,
}

impl SessionStore {
    pub(crate) fn resolve(home: &Path) -> Self {
        Self::resolve_in(
            home,
            &default_home(),
            std::env::var_os("CODEX_SQLITE_HOME").as_deref(),
            false,
        )
    }

    /// Resolve a caller-selected default/shared destination before its first attachment.
    /// This preserves the native default Home's SQLite environment fallback even when
    /// no shared-store marker has been written yet.
    pub(crate) fn for_default_home(home: &Path) -> Self {
        Self::resolve_in(
            home,
            home,
            std::env::var_os("CODEX_SQLITE_HOME").as_deref(),
            true,
        )
    }

    fn resolve_in(
        home: &Path,
        own_home: &Path,
        environment: Option<&OsStr>,
        default: bool,
    ) -> Self {
        let root = recognized_alias(home).unwrap_or_else(|| home.to_path_buf());
        let merge_rollouts = root.join(SHARED_INDEX_MARKER).exists();
        let use_environment =
            default || root != home || merge_rollouts || same_directory(home, own_home);
        let index_dir = configured_index_dir(&root)
            .or_else(|| {
                environment
                    .filter(|value| use_environment && !value.is_empty())
                    .map(PathBuf::from)
            })
            .or_else(|| {
                // An independent Home must not inherit a different Home's SQLite environment.
                if merge_rollouts {
                    read_absolute_path(&root.join(SHARED_INDEX_MARKER))
                } else {
                    None
                }
            })
            .unwrap_or_else(|| root.clone());
        Self {
            root,
            index_dir,
            merge_rollouts,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn index_dir(&self) -> &Path {
        &self.index_dir
    }

    /// Shared stores also contain migrated transcripts the native index has not seen yet.
    pub(crate) fn merge_rollouts(&self) -> bool {
        self.merge_rollouts
    }

    pub(crate) fn same_storage(&self, other: &Self) -> bool {
        self.root
            .canonicalize()
            .ok()
            .zip(other.root.canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
    }

    pub(crate) fn index(&self) -> io::Result<Option<(u32, PathBuf)>> {
        if !self.index_dir.is_dir() {
            return Ok(None);
        }
        Ok(fs::read_dir(&self.index_dir)?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let version = name
                    .to_str()?
                    .strip_prefix("state_")?
                    .strip_suffix(".sqlite")?
                    .parse()
                    .ok()?;
                Some((version, entry.path()))
            })
            .max_by_key(|(version, _)| *version))
    }

    pub(crate) fn mutation_guard(&self) -> io::Result<SessionMutation> {
        SessionMutation::acquire(&self.root)
    }

    /// Mutating operation with an explicit source and destination; no account discovery.
    pub(crate) fn attach_legacy_home(&self, source: &Path) -> io::Result<()> {
        migration::attach(source, self)
    }
}

fn read_absolute_path(path: &Path) -> Option<PathBuf> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PathBuf>(&bytes).ok())
        .filter(|path| path.is_absolute())
}

fn recognized_alias(home: &Path) -> Option<PathBuf> {
    read_absolute_path(&home.join(ALIAS_MARKER)).filter(|target| {
        fs::read_link(home.join("sessions")).ok().as_deref()
            == Some(target.join("sessions").as_path())
    })
}

fn configured_index_dir(home: &Path) -> Option<PathBuf> {
    fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|doc| {
            doc.get("sqlite_home")?
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
        })
        .map(|path| match path.strip_prefix("~/") {
            Some(rest) => crate::config::home_dir().join(rest),
            None => home.join(path),
        })
}

fn default_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config::home_dir().join(".codex"))
}

fn same_directory(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}
