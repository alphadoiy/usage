//! A small retry queue, not a second history index. Attachments request a full scan;
//! afterward only deferred paths need revisiting alongside native index candidates.
use super::*;
use serde::Deserialize;

const MARKER: &str = ".usage-provider-history-pending.json";

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Pending {
    generation: String,
    target: Option<String>,
    full_scan: bool,
    #[serde(default)]
    paths: BTreeSet<PathBuf>,
}

fn read(store: &SessionStore) -> Result<Option<Pending>> {
    match fs::read(store.root().join(MARKER)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| "会话迁移重试记录无效，请保留该文件后重试".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("无法读取会话迁移重试记录".into()),
    }
}

fn write(store: &SessionStore, state: &Pending) -> Result<()> {
    crate::private_fs::write(
        &store.root().join(MARKER),
        &serde_json::to_vec(state).map_err(|_| "无法生成会话迁移重试记录")?,
    )
    .map_err(|_| "无法保存会话迁移重试记录".into())
}

/// Caller holds native coordination until attachment finishes, including partial failures.
pub(super) fn require_scan(store: &SessionStore) -> Result<()> {
    write(
        store,
        &Pending {
            generation: uuid::Uuid::new_v4().to_string(),
            full_scan: true,
            ..Default::default()
        },
    )
}

impl Pending {
    pub(super) fn snapshot(store: &SessionStore, dry_run: bool) -> Result<Self> {
        // Taking the snapshot after an in-flight attachment completes prevents marking
        // a partially discovered import as fully scanned. Dry runs never create locks.
        let _guard = if dry_run {
            None
        } else {
            Some(store.mutation_guard().map_err(|e| e.to_string())?)
        };
        Ok(read(store)?.unwrap_or_default())
    }

    pub(super) fn scan(&self, requested: Scan, target: &str) -> Scan {
        if self.full_scan || self.target.as_deref() != Some(target) {
            Scan::All
        } else {
            requested
        }
    }

    pub(super) fn paths(&self) -> &BTreeSet<PathBuf> {
        &self.paths
    }

    /// Persist planned paths before the first mutation so process termination cannot
    /// strand unindexed transcripts or a second copy of an indexed conversation.
    pub(super) fn stage(self, store: &SessionStore, paths: BTreeSet<PathBuf>) -> Result<Self> {
        if paths.is_empty() {
            return Ok(self);
        }
        let _guard = store.mutation_guard().map_err(|e| e.to_string())?;
        let mut current = read(store)?.unwrap_or_default();
        let changed_generation = current.generation != self.generation;
        let previous_paths = current.paths.clone();
        current.paths.extend(paths);
        if changed_generation {
            // This plan did not observe the other operation's complete input set.
            current.full_scan = true;
        } else if current.paths == previous_paths {
            return Ok(self);
        }
        current.generation = uuid::Uuid::new_v4().to_string();
        write(store, &current)?;
        if changed_generation {
            // Keep the old token so finish cannot clear the new full-scan request.
            Ok(self)
        } else {
            Ok(current)
        }
    }

    pub(super) fn finish(
        self,
        store: &SessionStore,
        target: &str,
        paths: BTreeSet<PathBuf>,
    ) -> Result<()> {
        let _guard = store.mutation_guard().map_err(|e| e.to_string())?;
        let current = read(store)?.unwrap_or_default();
        // Another attachment or migration may have added pending work during planning.
        // Preserve its queue; the next launch will re-plan it with fresh classifications.
        if current.generation != self.generation
            || (!current.full_scan
                && current.target.as_deref() == Some(target)
                && current.paths == paths)
        {
            return Ok(());
        }
        write(
            store,
            &Pending {
                generation: uuid::Uuid::new_v4().to_string(),
                target: Some(target.into()),
                full_scan: false,
                paths,
            },
        )
    }
}
