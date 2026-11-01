use super::*;
use crate::http::{FakeHttp, HttpResponse};
use serde_json::json;

fn response(value: Value) -> std::result::Result<HttpResponse, crate::http::TransportError> {
    Ok(HttpResponse {
        status: 200,
        headers: Default::default(),
        body: serde_json::to_vec(&value).unwrap(),
    })
}
#[tokio::test]
async fn external_catalog_keeps_route_ids_and_capabilities_without_vendor_branches() {
    let http = FakeHttp::new();
    http.route(
        "http://127.0.0.1:3425/api/hello",
        response(json!({"name":"magpie","version":"fixture"})),
    );
    http.route("http://127.0.0.1:3425/v1/models",response(json!({"data":[
        {"id":"zcode/model","owned_by":"zcode","context_window":128000,"reasoning_levels":["low","high"],"modalities":{"input":["text","image"],"output":["text"]}},
        {"id":"group/coding"},{"id":"auto/model"},{"id":"image-only","modalities":{"output":["image"]}},
        {"id":"image-kind","type":"model","kind":"image"}
    ]})));
    let connection = Connection::new("http://127.0.0.1:3425/v1/", LOCAL_KEY).unwrap();
    assert_eq!(connection.hello(&http).await.unwrap().version, "fixture");
    let models = connection.models(&http).await.unwrap();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["group/coding", "auto/model", "zcode/model"]
    );
    let native = catalog::codex_models(&models).unwrap();
    assert_eq!(native["models"][2]["context_window"], 128000);
    assert_eq!(
        native["models"][2]["input_modalities"],
        json!(["text", "image"])
    );
    assert_eq!(
        native["models"][2]["supported_reasoning_levels"][1]["effort"],
        "high"
    );
}
#[test]
fn bind_and_migration_keep_profile_history_and_refuse_oauth_conversion() {
    let root = std::env::temp_dir().join(format!("usage-magpie-bind-{}", uuid::Uuid::new_v4()));
    let mut manager = Manager::open(root.clone()).unwrap();
    let legacy = manager
        .add(
            "Legacy",
            &codex_manager::config::new_config(
                "https://usage-plugin.invalid/account/v1",
                "old",
                None,
            )
            .unwrap(),
            "old-key",
        )
        .unwrap();
    let connection = Connection::new("https://gateway.example/subpath/v1", "remote-key").unwrap();
    let profile = connection
        .bind(
            &mut manager,
            "Migrated",
            "group/coding",
            None,
            Some("account-id"),
            Some(&legacy.id),
        )
        .unwrap();
    assert_eq!(legacy.id, profile.id);
    assert_eq!(profile.revision, legacy.revision + 1);
    assert!(legacy_endpoint(
        &codex_manager::config::summary(
            &manager
                .revision(&legacy.id, legacy.revision)
                .unwrap()
                .config
        )
        .unwrap()
        .0
    ));
    let saved = manager.revision(&profile.id, profile.revision).unwrap();
    assert!(is_config(&saved.config));
    assert!(!saved.config.contains("remote-key"));
    let doc = codex_manager::config::parse(&saved.config).unwrap();
    assert_eq!(
        doc["model_providers"][PROVIDER]["http_headers"]["X-Magpie-Account"].as_str(),
        Some("account-id")
    );
    assert_eq!(profile.base_url, "https://gateway.example/subpath/v1");
    let auth = root.join("synthetic-auth.json");
    std::fs::write(&auth,json!({"tokens":{"id_token":"synthetic-id","access_token":"synthetic-access","refresh_token":"synthetic-refresh","account_id":"workspace"},"user_id":"user","email":"fixture@example.invalid"}).to_string()).unwrap();
    let oauth = manager
        .import_oauth(Some("Native OAuth"), None, &auth, None)
        .unwrap();
    let error = connection
        .bind(
            &mut manager,
            "Wrong",
            "group/coding",
            None,
            None,
            Some(&oauth.id),
        )
        .unwrap_err();
    assert!(error.contains("原生 OAuth"));
    assert_eq!(manager.resolve(&oauth.id).unwrap().revision, oauth.revision);
    drop(manager);
    let _ = std::fs::remove_dir_all(root);
}
#[test]
fn quota_mapping_preserves_units_and_does_not_invent_zero_for_unknown_or_failed() {
    let (metrics, notes) = resources::quota_metrics(&json!({"data":[
        {"name":"Credits","windows":[{"name":"credit","amount":1.25,"limit":10,"unit":"credits"}]},
        {"name":"Deferred","readDeferred":true,"windows":[{"name":"week","used":0}]},
        {"name":"Failed","error":"private-vendor-error","windows":[{"name":"week","used":0}]},
        {"name":"Balance","balance":"¥12.30","windows":[]}
    ]}))
    .unwrap();
    assert_eq!(metrics.len(), 2);
    assert!(
        matches!(&metrics[0],crate::resources::ResourceMetric::AllowanceWindow {remaining,unit,..} if remaining=="8.75" && unit=="credits")
    );
    assert!(
        matches!(&metrics[1], crate::resources::ResourceMetric::ReportedBalance { display, .. } if display == "¥12.30")
    );
    assert!(!notes.join(" ").contains("private-vendor-error"));
}

#[tokio::test]
async fn builtin_catalog_collapses_legacy_presets_and_attaches_usage_and_balance() {
    let root = std::env::temp_dir().join(format!("usage-magpie-catalog-{}", uuid::Uuid::new_v4()));
    let mut manager = Manager::open(root.clone()).unwrap();
    let connection = Connection::new("http://127.0.0.1:3425", LOCAL_KEY).unwrap();
    for (name, model) in [("A", "group/coding"), ("B", "vendor/model")] {
        let legacy = manager
            .add(
                name,
                &codex_manager::config::new_config(
                    "https://usage-plugin.invalid/account/v1",
                    "old",
                    None,
                )
                .unwrap(),
                name,
            )
            .unwrap();
        connection
            .bind(&mut manager, name, model, None, None, Some(&legacy.id))
            .unwrap();
    }
    let http = Arc::new(FakeHttp::new());
    http.route(
        "http://127.0.0.1:3425/v1/magpie/quotas",
        response(json!({"data":[
            {"name":"Wallet","balance":"¥12.30","windows":[]}
        ]})),
    );
    http.route(
        "http://127.0.0.1:3425/v1/magpie/usage?period=today",
        response(summary()),
    );
    let mut plan = crate::provider_catalog::CatalogService::new(
        root.clone(),
        http.clone(),
        Arc::new(|_| panic!("no external credential lookup")),
    )
    .with_builtin_magpie()
    .load(&crate::config::AppConfig::default())
    .unwrap();
    assert_eq!(plan.catalog.providers.len(), 1);
    assert_eq!(plan.catalog.providers[0].id, BUILTIN_ID);
    assert!(
        plan.catalog
            .providers
            .iter()
            .all(|p| p.agent.as_deref() == Some("Codex"))
    );
    assert_eq!(plan.resources.len(), 2);
    assert_eq!(plan.resources[0].service(), "magpie_quotas");
    assert_eq!(
        plan.catalog
            .providers
            .iter()
            .filter(|p| p.usage.sources.len() == 2)
            .count(),
        1
    );
    for reader in &plan.resources {
        let target = reader.binding().cache_key(reader.service());
        assert!(plan.catalog.apply_resource(&target, reader.read().await));
    }
    assert_eq!(http.calls.lock().unwrap().len(), 2);
    for profile in &plan.catalog.providers {
        assert!(matches!(&profile.usage.sources[0].state,
            crate::provider_catalog::QueryState::Result(s) if s.metrics.iter().any(|metric|
                matches!(metric, crate::resources::ResourceMetric::ReportedBalance { display, .. } if display == "¥12.30"))));
        assert!(matches!(&profile.usage.sources[1].state,
            crate::provider_catalog::QueryState::Result(s) if matches!(s.metrics[0], crate::resources::ResourceMetric::RequestUsage { requests: 3, .. })));
    }
    assert!(
        plan.catalog
            .providers
            .iter()
            .all(|p| p.performance.is_none())
    );
    drop(manager);
    let _ = std::fs::remove_dir_all(root);
}

fn summary() -> Value {
    json!({"period":"today","calls":3,"errors":1,"input":120,"output":30,
        "cache_read":40,"cache_write":10,"reasoning":5,"unpriced":1,"cost":0.0123})
}

#[test]
fn builtin_magpie_can_be_configured_but_never_added_copied_or_removed() {
    let root =
        std::env::temp_dir().join(format!("usage-magpie-duplicates-{}", uuid::Uuid::new_v4()));
    let mut manager = Manager::open(root.clone()).unwrap();
    let connection = Connection::new("http://127.0.0.1:3425", LOCAL_KEY).unwrap();
    let original = connection
        .configure(&mut manager, "vendor/model", None, None)
        .unwrap();
    let original_config = manager
        .revision(&original.id, original.revision)
        .unwrap()
        .config;
    let equivalent = Connection::new("http://127.0.0.1:3425/v1/", LOCAL_KEY).unwrap();
    assert!(
        equivalent
            .bind(
                &mut manager,
                "Another name",
                "vendor/model",
                None,
                None,
                None
            )
            .unwrap_err()
            .contains("内置 Provider")
    );
    assert_eq!(manager.list().unwrap().len(), 1);
    assert_eq!(manager.resolve(&original.id).unwrap().revision, 1);
    for (model, effort, account) in [
        ("vendor/other", None, None),
        ("vendor/model", Some("high"), None),
        ("vendor/model", None, Some("account-a")),
    ] {
        let saved = connection
            .configure(&mut manager, model, effort, account)
            .unwrap();
        assert_eq!(saved.id, original.id);
    }
    let config = connection.config("vendor/other", None, None).unwrap();
    assert!(
        manager
            .add("Another", &config, "another-key")
            .unwrap_err()
            .to_string()
            .contains("内置")
    );
    for selector in [&original.id, BUILTIN_ID] {
        assert!(
            manager
                .copy(selector, "Copy")
                .unwrap_err()
                .to_string()
                .contains("不能复制")
        );
        assert!(
            manager
                .remove(selector)
                .unwrap_err()
                .to_string()
                .contains("不能删除")
        );
    }
    assert_eq!(manager.list().unwrap().len(), 1);
    assert_eq!(manager.resolve(BUILTIN_ID).unwrap().revision, 4);
    assert_eq!(
        manager.revision(&original.id, 1).unwrap().config,
        original_config
    );
    drop(manager);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn builtin_magpie_exists_without_a_store_and_discovery_never_writes() {
    let root = std::env::temp_dir().join(format!("usage-builtin-magpie-{}", uuid::Uuid::new_v4()));
    let plan = crate::provider_catalog::CatalogService::new(
        root.clone(),
        Arc::new(FakeHttp::new()),
        Arc::new(|_| panic!("no external credentials")),
    )
    .with_builtin_magpie()
    .load(&crate::config::AppConfig::default())
    .unwrap();
    assert!(!root.exists());
    assert_eq!(plan.catalog.providers.len(), 1);
    assert_eq!(plan.catalog.providers[0].id, BUILTIN_ID);
    assert_eq!(plan.resources.len(), 2);
}

#[test]
fn usage_summary_preserves_token_categories_and_marks_partial_estimates() {
    let (metrics, notes) = resources::usage_metrics(&summary()).unwrap();
    assert!(matches!(
        &metrics[0],
        crate::resources::ResourceMetric::RequestUsage {
            requests: 3,
            failed_requests: 1,
            input_tokens: 120,
            output_tokens: 30,
            cache_read_tokens: 40,
            cache_write_tokens: 10,
            reasoning_tokens: 5,
            ..
        }
    ));
    let notes = notes.join(" ");
    assert!(
        notes.contains("0.0123 USD")
            && notes.contains("1 次请求未定价")
            && notes.contains("非当前预设独占")
    );
    for field in ["calls", "input", "cache_read", "period"] {
        let mut value = summary();
        value.as_object_mut().unwrap().remove(field);
        assert!(resources::usage_metrics(&value).is_err());
    }
    let mut value = summary();
    value["input"] = json!(-1);
    assert!(resources::usage_metrics(&value).is_err());
    let mut value = summary();
    value.as_object_mut().unwrap().remove("cost");
    let (_, notes) = resources::usage_metrics(&value).unwrap();
    assert!(!notes.join(" ").contains("USD"));
}

#[test]
fn omitted_zero_amount_is_known_only_with_explicit_zero_usage() {
    let (metrics, _) = resources::quota_metrics(&json!({"data":[
        {"name":"Trae","windows":[{"name":"Credits","used":0,"remaining":100,"limit":1100,"unit":"credits"}]},
        {"name":"Unknown","windows":[{"name":"Credits","limit":1100,"unit":"credits"}]}
    ]})).unwrap();
    assert_eq!(metrics.len(), 1);
    assert!(
        matches!(&metrics[0], crate::resources::ResourceMetric::AllowanceWindow {
        used, remaining, unit, ..
    } if used=="0" && remaining=="1100" && unit=="credits")
    );
}

#[tokio::test]
async fn summary_permission_failure_does_not_hide_available_balance() {
    use crate::resources::{ResourceReader, ResourceState};
    let http = Arc::new(FakeHttp::new());
    http.route(
        "http://127.0.0.1:3425/v1/magpie/quotas",
        response(json!({"data":[
            {"name":"Balance","balance":"12.30 credits","windows":[]}
        ]})),
    );
    http.route(
        "http://127.0.0.1:3425/v1/magpie/usage?period=today",
        Ok(HttpResponse {
            status: 403,
            headers: Default::default(),
            body: b"private-error".to_vec(),
        }),
    );
    let connection = Connection::new("http://127.0.0.1:3425", LOCAL_KEY).unwrap();
    let quota = GatewayReader::quotas(connection.clone(), http.clone())
        .read()
        .await;
    let usage = GatewayReader::usage(connection, http).read().await;
    assert_eq!(quota.state, ResourceState::Ready);
    assert!(
        matches!(&quota.metrics[0], crate::resources::ResourceMetric::ReportedBalance { display, .. } if display == "12.30 credits")
    );
    assert_eq!(usage.state, ResourceState::TransientFailure);
    assert!(usage.metrics.is_empty());
    assert!(usage.message.unwrap().contains("权限"));
}

#[tokio::test]
async fn offline_permission_and_invalid_identity_errors_never_become_empty_success() {
    let connection = Connection::new("http://127.0.0.1:3425", "synthetic-private-key").unwrap();
    let http = FakeHttp::new();
    http.route(
        "http://127.0.0.1:3425/api/hello",
        response(json!({"name":"other","version":"0"})),
    );
    assert!(connection.hello(&http).await.is_err());
    http.route(
        "http://127.0.0.1:3425/v1/models",
        Ok(HttpResponse {
            status: 403,
            headers: Default::default(),
            body: b"synthetic-private-key".to_vec(),
        }),
    );
    let error = connection.models(&http).await.unwrap_err();
    assert!(error.contains("权限"));
    assert!(!error.contains("synthetic-private-key"));
}
