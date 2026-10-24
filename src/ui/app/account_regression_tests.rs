use super::*;
use async_trait::async_trait;

struct Account(&'static str);
#[async_trait]
impl UsageProvider for Account {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn display_name(&self) -> &'static str {
        "Codex"
    }
    fn cache_key(&self) -> String {
        self.0.into()
    }
    async fn query(&self) -> UsageSnapshot {
        crate::model::snapshot_ok(
            "codex",
            self.0,
            vec![crate::model::QuotaTier {
                name: "five_hour".into(),
                utilization: 95.0,
                resets_at: None,
            }],
            None,
            Utc::now(),
        )
    }
}

#[tokio::test]
async fn two_accounts_receive_their_own_card_results() {
    let mut app = Dashboard::new(
        vec![Arc::new(Account("a")), Arc::new(Account("b"))],
        isolated_config(),
        "/nonexistent/usage-ablation.toml".into(),
    );
    app.service_http = Arc::new(crate::http::FakeHttp::new());
    app.history_config.alerts.enabled = true;
    app.refresh(&Handle::current());
    tokio::time::timeout(Duration::from_secs(1), async {
        while app.pending > 0 {
            app.drain_events();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(app.alerts_queue.len(), 2);
    let names: Vec<_> = app
        .cards
        .iter()
        .map(|card| card.snapshot.as_ref().map(|s| s.display_name.as_str()))
        .collect();
    assert_eq!(
        names,
        [Some("a"), Some("b")],
        "refresh events still select the first card by tool type"
    );
}
