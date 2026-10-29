use super::*;
use fs2::FileExt;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("usage-native-store-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn directory(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn resolve(home: &Path, own: &Path, state: Option<&Path>) -> SessionStore {
    SessionStore::resolve_in(home, own, state.map(Path::as_os_str), false)
}

#[test]
fn state_precedence_and_environment_scope_are_shared_by_every_reader() {
    let fixture = Fixture::new();
    let home = fixture.directory("home");
    let unrelated = fixture.directory("other-home");
    let environment = fixture.directory("environment-state");
    let recorded = fixture.directory("recorded-state");
    fs::write(
        home.join("config.toml"),
        "sqlite_home='../configured-state'",
    )
    .unwrap();
    assert_eq!(
        resolve(&home, &home, Some(&environment)).index_dir(),
        home.join("../configured-state")
    );
    fs::remove_file(home.join("config.toml")).unwrap();
    assert_eq!(
        resolve(&home, &home, Some(&environment)).index_dir(),
        environment
    );
    assert_eq!(
        resolve(&home, &unrelated, Some(&environment)).index_dir(),
        home
    );
    assert_eq!(resolve(&home, &home, None).index_dir(), home);
    fs::write(
        home.join(SHARED_INDEX_MARKER),
        serde_json::to_vec(&recorded).unwrap(),
    )
    .unwrap();
    assert_eq!(resolve(&home, &unrelated, None).index_dir(), recorded);
    assert_eq!(
        resolve(&home, &unrelated, Some(&environment)).index_dir(),
        environment
    );
    fs::write(
        home.join("config.toml"),
        "sqlite_home='../configured-state'",
    )
    .unwrap();
    assert_eq!(
        resolve(&home, &unrelated, Some(&environment)).index_dir(),
        home.join("../configured-state")
    );
}

#[cfg(unix)]
#[test]
fn only_a_matching_marker_and_session_link_identify_a_shared_store() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let account = fixture.directory("account");
    let base = fixture.directory("base");
    let other = fixture.directory("other");
    fs::create_dir(base.join("sessions")).unwrap();
    let base_store = resolve(&base, &base, None);
    symlink(base.join("sessions"), account.join("sessions")).unwrap();
    assert!(!resolve(&account, &base, None).same_storage(&base_store));
    fs::write(
        account.join(ALIAS_MARKER),
        serde_json::to_vec(&other).unwrap(),
    )
    .unwrap();
    assert_eq!(resolve(&account, &base, None).root(), account);
    fs::write(
        account.join(ALIAS_MARKER),
        serde_json::to_vec(&base).unwrap(),
    )
    .unwrap();
    assert!(resolve(&account, &base, None).same_storage(&base_store));
    fs::remove_file(account.join("sessions")).unwrap();
    assert!(!resolve(&account, &base, None).same_storage(&base_store));
}

#[test]
fn sharing_an_index_does_not_merge_independent_session_identity() {
    let fixture = Fixture::new();
    let a = fixture.directory("a");
    let b = fixture.directory("b");
    let index = fixture.directory("index");
    let config = format!(
        "sqlite_home={}",
        toml::Value::String(index.to_string_lossy().into_owned())
    );
    fs::write(a.join("config.toml"), &config).unwrap();
    fs::write(b.join("config.toml"), config).unwrap();
    let a_store = resolve(&a, &a, None);
    let b_store = resolve(&b, &b, None);
    assert_eq!(a_store.index_dir(), b_store.index_dir());
    assert!(!a_store.same_storage(&b_store));
    assert!(!a_store.same_storage(&resolve(&fixture.0.join("missing"), &a, None)));
}

#[test]
fn index_location_version_and_rollout_reconciliation_come_from_one_layout() {
    let fixture = Fixture::new();
    let home = fixture.directory("home");
    let index = fixture.directory("index");
    fs::write(home.join("config.toml"), "sqlite_home='../index'").unwrap();
    for name in [
        "state_5.sqlite",
        "state_12.sqlite",
        "state_100.sqlite-wal",
        "state_bad.sqlite",
    ] {
        fs::write(index.join(name), b"").unwrap();
    }
    let store = resolve(&home, &home, None);
    assert_eq!(
        store.index().unwrap(),
        Some((12, home.join("../index/state_12.sqlite")))
    );
    assert!(!store.merge_rollouts());
    fs::write(
        home.join(SHARED_INDEX_MARKER),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
    assert!(resolve(&home, &home, None).merge_rollouts());
}

#[test]
fn explicit_migration_preserves_auth_and_invalidates_an_old_mutation_location() {
    let fixture = Fixture::new();
    let source = fixture.directory("legacy-home");
    let destination = fixture.directory("shared-home");
    fs::write(destination.join("config.toml"), "sqlite_home='../index'").unwrap();
    fs::write(source.join("auth.json"), "synthetic-auth").unwrap();
    fs::create_dir(source.join("sessions")).unwrap();
    fs::write(source.join("sessions/fixture.jsonl"), "synthetic-history").unwrap();
    let stale_store = resolve(&source, &source, None);
    let target = resolve(&destination, &destination, None);
    target.attach_legacy_home(&source).unwrap();
    target.attach_legacy_home(&source).unwrap();
    let resolved = resolve(&source, &destination, None);
    assert!(resolved.same_storage(&target));
    assert!(resolved.merge_rollouts());
    assert_eq!(resolved.index_dir(), destination.join("../index"));
    assert_eq!(
        fs::read_to_string(source.join("auth.json")).unwrap(),
        "synthetic-auth"
    );
    assert!(!destination.join("auth.json").exists());
    assert_eq!(
        fs::read_to_string(destination.join("sessions/fixture.jsonl")).unwrap(),
        "synthetic-history"
    );
    assert!(stale_store.mutation_guard().is_err());
    let id = uuid::Uuid::new_v4().to_string();
    let writer = fs::File::create(
        destination
            .join("thread-writer-locks")
            .join(format!("{id}.lock")),
    )
    .unwrap();
    writer.lock_exclusive().unwrap();
    let mut mutation = resolved.mutation_guard().unwrap();
    assert!(mutation.require_idle(std::slice::from_ref(&id)).is_err());
    drop(writer);
    mutation.require_idle(&[id]).unwrap();
}
