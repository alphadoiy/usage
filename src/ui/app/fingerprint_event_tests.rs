use super::*;
use crate::codex_manager::fingerprint::Snapshot;
use crate::provider_catalog::{Catalog, Models, Provider, ProviderKind};

fn provider(id: &str, kind: ProviderKind) -> Provider {
    Provider {
        operations: Vec::new(),
        id: id.into(),
        name: "Same display name".into(),
        agent: Some("Codex".into()),
        kind,
        configured_model: None,
        models: Models::Ready(Vec::new()),
        usage: Default::default(),
        performance: None,
    }
}
fn text(app: &Dashboard) -> String {
    home_lines(app, 100)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn fingerprint_events_and_provider_switches_keep_account_identity() {
    let mut app = Dashboard::new(
        Vec::new(),
        isolated_config(),
        PathBuf::from("/tmp/config.toml"),
    );
    app.overview_state.catalog = Some(Catalog {
        providers: vec![
            provider("account-a", ProviderKind::Subscription),
            provider("account-b", ProviderKind::Subscription),
            provider("workbuddy", ProviderKind::SubscriptionService),
        ],
        ..Default::default()
    });
    app.home_provider = Some("account-a".into());
    app.fingerprint.generation = 2;
    app.tx
        .send(AppEvent::FingerprintLoaded {
            generation: 1,
            result: Err("stale".into()),
        })
        .unwrap();
    assert!(!app.drain_events().0);
    app.tx
        .send(AppEvent::FingerprintLoaded {
            generation: 2,
            result: Ok(vec![
                Snapshot {
                    id: "account-a".into(),
                    warnings: vec!["A-only warning".into()],
                    ..Default::default()
                },
                Snapshot {
                    id: "account-b".into(),
                    warnings: vec!["B-only warning".into()],
                    ..Default::default()
                },
            ]),
        })
        .unwrap();
    assert!(app.drain_events().0);
    assert!(text(&app).contains("订阅测试"));
    app.handle_home_key(KeyCode::Char('t'), &Handle::current(), (100, 32));
    assert!(text(&app).contains("A-only warning"));
    assert!(!text(&app).contains("B-only warning"));
    app.handle_home_key(KeyCode::Right, &Handle::current(), (100, 32));
    assert!(!app.home_subscription_details);
    app.handle_home_key(KeyCode::Char('t'), &Handle::current(), (100, 32));
    assert!(text(&app).contains("B-only warning"));
    assert!(!text(&app).contains("A-only warning"));
    app.handle_home_key(KeyCode::Esc, &Handle::current(), (100, 32));
    assert!(!app.home_subscription_details);
    app.handle_home_key(KeyCode::Right, &Handle::current(), (100, 32));
    assert!(!text(&app).contains("订阅测试"));
    app.handle_home_key(KeyCode::Char('t'), &Handle::current(), (100, 32));
    assert!(!app.home_subscription_details);
}

#[cfg(unix)]
#[tokio::test]
async fn tui_schedule_switch_persists_only_the_selected_account_and_survives_navigation() {
    use crate::codex_manager::{Manager, fingerprint::FingerprintService};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let root = std::env::temp_dir().join(format!("usage-tui-fingerprint-{}", uuid::Uuid::new_v4()));
    let mut manager = Manager::open(root.join("manager")).unwrap();
    let mut providers = Vec::new();
    for workspace in ["synthetic-a", "synthetic-b"] {
        let claims = serde_json::json!({"sub":workspace,"email":"synthetic@example.test","https://api.openai.com/auth":{"chatgpt_account_id":workspace,"chatgpt_plan_type":"plus"}});
        let jwt = format!("e30.{}.fixture", URL_SAFE_NO_PAD.encode(claims.to_string()));
        let auth = root.join(format!("{workspace}.json"));
        std::fs::write(&auth, serde_json::json!({"auth_mode":"chatgpt","tokens":{"id_token":jwt,"access_token":jwt,"refresh_token":"synthetic","account_id":workspace},"last_refresh":"2026-10-11T00:00:00Z"}).to_string()).unwrap();
        let account = manager
            .import_oauth(Some("Same display name"), None, &auth, None)
            .unwrap();
        providers.push(provider(&account.id, ProviderKind::Subscription));
    }
    let engine = manager.root().join("fingerprint/tools/node_modules/lmfpd");
    std::fs::create_dir_all(engine.join("bin")).unwrap();
    std::fs::create_dir_all(engine.join("data")).unwrap();
    std::fs::write(
        engine.join("package.json"),
        r#"{"name":"lmfpd","version":"0.0.1"}"#,
    )
    .unwrap();
    std::fs::write(
        engine.join("data/unified_bank.json"),
        r#"{"models":[{"id":"gpt-6.1-sol"}]}"#,
    )
    .unwrap();
    let script = engine.join("bin/fpd");
    std::fs::write(&script, "#!/bin/sh\ntouch \"$0.started\"\nexit 1\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let bin = engine.parent().unwrap().join(".bin");
    std::fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(&script, bin.join("fpd")).unwrap();
    let service = Arc::new(FingerprintService::new(manager.root().to_owned()));
    let a = providers[0].id.clone();
    let b = providers[1].id.clone();
    // Read-only discovery includes installed engines even before a test is configured.
    let snapshots = service.snapshots().unwrap();
    assert_eq!(snapshots.len(), 2);
    assert!(snapshots.iter().all(|s| s.engine_available && !s.enabled));
    assert!(!manager.root().join("fingerprint/accounts").exists());
    let mut app = Dashboard::new(Vec::new(), isolated_config(), root.join("usage.toml"));
    app.fingerprint.service = service.clone();
    app.overview_state.catalog = Some(Catalog {
        providers,
        ..Default::default()
    });
    app.home_provider = Some(a.clone());
    app.subscription_tests.set(Ok(snapshots));
    let handle = Handle::current();
    app.handle_home_key(KeyCode::Char('e'), &handle, (100, 32));
    assert!(
        !app.subscription_tests.pending(),
        "the switch belongs to the detail page"
    );
    app.handle_home_key(KeyCode::Char('t'), &handle, (100, 32));
    assert!(text(&app).contains("测试引擎：已安装 · LMFPD 0.0.1"));
    app.handle_home_key(KeyCode::Char('e'), &handle, (100, 32));
    assert!(app.subscription_tests.pending());
    let generation = app.fingerprint.generation;
    app.handle_home_key(KeyCode::Char('e'), &handle, (100, 32));
    app.maybe_refresh_fingerprint(&handle);
    assert_eq!(app.fingerprint.generation, generation);
    app.handle_home_key(KeyCode::Right, &handle, (100, 32));
    app.handle_home_key(KeyCode::Char('t'), &handle, (100, 32));
    assert_eq!(app.home_provider.as_deref(), Some(b.as_str()));
    app.tx
        .send(AppEvent::FingerprintLoaded {
            generation: generation.wrapping_sub(1),
            result: Ok(Vec::new()),
        })
        .unwrap();
    async fn settle(app: &mut Dashboard) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while app.subscription_tests.pending() {
                app.drain_events();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    settle(&mut app).await;
    assert!(service.status(&a).unwrap().enabled);
    assert!(!service.status(&b).unwrap().enabled);
    assert!(text(&app).contains("[ ] 已关闭 · e 启用"));
    app.handle_home_key(KeyCode::Left, &handle, (100, 32));
    app.handle_home_key(KeyCode::Char('t'), &handle, (100, 32));
    assert!(text(&app).contains("[x] 已启用 · e 关闭"));
    // The action is also discoverable through the same shortcut menu dispatcher.
    assert!(app.handle_shortcuts_key(KeyCode::Char('?')).is_some());
    assert!(matches!(
        app.handle_shortcuts_key(KeyCode::Enter),
        Some(crate::ui::shortcuts::Decision::Run(KeyCode::Char('e')))
    ));
    app.handle_home_key(KeyCode::Char('e'), &handle, (100, 32));
    app.handle_home_key(KeyCode::Esc, &handle, (100, 32));
    settle(&mut app).await;
    assert!(!service.status(&a).unwrap().enabled);
    assert!(!script.with_extension("started").exists());
    assert!(
        !manager
            .root()
            .join(format!("fingerprint/accounts/{a}/runs"))
            .exists()
    );
    drop(app);
    drop(manager);
    std::fs::remove_dir_all(root).unwrap();
}
