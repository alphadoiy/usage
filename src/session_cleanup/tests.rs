use super::*;
use crate::sessions::AgentKind;

struct Fixture {
    root: PathBuf,
    store: Store,
    session: Session,
}
impl Fixture {
    fn new(kind: AgentKind) -> Self {
        let root = std::env::temp_dir().join(format!("usage-cleanup-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("home")).unwrap();
        let root = root.canonicalize().unwrap();
        let session = Session {
            agent: None,
            provider: kind,
            instance: "fixture-instance".into(),
            id: uuid::Uuid::new_v4().to_string(),
            cwd: root.clone(),
            title: "保留标题".into(),
            recency_at_ms: 1,
            home: root.join("home"),
        };
        Self {
            store: Store::at(root.join("trash")),
            root,
            session,
        }
    }
    fn put(&self, path: impl AsRef<Path>, text: &str) -> PathBuf {
        let path = self.session.home.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }
    fn claude(&self) -> PathBuf {
        self.put(format!("projects/project/{}.jsonl", self.session.id), &format!("{}\n", serde_json::json!({"type":"user","sessionId":self.session.id,"cwd":self.root,"message":{"content":"Task"}})))
    }
    fn old(&self) {
        age(&self.session.home);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn age(path: &Path) {
    if path.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            age(&entry.unwrap().path());
        }
    }
    File::open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(120)))
        .unwrap();
}

#[test]
fn claude_round_trip_includes_artifacts_and_is_scoped_to_home() {
    let f = Fixture::new(AgentKind::CLAUDE);
    let log = f.claude();
    let id = &f.session.id;
    let artifacts = [
        format!("projects/project/{id}/subagents/agent-one.jsonl"),
        format!("file-history/{id}/backup"),
        format!("session-env/{id}/env"),
        format!("todos/{id}-agent-one.json"),
    ];
    for path in &artifacts {
        f.put(path, "private artifact");
    }
    let untouched = f.put("projects/project/other.jsonl", "untouched");
    f.old();
    let entry = f.store.delete(&f.session).unwrap();
    assert!(!log.exists());
    assert!(untouched.exists());
    for path in &artifacts {
        assert!(!f.session.home.join(path).exists());
    }
    assert_eq!(f.store.list().unwrap().len(), 1);
    assert!(entry.bytes > 0);
    assert!(
        f.store
            .hidden("claude", &f.session.home)
            .unwrap()
            .contains(id)
    );
    assert!(f.store.hidden("codex", &f.session.home).unwrap().is_empty());
    assert!(f.store.hidden("claude", &f.root).unwrap().is_empty());
    let reopened = Store::at(f.store.path().to_path_buf());
    reopened.restore(&entry.key).unwrap();
    assert!(log.exists());
    for path in &artifacts {
        assert_eq!(
            fs::read_to_string(f.session.home.join(path)).unwrap(),
            "private artifact"
        );
    }
    assert!(reopened.list().unwrap().is_empty());
    assert!(
        reopened
            .hidden("claude", &f.session.home)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn recent_files_and_recent_artifacts_are_never_moved() {
    let f = Fixture::new(AgentKind::CLAUDE);
    let log = f.claude();
    assert!(f.store.delete(&f.session).unwrap_err().contains("一分钟"));
    f.old();
    let artifact = f.put(
        format!("projects/project/{}/subagents/recent.jsonl", f.session.id),
        "active",
    );
    assert!(f.store.delete(&f.session).unwrap_err().contains("一分钟"));
    assert!(log.exists() && artifact.exists());
    assert!(f.store.list().unwrap().is_empty());
}

#[test]
fn restore_preflights_every_destination_and_does_not_overwrite() {
    let f = Fixture::new(AgentKind::CLAUDE);
    let log = f.claude();
    let artifact = f.put(format!("session-env/{}/env", f.session.id), "old env");
    f.old();
    let entry = f.store.delete(&f.session).unwrap();
    fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    fs::write(&artifact, "new env").unwrap();
    assert!(
        f.store
            .restore(&entry.key)
            .unwrap_err()
            .contains("已有文件")
    );
    assert!(
        !log.exists(),
        "preflight must stop before restoring the first file"
    );
    assert_eq!(fs::read_to_string(&artifact).unwrap(), "new env");
    fs::remove_dir_all(artifact.parent().unwrap()).unwrap();
    f.store.restore(&entry.key).unwrap();
    assert!(log.exists());
    assert_eq!(fs::read_to_string(artifact).unwrap(), "old env");
}

#[test]
fn purged_sessions_keep_only_a_tombstone_and_cannot_reappear_from_native_index() {
    let f = Fixture::new(AgentKind::CODEX);
    let path = f.put(
        "sessions/day/rollout.jsonl",
        &format!(
            "{}\n",
            serde_json::json!({"type":"session_meta","payload":{"id":f.session.id,"cwd":f.root}})
        ),
    );
    f.old();
    let entry = f.store.delete(&f.session).unwrap();
    f.store.purge(&entry.key).unwrap();
    assert!(!path.exists());
    assert!(f.store.list().unwrap().is_empty());
    assert!(
        f.store
            .hidden("codex", &f.session.home)
            .unwrap()
            .contains(&f.session.id)
    );
    assert!(f.store.restore(&entry.key).is_err());
    let manifest = f.store.read(&entry.key).unwrap();
    assert!(
        manifest.title.is_empty()
            && manifest.cwd.as_os_str().is_empty()
            && manifest.paths.is_empty()
    );
    assert!(!f.store.dir(&entry.key).unwrap().join("files").exists());
    f.store.purge(&entry.key).unwrap();
}

#[test]
fn purging_a_missing_codex_home_preserves_legacy_child_ids_without_recreating_it() {
    for legacy in [false, true] {
        let f = Fixture::new(AgentKind::CODEX);
        let child = uuid::Uuid::new_v4().to_string();
        let grandchild = uuid::Uuid::new_v4().to_string();
        let metadata = |id: &str, parent: Option<&str>| {
            let mut payload = serde_json::json!({"id":id,"cwd":f.root});
            if let Some(parent) = parent {
                payload["source"] =
                    serde_json::json!({"subagent":{"thread_spawn":{"parent_thread_id":parent}}});
            }
            format!(
                "{}\n",
                serde_json::json!({"type":"session_meta","payload":payload})
            )
        };
        f.put("sessions/main.jsonl", &metadata(&f.session.id, None));
        f.put(
            "sessions/child.jsonl",
            &metadata(&child, Some(&f.session.id)),
        );
        // A valid plain twin is authoritative even when its in-progress archive is incomplete.
        f.put("sessions/child.jsonl.zst", "incomplete archive");
        fs::write(
            f.session.home.join("sessions/grandchild.jsonl.zst"),
            ruzstd::encoding::compress_to_vec(
                metadata(&grandchild, Some(&child)).as_bytes(),
                ruzstd::encoding::CompressionLevel::Fastest,
            ),
        )
        .unwrap();
        f.old();
        let entry = f.store.delete(&f.session).unwrap();
        if legacy {
            let mut manifest = serde_json::to_value(&entry).unwrap();
            manifest.as_object_mut().unwrap().remove("session_ids");
            fs::write(
                f.store.dir(&entry.key).unwrap().join("entry.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
        }
        fs::rename(&f.session.home, f.root.join("detached-home")).unwrap();
        f.store.purge(&entry.key).unwrap();
        assert!(
            !f.session.home.exists(),
            "purge must not recreate the original Home"
        );
        assert!(!f.store.dir(&entry.key).unwrap().join("files").exists());
        let hidden = f.store.hidden("codex", &f.session.home).unwrap();
        for id in [&f.session.id, &child, &grandchild] {
            assert!(
                hidden.contains(id),
                "child tombstones must survive legacy purge"
            );
        }
        f.store.purge(&entry.key).unwrap();
        assert!(!f.session.home.exists());
    }
}

#[test]
fn purged_tombstone_retry_does_not_reacquire_native_locks() {
    let f = Fixture::new(AgentKind::CODEX);
    f.put(
        "sessions/main.jsonl",
        &format!(
            "{}\n",
            serde_json::json!({"type":"session_meta","payload":{"id":f.session.id,"cwd":f.root}})
        ),
    );
    f.old();
    let entry = f.store.delete(&f.session).unwrap();
    f.store.purge(&entry.key).unwrap();
    let manifest = f.store.dir(&entry.key).unwrap().join("entry.json");
    let before = fs::read(&manifest).unwrap();
    let coordination = File::open(
        f.session
            .home
            .join("thread-writer-locks/.coordination.lock"),
    )
    .unwrap();
    coordination.lock_exclusive().unwrap();
    f.store.purge(&entry.key).unwrap();
    assert_eq!(fs::read(manifest).unwrap(), before);
}

#[test]
fn codex_moves_segments_compressed_twins_and_nested_subagents_but_keeps_independent_forks() {
    let f = Fixture::new(AgentKind::CODEX);
    let id = &f.session.id;
    let child = uuid::Uuid::new_v4().to_string();
    let grandchild = uuid::Uuid::new_v4().to_string();
    let fork = uuid::Uuid::new_v4().to_string();
    let mut moved = Vec::new();
    for (name, id, parent, forked) in [
        ("main", id, None, false),
        ("segment", id, None, false),
        ("child", &child, Some(id), false),
        ("grandchild", &grandchild, Some(&child), false),
        ("fork", &fork, Some(id), true),
    ] {
        let mut payload = serde_json::json!({"id":id,"cwd":f.root});
        if let Some(parent) = parent {
            if forked {
                payload["parent_thread_id"] = parent.as_str().into();
            } else {
                payload["source"] =
                    serde_json::json!({"subagent":{"thread_spawn":{"parent_thread_id":parent}}});
            }
        }
        let path = f.put(
            format!("sessions/day/{name}.jsonl"),
            &format!(
                "{}\n",
                serde_json::json!({"type":"session_meta","payload":payload})
            ),
        );
        if !forked {
            moved.push(path);
        }
    }
    let twin = f.put("sessions/day/main.jsonl.zst", "compression in progress");
    moved.push(twin);
    f.old();
    let entry = f.store.delete(&f.session).unwrap();
    assert_eq!(entry.paths.len(), moved.len());
    for path in &moved {
        assert!(!path.exists());
    }
    assert!(f.session.home.join("sessions/day/fork.jsonl").exists());
    f.store.restore(&entry.key).unwrap();
    for path in &moved {
        assert!(path.exists());
    }
}

#[test]
fn native_writer_locks_protect_idle_threads_children_and_restoration() {
    let f = Fixture::new(AgentKind::CODEX);
    let child = uuid::Uuid::new_v4().to_string();
    let main = f.put(
        "sessions/main.jsonl",
        &format!(
            "{}\n",
            serde_json::json!({
                "type":"session_meta", "payload":{"id":f.session.id,"cwd":f.root}
            })
        ),
    );
    let child_log = f.put(
        "sessions/child.jsonl",
        &format!(
            "{}\n",
            serde_json::json!({
                "type":"session_meta", "payload":{"id":child,"cwd":f.root,
                "source":{"subagent":{"thread_spawn":{"parent_thread_id":f.session.id}}}}
            })
        ),
    );
    f.old();
    let locks = f.session.home.join("thread-writer-locks");
    fs::create_dir_all(&locks).unwrap();
    let hold = |id: &str| {
        let file = File::create(locks.join(format!("{id}.lock"))).unwrap();
        file.lock_exclusive().unwrap();
        file
    };
    for id in [&f.session.id, &child] {
        let writer = hold(id);
        assert!(
            f.store
                .delete(&f.session)
                .unwrap_err()
                .contains("原生写入锁")
        );
        assert!(main.exists() && child_log.exists());
        assert!(f.store.list().unwrap().is_empty());
        drop(writer);
    }
    // Another live thread must not prevent cleaning this idle conversation.
    let _unrelated = hold(&uuid::Uuid::new_v4().to_string());
    let entry = f.store.delete(&f.session).unwrap();
    assert!(!main.exists() && !child_log.exists());
    assert!(entry.session_ids.contains(&child));
    let writer = hold(&child);
    assert!(
        f.store
            .restore(&entry.key)
            .unwrap_err()
            .contains("原生写入锁")
    );
    assert!(
        f.store
            .purge(&entry.key)
            .unwrap_err()
            .contains("原生写入锁")
    );
    drop(writer);
    f.store.restore(&entry.key).unwrap();
    assert!(main.exists() && child_log.exists());
    // Cleanup never deletes native lock files (which could have waiters).
    assert!(locks.join(format!("{child}.lock")).exists());
}

#[cfg(unix)]
#[test]
fn legacy_trash_restores_after_real_shared_migration_and_tombstones_follow_the_store() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new(AgentKind::CODEX);
    let mut manager = crate::codex_manager::Manager::open(f.root.join("manager")).unwrap();
    let config =
        crate::codex_manager::config::new_config("https://fixture.invalid/v1", "fixture", None)
            .unwrap();
    let account = manager.add("A", &config, "synthetic-key").unwrap();
    f.session.home = manager.materialize(&account.id, 1).unwrap();
    let metadata = format!(
        "{}\n",
        serde_json::json!({"type":"session_meta", "payload":{
            "id":f.session.id, "cwd":f.root, "source":"cli", "thread_source":"user"
        }})
    );
    let log = f.put("sessions/main.jsonl", &metadata);
    let child = uuid::Uuid::new_v4().to_string();
    let child_data = format!(
        "{}\n",
        serde_json::json!({"type":"session_meta", "payload":{
            "id":child, "cwd":f.root, "source":{"subagent":{"thread_spawn":{"parent_thread_id":f.session.id}}}
        }})
    );
    let archive = f.session.home.join("sessions/child.jsonl.zst");
    fs::write(
        &archive,
        ruzstd::encoding::compress_to_vec(
            child_data.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        ),
    )
    .unwrap();
    f.old();
    let entry = f.store.delete(&f.session).unwrap();
    // The persisted old-format entry contains no child-thread ownership list.
    let mut legacy = serde_json::to_value(&entry).unwrap();
    legacy.as_object_mut().unwrap().remove("session_ids");
    fs::write(
        f.store.dir(&entry.key).unwrap().join("entry.json"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    let binary = f.root.join("fake-codex");
    fs::write(
        &binary,
        "#!/bin/sh\nif [ \"$1\" = '--help' ]; then printf '%s\\n' '--no-daemon'; fi\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        manager
            .run(&account.id, &f.root, None, None, &binary, &[])
            .unwrap()
            .success()
    );
    let shared = crate::codex_native::SessionStore::resolve(&f.session.home)
        .root()
        .to_path_buf();
    assert_ne!(shared, f.session.home);
    assert!(
        f.store
            .hidden("codex", &shared)
            .unwrap()
            .contains(&f.session.id)
    );
    let lock = File::create(
        shared
            .join("thread-writer-locks")
            .join(format!("{child}.lock")),
    )
    .unwrap();
    lock.lock_exclusive().unwrap();
    assert!(
        f.store
            .restore(&entry.key)
            .unwrap_err()
            .contains("原生写入锁")
    );
    drop(lock);
    f.store.restore(&entry.key).unwrap();
    assert_eq!(fs::read_to_string(&log).unwrap(), metadata);
    assert!(archive.exists());
    assert!(f.store.hidden("codex", &shared).unwrap().is_empty());
    // A stale UI Session referring to the old Home also resolves the shared store.
    let entry = f.store.delete(&f.session).unwrap();
    f.store.purge(&entry.key).unwrap();
    assert!(
        f.store
            .hidden("codex", &shared)
            .unwrap()
            .contains(&f.session.id)
    );
    assert!(f.store.hidden("codex", &shared).unwrap().contains(&child));
    assert!(!log.exists() && !archive.exists());
}

#[test]
fn interrupted_move_is_recoverable_from_manifest_after_restart() {
    let f = Fixture::new(AgentKind::CLAUDE);
    let log = f.claude();
    f.put(format!("session-env/{}/env", f.session.id), "artifact");
    f.old();
    let entry = f.store.delete(&f.session).unwrap();
    let mut partial = entry.clone();
    partial.phase = Phase::Moving;
    let dir = f.store.dir(&entry.key).unwrap();
    let one = f.session.home.join(&entry.paths[1]);
    fs::rename(dir.join("files/1"), &one).unwrap();
    f.store.write(&partial).unwrap();
    assert!(f.store.purge(&entry.key).unwrap_err().contains("中断"));
    let reopened = Store::at(f.store.path().to_path_buf());
    reopened.restore(&entry.key).unwrap();
    assert!(log.exists() && one.exists());
    assert!(reopened.list().unwrap().is_empty());
}

#[test]
fn unsupported_agents_invalid_keys_and_manifests_cannot_delete_arbitrary_paths() {
    let f = Fixture::new(AgentKind::new(
        "custom",
        "Custom",
        None,
        "custom",
        "CUSTOM_HOME",
        3,
    ));
    assert!(f.store.delete(&f.session).unwrap_err().contains("暂不支持"));
    assert!(f.store.restore("../../outside").is_err());
    assert!(f.store.purge("../../outside").is_err());
    for path in [
        "../outside",
        "projects/../../outside",
        "/tmp/outside",
        "config.toml",
        "",
    ] {
        assert!(validate_relative(Path::new(path)).is_err());
    }
}

#[cfg(unix)]
#[test]
fn symlinks_in_session_paths_and_artifacts_are_refused() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new(AgentKind::CLAUDE);
    let log = f.claude();
    f.old();
    let outside = f.root.join("outside");
    fs::write(&outside, "keep").unwrap();
    let artifact = f
        .session
        .home
        .join(format!("projects/project/{}", f.session.id));
    symlink(&outside, &artifact).unwrap();
    assert!(f.store.delete(&f.session).unwrap_err().contains("符号链接"));
    assert!(log.exists());
    assert_eq!(fs::read_to_string(outside).unwrap(), "keep");
    assert!(f.store.list().unwrap().is_empty());
}

#[test]
fn exclusive_move_never_replaces_an_existing_file() {
    let f = Fixture::new(AgentKind::CLAUDE);
    let a = f.put("a", "a");
    let b = f.put("b", "b");
    assert!(move_exclusive(&a, &b).is_err());
    assert_eq!(fs::read_to_string(a).unwrap(), "a");
    assert_eq!(fs::read_to_string(b).unwrap(), "b");
}

#[test]
fn grok_moves_session_and_subagents_without_deleting_independent_forks() {
    let f = Fixture::new(AgentKind::GROK);
    let child = uuid::Uuid::new_v4().to_string();
    let fork = uuid::Uuid::new_v4().to_string();
    for (name, id) in [("main", &f.session.id), ("child", &child), ("fork", &fork)] {
        f.put(
            format!("sessions/project/{name}/summary.json"),
            &serde_json::json!({"info":{"id":id,"cwd":f.root},"parent_session_id":f.session.id})
                .to_string(),
        );
        f.put(
            format!("sessions/project/{name}/updates.jsonl"),
            "conversation",
        );
    }
    f.put(
        format!("sessions/project/main/subagents/{child}.json"),
        "{}",
    );
    f.old();
    let entry = f.store.delete(&f.session).unwrap();
    assert!(!f.session.home.join("sessions/project/main").exists());
    assert!(!f.session.home.join("sessions/project/child").exists());
    assert!(f.session.home.join("sessions/project/fork").exists());
    f.store.restore(&entry.key).unwrap();
    assert_eq!(
        fs::read_to_string(f.session.home.join("sessions/project/child/updates.jsonl")).unwrap(),
        "conversation"
    );
}

#[test]
fn native_codex_index_is_unchanged_and_registry_filters_deleted_rows() {
    let mut f = Fixture::new(AgentKind::CODEX);
    // Registry uses the application store, isolated by paths::data_dir under cfg(test).
    f.store = Store::default();
    f.put(
        "sessions/day/rollout.jsonl",
        &format!(
            "{}\n",
            serde_json::json!({"type":"session_meta","payload":{"id":f.session.id,"cwd":f.root}})
        ),
    );
    let db = rusqlite::Connection::open(f.session.home.join("state_5.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE threads(id TEXT, cwd TEXT, name TEXT, title TEXT, preview TEXT, recency_at_ms INTEGER, archived INTEGER, thread_source TEXT, source TEXT);").unwrap();
    db.execute(
        "INSERT INTO threads VALUES(?1,?2,'fixture','','',1,0,'user','cli')",
        rusqlite::params![f.session.id, f.root.to_string_lossy()],
    )
    .unwrap();
    f.old();
    let homes = crate::sessions::SessionHomes::new().with("codex", &f.session.home);
    let mut registry = crate::sessions::SessionRegistry::new();
    registry
        .register(crate::sessions::SessionSource {
            kind: AgentKind::CODEX,
            load: crate::sessions::load_codex,
        })
        .unwrap();
    assert_eq!(registry.load_all(&homes).sessions.len(), 1);
    let entry = f.store.delete(&f.session).unwrap();
    assert!(registry.load_all(&homes).sessions.is_empty());
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM threads WHERE archived=0", [], |r| r
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        1
    );
    f.store.restore(&entry.key).unwrap();
    assert_eq!(registry.load_all(&homes).sessions.len(), 1);
}
