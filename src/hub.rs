//! Application façade: configured providers compose an agent and resource services.

use std::sync::Arc;

use crate::agents::{AgentAdapter, AgentContext, AgentRegistry, SpawnOutcome, SpawnRequest};
use crate::registry::RegistryError;
use crate::resources::{ResourceContext, ResourceReader, ResourceRegistry, ResourceSnapshot};
use crate::sessions::{SessionCatalog, SessionHomes, SessionRegistry};

pub struct ResourceSpec {
    pub service: String,
    pub context: ResourceContext,
}

pub struct ProviderDefinition {
    /// Configured provider identity, separate from agent/service type IDs.
    pub id: String,
    pub agent: String,
    pub context: AgentContext,
    pub resources: Vec<ResourceSpec>,
}

pub struct Hub {
    pub agents: AgentRegistry,
    pub resources: ResourceRegistry,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            agents: crate::agents::builtin_registry(),
            resources: crate::resources::builtin_registry(),
        }
    }
}

impl Hub {
    /// Factories construct adapters only; no process, network or store is opened here.
    pub fn provider(&self, definition: ProviderDefinition) -> Result<Provider, RegistryError> {
        if definition.id.trim().is_empty() {
            return Err(RegistryError::EmptyId);
        }
        let agent = self
            .agents
            .create(&definition.agent, definition.context.clone())?;
        if agent.kind().id() != definition.agent {
            return Err(RegistryError::Factory("Agent 工厂返回的类型不匹配".into()));
        }
        let mut resources: Vec<Arc<dyn ResourceReader>> = Vec::new();
        for spec in definition.resources {
            if spec.context.binding.account.trim().is_empty() {
                return Err(RegistryError::EmptyId);
            }
            let reader = self.resources.create(&spec.service, spec.context.clone())?;
            if reader.service() != spec.service || reader.binding() != &spec.context.binding {
                return Err(RegistryError::Factory(
                    "资源工厂返回的类型或账号绑定不匹配".into(),
                ));
            }
            if resources.iter().any(|existing| {
                existing.service() == reader.service() && existing.binding() == reader.binding()
            }) {
                return Err(RegistryError::Duplicate(
                    spec.context.binding.cache_key(&spec.service),
                ));
            }
            resources.push(reader);
        }
        Ok(Provider {
            id: definition.id,
            context: definition.context,
            agent,
            resources,
        })
    }
}

pub struct Provider {
    id: String,
    context: AgentContext,
    agent: Arc<dyn AgentAdapter>,
    resources: Vec<Arc<dyn ResourceReader>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub spawn: bool,
    pub sessions: bool,
    pub resources: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityError {
    Unsupported(&'static str),
    Failed(String),
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(capability) => write!(f, "此提供方不支持 {capability}"),
            Self::Failed(message) => message.fmt(f),
        }
    }
}

impl std::error::Error for CapabilityError {}

impl Provider {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            spawn: self.agent.launcher().is_some(),
            sessions: self.agent.session_source().is_some(),
            resources: !self.resources.is_empty(),
        }
    }

    /// Each call produces a separate launch; the adapter is reusable, not a process singleton.
    pub fn spawn(&self, request: &SpawnRequest) -> Result<SpawnOutcome, CapabilityError> {
        self.agent
            .launcher()
            .ok_or(CapabilityError::Unsupported("spawn"))?
            .spawn(request)
            .map_err(CapabilityError::Failed)
    }

    /// Query services concurrently, preserve configured order, and bound arbitrary adapters.
    pub async fn resources(&self) -> Result<Vec<ResourceSnapshot>, CapabilityError> {
        self.resources_with_deadline(crate::providers::QUERY_TIMEOUT)
            .await
    }

    async fn resources_with_deadline(
        &self,
        timeout: std::time::Duration,
    ) -> Result<Vec<ResourceSnapshot>, CapabilityError> {
        if self.resources.is_empty() {
            return Err(CapabilityError::Unsupported("resources"));
        }
        let mut tasks = tokio::task::JoinSet::new();
        let mut indices = std::collections::HashMap::new();
        for (index, reader) in self.resources.iter().enumerate() {
            let reader = Arc::clone(reader);
            let task =
                tasks.spawn(async move { tokio::time::timeout(timeout, reader.read()).await });
            indices.insert(task.id(), index);
        }
        let mut snapshots = Vec::with_capacity(self.resources.len());
        while let Some(result) = tasks.join_next_with_id().await {
            let (id, result) = match result {
                Ok((id, snapshot)) => (id, snapshot.map_err(|_| "资源查询超时")),
                Err(error) => (error.id(), Err("资源查询任务中断")),
            };
            let index = indices[&id];
            let snapshot = result.unwrap_or_else(|message| {
                let reader = &self.resources[index];
                tracing::warn!(event = "resource_query_failed", service = reader.service(),
                    target = %reader.binding().cache_key(reader.service()), task = %id, reason = message);
                ResourceSnapshot {
                    binding: reader.binding().clone(),
                    service: reader.service().into(),
                    state: crate::resources::ResourceState::TransientFailure,
                    metrics: Vec::new(),
                    queried_at: None,
                    message: Some(message.into()),
                }
            });
            snapshots.push((index, snapshot));
        }
        snapshots.sort_by_key(|(index, _)| *index);
        Ok(snapshots
            .into_iter()
            .map(|(_, snapshot)| snapshot)
            .collect())
    }

    /// A home catalog is not proof that every conversation belongs to a particular run.
    pub fn sessions(&self) -> Result<SessionCatalog, CapabilityError> {
        let source = self
            .agent
            .session_source()
            .ok_or(CapabilityError::Unsupported("sessions"))?;
        let home = match &self.context {
            AgentContext::Local { home } => home.clone(),
            AgentContext::ManagedCodex {
                root,
                account,
                revision,
            } => {
                let manager = crate::codex_manager::Manager::open(root.clone())
                    .map_err(|error| CapabilityError::Failed(error.to_string()))?;
                let profile = manager
                    .resolve(account)
                    .map_err(|error| CapabilityError::Failed(error.to_string()))?;
                manager
                    .home(&profile.id, revision.unwrap_or(profile.revision))
                    .map_err(|error| CapabilityError::Failed(error.to_string()))?
            }
        };
        let homes = SessionHomes::new().with_instance(&self.id, self.agent.kind().id(), home);
        let mut registry = SessionRegistry::new();
        registry.register(source).map_err(CapabilityError::Failed)?;
        let mut catalog = registry.load_all(&homes);
        for session in &mut catalog.sessions {
            session.agent = Some(self.agent.clone());
        }
        Ok(catalog)
    }
}

/// Common application entry point for account launches and managed session resumes.
pub fn spawn_managed_codex(
    root: &std::path::Path,
    account: &str,
    revision: Option<i64>,
    request: &SpawnRequest,
) -> Result<SpawnOutcome, String> {
    let provider = Hub::default()
        .provider(ProviderDefinition {
            id: account.into(),
            agent: "codex".into(),
            context: AgentContext::ManagedCodex {
                root: root.to_path_buf(),
                account: account.into(),
                revision,
            },
            resources: Vec::new(),
        })
        .map_err(|error| error.to_string())?;
    provider.spawn(request).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentKind;
    use crate::resources::{
        ApiConnection, Money, ResourceBinding, ResourceConnection, ResourceMetric, ResourceState,
    };
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CatalogAgent;
    impl AgentAdapter for CatalogAgent {
        fn kind(&self) -> AgentKind {
            AgentKind::new(
                "custom",
                "Custom",
                None,
                "custom --resume",
                "CUSTOM_HOME",
                0,
            )
            .with_session_id_validator(|id| id == "conversation-1")
        }
        fn session_source(&self) -> Option<crate::sessions::SessionSource> {
            Some(crate::sessions::SessionSource {
                kind: self.kind(),
                load: |home| {
                    Ok(vec![crate::sessions::SessionRecord {
                        id: "conversation-1".into(),
                        cwd: home.to_path_buf(),
                        title: "Custom session".into(),
                        recency_at_ms: 1,
                    }])
                },
            })
        }
    }

    struct BalanceReader {
        binding: ResourceBinding,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ResourceReader for BalanceReader {
        fn service(&self) -> &str {
            "balance_api"
        }
        fn binding(&self) -> &ResourceBinding {
            &self.binding
        }
        async fn read(&self) -> ResourceSnapshot {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ResourceSnapshot {
                binding: self.binding.clone(),
                service: self.service().into(),
                state: ResourceState::Ready,
                metrics: vec![ResourceMetric::Balance {
                    remaining: Money {
                        amount: "123456789.12345678".into(),
                        currency: "CNY".into(),
                    },
                }],
                queried_at: None,
                message: None,
            }
        }
    }

    fn context(account: &str) -> ResourceContext {
        ResourceContext {
            binding: ResourceBinding {
                account: account.into(),
                revision: "r1".into(),
            },
            connection: ResourceConnection::Api(ApiConnection {
                base_url: "https://example.invalid".into(),
                credential_ref: "fixture-ref".into(),
                http: Arc::new(crate::http::FakeHttp::new()),
                credentials: Arc::new(|_| panic!("factory must not read secrets")),
                now: Arc::new(chrono::Utc::now),
            }),
        }
    }

    #[tokio::test]
    async fn custom_agent_and_balance_service_compose_without_changing_builtin_dispatch() {
        let mut hub = Hub {
            agents: AgentRegistry::new(),
            resources: ResourceRegistry::new(),
        };
        hub.agents
            .register("custom", |_| {
                Ok(Arc::new(CatalogAgent) as Arc<dyn AgentAdapter>)
            })
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        hub.resources
            .register("balance_api", move |context| {
                Ok(Arc::new(BalanceReader {
                    binding: context.binding,
                    calls: counter.clone(),
                }) as Arc<dyn ResourceReader>)
            })
            .unwrap();
        let home = std::env::temp_dir().join(format!("usage-hub-session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&home).unwrap();
        let provider = hub
            .provider(ProviderDefinition {
                id: "work".into(),
                agent: "custom".into(),
                context: AgentContext::Local { home: home.clone() },
                resources: vec![
                    ResourceSpec {
                        service: "balance_api".into(),
                        context: context("a"),
                    },
                    ResourceSpec {
                        service: "balance_api".into(),
                        context: context("b"),
                    },
                ],
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            provider.capabilities(),
            Capabilities {
                spawn: false,
                sessions: true,
                resources: true
            }
        );
        let request = SpawnRequest {
            cwd: PathBuf::new(),
            program: "unavailable".into(),
            args: Vec::new(),
            resume: None,
        };
        assert!(matches!(
            provider.spawn(&request),
            Err(CapabilityError::Unsupported("spawn"))
        ));
        let snapshots = provider.resources().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(snapshots[0].binding.account, "a");
        assert_eq!(snapshots[1].binding.account, "b");
        assert_eq!(
            serde_json::to_value(&snapshots[0]).unwrap()["metrics"][0]["remaining"]["amount"],
            "123456789.12345678"
        );
        let homes = SessionHomes::new().with("custom", &home);
        let sessions = provider.sessions().unwrap();
        assert_eq!(sessions.sessions[0].id, "conversation-1");
        assert_eq!(sessions.sessions[0].instance, "work");
        assert!(sessions.sessions[0].agent.is_some());
        let sessions = SessionRegistry::from_agents(Arc::new(hub.agents)).load_all(&homes);
        assert_eq!(sessions.sessions.len(), 1);
        std::fs::remove_dir(home).unwrap();
    }

    #[test]
    fn resource_identity_separates_service_account_and_revision() {
        let binding = ResourceBinding {
            account: "a".into(),
            revision: "1".into(),
        };
        let mut other = binding.clone();
        other.revision = "2".into();
        assert_ne!(binding.cache_key("new_api"), other.cache_key("new_api"));
        other = binding.clone();
        other.account = "b".into();
        assert_ne!(binding.cache_key("new_api"), other.cache_key("new_api"));
        assert_ne!(binding.cache_key("new_api"), binding.cache_key("deepseek"));
    }

    #[tokio::test]
    async fn absent_resource_capability_is_explicit() {
        let provider = Hub::default()
            .provider(ProviderDefinition {
                id: "local".into(),
                agent: "codex".into(),
                context: AgentContext::Local {
                    home: PathBuf::new(),
                },
                resources: Vec::new(),
            })
            .unwrap();
        assert!(matches!(
            provider.resources().await,
            Err(CapabilityError::Unsupported("resources"))
        ));
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use crate::resources::{ResourceBinding, ResourceState};
    use async_trait::async_trait;

    struct Reader {
        binding: ResourceBinding,
        panic: bool,
        pending: bool,
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
            if self.pending {
                return std::future::pending().await;
            }
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
    async fn failed_or_stalled_resource_preserves_healthy_result() {
        let agent = crate::agents::builtin_registry()
            .create(
                "codex",
                AgentContext::Local {
                    home: "/nonexistent".into(),
                },
            )
            .unwrap();
        let mut provider = Provider {
            id: "probe".into(),
            context: AgentContext::Local {
                home: "/nonexistent".into(),
            },
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
                        pending: false,
                    }) as Arc<dyn ResourceReader>
                })
                .collect(),
        };
        let snapshots = provider
            .resources()
            .await
            .expect("one resource task must not discard its healthy sibling");
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].binding.account, "healthy");
        assert_eq!(snapshots[1].binding.account, "broken");
        assert_eq!(snapshots[1].state, ResourceState::TransientFailure);
        assert_eq!(snapshots[1].message.as_deref(), Some("资源查询任务中断"));
        assert!(
            snapshots
                .iter()
                .any(|s| s.binding.account == "healthy" && s.state == ResourceState::Ready)
        );
        provider.resources[1] = Arc::new(Reader {
            binding: ResourceBinding {
                account: "pending".into(),
                revision: "1".into(),
            },
            panic: false,
            pending: true,
        });
        let snapshots = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            provider.resources_with_deadline(std::time::Duration::from_millis(20)),
        )
        .await
        .expect("Hub must bound stalled adapters")
        .unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].state, ResourceState::Ready);
        assert_eq!(snapshots[1].binding.account, "pending");
        assert_eq!(snapshots[1].state, ResourceState::TransientFailure);
        assert_eq!(snapshots[1].message.as_deref(), Some("资源查询超时"));
    }

    #[test]
    fn independent_accounts_receive_independent_quota_alerts() {
        let mut monitor = crate::alerts::QuotaMonitor::default();
        let config = crate::config::AlertConfig {
            enabled: true,
            ..Default::default()
        };
        let now = chrono::Utc::now();
        for (offset, account) in [
            (0, "account-a"),
            (1, "account-b"),
            (2, "account-c"),
            (3, "account-d"),
        ] {
            let observed = now + chrono::Duration::seconds(offset);
            let mut snapshot = crate::model::snapshot_ok(
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
            // Neither an absent email nor a shared email identifies a configured connection.
            if offset > 1 {
                snapshot.account_email = Some("same@example.invalid".into());
            }
            let alerts = monitor.observe(account, &snapshot, &config, observed);
            assert_eq!(
                alerts.len(),
                1,
                "independent account {account} shared another account's alert state"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn session_resume_uses_registered_launcher() {
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
            crate::sessions::resume_shell_command(&catalog.sessions[0]).is_err(),
            "shell export must not bypass an adapter that only supports direct launch"
        );
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
