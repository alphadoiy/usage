use super::*;
use crate::codex_manager::tests::{Fixture, native};
use crate::sessions::{SessionHomes, load_all};
use fs2::FileExt;
use std::fs::{self, File};

fn rollout(home: &Path, cwd: &Path, id: &str) -> PathBuf {
    let path = home
        .join("sessions/2026/10/28")
        .join(format!("rollout-2026-10-28T12-00-00-{id}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("{}\n{}\n",
        serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":cwd,"thread_source":"user","source":"cli"}}),
        serde_json::json!({"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"shared conversation"}]}})
    )).unwrap();
    path
}

#[test]
fn default_and_managed_providers_see_one_store_including_unindexed_legacy_sessions() {
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let a = manager
        .add("A", &native("https://a.invalid/v1", "a"), "key-a")
        .unwrap();
    let b = manager
        .add("B", &native("https://b.invalid/v1", "b"), "key-b")
        .unwrap();
    let home_a = manager.materialize(&a.id, 1).unwrap();
    let home_b = manager.materialize(&b.id, 1).unwrap();
    let base = context::base_home(manager.root());
    let ids: Vec<_> = (0..3).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    let old_path = rollout(&home_a, &fixture.path, &ids[0]);
    rollout(&home_b, &fixture.path, &ids[1]);
    rollout(&base, &fixture.path, &ids[2]);
    let content = fs::read(&old_path).unwrap();
    fs::create_dir_all(home_a.join("archived_sessions")).unwrap();
    fs::write(
        home_a.join("archived_sessions/archive.jsonl.zst"),
        b"fixture archive",
    )
    .unwrap();
    // An already-initialized native index must not hide imported rollouts.
    let conn = rusqlite::Connection::open(base.join("state_5.sqlite")).unwrap();
    conn.execute_batch("CREATE TABLE threads (id TEXT, cwd TEXT, name TEXT, title TEXT, preview TEXT, recency_at_ms INTEGER, archived INTEGER, source TEXT, thread_source TEXT);").unwrap();
    conn.execute(
        "INSERT INTO threads VALUES (?1,?2,'indexed title',NULL,NULL,1,0,'cli','user')",
        rusqlite::params![ids[2], fixture.path.to_string_lossy()],
    )
    .unwrap();
    prepare(&home_a, &base).unwrap();
    prepare(&home_b, &base).unwrap();
    prepare(&home_a, &base).unwrap();
    assert!(home_a.join("sessions").is_symlink());
    assert_eq!(fs::read(&old_path).unwrap(), content);
    assert_eq!(
        fs::read(base.join("archived_sessions/archive.jsonl.zst")).unwrap(),
        b"fixture archive"
    );
    for directory in [&base, &home_a, &home_b] {
        let records = crate::sessions::load_codex(directory).unwrap();
        assert_eq!(records.len(), 3);
        assert!(
            ids.iter()
                .all(|id| records.iter().any(|record| &record.id == id))
        );
        assert_eq!(
            records
                .iter()
                .find(|record| record.id == ids[2])
                .unwrap()
                .title,
            "indexed title"
        );
    }
    let catalog = load_all(
        &SessionHomes::new()
            .with("codex", &base)
            .with_instance("A", "codex", &home_a)
            .with_instance("B", "codex", &home_b),
    );
    assert!(catalog.warnings.is_empty(), "{:?}", catalog.warnings);
    assert_eq!(catalog.sessions.len(), 3);
    assert!(catalog.sessions.iter().all(|session| session.home == base));
    assert!(
        !fs::read_to_string(home_a.join("config.toml"))
            .unwrap()
            .contains("b.invalid")
    );
}

#[cfg(unix)]
#[test]
fn invalid_launch_inputs_do_not_materialize_or_migrate_any_account() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let target = manager
        .add("A", &native("https://a.invalid/v1", "a"), "key-a")
        .unwrap();
    let other = manager
        .add("B", &native("https://b.invalid/v1", "b"), "key-b")
        .unwrap();
    let target_home = manager.home(&target.id, 1).unwrap();
    let other_home = manager.materialize(&other.id, 1).unwrap();
    let old_path = rollout(
        &other_home,
        &fixture.path,
        &uuid::Uuid::new_v4().to_string(),
    );
    let binary = fixture.path.join("synthetic-codex");
    fs::write(
        &binary,
        "#!/bin/sh\nprintf '%s\\n' '--no-daemon --profile'\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let missing = fixture.path.join("missing");
    let remote = ["--remote".to_string(), "ws://localhost:9".to_string()];
    for (cwd, resume, program, args) in [
        (missing.as_path(), None, binary.as_path(), &[][..]),
        (binary.as_path(), None, binary.as_path(), &[][..]),
        (
            fixture.path.as_path(),
            Some("invalid-id"),
            binary.as_path(),
            &[][..],
        ),
        (fixture.path.as_path(), None, missing.as_path(), &[][..]),
        (
            fixture.path.as_path(),
            None,
            binary.as_path(),
            remote.as_slice(),
        ),
    ] {
        assert!(
            manager
                .run(&target.id, cwd, None, resume, program, args)
                .is_err()
        );
        assert!(
            !target_home.exists(),
            "invalid inputs materialized the target Home"
        );
        assert!(
            !other_home.join("sessions").is_symlink(),
            "invalid inputs migrated another account"
        );
        assert!(old_path.is_file());
        assert!(!SessionStore::resolve(&context::base_home(manager.root())).merge_rollouts());
        assert!(manager.recent_runs(10).unwrap().is_empty());
    }
}

#[test]
fn active_legacy_writer_and_conflicts_leave_original_sessions_intact() {
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let account = manager
        .add("A", &native("https://a.invalid/v1", "a"), "key-a")
        .unwrap();
    let account_home = manager.materialize(&account.id, 1).unwrap();
    let base = context::base_home(manager.root());
    let id = uuid::Uuid::new_v4().to_string();
    let old_path = rollout(&account_home, &fixture.path, &id);
    fs::create_dir_all(account_home.join("thread-writer-locks")).unwrap();
    let lock = File::create(
        account_home
            .join("thread-writer-locks")
            .join(format!("{id}.lock")),
    )
    .unwrap();
    lock.lock_exclusive().unwrap();
    assert!(
        prepare(&account_home, &base)
            .unwrap_err()
            .to_string()
            .contains("运行中的旧会话")
    );
    assert!(!account_home.join("sessions").is_symlink());
    assert!(old_path.exists());
    drop(lock);
    let conflict = rollout(&base, &fixture.path, &id);
    fs::write(&conflict, b"different data").unwrap();
    assert!(prepare(&account_home, &base).is_err());
    assert_eq!(fs::read(conflict).unwrap(), b"different data");
    assert!(old_path.exists());
    assert!(!account_home.join("sessions").is_symlink());
}

#[test]
fn migration_can_resume_after_a_file_was_linked_but_not_unlinked() {
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let account = manager
        .add("A", &native("https://a.invalid/v1", "a"), "key-a")
        .unwrap();
    let account_home = manager.materialize(&account.id, 1).unwrap();
    let base = context::base_home(manager.root());
    let path = rollout(
        &account_home,
        &fixture.path,
        &uuid::Uuid::new_v4().to_string(),
    );
    let destination = base.join(path.strip_prefix(&account_home).unwrap());
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::hard_link(&path, &destination).unwrap();
    prepare(&account_home, &base).unwrap();
    assert_eq!(fs::read(&path).unwrap(), fs::read(destination).unwrap());
    assert_eq!(SessionStore::resolve(&account_home).root(), base);
}

#[cfg(unix)]
#[test]
fn resume_uses_the_target_provider_credentials_model_and_shared_transcript() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let a = manager
        .add("A", &native("https://a.invalid/v1", "model-a"), "key-a")
        .unwrap();
    let b = manager
        .add(
            "B",
            &native("https://b.invalid/v1", "model-b").replace("usage_byok", "second_provider"),
            "key-b",
        )
        .unwrap();
    let home_a = manager.materialize(&a.id, 1).unwrap();
    let home_b = manager.materialize(&b.id, 1).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    rollout(&home_a, &fixture.path, &id);
    let runtime = fs::read_to_string(home_b.join("config.toml"))
        .unwrap()
        .replace("model-b", "native-b-choice");
    fs::write(home_b.join("config.toml"), &runtime).unwrap();
    let binary = fixture.path.join("codex-fixture");
    fs::write(&binary, r#"#!/usr/bin/env python3
import json,os,pathlib,sys
if sys.argv[1:] == ['--help']:
    print('--no-daemon'); sys.exit(0)
args=sys.argv[1:]
thread=args[args.index('resume')+1]
home=pathlib.Path(os.environ['CODEX_HOME'])
transcript=next((home/'sessions').rglob('*'+thread+'.jsonl')).read_text()
pathlib.Path('capture.json').write_text(json.dumps({'args':args,'key':os.environ['USAGE_CODEX_API_KEY'],
    'home':str(home),'transcript':transcript,'config':(home/'config.toml').read_text()}))
"#).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        manager
            .run(&b.id, &fixture.path, None, Some(&id), &binary, &[])
            .unwrap()
            .success()
    );
    let capture: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.path.join("capture.json")).unwrap()).unwrap();
    assert_eq!(capture["key"], "key-b");
    assert_eq!(capture["home"].as_str(), home_b.to_str());
    assert!(
        capture["transcript"]
            .as_str()
            .unwrap()
            .contains("shared conversation")
    );
    assert!(
        capture["config"]
            .as_str()
            .unwrap()
            .contains("https://b.invalid/v1")
    );
    let args = capture["args"].as_array().unwrap();
    assert!(
        args.iter()
            .any(|arg| arg == "model_provider=\"second_provider\"")
    );
    assert!(args.iter().any(|arg| arg == "model=\"native-b-choice\""));
    assert!(args.iter().any(|arg| {
        arg.as_str()
            .is_some_and(|s| s.starts_with("sqlite_home=") && s.contains("user-codex"))
    }));
    assert_eq!(
        fs::read_to_string(home_b.join("config.toml")).unwrap(),
        runtime
    );
}
