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
