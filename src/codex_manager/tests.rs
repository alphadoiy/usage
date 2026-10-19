// Synthetic credentials only. No access to real Keychain items or user configurations.
use super::*;
use serde_json::json;

pub(super) struct Fixture {
    pub(super) path: PathBuf,
}
impl Fixture {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("usage-codex-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self {
            path: path.canonicalize().unwrap(),
        }
    }
    pub(super) fn manager(&self) -> Manager {
        Manager::open(self.path.join("manager")).unwrap()
    }
    fn source(&self) -> Connection {
        let conn = Connection::open(self.path.join("cc-switch.db")).unwrap();
        conn.execute_batch("CREATE TABLE providers(id TEXT,app_type TEXT,name TEXT,settings_config TEXT,meta TEXT DEFAULT '{}',PRIMARY KEY(id,app_type)); CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT);").unwrap();
        conn
    }
    fn db(&self) -> PathBuf {
        self.path.join("cc-switch.db")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
pub(super) fn native(url: &str, model: &str) -> String {
    config::new_config(url, model, Some("high")).unwrap()
}

#[test]
fn add_rejects_equivalent_provider_configs_but_keeps_distinct_keys_models_and_explicit_copies() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let text = native("https://example.com/v1", "model-a");
    let first = manager.add("Existing", &text, "synthetic-key").unwrap();
    for candidate in [
        text.clone(),
        format!("# another comment\n{text}"),
        native("https://example.com/v1/", "model-a"),
    ] {
        let error = manager
            .add("Another name", &candidate, "synthetic-key")
            .unwrap_err()
            .to_string();
        assert!(error.contains("Provider 配置已存在：Existing"));
        assert!(!error.contains("synthetic-key"));
    }
    assert_eq!(manager.list().unwrap().len(), 1);
    assert_eq!(
        fs::read_dir(manager.root().join("keys")).unwrap().count(),
        1
    );
    manager.add("Other key", &text, "different-key").unwrap();
    manager
        .add(
            "Other model",
            &native("https://example.com/v1", "model-b"),
            "synthetic-key",
        )
        .unwrap();
    manager
        .add(
            "Other effort",
            &config::new_config("https://example.com/v1", "model-a", Some("low")).unwrap(),
            "synthetic-key",
        )
        .unwrap();
    manager.copy(&first.id, "Explicit copy").unwrap();
    assert_eq!(manager.list().unwrap().len(), 5);
    manager.remove(&first.id).unwrap();
    let copy = manager.resolve("Explicit copy").unwrap();
    manager.remove(&copy.id).unwrap();
    manager.add("Restored", &text, "synthetic-key").unwrap();
}

#[test]
fn simultaneous_provider_adds_cannot_save_duplicate_profiles_or_orphan_keys() {
    let f = Fixture::new();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|n| {
            let mut manager = f.manager();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                manager.add(
                    &format!("Provider-{n}"),
                    &native("https://example.com/v1", "model-a"),
                    "synthetic-key",
                )
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(f.manager().list().unwrap().len(), 1);
    assert_eq!(
        fs::read_dir(f.path.join("manager/keys")).unwrap().count(),
        1
    );
}

#[test]
fn overview_read_only_discovery_does_not_create_or_migrate_store() {
    let f = Fixture::new();
    let missing = f.path.join("missing");
    assert!(Manager::open_read_only(missing.clone()).unwrap().is_none());
    assert!(!missing.exists());
    let manager = f.manager();
    manager.conn.execute_batch("DROP TABLE run_contexts; DROP TABLE run_events; DROP TABLE run_sessions;
        INSERT INTO profiles(id,name,revision) VALUES('legacy','Legacy',1);
        INSERT INTO revisions VALUES('legacy',1,'','unused','2026-10-16T00:00:00Z');
        INSERT INTO runs VALUES('legacy-run','legacy',1,'/legacy','2026-10-16T00:00:00Z',NULL,NULL,NULL);").unwrap();
    let reader = Manager::open_read_only(manager.root().to_owned())
        .unwrap()
        .unwrap();
    let runs = reader.recent_runs(10).unwrap();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].context.is_none());
    assert!(runs[0].process_identity.is_none());
    assert!(runs[0].sessions.is_empty());
    assert!(reader.conn.execute("DELETE FROM runs", []).is_err());
    assert_eq!(
        manager
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='run_contexts'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn launch_identity_applies_toml_paths_and_replaces_table_assignments_exactly() {
    let f = Fixture::new();
    fs::create_dir(f.path.join(".git")).unwrap();
    let text = native("https://a.invalid/v1", "saved");
    let args = [
        "--config=model='first'",
        "-cmodel='second'",
        "-c",
        "model_providers.\"a.b\"={base_url='https://b.invalid/v1',env_key='OTHER'}",
        "-c",
        "model_provider='a.b'",
        "--model=flag-model",
    ];
    let resolution = effective::resolve(
        &text,
        AuthMode::Byok,
        &f.path,
        false,
        &args.map(str::to_string),
        Some("managed-key"),
        &|name| (name == "OTHER").then(|| "other-key".into()),
    );
    assert_eq!(
        resolution.identity.state,
        crate::runs::IdentityState::Resolved
    );
    assert_eq!(resolution.identity.model.as_deref(), Some("flag-model"));
    assert_eq!(
        resolution.identity.endpoint.as_deref(),
        Some("https://b.invalid/v1")
    );
    assert_eq!(resolution.key.as_deref(), Some("other-key"));
    // Replacing the provider table must not retain the old managed env_key.
    let args = [
        "-c",
        "model_providers.usage_byok={base_url='https://b.invalid/v1'}",
    ];
    let resolution = effective::resolve(
        &text,
        AuthMode::Byok,
        &f.path,
        false,
        &args.map(str::to_string),
        Some("managed-key"),
        &|_| None,
    );
    assert_eq!(
        resolution.identity.state,
        crate::runs::IdentityState::Unresolved
    );
    assert!(resolution.key.is_none());
}

#[test]
fn launch_identity_does_not_treat_option_values_or_delimited_prompts_as_overrides() {
    let f = Fixture::new();
    fs::create_dir(f.path.join(".git")).unwrap();
    let text = native("https://a.invalid/v1", "saved");
    for args in [
        vec!["--image", "--profile", "--", "--oss"],
        vec!["exec", "--", "-c", "model_provider='secret'"],
    ] {
        let resolution = effective::resolve(
            &text,
            AuthMode::Byok,
            &f.path,
            false,
            &args.into_iter().map(str::to_string).collect::<Vec<_>>(),
            Some("key"),
            &|_| None,
        );
        assert_eq!(
            resolution.identity.state,
            crate::runs::IdentityState::Resolved
        );
        assert_eq!(resolution.identity.model.as_deref(), Some("saved"));
    }
    for (mode, resume) in [(AuthMode::Oauth, false), (AuthMode::Byok, true)] {
        let resolution =
            effective::resolve(&text, mode, &f.path, resume, &[], Some("key"), &|_| None);
        assert_eq!(
            resolution.identity.state,
            crate::runs::IdentityState::Unresolved
        );
    }
}
fn insert(conn: &Connection, id: &str, name: &str, text: &str, key: &str, meta: serde_json::Value) {
    conn.execute(
        "INSERT OR REPLACE INTO providers VALUES (?1,'codex',?2,?3,?4)",
        params![
            id,
            name,
            json!({"auth":{"OPENAI_API_KEY":key},"config":text}).to_string(),
            meta.to_string()
        ],
    )
    .unwrap();
}

#[test]
fn saved_revisions_keep_comments_and_do_not_contain_keys() {
    let f = Fixture::new();
    let mut m = f.manager();
    let config = format!(
        "# my settings\n{}\n[features]\n# preserve\nshell_snapshot = false\n",
        native("https://example.com/a/v1", "model-a")
    );
    let p = m.add("A", &config, "fixture-key-a").unwrap();
    let r = m.revision(&p.id, 1).unwrap();
    assert!(r.config.contains("# my settings"));
    assert!(r.config.contains("# preserve"));
    assert!(!r.config.contains("fixture-key-a"));
    assert!(r.config.contains(config::KEY_ENV));
    assert_eq!(
        secrets::get(m.root(), &r.secret_ref).unwrap(),
        "fixture-key-a"
    );
    let dump = fs::read(m.root().join("manager.sqlite3")).unwrap();
    assert!(
        !dump
            .windows(b"fixture-key-a".len())
            .any(|s| s == b"fixture-key-a")
    );
    m.export(&p.id, &f.path.join("export.toml")).unwrap();
    assert_eq!(
        fs::read_to_string(f.path.join("export.toml")).unwrap(),
        r.config
    );
}

#[test]
fn native_and_legacy_openai_urls_are_preserved_exactly() {
    let legacy = "model = 'm'\nmodel_provider = 'openai'\n[model_providers.openai]\nbase_url = 'https://example.com/compatible-mode/v1'\nexperimental_bearer_token = 'fixture-inline'\n";
    let normalized = config::normalize(legacy).unwrap();
    assert_eq!(
        config::summary(&normalized).unwrap().0,
        "https://example.com/compatible-mode/v1"
    );
    assert!(!normalized.contains("fixture-inline"));
    assert!(!normalized.contains("[model_providers.openai]"));
    assert!(normalized.contains("requires_openai_auth = false"));
}

#[test]
fn incorrect_toml_shapes_are_errors_instead_of_panics() {
    for text in [
        "model='m'\nmodel_providers=1",
        "model='m'\n[model_providers]\nopenai=1",
        "model='m'\nmodel_provider='vendor'\n[model_providers]\nvendor=1",
    ] {
        assert!(config::normalize(text).is_err());
        assert!(config::patch(text, Some("https://example.com/v1"), None, None).is_err());
    }
    assert!(config::validate_url("https://example.com/\nv1").is_err());
}

#[test]
fn revision_updates_do_not_rewrite_existing_home_or_credentials() {
    let f = Fixture::new();
    let mut m = f.manager();
    let p = m
        .add(
            "A",
            &native("https://example.com/v1", "m1"),
            "fixture-key-1",
        )
        .unwrap();
    let home1 = m.materialize(&p.id, 1).unwrap();
    let before = fs::read(home1.join("config.toml")).unwrap();
    let changed = config::patch(
        &m.revision(&p.id, 1).unwrap().config,
        None,
        Some("m2"),
        None,
    )
    .unwrap();
    let updated = m
        .update(&p.id, 1, &changed, Some("fixture-key-2"), None)
        .unwrap();
    assert_eq!(updated.revision, 2);
    let home2 = m.materialize(&p.id, 2).unwrap();
    assert_ne!(home1, home2);
    assert_eq!(fs::read(home1.join("config.toml")).unwrap(), before);
    assert_eq!(
        secrets::get(m.root(), &m.revision(&p.id, 1).unwrap().secret_ref).unwrap(),
        "fixture-key-1"
    );
    assert_eq!(
        secrets::get(m.root(), &m.revision(&p.id, 2).unwrap().secret_ref).unwrap(),
        "fixture-key-2"
    );
    let binding: HomeBinding =
        serde_json::from_slice(&fs::read(home1.join("usage-binding.json")).unwrap()).unwrap();
    assert_eq!(binding.revision, 1);
    assert_eq!(binding.profile_id, p.id);
    let performance1 = m.performance_binding(&updated, 1).unwrap();
    let performance2 = m.performance_binding(&updated, 2).unwrap();
    assert_eq!(performance1.home, home1);
    assert_eq!(performance2.home, home2);
    assert_ne!(performance1.scope(), performance2.scope());
    assert_eq!(performance1.revision, "1");
    assert!(performance1.expected_upstream.is_some());
    let provider = crate::hub::Hub::default()
        .provider(crate::hub::ProviderDefinition {
            id: "configured-provider".into(),
            agent: "codex".into(),
            context: crate::agents::AgentContext::ManagedCodex {
                root: m.root().to_owned(),
                account: p.id.clone(),
                revision: Some(1),
            },
            resources: Vec::new(),
            auth: None,
        })
        .unwrap();
    let binding = provider.performance_binding().unwrap();
    assert_eq!(binding.provider, "configured-provider");
    assert_eq!(binding.home, home1);
    assert_eq!(binding.scope(), performance1.scope());
    assert!(provider.capabilities().performance);
}

#[test]
fn manual_home_edits_are_preserved_and_require_explicit_import() {
    let f = Fixture::new();
    let mut m = f.manager();
    let p = m
        .add("A", &native("https://example.com/v1", "m1"), "fixture-key")
        .unwrap();
    let home = m.materialize(&p.id, 1).unwrap();
    fs::write(home.join("config.toml"), "# edited manually\n").unwrap();
    assert!(
        m.materialize(&p.id, 1)
            .unwrap_err()
            .to_string()
            .contains("手动修改")
    );
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        "# edited manually\n"
    );
}

#[test]
fn stale_editor_cannot_overwrite_another_process_update() {
    let f = Fixture::new();
    let mut m = f.manager();
    let p = m
        .add("A", &native("https://example.com/v1", "m1"), "fixture-key")
        .unwrap();
    let mut other = f.manager();
    other
        .update(
            &p.id,
            1,
            &native("https://example.com/v1", "m2"),
            None,
            None,
        )
        .unwrap();
    assert!(
        m.update(
            &p.id,
            1,
            &native("https://example.com/v1", "m3"),
            None,
            None
        )
        .is_err()
    );
    assert_eq!(m.resolve(&p.id).unwrap().model, "m2");
}

#[test]
fn simultaneous_follow_sync_applies_one_revision() {
    let f = Fixture::new();
    let conn = f.source();
    let mut manager = f.manager();
    insert(
        &conn,
        "a",
        "A",
        &native("https://example.com/v1", "before"),
        "fixture-a",
        json!({}),
    );
    manager.import_ccswitch(&f.db(), true, &[], false).unwrap();
    insert(
        &conn,
        "a",
        "A",
        &native("https://example.com/v1", "after"),
        "fixture-updated",
        json!({}),
    );
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let root = manager.root().to_path_buf();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut manager = Manager::open(root).unwrap();
                barrier.wait();
                manager.sync(false).unwrap()
            })
        })
        .collect();
    let reports: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(reports.iter().map(|r| r.updated.len()).sum::<usize>(), 1);
    assert_eq!(manager.resolve("A").unwrap().revision, 2);
}

#[test]
fn automatic_sync_and_edits_use_file_secret_storage() {
    let f = Fixture::new();
    let conn = f.source();
    let mut manager = f.manager();
    insert(
        &conn,
        "a",
        "A",
        &native("https://example.com/v1", "before"),
        "fixture-a",
        json!({}),
    );
    manager.import_ccswitch(&f.db(), true, &[], false).unwrap();
    insert(
        &conn,
        "a",
        "A",
        &native("https://example.com/v1", "after"),
        "fixture-new",
        json!({}),
    );
    let mut automatic = Manager::open(manager.root().to_path_buf()).unwrap();
    automatic.sync(false).unwrap();
    let profile = automatic.resolve("A").unwrap();
    let saved = automatic.revision(&profile.id, profile.revision).unwrap();
    assert!(saved.secret_ref.starts_with("file:"));
    assert_eq!(
        secrets::get(automatic.root(), &saved.secret_ref).unwrap(),
        "fixture-new"
    );
    automatic
        .update(
            &profile.id,
            profile.revision,
            &saved.config,
            Some("fixture-local"),
            None,
        )
        .unwrap();
    assert!(
        automatic
            .revision(&profile.id, profile.revision + 1)
            .unwrap()
            .secret_ref
            .starts_with("file:")
    );
}

#[test]
fn unchanged_cc_switch_source_can_repair_legacy_refs_without_keychain_access() {
    let f = Fixture::new();
    let conn = f.source();
    let mut manager = f.manager();
    insert(
        &conn,
        "a",
        "A",
        &native("https://example.com/v1", "m"),
        "fixture-source-key",
        json!({}),
    );
    manager.import_ccswitch(&f.db(), true, &[], false).unwrap();
    let profile = manager.resolve("A").unwrap();
    let home = manager.materialize(&profile.id, 1).unwrap();
    let before = fs::read(home.join("config.toml")).unwrap();
    let reference = format!("keychain:{}", uuid::Uuid::new_v4());
    manager
        .conn
        .execute(
            "UPDATE revisions SET secret_ref=?1 WHERE profile_id=?2",
            params![reference, profile.id],
        )
        .unwrap();
    assert!(
        secrets::get(manager.root(), &reference)
            .unwrap_err()
            .to_string()
            .contains("重新输入")
    );
    assert_eq!(manager.sync(true).unwrap().updated, vec!["A"]);
    assert_eq!(
        manager.revision(&profile.id, 1).unwrap().secret_ref,
        reference
    );
    assert_eq!(manager.sync(false).unwrap().updated, vec!["A"]);
    let saved = manager.revision(&profile.id, 1).unwrap();
    assert!(saved.secret_ref.starts_with("file:"));
    assert_eq!(
        secrets::get(manager.root(), &saved.secret_ref).unwrap(),
        "fixture-source-key"
    );
    assert_eq!(fs::read(home.join("config.toml")).unwrap(), before);
    assert_eq!(manager.resolve("A").unwrap().revision, 1);
}

#[test]
fn import_is_idempotent_and_reads_committed_wal_without_writing_source() {
    let f = Fixture::new();
    let conn = f.source();
    let mut m = f.manager();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    insert(
        &conn,
        "upstream-a",
        "A",
        &native("https://example.com/path/v1", "m1"),
        "fixture-cc-key",
        json!({}),
    );
    conn.execute(
        "INSERT INTO providers VALUES ('claude-only','claude','Claude','{}','{}')",
        [],
    )
    .unwrap();
    let before = fs::read(f.db()).unwrap();
    let wal = fs::read(f.db().with_extension("db-wal")).unwrap();
    let report = m.import_ccswitch(&f.db(), true, &[], false).unwrap();
    assert_eq!(report.created, vec!["A"]);
    assert_eq!(m.list().unwrap().len(), 1);
    let report = m.import_ccswitch(&f.db(), true, &[], false).unwrap();
    assert_eq!(report.unchanged, vec!["A"]);
    assert_eq!(m.list().unwrap()[0].revision, 1);
    assert_eq!(fs::read(f.db()).unwrap(), before);
    assert_eq!(fs::read(f.db().with_extension("db-wal")).unwrap(), wal);
}

#[test]
fn import_preview_and_provider_selection_do_not_save_other_accounts() {
    let f = Fixture::new();
    let conn = f.source();
    let mut m = f.manager();
    insert(
        &conn,
        "a",
        "A",
        &native("https://example.com/a/v1", "a"),
        "fixture-a",
        json!({}),
    );
    insert(
        &conn,
        "b",
        "B",
        &native("https://example.com/b/v1", "b"),
        "fixture-b",
        json!({}),
    );
    assert_eq!(
        m.import_ccswitch(&f.db(), true, &[], true)
            .unwrap()
            .created
            .len(),
        2
    );
    assert!(m.list().unwrap().is_empty());
    assert!(!m.root().join("keys").exists());
    m.import_ccswitch(&f.db(), false, &["b".into()], false)
        .unwrap();
    assert_eq!(m.list().unwrap()[0].name, "B");
    assert_eq!(m.list().unwrap().len(), 1);
    assert!(
        m.import_ccswitch(&f.db(), false, &["missing".into()], false)
            .is_err()
    );
}

#[test]
fn synchronization_preserves_pinned_versions_and_does_not_follow_current_provider() {
    let f = Fixture::new();
    let conn = f.source();
    let mut m = f.manager();
    insert(
        &conn,
        "a",
        "A",
        &native("https://example.com/a/v1", "a1"),
        "fixture-a1",
        json!({}),
    );
    insert(
        &conn,
        "b",
        "B",
        &native("https://example.com/b/v1", "b1"),
        "fixture-b1",
        json!({}),
    );
    m.import_ccswitch(&f.db(), true, &[], false).unwrap();
    let a = m.resolve("A").unwrap();
    let b = m.resolve("B").unwrap();
    let home = m.materialize(&a.id, 1).unwrap();
    let original = fs::read(home.join("config.toml")).unwrap();
    insert(
        &conn,
        "a",
        "A",
        &native("https://example.com/a/v1", "a2"),
        "fixture-a2",
        json!({}),
    );
    let report = m.sync(false).unwrap();
    assert_eq!(report.updated, vec!["A"]);
    assert_eq!(m.resolve(&a.id).unwrap().revision, 2);
    assert_eq!(m.resolve(&b.id).unwrap().revision, 1);
    assert_eq!(fs::read(home.join("config.toml")).unwrap(), original);
    assert_eq!(
        secrets::get(m.root(), &m.revision(&a.id, 1).unwrap().secret_ref).unwrap(),
        "fixture-a1"
    );
    assert_eq!(
        secrets::get(m.root(), &m.revision(&a.id, 2).unwrap().secret_ref).unwrap(),
        "fixture-a2"
    );
}

#[test]
fn local_edits_pause_following_and_reimport_reports_conflict() {
    let f = Fixture::new();
    let conn = f.source();
    let mut m = f.manager();
    insert(
        &conn,
        "a",
        "A",
        &native("https://example.com/v1", "a1"),
        "fixture-a1",
        json!({}),
    );
    m.import_ccswitch(&f.db(), true, &[], false).unwrap();
    let a = m.resolve("A").unwrap();
    let edited = m
        .update(
            &a.id,
            1,
            &native("https://example.com/v1", "local"),
            None,
            None,
        )
        .unwrap();
    assert!(!edited.follow);
    insert(
        &conn,
        "a",
        "A",
        &native("https://example.com/v1", "remote"),
        "fixture-remote",
        json!({}),
    );
    assert!(m.sync(false).unwrap().updated.is_empty());
    assert_eq!(
        m.import_ccswitch(&f.db(), true, &[], false)
            .unwrap()
            .conflicts,
        vec!["A"]
    );
    assert_eq!(m.resolve(&a.id).unwrap().model, "local");
    assert!(m.follow(&a.id, true).is_err());
    m.detach(&a.id).unwrap();
    assert_eq!(
        m.import_ccswitch(&f.db(), true, &[], false)
            .unwrap()
            .created,
        vec!["A"]
    );
    assert_eq!(m.list().unwrap().len(), 2);
    assert!(m.resolve("A").is_err()); // Ambiguous names must never silently choose an account.
}

#[test]
fn missing_source_accounts_and_archived_profiles_keep_history() {
    let f = Fixture::new();
    let conn = f.source();
    let mut m = f.manager();
    insert(
        &conn,
        "a",
        "A",
        &native("https://example.com/v1", "a"),
        "fixture-a",
        json!({}),
    );
    m.import_ccswitch(&f.db(), true, &[], false).unwrap();
    let a = m.resolve("A").unwrap();
    let home = m.materialize(&a.id, 1).unwrap();
    conn.execute("DELETE FROM providers WHERE id='a'", [])
        .unwrap();
    assert_eq!(m.sync(false).unwrap().missing, vec!["A"]);
    assert!(home.join("config.toml").exists());
    m.remove(&a.id).unwrap();
    assert!(m.list().unwrap().is_empty());
    assert_eq!(m.resolve(&a.id).unwrap().revision, 1);
    assert!(m.revision(&a.id, 1).is_ok());
    insert(
        &conn,
        "a",
        "A",
        &native("https://example.com/v1", "a"),
        "fixture-a",
        json!({}),
    );
    m.import_ccswitch(&f.db(), false, &[], false).unwrap();
    assert_eq!(m.list().unwrap().len(), 1);
}

#[test]
fn malformed_oauth_and_protocol_conversion_rows_are_skipped_individually() {
    let f = Fixture::new();
    let conn = f.source();
    insert(
        &conn,
        "good",
        "Good",
        &native("https://example.com/v1", "a"),
        "fixture-good",
        json!({}),
    );
    insert(
        &conn,
        "chat",
        "Chat",
        &native("https://example.com/v1", "a"),
        "fixture-chat",
        json!({"apiFormat":"openai_chat"}),
    );
    insert(
        &conn,
        "proxy",
        "Proxy",
        &native("http://127.0.0.1:15721/v1", "a"),
        "PROXY_MANAGED",
        json!({}),
    );
    insert(
        &conn,
        "oauth",
        "OAuth",
        &native("https://example.com/v1", "a"),
        "",
        json!({}),
    );
    conn.execute(
        "INSERT INTO providers VALUES ('bad','codex','Bad','bad json','{}')",
        [],
    )
    .unwrap();
    let result = ccswitch::read(&f.db()).unwrap();
    assert_eq!(result.candidates.len(), 1);
    assert_eq!(result.skipped.len(), 4);
    let mut m = f.manager();
    assert_eq!(
        m.import_ccswitch(&f.db(), false, &[], false)
            .unwrap()
            .created,
        vec!["Good"]
    );
}

#[test]
fn legacy_enabled_common_config_merges_without_losing_nested_settings() {
    let f = Fixture::new();
    let conn = f.source();
    let text = format!(
        "{}\n[mcp_servers.example]\ncommand='old'\nargs=['keep']\n",
        native("https://example.com/v1", "a")
    );
    insert(
        &conn,
        "a",
        "A",
        &text,
        "fixture-a",
        json!({"commonConfigEnabled":true}),
    );
    conn.execute(
        "INSERT INTO settings VALUES ('common_config_codex',?1)",
        ["model_reasoning_effort='low'\n[mcp_servers.example]\ncommand='new'\n"],
    )
    .unwrap();
    let result = ccswitch::read(&f.db()).unwrap();
    let doc = config::parse(&result.candidates[0].config).unwrap();
    assert_eq!(doc["model_reasoning_effort"].as_str(), Some("low"));
    assert_eq!(
        doc["mcp_servers"]["example"]["command"].as_str(),
        Some("new")
    );
    assert_eq!(
        doc["mcp_servers"]["example"]["args"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn configuration_errors_and_exports_do_not_leak_inline_credentials() {
    let invalid = "model='m'\n[model_providers.x]\nexperimental_bearer_token = 'fixture-secret\n";
    let error = config::normalize(invalid).unwrap_err().to_string();
    assert!(!error.contains("fixture-secret"));
    assert!(config::validate_url("https://user:fixture-secret@example.com/v1").is_err());
    let text = "model='m'\nmodel_provider='x'\n[model_providers.x]\nbase_url='https://example.com/v1'\nwire_api='chat'\n";
    assert!(config::normalize(text).is_err());
}

#[cfg(unix)]
#[test]
fn secret_and_config_files_have_private_permissions_without_chmod_on_export_parent() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let mut m = f.manager();
    let p = m
        .add("A", &native("https://example.com/v1", "a"), "fixture-a")
        .unwrap();
    let home = m.materialize(&p.id, 1).unwrap();
    assert_eq!(
        fs::metadata(m.root()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(m.root().join("keys"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(home.join("config.toml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let reference = m.revision(&p.id, 1).unwrap().secret_ref;
    assert_eq!(
        fs::metadata(
            m.root()
                .join("keys")
                .join(reference.strip_prefix("file:").unwrap())
        )
        .unwrap()
        .permissions()
        .mode()
            & 0o777,
        0o600
    );
    let parent = f.path.join("export-dir");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    m.export(&p.id, &parent.join("config.toml")).unwrap();
    assert_eq!(
        fs::metadata(parent).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[cfg(unix)]
#[test]
fn concurrent_accounts_receive_separate_homes_credentials_and_endpoints() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let mut m = f.manager();
    let a = m
        .add("A", &native("https://example.com/a/v1", "a"), "fixture-a")
        .unwrap();
    let b = m
        .add("B", &native("https://example.com/b/v1", "b"), "fixture-b")
        .unwrap();
    let script = f.path.join("fake-codex");
    fs::write(
        &script,
        r#"#!/bin/sh
if [ "$1" = "--help" ]; then printf '%s\n' '--no-daemon'; exit 0; fi
if [ "$1" = "--no-daemon" ]; then shift; else exit 9; fi
printf '%s\n' "$CODEX_HOME" "$USAGE_CODEX_API_KEY" "${OPENAI_API_KEY-unset}" > "$1"
cp "$CODEX_HOME/config.toml" "$1.config"
sleep 0.3
"#,
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let mut threads = Vec::new();
    for (id, capture) in [(a.id, "a"), (b.id, "b")] {
        let root = m.root().to_path_buf();
        let cwd = f.path.clone();
        let binary = script.clone();
        let output = f.path.join(capture);
        threads.push(std::thread::spawn(move || {
            let mut manager = Manager::open(root).unwrap();
            manager
                .run(
                    &id,
                    &cwd,
                    None,
                    None,
                    &binary,
                    &[output.to_string_lossy().into()],
                )
                .unwrap()
        }));
    }
    for thread in threads {
        assert!(thread.join().unwrap().success());
    }
    let runs = m.recent_runs(10).unwrap();
    assert_eq!(runs.len(), 2);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    assert!(runs.iter().all(|run| run.process_identity.is_some()));
    assert!(
        runs.iter()
            .all(|run| crate::runs::RunState::observe(run) == crate::runs::RunState::Completed)
    );
    let public = serde_json::to_string(&runs).unwrap();
    assert!(!public.contains("process_identity"));
    assert!(!public.contains("proofs"));
    let ac = fs::read_to_string(f.path.join("a")).unwrap();
    let bc = fs::read_to_string(f.path.join("b")).unwrap();
    assert_ne!(ac.lines().next(), bc.lines().next());
    assert_eq!(ac.lines().nth(1), Some("fixture-a"));
    assert_eq!(bc.lines().nth(1), Some("fixture-b"));
    assert_eq!(ac.lines().nth(2), Some("unset"));
    assert!(
        fs::read_to_string(f.path.join("a.config"))
            .unwrap()
            .contains("https://example.com/a/v1")
    );
    assert!(
        fs::read_to_string(f.path.join("b.config"))
            .unwrap()
            .contains("https://example.com/b/v1")
    );
    let (latest_start, earliest_finish): (String, String) = m
        .conn
        .query_row(
            "SELECT MAX(started_at),MIN(finished_at) FROM runs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        chrono::DateTime::parse_from_rfc3339(&latest_start).unwrap()
            < chrono::DateTime::parse_from_rfc3339(&earliest_finish).unwrap()
    );
}

fn response_server(body: &str) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/custom/v1", listener.local_addr().unwrap());
    let body = body.to_string();
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut data = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let n = socket.read(&mut buffer).unwrap();
            assert!(n > 0);
            data.extend_from_slice(&buffer[..n]);
            if let Some(end) = data.windows(4).position(|x| x == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&data[..end]);
                let len = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if data.len() >= end + 4 + len {
                    break;
                }
            }
        }
        write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
        String::from_utf8(data).unwrap()
    });
    (base, thread)
}

#[tokio::test]
async fn test_request_uses_selected_key_model_and_exact_responses_endpoint() {
    let (base, server) =
        response_server(r#"{"object":"response","id":"resp_fixture","output":[],"error":null}"#);
    let f = Fixture::new();
    let mut m = f.manager();
    let p = m
        .add("A", &native(&base, "fixture-model"), "fixture-api-key")
        .unwrap();
    assert_eq!(m.test_connection(&p.id).await.unwrap(), 200);
    let request = server.join().unwrap();
    assert!(request.starts_with("POST /custom/v1/responses HTTP/1.1"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-api-key")
    );
    assert!(request.contains("fixture-model"));
}

#[tokio::test]
async fn http_200_with_wrong_protocol_does_not_pass_connection_test() {
    let (base, server) = response_server(r#"{"object":"chat.completion","choices":[]}"#);
    let f = Fixture::new();
    let mut m = f.manager();
    let p = m
        .add("A", &native(&base, "fixture-model"), "fixture-api-key")
        .unwrap();
    assert!(
        m.test_connection(&p.id)
            .await
            .unwrap_err()
            .to_string()
            .contains("不符合")
    );
    server.join().unwrap();
}

pub(super) fn oauth_source(f: &Fixture, workspace: &str, refresh: &str) -> PathBuf {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let claims = json!({"sub":"fixture-user","email":"fixture@example.test",
        "https://api.openai.com/auth":{"chatgpt_account_id":workspace,"chatgpt_user_id":"fixture-user","chatgpt_plan_type":"plus"}});
    let jwt = format!("e30.{}.fixture", URL_SAFE_NO_PAD.encode(claims.to_string()));
    let path = f.path.join(format!("{workspace}.json"));
    fs::write(&path,json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"id_token":jwt,"access_token":jwt,"refresh_token":refresh,"account_id":workspace},"last_refresh":"2026-10-11T00:00:00Z"}).to_string()).unwrap();
    path
}

#[test]
fn oauth_import_is_read_only_deduplicates_identity_and_isolates_workspaces() {
    let f = Fixture::new();
    let mut m = f.manager();
    let source = oauth_source(&f, "workspace-a", "fixture-refresh-a");
    let before = fs::read(&source).unwrap();
    let a = m.import_oauth(Some("A"), None, &source, None).unwrap();
    assert_eq!(a.auth_mode, AuthMode::Oauth);
    assert_eq!(a.oauth.as_ref().unwrap().email, "fixture@example.test");
    assert_eq!(fs::read(&source).unwrap(), before);
    let again = m.import_oauth(None, None, &source, None).unwrap();
    assert_eq!(again.id, a.id);
    assert_eq!(again.name, "A");
    assert_eq!(again.revision, 1);
    let b_source = oauth_source(&f, "workspace-b", "fixture-refresh-b");
    let b = m.import_oauth(Some("B"), None, &b_source, None).unwrap();
    assert_ne!(a.id, b.id);
    assert_ne!(m.home(&a.id, 1).unwrap(), m.home(&b.id, 1).unwrap());
    assert!(
        m.import_oauth(None, None, &b_source, Some(&a.id))
            .unwrap_err()
            .to_string()
            .contains("身份")
    );
    assert!(
        fs::read_to_string(m.home(&a.id, 1).unwrap().join("auth.json"))
            .unwrap()
            .contains("fixture-refresh-a")
    );
    assert!(m.copy(&a.id, "copy").is_err());
    let db = fs::read(m.root().join("manager.sqlite3")).unwrap();
    assert!(
        !db.windows(b"fixture-refresh-a".len())
            .any(|w| w == b"fixture-refresh-a")
    );
    assert!(
        !m.revision(&a.id, 1)
            .unwrap()
            .config
            .contains("fixture-refresh")
    );
}

#[test]
fn oauth_profile_allows_cli_onboarding_state_but_preserves_versioned_settings() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let source = oauth_source(&f, "workspace-a", "fixture-refresh");
    let account = manager
        .import_oauth(Some("Native state"), None, &source, None)
        .unwrap();
    let saved = manager.revision(&account.id, 1).unwrap().config;
    let home = manager.materialize(&account.id, 1).unwrap();
    let path = home.join("usage-r1.config.toml");
    let runtime = format!("{saved}\n[tui]\nscreen_reader_detection_done = true\n");
    fs::write(&path, &runtime).unwrap();
    assert_eq!(manager.materialize(&account.id, 1).unwrap(), home);
    assert_eq!(fs::read_to_string(&path).unwrap(), runtime);
    assert_eq!(manager.revision(&account.id, 1).unwrap().config, saved);
    for invalid in [
        runtime.replace(
            "screen_reader_detection_done = true",
            "screen_reader_detection_done = 'yes'",
        ),
        runtime.replace("model_provider = \"openai\"", "model_provider = \"other\""),
        format!("openai_base_url = 'https://other.example'\n{runtime}"),
    ] {
        fs::write(&path, &invalid).unwrap();
        assert!(manager.materialize(&account.id, 1).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
    }
}

#[test]
fn oauth_configuration_revisions_retain_one_live_credential_file() {
    let f = Fixture::new();
    let mut m = f.manager();
    let source = oauth_source(&f, "workspace-a", "fixture-refresh-a");
    let a = m
        .import_oauth(
            Some("A"),
            Some("# original\nmodel='model-a'\n[features]\nshell_snapshot=false\n"),
            &source,
            None,
        )
        .unwrap();
    let home = m.home(&a.id, 1).unwrap();
    let mut auth: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join("auth.json")).unwrap()).unwrap();
    auth["tokens"]["refresh_token"] = json!("fixture-rotated-refresh");
    private_write(&home.join("auth.json"), auth.to_string().as_bytes()).unwrap();
    let old = m.revision(&a.id, 1).unwrap();
    let updated = oauth::patch_config(&old.config, Some("model-b"), Some("high")).unwrap();
    let a = m.update(&a.id, 1, &updated, None, Some("Renamed")).unwrap();
    assert_eq!(a.revision, 2);
    assert_eq!(m.materialize(&a.id, 2).unwrap(), home);
    assert_eq!(m.instance_binding(&home).unwrap(), Some((a.id.clone(), 2)));
    assert!(
        m.import_oauth(None, None, &home.join("auth.json"), None)
            .is_err()
    );
    m.materialize(&a.id, 1).unwrap();
    assert_eq!(
        fs::read_to_string(home.join("usage-r1.config.toml")).unwrap(),
        old.config
    );
    assert_eq!(
        fs::read_to_string(home.join("usage-r2.config.toml")).unwrap(),
        updated
    );
    assert!(
        fs::read_to_string(home.join("auth.json"))
            .unwrap()
            .contains("fixture-rotated-refresh")
    );
    assert_eq!(
        m.conn
            .query_row(
                "SELECT COUNT(*) FROM homes WHERE profile_id=?1",
                [&a.id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert!(m.update(&a.id, 2, &updated, Some("key"), None).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(home.join("auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[cfg(unix)]
#[test]
fn nonblocking_oauth_handle_retains_shared_lease_until_process_is_reaped() {
    use crate::agents::RunningAgent;
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let mut manager = f.manager();
    let source = oauth_source(&f, "workspace-a", "fixture-refresh-a");
    let profile = manager.import_oauth(None, None, &source, None).unwrap();
    let binary = f.path.join("oauth codex");
    std::fs::write(
        &binary,
        r#"#!/bin/sh
if [ "$1" = --version ]; then echo 'codex-cli 0.163.0'; exit 0; fi
if [ "$1" = --help ]; then echo '--profile --no-daemon'; exit 0; fi
for value; do done_file="$value"; done
while [ ! -f "$done_file" ]; do sleep 0.01; done
exit 0
"#,
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let done = f.path.join("done");
    let mut handle = manager
        .start_recorded_with_config(
            &profile.id,
            &f.path,
            None,
            None,
            &binary,
            &[done.to_string_lossy().into_owned()],
            None,
        )
        .unwrap();
    assert!(handle.try_wait().unwrap().is_none());
    assert!(manager.oauth_lease(&profile.id).is_err());
    assert!(manager.import_oauth(None, None, &source, None).is_err());
    std::fs::write(done, "").unwrap();
    assert!(handle.wait().unwrap().status.success());
    assert!(manager.oauth_lease(&profile.id).is_ok());
}

#[test]
fn oauth_shared_runs_coexist_and_replacement_waits_until_every_run_exits() {
    let f = Fixture::new();
    let mut m = f.manager();
    let source = oauth_source(&f, "workspace-a", "fixture-refresh-a");
    let a = m.import_oauth(None, None, &source, None).unwrap();
    let b_source = oauth_source(&f, "workspace-b", "fixture-refresh-b");
    let b = m.import_oauth(None, None, &b_source, None).unwrap();
    let lease = m.oauth_run_lease(&a.id).unwrap();
    let mut second = f.manager();
    let second_run = second.oauth_run_lease(&a.id).unwrap();
    assert!(second.oauth_lease(&a.id).is_err());
    assert!(second.oauth_run_lease(&b.id).is_ok());
    assert!(second.import_oauth(None, None, &source, None).is_err());
    drop(lease);
    // One remaining process must continue to protect the live login file.
    assert!(second.oauth_lease(&a.id).is_err());
    drop(second_run);
    let replacement = second.oauth_lease(&a.id).unwrap();
    assert!(m.oauth_run_lease(&a.id).is_err());
    drop(replacement);
    assert!(second.import_oauth(None, None, &source, None).is_ok());
    assert!(m.oauth_run_lease(&a.id).is_ok());
}

#[test]
fn oauth_cockpit_file_import_and_invalid_auth_errors_hide_tokens() {
    let f = Fixture::new();
    let mut m = f.manager();
    let source = oauth_source(&f, "workspace-a", "fixture-refresh-a");
    let mut doc: serde_json::Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
    doc["auth_mode"] = json!("oauth");
    doc["email"] = json!("fixture@example.test");
    doc["account_id"] = json!("workspace-a");
    doc["tokens"].as_object_mut().unwrap().remove("account_id");
    doc.as_object_mut().unwrap().remove("last_refresh");
    doc["token_updated_at"] = json!(1780000000);
    fs::write(&source, doc.to_string()).unwrap();
    let a = m.import_oauth(None, None, &source, None).unwrap();
    assert_eq!(a.oauth.as_ref().unwrap().account_id, "workspace-a");
    let auth: serde_json::Value =
        serde_json::from_slice(&fs::read(m.home(&a.id, 1).unwrap().join("auth.json")).unwrap())
            .unwrap();
    assert_eq!(auth["auth_mode"], "chatgpt");
    assert!(
        auth["last_refresh"]
            .as_str()
            .unwrap()
            .starts_with("2026-05")
    );
    for invalid in [
        json!({"tokens":{"access_token":"fixture-sensitive"}}),
        json!({"auth_mode":"apikey","OPENAI_API_KEY":"fixture-sensitive"}),
        json!({"requires_reauth":true,"tokens":doc["tokens"]}),
    ] {
        fs::write(&source, invalid.to_string()).unwrap();
        let error = m
            .import_oauth(None, None, &source, None)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("fixture-sensitive"));
    }
    fs::write(&source, "{invalid fixture-sensitive}").unwrap();
    assert!(
        !oauth::read_auth(&source)
            .err()
            .unwrap()
            .to_string()
            .contains("fixture-sensitive")
    );
    assert_eq!(m.list().unwrap().len(), 1);
}

#[test]
fn oauth_config_forces_file_storage_preserves_settings_and_excludes_byok_routes() {
    let text = "# comment\nmodel='model-a'\ncli_auth_credentials_store='keyring'\nforced_login_method='api'\nchatgpt_base_url='https://other.test'\n[model_providers.other]\nexperimental_bearer_token='fixture-sensitive'\n[features]\nshell_snapshot=false\n";
    let normalized = oauth::normalize_config(text).unwrap();
    assert!(normalized.contains("# comment"));
    assert!(normalized.contains("shell_snapshot=false"));
    assert!(normalized.contains("\"file\""));
    assert!(normalized.contains("\"chatgpt\""));
    assert!(!normalized.contains("fixture-sensitive"));
    assert!(!normalized.contains("chatgpt_base_url"));
    assert!(oauth::normalize_config("model_provider='other'").is_err());
    assert!(oauth::normalize_config("model=[]").is_err());
    for args in [
        vec!["--config", "model_provider=other"],
        vec!["exec", "-pother"],
        vec!["login"],
        vec!["--profile=other"],
    ] {
        assert!(
            oauth::validate_run_args(&args.into_iter().map(str::to_owned).collect::<Vec<_>>())
                .is_err()
        );
    }
    assert!(oauth::validate_run_args(&["exec".into(), "hello".into()]).is_ok());
}

#[test]
fn existing_byok_database_opens_with_oauth_table_added_without_changes() {
    let f = Fixture::new();
    let mut m = f.manager();
    let a = m
        .add(
            "BYOK",
            &native("https://example.test/v1", "m"),
            "fixture-key",
        )
        .unwrap();
    m.conn.execute_batch("DROP TABLE oauth_accounts").unwrap();
    drop(m);
    let m = f.manager();
    assert_eq!(m.resolve(&a.id).unwrap().auth_mode, AuthMode::Byok);
    assert_eq!(m.resolve(&a.id).unwrap().revision, 1);
    assert_eq!(
        secrets::get(m.root(), &m.revision(&a.id, 1).unwrap().secret_ref).unwrap(),
        "fixture-key"
    );
}

#[test]
fn oauth_home_initialization_does_not_overwrite_cli_trust_settings() {
    let f = Fixture::new();
    let mut m = f.manager();
    let source = oauth_source(&f, "workspace-a", "fixture-refresh");
    let account = m.import_oauth(None, None, &source, None).unwrap();
    let home = m.home(&account.id, 1).unwrap();
    let base = home.join("config.toml");
    let config = format!(
        "{}\n[projects.fixture]\ntrust_level='trusted'\n",
        fs::read_to_string(&base).unwrap()
    );
    private_write(&base, config.as_bytes()).unwrap();
    m.materialize(&account.id, 1).unwrap();
    assert_eq!(fs::read_to_string(&base).unwrap(), config);
    // Atomic initialization also preserves a file created between lookup and publication.
    private_create(&base, b"should not replace existing contents").unwrap();
    assert_eq!(fs::read_to_string(&base).unwrap(), config);
}

#[cfg(unix)]
#[test]
fn startup_sync_selects_the_latest_followed_revision() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let source = f.source();
    insert(
        &source,
        "vendor",
        "Imported",
        &native("https://example.com/v1", "before"),
        "fixture-key",
        json!({}),
    );
    let mut m = f.manager();
    m.import_ccswitch(&f.db(), true, &[], false).unwrap();
    let id = m.list().unwrap()[0].id.clone();
    insert(
        &source,
        "vendor",
        "Imported",
        &native("https://example.com/v1", "after"),
        "fixture-key",
        json!({}),
    );
    let binary = f.path.join("mock-cli");
    fs::write(
        &binary,
        "#!/bin/sh\nif [ \"$1\" = --help ]; then echo --no-daemon; fi\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        m.run(&id, &f.path, None, None, &binary, &[])
            .unwrap()
            .success()
    );
    assert_eq!(m.resolve(&id).unwrap().revision, 2);
    let used: i64 = m
        .conn
        .query_row("SELECT revision FROM runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(used, 2);
}

#[test]
fn cc_switch_common_and_provider_remain_in_one_read_snapshot() {
    let f = Fixture::new();
    let source = f.source();
    source.execute_batch("PRAGMA journal_mode=WAL;INSERT INTO settings VALUES ('common_config_codex','model_reasoning_effort=\"low\"');").unwrap();
    insert(
        &source,
        "vendor",
        "Old",
        &native("https://example.com/v1", "before"),
        "fixture-key",
        json!({"commonConfigEnabled":true}),
    );
    let result = ccswitch::read_with_hook(&f.db(), || {
        source.execute_batch("BEGIN IMMEDIATE;UPDATE providers SET name='New';UPDATE settings SET value='model_reasoning_effort=\"high\"';COMMIT;")?;
        Ok(())
    }).unwrap();
    assert_eq!(result.candidates[0].name, "Old");
    assert!(result.candidates[0].config.contains("\"low\""));
    let next = ccswitch::read(&f.db()).unwrap();
    assert_eq!(next.candidates[0].name, "New");
    assert!(next.candidates[0].config.contains("\"high\""));
}

#[test]
fn inactive_provider_secrets_never_reach_saved_config_or_exports() {
    let f = Fixture::new();
    let mut m = f.manager();
    let text = "model='m'\nmodel_provider='active'\n[model_providers.active]\nbase_url='https://example.com/v1'\n[model_providers.inactive]\nbase_url='https://inactive.example/v1'\nexperimental_bearer_token='fixture-other-secret'\nauth='fixture-other-secret'\n";
    let p = m.add("Inactive", text, "fixture-active-key").unwrap();
    assert!(
        !m.revision(&p.id, 1)
            .unwrap()
            .config
            .contains("fixture-other-secret")
    );
    m.export(&p.id, &f.path.join("export.toml")).unwrap();
    assert!(
        !fs::read_to_string(f.path.join("export.toml"))
            .unwrap()
            .contains("fixture-other-secret")
    );
}

#[test]
fn byok_project_trust_updates_survive_relaunch_without_changing_the_revision() {
    let f = Fixture::new();
    let mut m = f.manager();
    let p = m
        .add(
            "Trust",
            &native("https://example.com/v1", "m"),
            "fixture-key",
        )
        .unwrap();
    let saved = m.revision(&p.id, 1).unwrap().config;
    let home = m.materialize(&p.id, 1).unwrap();
    let mut runtime = saved.clone();
    runtime.push_str("\n[projects.\"/fixture/project\"]\ntrust_level='trusted'\n");
    fs::write(home.join("config.toml"), &runtime).unwrap();
    assert_eq!(m.materialize(&p.id, 1).unwrap(), home);
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        runtime
    );
    assert_eq!(m.revision(&p.id, 1).unwrap().config, saved);
    // Native updates are mutable; model and credential routing remain versioned.
    let changed = config::patch(&runtime, None, Some("different-model"), None).unwrap();
    fs::write(home.join("config.toml"), &changed).unwrap();
    assert!(m.materialize(&p.id, 1).is_err());
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        changed
    );
}

#[test]
fn byok_runtime_config_only_excludes_valid_project_trust_state() {
    let saved = "model='m'\n[projects.\"/saved/project\"]\ntrust_level='trusted'\ncustom='kept'\n";
    let changed = saved.replace("trust_level='trusted'", "trust_level='untrusted'");
    assert!(config::matches_runtime_config(&changed, saved));
    assert!(!config::matches_runtime_config(
        &changed.replace("custom='kept'", "custom='changed'"),
        saved
    ));
    assert!(!config::matches_runtime_config(
        &changed.replace("'untrusted'", "'invalid'"),
        saved
    ));
    assert!(!config::matches_runtime_config(
        &changed.replace("'untrusted'", "true"),
        saved
    ));
    assert!(!config::matches_runtime_config(
        "model='m'\nprojects=1",
        "model='m'"
    ));
    assert!(!config::matches_runtime_config(
        "model='m'\n[projects]\nwrong=1",
        "model='m'"
    ));
    assert!(!config::matches_runtime_config(
        "model='m'\n[projects.\"/new\"]\nmodel_provider='other'",
        "model='m'"
    ));
}

#[test]
fn oauth_argument_policy_respects_prompt_boundaries_and_option_values() {
    let allowed = [
        vec!["exec", "--", "-please explain this project"],
        vec!["exec", "--", "--config=not a flag"],
        vec!["exec", "--", "--remote"],
        vec!["--image", "-profile.png", "exec", "explain"],
        vec!["exec", "--output-last-message", "-capture.txt", "hello"],
        vec!["--model", "logout", "exec", "explain"],
        vec!["exec", "logout"],
    ];
    for args in allowed {
        assert!(
            oauth::validate_run_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .is_ok(),
            "{args:?}"
        );
    }
    let forbidden = [
        vec!["--model", "m", "logout"],
        vec!["--sandbox", "workspace-write", "login"],
        vec!["--", "logout"],
        vec!["exec", "--profile=other", "hello"],
        vec!["exec", "-cmodel_provider='other'", "hello"],
        vec!["exec", "--remote=server", "hello"],
    ];
    for args in forbidden {
        assert!(
            oauth::validate_run_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .is_err(),
            "{args:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn concurrent_native_processes_only_inherit_their_own_account_run_lease() {
    struct Running(std::process::Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let f = Fixture::new();
    let mut m = f.manager();
    let source = oauth_source(&f, "workspace-a", "fixture-refresh-a");
    let a = m.import_oauth(Some("A"), None, &source, None).unwrap();
    let source = oauth_source(&f, "workspace-b", "fixture-refresh-b");
    let b = m.import_oauth(Some("B"), None, &source, None).unwrap();
    let lease_a = m.oauth_run_lease(&a.id).unwrap();
    let mut command_a = Command::new("python3");
    command_a.args(["-c", "import time; time.sleep(10)"]);
    launcher::inherit_run_lease(&mut command_a, &lease_a).unwrap();
    let mut child_a = Running(command_a.spawn().unwrap());
    let lease_b = m.oauth_run_lease(&b.id).unwrap();
    let mut command_b = Command::new("python3");
    command_b.args(["-c", "import time; time.sleep(10)"]);
    launcher::inherit_run_lease(&mut command_b, &lease_b).unwrap();
    let mut child_b = Running(command_b.spawn().unwrap());
    drop(command_a);
    drop(command_b);
    drop(lease_a);
    drop(lease_b);
    assert!(m.oauth_lease(&a.id).is_err());
    assert!(m.oauth_lease(&b.id).is_err());
    child_a.0.kill().unwrap();
    child_a.0.wait().unwrap();
    assert!(child_b.0.try_wait().unwrap().is_none());
    assert!(m.oauth_lease(&a.id).is_ok(), "B must not retain A's lease");
    assert!(m.oauth_lease(&b.id).is_err());
    child_b.0.kill().unwrap();
    child_b.0.wait().unwrap();
    assert!(m.oauth_lease(&b.id).is_ok());
}

#[test]
fn byok_cli_onboarding_and_migration_state_survive_relaunch() {
    let f = Fixture::new();
    let mut m = f.manager();
    let p = m
        .add(
            "Native state",
            &native("https://example.com/v1", "fixture-model"),
            "fixture-key",
        )
        .unwrap();
    let saved = m.revision(&p.id, 1).unwrap().config;
    let home = m.materialize(&p.id, 1).unwrap();
    let runtime = format!(
        "{saved}\n[projects.\"/fixture/project\"]\ntrust_level='trusted'\n[notice.model_migrations]\nfixture-model='fixture-model-v2'\n[tui]\nscreen_reader_detection_done=true\n[tui.model_availability_nux]\nfixture-model-v2=1\n"
    );
    fs::write(home.join("config.toml"), &runtime).unwrap();
    assert_eq!(m.materialize(&p.id, 1).unwrap(), home);
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        runtime
    );
    assert_eq!(m.revision(&p.id, 1).unwrap().config, saved);
    // Runtime metadata never authorizes a changed endpoint or model.
    let changed = config::patch(&runtime, Some("https://other.example/v1"), None, None).unwrap();
    fs::write(home.join("config.toml"), &changed).unwrap();
    assert!(m.materialize(&p.id, 1).is_err());
}

#[test]
fn byok_runtime_metadata_requires_known_fields_and_valid_shapes() {
    let saved = "model='m'\n[tui]\nnotifications=false\n";
    let allowed = "model='m'\n[notice.model_migrations]\nm='new-model'\n[tui]\nnotifications=false\nscreen_reader_detection_done=true\n[tui.model_availability_nux]\nnew-model=1\n";
    assert!(config::matches_runtime_config(allowed, saved));
    assert!(!config::matches_runtime_config(
        &allowed.replace("notifications=false", "notifications=true"),
        saved
    ));
    assert!(!config::matches_runtime_config(
        &allowed.replace(
            "screen_reader_detection_done=true",
            "screen_reader_detection_done='yes'"
        ),
        saved
    ));
    assert!(!config::matches_runtime_config(
        &allowed.replace("m='new-model'", "m=1"),
        saved
    ));
    assert!(!config::matches_runtime_config(
        &allowed.replace("new-model=1", "new-model='yes'"),
        saved
    ));
    assert!(!config::matches_runtime_config(
        "model='m'\nnotice=1",
        "model='m'"
    ));
    assert!(!config::matches_runtime_config(
        "model='m'\ntui=1",
        "model='m'"
    ));
    assert!(!config::matches_runtime_config(
        "model='m'\n[tui]\nmodel_provider='other'",
        "model='m'"
    ));
    assert!(!config::matches_runtime_config(
        "model='m'\n[notice]\nmodel_provider='other'",
        "model='m'"
    ));
}

#[test]
fn byok_hook_review_state_survives_relaunch_without_allowing_new_definitions() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let profile = manager
        .add(
            "Hooks",
            &native("https://example.test/v1", "m"),
            "fixture-key",
        )
        .unwrap();
    let home = manager.materialize(&profile.id, 1).unwrap();
    let saved = manager.revision(&profile.id, 1).unwrap().config;
    let runtime = format!(
        "{saved}\n[hooks.state.\"/<session-flags>/config.toml:permission_request:0:0\"]\nenabled=true\ntrusted_hash='sha256:{}'\n",
        "a".repeat(64)
    );
    fs::write(home.join("config.toml"), &runtime).unwrap();
    manager.materialize(&profile.id, 1).unwrap();
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        runtime
    );
    assert_eq!(manager.revision(&profile.id, 1).unwrap().config, saved);
    for invalid in [
        runtime.replace("enabled=true", "enabled='yes'"),
        runtime.replace("trusted_hash='sha256:", "trusted_hash='invalid:"),
        format!("{runtime}\ncommand='run another hook'\n"),
        format!(
            "{runtime}\n[[hooks.PermissionRequest]]\n[[hooks.PermissionRequest.hooks]]\ntype='command'\ncommand='new hook'\n"
        ),
    ] {
        assert!(!config::matches_runtime_config(&invalid, &saved));
    }
}
