//! Account policy for choosing the shared store; native filesystem mechanics live in codex_native.
use super::{Result, context};
use crate::codex_native::SessionStore;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

/// Read-only destination policy used by managed launch and resume planning.
pub(crate) fn target_home(root: &Path) -> PathBuf {
    context::base_home(root)
}

/// The manager chooses when and which homes to migrate. The native adapter only
/// receives the explicit source and destination and owns the filesystem protocol.
pub(super) fn prepare(source: &Path, destination: &Path) -> Result<()> {
    SessionStore::for_default_home(destination)
        .attach_legacy_home(source)
        .map_err(Into::into)
}

pub(crate) fn unify_third_party(
    root: &Path,
    dry_run: bool,
) -> Result<crate::codex_native::ProviderHistoryReport> {
    SessionStore::for_default_home(&target_home(root))
        .unify_third_party(
            super::config::THIRD_PARTY_PROVIDER,
            &root.join("backups/third-party-history-v1"),
            dry_run,
        )
        .map_err(super::Error::message)
}

/// Launch maintenance uses indexed legacy providers; a newly attached legacy Home
/// also needs a full scan because its transcripts may not have native index rows yet.
pub(super) fn prepare_third_party(
    root: &Path,
) -> Result<crate::codex_native::ProviderHistoryReport> {
    SessionStore::for_default_home(&target_home(root))
        .prepare_third_party(
            super::config::THIRD_PARTY_PROVIDER,
            &root.join("backups/third-party-history-v1"),
        )
        .map_err(super::Error::message)
}
