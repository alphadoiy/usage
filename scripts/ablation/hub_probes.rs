// Appended as a child module of hub in an isolated copy only.
#[cfg(test)]
mod phase1_audit_probes {
    use super::*;
    use crate::resources::{ResourceBinding, ResourceState};
    use async_trait::async_trait;

    struct Reader {
        binding: ResourceBinding,
        panic: bool,
    }
    #[async_trait]
    impl ResourceReader for Reader {
        fn service(&self) -> &str {
            "probe"
        }
        fn binding(&self) -> &ResourceBinding {
            &self.binding
        }
        async fn read(&self) -> ResourceSnapshot {
            assert!(!self.panic, "synthetic adapter panic");
            ResourceSnapshot {
                binding: self.binding.clone(),
                service: "probe".into(),
                state: ResourceState::Ready,
                metrics: Vec::new(),
                queried_at: None,
                message: None,
            }
        }
    }

    #[tokio::test]
    async fn phase1_probe_one_panicking_resource_preserves_healthy_result() {
        let agent = crate::agents::builtin_registry()
            .create(
                "codex",
                AgentContext::Local {
                    home: "/nonexistent".into(),
                },
            )
            .unwrap();
        let provider = Provider {
            id: "probe".into(),
            agent,
            resources: [false, true]
                .into_iter()
                .map(|panic| {
                    Arc::new(Reader {
                        binding: ResourceBinding {
                            account: if panic { "broken" } else { "healthy" }.into(),
                            revision: "1".into(),
                        },
                        panic,
                    }) as Arc<dyn ResourceReader>
                })
                .collect(),
        };
        let snapshots = provider
            .resources()
            .await
            .expect("one resource task must not discard its healthy sibling");
        assert!(
            snapshots
                .iter()
                .any(|s| s.binding.account == "healthy" && s.state == ResourceState::Ready)
        );
    }

    #[test]
    fn phase1_probe_independent_accounts_receive_independent_quota_alerts() {
        let mut monitor = crate::alerts::QuotaMonitor::default();
        let config = crate::config::AlertConfig {
            enabled: true,
            ..Default::default()
        };
        let now = chrono::Utc::now();
        for (offset, account) in [(0, "account-a"), (1, "account-b")] {
            let observed = now + chrono::Duration::seconds(offset);
            let snapshot = crate::model::snapshot_ok(
                "codex",
                account,
                vec![crate::model::QuotaTier {
                    name: "five_hour".into(),
                    utilization: 95.0,
                    resets_at: None,
                }],
                None,
                observed,
            );
            let alerts = monitor.observe(&snapshot, &config, observed);
            assert_eq!(
                alerts.len(),
                1,
                "independent account {account} shared another account's alert state"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn phase1_probe_session_resume_uses_registered_launcher() {
        use crate::agents::{AgentKind, Launcher};
        use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CustomAgent {
            kind: AgentKind,
            calls: Arc<AtomicUsize>,
        }
        impl AgentAdapter for CustomAgent {
            fn kind(&self) -> AgentKind {
                self.kind
            }
            fn launcher(&self) -> Option<&dyn Launcher> {
                Some(self)
            }
            fn session_source(&self) -> Option<crate::sessions::SessionSource> {
                Some(crate::sessions::SessionSource {
                    kind: self.kind,
                    load: |home| {
                        Ok(vec![crate::sessions::SessionRecord {
                            id: "123e4567-e89b-12d3-a456-426614174001".into(),
                            cwd: home.to_path_buf(),
                            title: "synthetic".into(),
                            recency_at_ms: 1,
                        }])
                    },
                })
            }
        }
        impl Launcher for CustomAgent {
            fn spawn(&self, _: &SpawnRequest) -> Result<SpawnOutcome, String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(SpawnOutcome {
                    status: std::process::ExitStatus::from_raw(0),
                    run: None,
                })
            }
        }
        let home =
            std::env::temp_dir().join(format!("usage-ablation-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&home).unwrap();
        let command = home.join("synthetic-agent");
        std::fs::write(&command, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let command: &'static str =
            Box::leak(format!("{} resume", command.display()).into_boxed_str());
        let kind = AgentKind::new("probe-agent", "Probe", None, command, "PROBE_AGENT_HOME", 0);
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let mut agents = AgentRegistry::new();
        agents
            .register("probe-agent", move |_| {
                Ok(Arc::new(CustomAgent {
                    kind,
                    calls: counter.clone(),
                }) as Arc<dyn AgentAdapter>)
            })
            .unwrap();
        let catalog = SessionRegistry::from_agents(Arc::new(agents))
            .load_all(&SessionHomes::new().with("probe-agent", &home));
        assert_eq!(catalog.sessions.len(), 1);
        assert!(
            crate::sessions::resume_in_current(&catalog.sessions[0])
                .unwrap()
                .success()
        );
        std::fs::remove_dir_all(home).unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "resume bypassed the registered agent launcher"
        );
    }
}
