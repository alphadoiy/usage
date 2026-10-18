use super::*;
struct Fixture {
    path: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("usage-plugin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self { path }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
use axum::{
    Router,
    extract::{Path as RoutePath, State},
    http::HeaderMap,
    routing::post,
};
use serde_json::json;

struct Harness {
    f: Fixture,
    store: store::Store,
    runtime: Runtime,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn harness() -> Harness {
    let f = Fixture::new();
    let root = f.path.join("plugins");
    let store = store::Store::new(root.clone());
    let entry = f.path.join("fixture.mjs");
    fs::write(&entry, include_str!("fixture.mjs")).unwrap();
    store
        .register(store::Plugin {
            id: "fixture".into(),
            package: "local".into(),
            version: "1.0.0".into(),
            entry,
            runtime: "node".into(),
            enabled: true,
            revision: 1,
        })
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runtime = Runtime::new(root, listener.local_addr().unwrap().port());
    async fn broker(
        State(runtime): State<Runtime>,
        RoutePath(plugin): RoutePath<String>,
        headers: HeaderMap,
        axum::Json(value): axum::Json<Value>,
    ) -> axum::response::Response {
        let token = headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap();
        runtime.credentials(&plugin, token, value).await
    }
    let router = Router::new()
        .route("/_usage/plugins/credentials/{plugin}", post(broker))
        .with_state(runtime.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Harness {
        f,
        store,
        runtime,
        server,
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_contracts_account_scope_refresh_and_responses_stream_work_end_to_end() {
    let h = harness().await;
    let providers = h
        .runtime
        .call(json!({"operation":"providers","plugin":"fixture"}))
        .await
        .unwrap();
    assert_eq!(providers.as_array().unwrap().len(), 2);
    assert!(providers[0]["capabilities"]["refresh"].as_bool().unwrap());
    let a=h.store.create_account("fixture","fixture","Alice",json!({"type":"oauth","uid":"alice","access":"stale","refresh":"synthetic","expires":1})).unwrap();
    let b = h
        .store
        .create_account(
            "fixture",
            "fixture-ai",
            "Bob",
            json!({"type":"oauth","uid":"bob","access":"stale","refresh":"synthetic","expires":1}),
        )
        .unwrap();
    let (one, two) = tokio::join!(
        h.runtime.call(json!({"operation":"usage","account":a.id})),
        h.runtime.call(json!({"operation":"usage","account":b.id}))
    );
    assert_eq!(one.unwrap()["user"], "alice");
    assert_eq!(two.unwrap()["user"], "bob");
    let stored = h.store.credential("fixture", &a.id).unwrap();
    assert_eq!(stored["auth"]["access"], "fresh-alice");
    assert_eq!(stored["generation"], 2);
    let (models, usage) = tokio::join!(
        h.runtime.call(json!({"operation":"models","account":a.id})),
        h.runtime.call(json!({"operation":"usage","account":a.id}))
    );
    assert!(
        models
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "account-alice")
    );
    assert_eq!(usage.unwrap()["user"], "alice");
    assert_eq!(
        h.store.credential("fixture", &a.id).unwrap()["generation"],
        2,
        "one refresh for concurrent consumers"
    );
    let response = h
        .runtime
        .inference(
            &a.id,
            "responses",
            json!({"model":"fixture-model","stream":true,"input":"hi"}),
            json!({"session":"session-alice"}),
        )
        .await;
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("alice:session-alice"));
    assert!(text.contains("response.completed"));
    assert!(text.contains("response.function_call_arguments.delta"));
    assert!(text.contains("\"output_tokens\":12"));
    assert!(!text.contains("fresh-alice"));
    assert_eq!(
        h.runtime.active.load(std::sync::atomic::Ordering::Acquire),
        0
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_requires_explicit_commit_and_cancel_cannot_save_late_results() {
    let h = harness().await;
    let begin=h.runtime.call(json!({"operation":"login.begin","plugin":"fixture","provider":"fixture","method":"0","inputs":{"uid":"new"}})).await.unwrap();
    assert!(h.store.accounts().unwrap().is_empty());
    let ready=h.runtime.call(json!({"operation":"login.finish","plugin":"fixture","provider":"fixture","session":begin["session"]})).await.unwrap();
    assert_eq!(ready["ready"], true);
    assert!(h.store.accounts().unwrap().is_empty());
    assert!(ready.get("access").is_none());
    let account=h.runtime.call(json!({"operation":"login.commit","plugin":"fixture","provider":"fixture","session":begin["session"]})).await.unwrap();
    assert_eq!(account["name"], "new");
    assert!(account.get("auth").is_none());
    assert!(h.runtime.call(json!({"operation":"login.commit","plugin":"fixture","provider":"fixture","session":begin["session"]})).await.is_err());
    let begin=h.runtime.call(json!({"operation":"login.begin","plugin":"fixture","provider":"fixture","method":"0","inputs":{"uid":"cancelled","delay":"100"}})).await.unwrap();
    let rt = h.runtime.clone();
    let session = begin["session"].clone();
    let finish = tokio::spawn(async move {
        rt.call(json!({"operation":"login.finish","plugin":"fixture","provider":"fixture","session":session})).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    h.runtime.call(json!({"operation":"login.cancel","plugin":"fixture","provider":"fixture","session":begin["session"]})).await.unwrap();
    assert!(finish.await.unwrap().is_err());
    assert_eq!(h.store.accounts().unwrap().len(), 1);
}
#[test]
fn credential_store_rejects_stale_writes_cross_plugin_access_and_corruption() {
    let f = Fixture::new();
    let store = store::Store::new(f.path.join("plugins"));
    store
        .register(store::Plugin {
            id: "test".into(),
            package: "local".into(),
            version: "1".into(),
            entry: f.path.join("entry"),
            runtime: "node".into(),
            enabled: true,
            revision: 1,
        })
        .unwrap();
    let a = store
        .create_account(
            "test",
            "provider",
            "name",
            json!({"type":"api","key":"synthetic"}),
        )
        .unwrap();
    assert!(store.credential("other", &a.id).is_err());
    store
        .update("test", &a.id, 1, json!({"key":"new"}))
        .unwrap();
    assert!(
        store
            .update("test", &a.id, 1, json!({"key":"old"}))
            .is_err()
    );
    assert!(store.account("../escape").is_err());
    let path = store.root.join("accounts").join(format!("{}.json", a.id));
    fs::write(&path, b"corrupt").unwrap();
    assert!(store.update("test", &a.id, 2, json!({})).is_err());
    assert_eq!(fs::read(path).unwrap(), b"corrupt");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capabilities_register_dynamic_auth_and_disabled_plugins_fail_closed() {
    let h = harness().await;
    let d = h
        .runtime
        .call(json!({"operation":"providers","plugin":"fixture"}))
        .await
        .unwrap();
    let mut registry = crate::auth::builtin_registry();
    auth::register(&mut registry, "fixture", &d).unwrap();
    let a = registry
        .create(
            "plugin:fixture:fixture",
            crate::auth::AuthContext {
                root: h.store.root.clone(),
                account: None,
            },
        )
        .unwrap();
    assert_eq!(a.methods()[0].label, "Synthetic browser");
    let mut plugin = h.store.plugin("fixture").unwrap();
    plugin.enabled = false;
    h.store.register(plugin).unwrap();
    assert!(
        h.runtime
            .call(json!({"operation":"providers","plugin":"fixture"}))
            .await
            .is_err()
    );
    assert!(!h.f.path.join("unused").exists());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_key_methods_without_authorize_and_readonly_catalog_use_generic_bridges() {
    let h = harness().await;
    let ready=h.runtime.call(json!({"operation":"login.begin","plugin":"fixture","provider":"fixture","method":"2","inputs":{"key":"synthetic-api"}})).await.unwrap();
    assert_eq!(ready["ready"], true);
    let a=h.runtime.call(json!({"operation":"login.commit","plugin":"fixture","provider":"fixture","session":ready["session"]})).await.unwrap();
    let id = a["id"].as_str().unwrap();
    assert_eq!(
        h.store.credential("fixture", id).unwrap()["auth"]["key"],
        "synthetic-api"
    );
    let http = std::sync::Arc::new(crate::http::FakeHttp::new());
    let service = crate::provider_catalog::CatalogService::new(
        h.f.path.join("manager"),
        http.clone(),
        std::sync::Arc::new(|_| Err("no credentials".into())),
    )
    .with_plugin_root(h.store.root.clone());
    let mut plan = service.load(&crate::config::AppConfig::default()).unwrap();
    assert_eq!(plan.catalog.providers.len(), 1);
    assert_eq!(
        plan.catalog.providers[0].kind,
        crate::provider_catalog::ProviderKind::Plugin
    );
    assert_eq!(plan.resources[0].binding().account, id);
    assert!(http.calls.lock().unwrap().is_empty());
    let reader = &plan.resources[0];
    let target = reader.binding().cache_key(reader.service());
    assert!(plan.catalog.apply_resource(
        &target,
        crate::resources::ResourceSnapshot {
            binding: reader.binding().clone(),
            service: reader.service().into(),
            scope: crate::resources::ResourceScope::Account,
            state: crate::resources::ResourceState::Ready,
            metrics: vec![crate::resources::ResourceMetric::Allowance {
                remaining: Some("75".into()),
                used: Some("25".into()),
                unit: "credits".into(),
                unlimited: false
            }],
            queried_at: None,
            message: None,
        }
    ));
    assert!(
        matches!(
            plan.catalog.providers[0].usage.sources[0].state,
            crate::provider_catalog::QueryState::Result(_)
        ),
        "a real Dashboard resource event must replace the loading state"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_plugin_can_pass_native_responses_and_reload_without_interrupting_old_streams() {
    let h = harness().await;
    let entry = h.f.path.join("native.mjs");
    fs::write(&entry,r#"export default async()=>({
      config:async c=>{c.provider.native={npm:'@ai-sdk/openai',api:'https://example.invalid/v1',models:{m:{name:'Native'}}};},
      auth:{provider:'native',methods:[{type:'api',label:'Key'}],loader:async getAuth=>({baseURL:'https://example.invalid/v1',apiKey:(await getAuth()).key,fetch:async(url,init)=>{
        const b=JSON.parse(init.body);if(b.input!=='native input'||b.messages)throw Error('unexpected protocol translation');
        return new Response(new ReadableStream({start(c){
          const e=new TextEncoder();c.enqueue(e.encode('data: {"type":"response.output_text.delta","delta":"native"}\n\n'));
          setTimeout(()=>{c.enqueue(e.encode('data: {"type":"response.completed","response":{"model":"m","status":"completed","usage":{"output_tokens":7}}}\n\n'));c.close();},500);
        }}),{headers:{'content-type':'text/event-stream'}});
      }})}
    });"#).unwrap();
    h.store
        .register(store::Plugin {
            id: "native".into(),
            package: "second-plugin".into(),
            version: "1.0.0".into(),
            entry,
            runtime: "node".into(),
            enabled: true,
            revision: 1,
        })
        .unwrap();
    let account = h
        .store
        .create_account(
            "native",
            "native",
            "Native",
            json!({"type":"api","key":"synthetic-native"}),
        )
        .unwrap();
    let response = h
        .runtime
        .inference(
            &account.id,
            "responses",
            json!({"model":"m","input":"native input","stream":true}),
            json!({}),
        )
        .await;
    assert_eq!(response.status(), 200);
    let plugin = h.store.plugin("native").unwrap();
    h.store.register(plugin).unwrap();
    let d = h
        .runtime
        .call(json!({"operation":"providers","plugin":"native"}))
        .await
        .unwrap();
    assert_eq!(d[0]["protocol"], "responses");
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("\"output_tokens\":7"));
    assert_eq!(
        h.runtime
            .call(json!({"operation":"providers","plugin":"fixture"}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_metadata_does_not_hold_up_same_account_inference() {
    let h = harness().await;
    h.runtime
        .call(json!({"operation":"providers","plugin":"fixture"}))
        .await
        .unwrap();
    let account=h.store.create_account("fixture","fixture","Slow",json!({"type":"oauth","uid":"slow","access":"fresh-slow","expires":chrono::Utc::now().timestamp_millis()+3600000})).unwrap();
    let runtime = h.runtime.clone();
    let id = account.id.clone();
    let usage = tokio::spawn(async move {
        runtime
            .call(json!({"operation":"usage","account":id}))
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let response = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        h.runtime.inference(
            &account.id,
            "responses",
            json!({"model":"fixture-model","input":"hi","stream":true}),
            json!({}),
        ),
    )
    .await
    .expect("metadata must not block inference");
    assert_eq!(response.status(), 200);
    assert!(
        !usage.is_finished(),
        "the metadata call is still slow while inference has started"
    );
    axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(usage.await.unwrap().unwrap()["user"], "slow");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_concurrent_same_account_refreshes_once() {
    let h = harness().await;
    let a=h.store.create_account("fixture","fixture","Alice",json!({"type":"oauth","uid":"alice","access":"stale","refresh":"synthetic","expires":1})).unwrap();
    let (one, two) = tokio::join!(
        h.runtime.call(json!({"operation":"usage","account":a.id})),
        h.runtime.call(json!({"operation":"models","account":a.id}))
    );
    one.unwrap();
    two.unwrap();
    let v = h
        .runtime
        .call(json!({"operation":"usage","account":a.id}))
        .await
        .unwrap();
    assert_eq!(
        v["refresh_attempts"], 1,
        "expired concurrent consumers must share the refresh attempt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_plugin_accounts_share_resources_without_duplicate_provider_cards() {
    use crate::provider_catalog::{CatalogService, Models, ProviderKind};
    let h = harness().await;
    let account = h
        .store
        .create_account(
            "fixture",
            "fixture",
            "Alice",
            json!({"type":"api","key":"synthetic"}),
        )
        .unwrap();
    let root = h.f.path.join("manager");
    let mut manager = crate::codex_manager::Manager::open(root.clone()).unwrap();
    let text = crate::codex_manager::config::new_config(
        &account_endpoint(&account.id),
        "fixture-model",
        None,
    )
    .unwrap();
    for name in ["First", "Second"] {
        manager
            .add(name, &text, &h.store.inference_key(&account.id).unwrap())
            .unwrap();
    }
    let http = std::sync::Arc::new(crate::http::FakeHttp::new());
    let service = CatalogService::new(
        root,
        http.clone(),
        std::sync::Arc::new(|_| Err("unused".into())),
    )
    .with_plugin_root(h.store.root.clone());
    let config = crate::config::AppConfig::default();
    let plan = service.load(&config).unwrap();
    assert_eq!(plan.catalog.providers.len(), 2);
    assert_eq!(
        plan.queries.len(),
        2,
        "one model query per configured provider"
    );
    assert_eq!(
        plan.resources.len(),
        1,
        "both profiles share the same account resource"
    );
    assert!(
        plan.catalog
            .providers
            .iter()
            .all(|p| p.kind == ProviderKind::Plugin && p.usage.sources.len() == 1)
    );
    let mut plugin = h.store.plugin("fixture").unwrap();
    plugin.enabled = false;
    h.store.register(plugin).unwrap();
    let disabled = service.load(&config).unwrap();
    assert_eq!(disabled.catalog.providers.len(), 2);
    assert!(
        disabled
            .catalog
            .providers
            .iter()
            .all(|p| matches!(p.models, Models::Unavailable(_)))
    );
    assert!(disabled.queries.is_empty());
    assert!(disabled.resources.is_empty());
    assert!(
        http.calls.lock().unwrap().is_empty(),
        "reserved endpoints must never become external HTTP requests"
    );
}
