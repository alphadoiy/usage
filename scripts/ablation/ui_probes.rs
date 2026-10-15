// Appended as a child module of ui::app in an isolated copy only.
#[cfg(test)]
mod phase1_audit_probes {
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
            crate::model::snapshot_ok("codex", self.0, Vec::new(), None, Utc::now())
        }
    }

    #[test]
    fn phase1_probe_two_accounts_receive_their_own_card_results() {
        let mut app = Dashboard::new(
            vec![Arc::new(Account("a")), Arc::new(Account("b"))],
            AppConfig::default(),
            "/nonexistent/usage-ablation.toml".into(),
        );
        app.pending = 2;
        for account in ["a", "b"] {
            app.tx
                .send(AppEvent::Quota(Box::new(RefreshEvent {
                    generation: app.generation,
                    tool: "codex".into(),
                    snapshot: crate::model::snapshot_ok(
                        "codex",
                        account,
                        Vec::new(),
                        None,
                        Utc::now(),
                    ),
                })))
                .unwrap();
        }
        app.drain_events();
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
}
