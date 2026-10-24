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
    app.overview = Some(Catalog {
        providers: vec![
            provider("account-a", ProviderKind::Subscription),
            provider("account-b", ProviderKind::Subscription),
            provider("workbuddy", ProviderKind::SubscriptionService),
        ],
        ..Default::default()
    });
    app.home_provider = Some("account-a".into());
    app.fingerprint_generation = 2;
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
