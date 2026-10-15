// Injected into the isolated copy as an integration test; synthetic HTTP and credentials only.
use std::sync::Arc;
use usage::http::{FakeHttp, HttpResponse};
use usage::providers::{ProviderContext, UsageProvider, builtin_factory};

#[tokio::test]
async fn phase1_probe_builtin_accounts_do_not_share_cached_results() {
    let root = std::env::temp_dir().join(format!("usage-ablation-cache-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let now = chrono::Utc::now();
    let factory = builtin_factory();
    let mut readers: Vec<Arc<dyn UsageProvider>> = Vec::new();
    let mut clients = Vec::new();
    for (account, used) in [("a", 20), ("b", 80)] {
        let home = root.join(account);
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join("auth.json"), serde_json::json!({
            "auth_mode":"chatgpt", "tokens":{"access_token":"synthetic-token", "account_id":account}
        }).to_string()).unwrap();
        let http = Arc::new(FakeHttp::new());
        http.route(
            "https://chatgpt.com/backend-api/wham/usage",
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: serde_json::to_vec(&serde_json::json!({"rate_limit":{"primary_window":{
                    "used_percent":used,"limit_window_seconds":18000
                }}}))
                .unwrap(),
            }),
        );
        let context = Arc::new(ProviderContext {
            claude_dir: root.join("missing-claude"),
            codex_dir: home,
            grok_dir: root.join("missing-grok"),
            http: http.clone(),
            keychain: Arc::new(|_, _| None),
            now: Arc::new(move || now),
        });
        readers.push(Arc::from(factory.create("codex", context).unwrap()));
        clients.push(http);
    }
    let path = root.join("cache.json");
    let first =
        usage::statusline::collect(&readers[..1], &Default::default(), &path, 60, now).await;
    assert_eq!(first[0].tiers[0].utilization, 20.0);
    let second =
        usage::statusline::collect(&readers[1..], &Default::default(), &path, 60, now).await;
    assert_eq!(
        second[0].tiers[0].utilization, 80.0,
        "account B reused account A's cached quota"
    );
    assert_eq!(clients[1].calls.lock().unwrap().len(), 1);
    std::fs::remove_dir_all(root).unwrap();
}
