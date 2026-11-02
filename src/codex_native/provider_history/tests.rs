use super::*;
use fs2::FileExt;
use std::fs::File;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("usage-provider-history-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }
    fn store(&self) -> SessionStore {
        SessionStore::resolve_in(&self.0, &self.0, None, false)
    }
    fn rollout(
        &self,
        provider: &str,
        archive: bool,
        compressed: bool,
    ) -> (String, PathBuf, Vec<u8>) {
        let id = uuid::Uuid::new_v4().to_string();
        let folder = self.0.join(if archive {
            "archived_sessions"
        } else {
            "sessions/2026/11/02"
        });
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join(format!(
            "rollout-{id}.jsonl{}",
            if compressed { ".zst" } else { "" }
        ));
        let mut bytes = serde_json::to_vec(&serde_json::json!({"type":"session_meta","payload":{"id":id,"model_provider":provider,"cwd":"/synthetic/project"}})).unwrap();
        bytes.extend_from_slice(b"\r\n{\"type\":\"response_item\", \"payload\":{\"type\":\"reasoning\",\"encrypted_content\":\"keep-byte-for-byte\"}}\n{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"fixture conversation\"}]}}\n");
        let raw = if compressed {
            // The history body is in a separate frame; single-frame decoding would
            // silently lose it during migration or backup verification.
            let split = bytes.iter().position(|b| *b == b'\n').unwrap() + 1;
            let mut frames = ruzstd::encoding::compress_to_vec(
                &bytes[..split],
                ruzstd::encoding::CompressionLevel::Fastest,
            );
            frames.extend(ruzstd::encoding::compress_to_vec(
                &bytes[split..],
                ruzstd::encoding::CompressionLevel::Fastest,
            ));
            frames
        } else {
            bytes.clone()
        };
        fs::write(&path, raw).unwrap();
        (id, path, bytes)
    }
    fn index(&self, version: u32, entries: &[(&str, &str)]) -> Connection {
        let conn = Connection::open(self.0.join(format!("state_{version}.sqlite"))).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE threads(id TEXT PRIMARY KEY,model_provider TEXT, title TEXT);").unwrap();
        for (id, provider) in entries {
            conn.execute(
                "INSERT INTO threads VALUES (?1,?2,'unchanged fixture title')",
                params![id, provider],
            )
            .unwrap();
        }
        conn
    }
    fn migrate(&self, dry_run: bool) -> ProviderHistoryReport {
        unify(
            &self.store(),
            "custom",
            &self.0.join("backups"),
            dry_run,
            Scan::All,
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn migration_unifies_all_third_party_formats_and_indexes_but_preserves_official_history() {
    let f = Fixture::new();
    let (byok, byok_path, byok_bytes) = f.rollout("usage_byok", false, false);
    let (magpie, magpie_path, magpie_bytes) = f.rollout("magpie", true, true);
    let (_, unindexed, _) = f.rollout("imported-vendor", false, false);
    let (official, official_path, official_bytes) = f.rollout("openai", false, false);
    let first = f.index(
        5,
        &[
            (&byok, "usage_byok"),
            (&magpie, "magpie"),
            (&official, "openai"),
        ],
    );
    let second = f.index(
        6,
        &[
            (&byok, "usage_byok"),
            (&magpie, "magpie"),
            (&official, "openai"),
        ],
    );
    let modified = fs::metadata(&byok_path).unwrap().modified().unwrap();
    let preview = f.migrate(true);
    assert_eq!((preview.files, preview.index_rows), (3, 4));
    assert!(!f.0.join("backups").exists());
    assert!(!f.0.join("thread-writer-locks").exists());
    assert_eq!(fs::read(&byok_path).unwrap(), byok_bytes);
    let report = f.migrate(false);
    assert_eq!(
        (report.files, report.index_rows, report.deferred),
        (3, 4, 0)
    );
    let backup = report.backup.unwrap();
    for (path, original) in [(&byok_path, &byok_bytes), (&magpie_path, &magpie_bytes)] {
        let mut current = Vec::new();
        reader(path).unwrap().read_to_end(&mut current).unwrap();
        let old = header(original).unwrap();
        let new = header(&current).unwrap();
        assert_eq!(new.provider, "custom");
        assert_eq!(new.id, old.id);
        assert_eq!(&current[new.range.end..], &original[old.range.end..]);
        assert!(current[new.range.clone()].ends_with(b"\r\n"));
        let archived = backup
            .join("rollouts")
            .join(path.strip_prefix(&f.0).unwrap());
        let mut original_backup = Vec::new();
        reader(&archived)
            .unwrap()
            .read_to_end(&mut original_backup)
            .unwrap();
        assert_eq!(&original_backup, original);
    }
    assert_eq!(read_header(&unindexed).unwrap().unwrap().provider, "custom");
    assert_eq!(fs::read(official_path).unwrap(), official_bytes);
    assert_eq!(
        fs::metadata(byok_path).unwrap().modified().unwrap(),
        modified
    );
    for conn in [&first, &second] {
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT id,model_provider FROM threads ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            rows.iter()
                .all(|(id, p)| p == if id == &official { "openai" } else { "custom" })
        );
    }
    // SQLite backup must contain rows from the live WAL, before any migration.
    let snapshot = Connection::open(backup.join("state_5.sqlite")).unwrap();
    assert_eq!(
        snapshot
            .query_row(
                "SELECT model_provider FROM threads WHERE id=?1",
                [byok],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "usage_byok"
    );
    let again = f.migrate(false);
    assert_eq!((again.files, again.index_rows), (0, 0));
    assert!(again.backup.is_none());
}

#[test]
fn active_writers_are_deferred_until_their_native_lease_is_released() {
    let f = Fixture::new();
    let (id, path, bytes) = f.rollout("magpie", false, false);
    let conn = f.index(5, &[(&id, "magpie")]);
    fs::create_dir_all(f.0.join("thread-writer-locks")).unwrap();
    let lease = File::create(f.0.join("thread-writer-locks").join(format!("{id}.lock"))).unwrap();
    lease.lock_exclusive().unwrap();
    let report = f.migrate(false);
    assert_eq!(
        (report.files, report.index_rows, report.deferred),
        (0, 0, 1)
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(
        conn.query_row("SELECT model_provider FROM threads", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "magpie"
    );
    assert!(!f.0.join("backups").exists());
    drop(lease);
    assert_eq!(f.migrate(false).files, 1);
}

#[test]
fn invalid_trailing_compressed_frame_never_discards_history_or_changes_its_index() {
    let f = Fixture::new();
    let (id, path, _) = f.rollout("magpie", false, true);
    let mut original = fs::read(&path).unwrap();
    original.extend_from_slice(b"invalid-trailing-zstd-frame");
    fs::write(&path, &original).unwrap();
    let conn = f.index(5, &[(&id, "magpie")]);
    let report = f.migrate(false);
    assert_eq!(
        (report.files, report.index_rows, report.deferred),
        (0, 0, 1)
    );
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(
        conn.query_row("SELECT model_provider FROM threads", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "magpie"
    );
    assert!(report.backup.is_none());
}

#[test]
fn interrupted_index_and_file_updates_can_reconcile_without_touching_official_conflicts() {
    let f = Fixture::new();
    let (file_done, _, _) = f.rollout("custom", false, false);
    let (index_done, path, _) = f.rollout("usage_byok", false, false);
    let (conflict, protected, bytes) = f.rollout("magpie", false, false);
    let conn = f.index(
        5,
        &[
            (&file_done, "magpie"),
            (&index_done, "custom"),
            (&conflict, "openai"),
        ],
    );
    let report = f.migrate(false);
    assert_eq!((report.files, report.index_rows), (1, 1));
    assert_eq!(read_header(&path).unwrap().unwrap().provider, "custom");
    assert_eq!(fs::read(protected).unwrap(), bytes);
    assert_eq!(
        conn.query_row(
            "SELECT model_provider FROM threads WHERE id=?1",
            [file_done],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "custom"
    );
    assert_eq!(
        conn.query_row(
            "SELECT model_provider FROM threads WHERE id=?1",
            [conflict],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "openai"
    );
}

#[test]
fn rejected_index_update_rolls_back_all_indexes_before_any_transcript_write() {
    let f = Fixture::new();
    let (id, path, original) = f.rollout("usage_byok", false, false);
    let first = f.index(5, &[(&id, "usage_byok")]);
    let second = f.index(6, &[(&id, "usage_byok")]);
    second.execute_batch("CREATE TRIGGER reject_classification BEFORE UPDATE OF model_provider ON threads BEGIN UPDATE threads SET model_provider='openai' WHERE id=OLD.id; SELECT RAISE(IGNORE); END;").unwrap();
    let error = unify(&f.store(), "custom", &f.0.join("backups"), false, Scan::All).unwrap_err();
    assert!(error.contains("冲突"), "{error}");
    assert_eq!(fs::read(&path).unwrap(), original);
    for conn in [&first, &second] {
        assert_eq!(provider(conn, &id).unwrap().as_deref(), Some("usage_byok"));
    }
    let generation = fs::read_dir(f.0.join("backups"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let journal: Value = serde_json::from_slice(
        &fs::read(generation.join("changes").join(format!("{id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(journal["phase"], "prepared");
}

#[test]
fn duplicate_transcripts_share_one_writer_lease_and_one_index_update() {
    let f = Fixture::new();
    let (id, path, _) = f.rollout("magpie", false, false);
    let duplicate = f.0.join("archived_sessions/duplicate.jsonl");
    fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
    fs::copy(&path, &duplicate).unwrap();
    let conn = f.index(5, &[(&id, "magpie")]);
    fs::create_dir_all(f.0.join("thread-writer-locks")).unwrap();
    File::create(f.0.join("thread-writer-locks").join(format!("{id}.lock"))).unwrap();
    let preview = f.migrate(true);
    assert_eq!((preview.files, preview.index_rows), (2, 1));
    let report = f.migrate(false);
    assert_eq!(
        (report.files, report.index_rows, report.deferred),
        (2, 1, 0)
    );
    assert_eq!(read_header(&path).unwrap().unwrap().provider, "custom");
    assert_eq!(read_header(&duplicate).unwrap().unwrap().provider, "custom");
    assert_eq!(provider(&conn, &id).unwrap().as_deref(), Some("custom"));
}

#[test]
fn official_classification_added_after_planning_fences_the_conversation() {
    let f = Fixture::new();
    let (id, path, original) = f.rollout("magpie", false, false);
    let conn = f.index(5, &[(&id, "magpie")]);
    let store = f.store();
    let mut indexes = indexes(store.index_dir(), false).unwrap();
    let pending = pending::Pending::snapshot(&store, false).unwrap();
    let mut planned = plan(&store, &indexes, "custom", Scan::All, false, &pending).unwrap();
    conn.execute("UPDATE threads SET model_provider='openai'", [])
        .unwrap();
    let mut backup = apply::Backup::new(&f.0.join("backups"), &store, "custom");
    assert!(matches!(
        apply::session(
            &store,
            &mut indexes,
            planned.sessions.remove(0),
            "custom",
            &mut backup
        )
        .unwrap(),
        apply::Outcome::Unchanged
    ));
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(provider(&conn, &id).unwrap().as_deref(), Some("openai"));
    assert!(backup.created_path().is_none());
}

fn add_native_paths(conn: &Connection, id: &str, path: &Path) {
    conn.execute_batch("ALTER TABLE threads ADD COLUMN rollout_path TEXT;")
        .unwrap();
    conn.execute(
        "UPDATE threads SET rollout_path=?1 WHERE id=?2",
        params![path.to_string_lossy(), id],
    )
    .unwrap();
}

#[test]
fn indexed_maintenance_reads_only_pending_rows_and_explicit_migration_finds_unindexed_history() {
    let f = Fixture::new();
    let (indexed, indexed_path, _) = f.rollout("magpie", false, false);
    let conn = f.index(5, &[(&indexed, "magpie")]);
    add_native_paths(&conn, &indexed, &indexed_path);
    let report = f
        .store()
        .prepare_third_party("custom", &f.0.join("backups"))
        .unwrap();
    assert_eq!((report.files, report.index_rows), (1, 1));
    let (_, unindexed, original) = f.rollout("usage_byok", false, false);
    // A file outside indexed candidates is not even decoded during normal launch.
    fs::write(
        indexed_path.parent().unwrap().join("unreadable.jsonl.zst"),
        b"invalid",
    )
    .unwrap();
    let report = f
        .store()
        .prepare_third_party("custom", &f.0.join("backups"))
        .unwrap();
    assert_eq!(
        (report.files, report.index_rows, report.deferred),
        (0, 0, 0)
    );
    assert_eq!(fs::read(&unindexed).unwrap(), original);
    let report = f.migrate(false);
    assert_eq!(
        (report.files, report.index_rows, report.deferred),
        (1, 0, 1)
    );
    assert_eq!(read_header(&unindexed).unwrap().unwrap().provider, "custom");
}

#[test]
fn deferred_unindexed_import_is_retried_without_rescanning_the_entire_store() {
    let f = Fixture::new();
    let (id, path, _) = f.rollout("magpie", false, false);
    let conn = f.index(5, &[]);
    conn.execute_batch("ALTER TABLE threads ADD COLUMN rollout_path TEXT;")
        .unwrap();
    let store = f.store();
    {
        let _guard = store.mutation_guard().unwrap();
        schedule_full_scan(&store).unwrap();
    }
    let lease = File::create(f.0.join("thread-writer-locks").join(format!("{id}.lock"))).unwrap();
    lease.lock_exclusive().unwrap();
    let report = store
        .prepare_third_party("custom", &f.0.join("backups"))
        .unwrap();
    assert_eq!(report.deferred_reasons.get("busy"), Some(&1));
    drop(lease);
    fs::write(
        path.parent().unwrap().join("unrelated.jsonl.zst"),
        b"invalid",
    )
    .unwrap();
    let report = store
        .prepare_third_party("custom", &f.0.join("backups"))
        .unwrap();
    assert_eq!(
        (report.files, report.index_rows, report.deferred),
        (1, 0, 0)
    );
    assert_eq!(read_header(&path).unwrap().unwrap().provider, "custom");
}

#[test]
fn an_attachment_during_planning_keeps_its_full_scan_request() {
    let f = Fixture::new();
    let store = f.store();
    let before = pending::Pending::snapshot(&store, false).unwrap();
    {
        let _guard = store.mutation_guard().unwrap();
        schedule_full_scan(&store).unwrap();
    }
    before.finish(&store, "custom", BTreeSet::new()).unwrap();
    let after = pending::Pending::snapshot(&store, false).unwrap();
    assert!(matches!(after.scan(Scan::Indexed, "custom"), Scan::All));
}

#[test]
fn indexed_paths_outside_the_store_are_never_migrated() {
    let f = Fixture::new();
    let outside = Fixture::new();
    let (id, path, original) = outside.rollout("magpie", false, false);
    let conn = f.index(5, &[(&id, "magpie")]);
    add_native_paths(&conn, &id, &path);
    pending::Pending::snapshot(&f.store(), false)
        .unwrap()
        .finish(&f.store(), "custom", BTreeSet::new())
        .unwrap();
    assert!(
        f.store()
            .prepare_third_party("custom", &f.0.join("backups"))
            .unwrap_err()
            .contains("共享目录之外")
    );
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(provider(&conn, &id).unwrap().as_deref(), Some("magpie"));
}

#[test]
fn later_legacy_attachment_schedules_unindexed_history_even_after_initial_migration() {
    let f = Fixture::new();
    let legacy = Fixture::new();
    let (id, old_path, _) = legacy.rollout("magpie", false, false);
    let conn = f.index(5, &[]);
    conn.execute_batch("ALTER TABLE threads ADD COLUMN rollout_path TEXT;")
        .unwrap();
    let store = f.store();
    store
        .prepare_third_party("custom", &f.0.join("backups"))
        .unwrap();
    store.attach_legacy_home(&legacy.0).unwrap();
    let report = store
        .prepare_third_party("custom", &f.0.join("backups"))
        .unwrap();
    assert_eq!((report.files, report.index_rows), (1, 0));
    assert_eq!(read_header(&old_path).unwrap().unwrap().provider, "custom");
    assert_eq!(read_header(&old_path).unwrap().unwrap().id, id);
}

#[test]
fn staged_paths_recover_duplicate_transcripts_after_an_interrupted_write() {
    let f = Fixture::new();
    let (id, path, _) = f.rollout("magpie", false, false);
    let duplicate = f.0.join("archived_sessions/duplicate.jsonl");
    fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
    fs::copy(&path, &duplicate).unwrap();
    let conn = f.index(5, &[(&id, "magpie")]);
    add_native_paths(&conn, &id, &path);
    let store = f.store();
    pending::Pending::snapshot(&store, false)
        .unwrap()
        .finish(&store, "custom", BTreeSet::new())
        .unwrap();
    let pending = pending::Pending::snapshot(&store, false).unwrap();
    // Emulate termination after one transcript replacement, before SQLite commits
    // or the final retry queue is written. Its other copy is not in the native index.
    pending
        .stage(&store, BTreeSet::from([path.clone(), duplicate.clone()]))
        .unwrap();
    let changed = fs::read_to_string(&path)
        .unwrap()
        .replace("magpie", "custom");
    fs::write(&path, changed).unwrap();
    fs::write(
        path.parent().unwrap().join("unrelated.jsonl.zst"),
        b"invalid",
    )
    .unwrap();
    let report = store
        .prepare_third_party("custom", &f.0.join("backups"))
        .unwrap();
    assert_eq!(
        (report.files, report.index_rows, report.deferred),
        (1, 1, 0)
    );
    assert_eq!(read_header(&duplicate).unwrap().unwrap().provider, "custom");
    assert_eq!(provider(&conn, &id).unwrap().as_deref(), Some("custom"));
}
