use super::*;
use crate::{
    codex_manager::tests::Fixture,
    http::{FakeHttp, HttpResponse, TransportError},
};
use serde_json::json;

fn registry(value: Value, status: u16) -> FakeHttp {
    let http = FakeHttp::new();
    http.route(
        REGISTRY,
        Ok(HttpResponse {
            status,
            headers: Default::default(),
            body: serde_json::to_vec(&value).unwrap(),
        }),
    );
    http
}
fn package(f: &Fixture, version: &str) -> PathBuf {
    let path = f.path.join("lmfpd/bin/fpd.js");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "not executed").unwrap();
    fs::write(
        path.parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("package.json"),
        json!({"name":"lmfpd", "version":version}).to_string(),
    )
    .unwrap();
    path
}
#[tokio::test]
async fn semantic_updates_are_cached_public_and_disappear_after_install() {
    let f = Fixture::new();
    let root = f.path.join("data");
    let fpd = package(&f, "0.0.1792242250521");
    let http = registry(json!({"name":"lmfpd","version":"0.0.1792242250522"}), 200);
    check(&root, &http, 100_000).await;
    check(&root, &http, 100_000 + SUCCESS_TTL - 1).await;
    {
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "GET");
        assert_eq!(calls[0].url, REGISTRY);
        assert!(calls[0].headers.is_empty());
        assert!(calls[0].body.is_none());
    }
    assert!(
        notice(&root, Some(&fpd))
            .unwrap()
            .contains("0.0.1792242250521 → 0.0.1792242250522")
    );
    package(&f, "0.0.1792242250522");
    assert!(notice(&root, Some(&fpd)).is_none());
    package(&f, "0.0.1792242250523");
    assert!(notice(&root, Some(&fpd)).is_none());
    check(&root, &http, 100_000 + SUCCESS_TTL).await;
    assert_eq!(http.calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn failed_checks_back_off_and_keep_last_success() {
    let f = Fixture::new();
    let fpd = package(&f, "0.0.1");
    let ok = registry(json!({"name":"lmfpd","version":"0.0.2"}), 200);
    check(&f.path, &ok, 100_000).await;
    let failed = FakeHttp::new();
    failed.route(REGISTRY, Err(TransportError("offline".into())));
    let attempt = 100_000 + SUCCESS_TTL;
    check(&f.path, &failed, attempt).await;
    check(&f.path, &failed, attempt + FAILURE_TTL - 1).await;
    assert_eq!(failed.calls.lock().unwrap().len(), 1);
    assert!(
        notice(&f.path, Some(&fpd))
            .unwrap()
            .contains("0.0.1 → 0.0.2")
    );
    let cache: Cache = read_json(&cache_path(&f.path)).unwrap();
    assert_eq!(cache.succeeded_at, Some(100_000));
    check(&f.path, &failed, attempt + FAILURE_TTL).await;
    assert_eq!(failed.calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn invalid_metadata_never_becomes_an_update() {
    let f = Fixture::new();
    let fpd = package(&f, "0.0.1");
    for (value, status) in [
        (json!({"name":"lmfpd", "version":"bad"}), 200),
        (json!({"name":"other", "version":"9.0.0"}), 200),
        (json!({"name":"lmfpd", "version":"9.0.0"}), 429),
        (
            json!({"name":"lmfpd", "version":"9.0.0", "extra":"x".repeat(MAX_BYTES)}),
            200,
        ),
    ] {
        let _ = fs::remove_file(cache_path(&f.path));
        check(&f.path, &registry(value, status), 100_000).await;
        assert!(notice(&f.path, Some(&fpd)).is_none());
    }
}
#[test]
fn semver_precedence_and_package_identity_are_respected() {
    let f = Fixture::new();
    let fpd = package(&f, "1.0.0-beta.1");
    let mut cache = Cache {
        attempted_at: 100_000,
        succeeded_at: Some(100_000),
        latest: Some("1.0.0".into()),
    };
    store::save(&cache_path(&f.path), &cache).unwrap();
    assert!(notice(&f.path, Some(&fpd)).is_some());
    package(&f, "1.0.0+local");
    cache.latest = Some("1.0.0+registry".into());
    store::save(&cache_path(&f.path), &cache).unwrap();
    assert!(notice(&f.path, Some(&fpd)).is_none());
    fs::write(
        fpd.parent().unwrap().parent().unwrap().join("package.json"),
        json!({"name":"other", "version":"1.0.0"}).to_string(),
    )
    .unwrap();
    assert!(installed(&fpd).is_none());
    assert!(installed(Path::new("/missing/fpd")).is_none());
}
#[tokio::test]
async fn no_installation_does_not_contact_registry_or_initialize_store() {
    let f = Fixture::new();
    let root = f.path.join("missing");
    let http = FakeHttp::new();
    FingerprintService::new(root.clone())
        .check_updates(&http)
        .await;
    assert!(http.calls.lock().unwrap().is_empty());
    assert!(!root.exists());
}

#[tokio::test]
async fn configured_detector_flows_through_service_snapshots_and_install_symlinks() {
    use crate::codex_manager::tests::oauth_source;
    let f = Fixture::new();
    let mut manager = f.manager();
    let source = oauth_source(&f, "update-fixture", "synthetic-refresh");
    let p = manager.import_oauth(None, None, &source, None).unwrap();
    let fpd = package(&f, "0.0.1");
    let service = FingerprintService::new(manager.root().to_owned());
    service.enable(&p.id, Some(&fpd)).unwrap();
    let http = registry(json!({"name":"lmfpd","version":"0.0.2"}), 200);
    service.check_updates(&http).await;
    assert_eq!(http.calls.lock().unwrap().len(), 1);
    assert!(
        service
            .status(&p.id)
            .unwrap()
            .update_notice
            .unwrap()
            .contains("0.0.1 → 0.0.2")
    );
    assert!(service.snapshots().unwrap()[0].update_notice.is_some());
    #[cfg(unix)]
    {
        let link = default_fpd(manager.root());
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&fpd, &link).unwrap();
        assert_eq!(installed(&link).unwrap().to_string(), "0.0.1");
    }
}
