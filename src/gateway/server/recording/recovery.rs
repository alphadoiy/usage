//! Small immutable batches preserve source decisions across a failed metrics store and restart.
use super::*;
use std::io::Read;

const MAX_BYTES: u64 = 64 * 1024;
const MAX_SOURCES: usize = 256;

#[derive(Serialize, Deserialize)]
struct Batch {
    version: u32,
    sources: Vec<Key>,
}
impl Batch {
    fn valid(&self) -> bool {
        self.version == 1
            && !self.sources.is_empty()
            && self.sources.len() <= MAX_SOURCES
            && self.sources.iter().all(|(scope, run)| {
                scope.len() == 64
                    && scope.bytes().all(|b| b.is_ascii_hexdigit())
                    && uuid::Uuid::parse_str(run).is_ok()
            })
    }
}

#[derive(Clone)]
pub(super) struct Recovery {
    directory: PathBuf,
}
impl Recovery {
    pub fn new(directory: &Path) -> Self {
        Self {
            directory: directory.join("pending-sources"),
        }
    }
    pub fn save(&self, mut sources: Vec<Key>) -> Result<(), &'static str> {
        if sources.is_empty() {
            return Ok(());
        }
        sources.sort_unstable();
        sources.dedup();
        let batch = Batch {
            version: 1,
            sources,
        };
        if !batch.valid() {
            return Err("invalid_source_batch");
        }
        private_dir(&self.directory).map_err(|_| "recovery_directory_unavailable")?;
        let bytes = serde_json::to_vec(&batch).map_err(|_| "recovery_encoding_failed")?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("recovery_batch_too_large");
        }
        // Never replace an earlier shutdown's decisions if its database is still unavailable.
        let path = self
            .directory
            .join(format!("{}.json", uuid::Uuid::new_v4()));
        crate::codex_manager::private_write(&path, &bytes).map_err(|_| "recovery_write_failed")?;
        File::open(&self.directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| "recovery_sync_failed")
    }
    pub fn replay_one(&self, service: &crate::performance::Service) -> Result<bool, &'static str> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err("recovery_directory_unavailable"),
        };
        for entry in entries {
            let path = entry.map_err(|_| "recovery_read_failed")?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let mut bytes = Vec::new();
            File::open(&path)
                .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
                .map_err(|_| "recovery_read_failed")?;
            let batch = (bytes.len() as u64 <= MAX_BYTES)
                .then(|| serde_json::from_slice::<Batch>(&bytes).ok())
                .flatten()
                .filter(Batch::valid);
            let Some(batch) = batch else {
                // Preserve damaged input for diagnosis without starving subsequent valid batches.
                fs::rename(&path, path.with_extension("invalid"))
                    .map_err(|_| "recovery_quarantine_failed")?;
                return Err("invalid_recovery_batch_preserved");
            };
            // The database transaction deduplicates both within and across immutable batches.
            service
                .mark_gateway_cli_sources(&batch.sources)
                .map_err(|_| "metrics_store_unavailable")?;
            fs::remove_file(path).map_err(|_| "recovery_remove_failed")?;
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_batches_preserve_old_decisions_and_replay_idempotently() {
        let f = super::super::tests::Fixture::new();
        let recovery = Recovery::new(&f.root.join("gateway"));
        let first = ("a".repeat(64), uuid::Uuid::new_v4().to_string());
        let second = ("b".repeat(64), uuid::Uuid::new_v4().to_string());
        recovery.save(vec![first.clone(), first.clone()]).unwrap();
        recovery.save(vec![first, second]).unwrap();
        // A failed restart must retain every previous batch.
        fs::write(f.root.join("index"), b"block storage").unwrap();
        assert!(recovery.replay_one(&f.service).is_err());
        assert_eq!(fs::read_dir(&recovery.directory).unwrap().count(), 2);
        fs::remove_file(f.root.join("index")).unwrap();
        assert!(recovery.replay_one(&f.service).unwrap());
        assert!(recovery.replay_one(&f.service).unwrap());
        assert!(!recovery.replay_one(&f.service).unwrap());
        assert_eq!(f.markers(), 2);
    }

    #[test]
    fn malformed_and_oversized_recovery_files_are_preserved_without_replay() {
        let f = super::super::tests::Fixture::new();
        let recovery = Recovery::new(&f.root.join("gateway"));
        private_dir(&recovery.directory).unwrap();
        for bytes in [
            b"not json".to_vec(),
            vec![b'x'; MAX_BYTES as usize + 1],
            br#"{"version":1,"sources":[["not-a-scope","not-a-run"]]}"#.to_vec(),
        ] {
            let path = recovery
                .directory
                .join(format!("{}.json", uuid::Uuid::new_v4()));
            fs::write(&path, bytes).unwrap();
            assert_eq!(
                recovery.replay_one(&f.service),
                Err("invalid_recovery_batch_preserved")
            );
            assert!(path.with_extension("invalid").exists());
        }
        assert!(!f.path.exists(), "invalid data must not create metrics");
        recovery
            .save(vec![("a".repeat(64), uuid::Uuid::new_v4().to_string())])
            .unwrap();
        assert!(recovery.replay_one(&f.service).unwrap());
        assert_eq!(f.markers(), 1);
    }
}
