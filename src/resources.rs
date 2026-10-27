//! Billing/resource adapters are independent of the agent that consumes them.
//! Legacy subscription readers remain behind a compatibility capability.

mod api;
pub mod connections;
mod new_api;
mod openai_costs;
mod provider_usage;
pub use new_api::QuotaConversion;
pub(crate) use provider_usage::balance_endpoint;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::Instrument;

use crate::model::{CredentialStatus, UsageSnapshot};
use crate::providers::{ProviderContext, UsageProvider, UsageReset};
use crate::registry::FactoryRegistry;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ResourceBinding {
    /// Configured account/connection identity, never a credential value.
    pub account: String,
    /// Saved configuration revision; empty for an unmanaged local home.
    pub revision: String,
}

impl ResourceBinding {
    pub fn cache_key(&self, service: &str) -> String {
        let mut digest = Sha256::new();
        for part in [service, &self.account, &self.revision] {
            digest.update((part.len() as u64).to_le_bytes());
            digest.update(part.as_bytes());
        }
        format!("resource-v1:{:x}", digest.finalize())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceState {
    Ready,
    NeedsConfiguration,
    AuthenticationFailed,
    TransientFailure,
    Failure,
}

/// Monetary values retain the service's decimal representation and currency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Money {
    pub amount: String,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResourceMetric {
    ResetCredits {
        available: u32,
    },
    QuotaWindow {
        name: String,
        used_percent: f64,
        resets_at: Option<DateTime<Utc>>,
    },
    AllowanceWindow {
        name: String,
        used: String,
        limit: String,
        remaining: String,
        unit: String,
        resets_at: Option<DateTime<Utc>>,
    },
    Balance {
        remaining: Money,
    },
    Cost {
        used: Money,
        period_start: DateTime<Utc>,
        period_end: DateTime<Utc>,
    },
    Spending {
        used: Option<f64>,
        limit: Option<f64>,
        currency: Option<String>,
    },
    /// Raw vendor quota stays raw until the service's conversion is known.
    Allowance {
        remaining: Option<String>,
        used: Option<String>,
        unit: String,
        unlimited: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResourceScope {
    Unspecified,
    Account,
    Token,
    Organization,
    Project { id: String },
    ApiKey { id: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceSnapshot {
    pub binding: ResourceBinding,
    pub service: String,
    pub scope: ResourceScope,
    pub state: ResourceState,
    pub metrics: Vec<ResourceMetric>,
    pub queried_at: Option<DateTime<Utc>>,
    pub message: Option<String>,
}

#[async_trait]
pub trait ResourceReader: Send + Sync {
    fn service(&self) -> &str;
    fn binding(&self) -> &ResourceBinding;
    fn scope(&self) -> ResourceScope {
        ResourceScope::Unspecified
    }
    /// The calling query boundary (Hub) owns the deadline, including credential loading.
    async fn read(&self) -> ResourceSnapshot;

    /// Transitional capability for the current quota-only CLI and views.
    fn legacy_usage(&self) -> Option<&dyn UsageProvider> {
        None
    }
}

#[derive(Clone)]
pub struct ResourceContext {
    pub binding: ResourceBinding,
    pub connection: ResourceConnection,
}

/// Secrets are resolved during query, never during registration or construction.
pub type CredentialResolver = dyn Fn(&str) -> Result<String, String> + Send + Sync;

#[derive(Clone)]
pub struct ApiConnection {
    pub base_url: String,
    pub credential_ref: String,
    /// Validated by each service's factory, independently of other protocols.
    pub options: serde_json::Value,
    pub http: Arc<dyn crate::http::HttpClient>,
    pub credentials: Arc<CredentialResolver>,
    pub now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

#[derive(Clone)]
pub enum ResourceConnection {
    Plugin {
        root: std::path::PathBuf,
        account: String,
    },
    LocalSubscription(Arc<ProviderContext>),
    Api(ApiConnection),
}

pub type ResourceRegistry = FactoryRegistry<ResourceContext, Arc<dyn ResourceReader>>;

/// Canonical endpoint validation shared with launch identity matching.
pub(crate) fn api_endpoint(value: &str) -> Option<String> {
    api::validate_base_url(value)
        .ok()
        .map(|url| url.to_string())
}

struct SubscriptionResource {
    service: &'static str,
    binding: ResourceBinding,
    usage: Box<dyn UsageProvider>,
}

#[async_trait]
impl ResourceReader for SubscriptionResource {
    fn service(&self) -> &str {
        self.service
    }
    fn binding(&self) -> &ResourceBinding {
        &self.binding
    }
    fn scope(&self) -> ResourceScope {
        ResourceScope::Account
    }
    fn legacy_usage(&self) -> Option<&dyn UsageProvider> {
        Some(self.usage.as_ref())
    }
    async fn read(&self) -> ResourceSnapshot {
        let snapshot = self.usage.query().await;
        let state = if snapshot.success {
            ResourceState::Ready
        } else if snapshot.transient {
            ResourceState::TransientFailure
        } else {
            match snapshot.credential_status {
                CredentialStatus::NotFound => ResourceState::NeedsConfiguration,
                CredentialStatus::Expired => ResourceState::AuthenticationFailed,
                CredentialStatus::RefreshRequired => ResourceState::Failure,
                _ => ResourceState::Failure,
            }
        };
        let mut metrics: Vec<_> = snapshot
            .tiers
            .into_iter()
            .map(|tier| ResourceMetric::QuotaWindow {
                name: tier.name,
                used_percent: tier.utilization,
                resets_at: tier.resets_at,
            })
            .collect();
        if let Some(available) = snapshot.reset_credits_available {
            metrics.push(ResourceMetric::ResetCredits { available });
        }
        if let Some(extra) = snapshot.extra_usage.filter(|extra| extra.is_enabled) {
            metrics.push(ResourceMetric::Spending {
                used: extra.used_credits,
                limit: extra.monthly_limit,
                currency: extra.currency,
            });
        }
        ResourceSnapshot {
            binding: self.binding.clone(),
            service: self.service.into(),
            scope: self.scope(),
            state,
            metrics,
            queried_at: snapshot.queried_at,
            message: snapshot.error.or(snapshot.credential_message),
        }
    }
}

pub fn builtin_registry() -> ResourceRegistry {
    use crate::providers::{ClaudeProvider, CodexProvider, GrokProvider};
    let mut registry = ResourceRegistry::new();
    type Constructor = fn(Arc<ProviderContext>) -> Box<dyn UsageProvider>;
    for (service, constructor) in [
        (
            "claude_subscription",
            (|ctx| Box::new(ClaudeProvider::new(ctx))) as Constructor,
        ),
        (
            "chatgpt_subscription",
            (|ctx| Box::new(CodexProvider::new(ctx))) as Constructor,
        ),
        (
            "grok_subscription",
            (|ctx| Box::new(GrokProvider::new(ctx))) as Constructor,
        ),
    ] {
        registry
            .register(service, move |context: ResourceContext| {
                let ResourceConnection::LocalSubscription(legacy) = context.connection else {
                    return Err(crate::registry::RegistryError::Factory(
                        "订阅资源需要本地登录配置".into(),
                    ));
                };
                Ok(Arc::new(SubscriptionResource {
                    service,
                    binding: context.binding,
                    usage: constructor(legacy),
                }) as Arc<dyn ResourceReader>)
            })
            .expect("内置资源服务 ID 唯一");
    }
    api::register::<new_api::Token>(&mut registry);
    api::register::<new_api::Account>(&mut registry);
    api::register::<openai_costs::Costs>(&mut registry);
    provider_usage::register(&mut registry);
    registry
}

/// Keeps existing output/reset behavior while assigning a scoped cache identity.
pub(crate) struct LegacyUsageAdapter {
    reader: Arc<dyn ResourceReader>,
}

impl LegacyUsageAdapter {
    pub(crate) fn new(reader: Arc<dyn ResourceReader>) -> Self {
        assert!(
            reader.legacy_usage().is_some(),
            "legacy usage capability required"
        );
        Self { reader }
    }
    fn usage(&self) -> &dyn UsageProvider {
        self.reader.legacy_usage().expect("checked at construction")
    }
}

#[async_trait]
impl UsageProvider for LegacyUsageAdapter {
    fn id(&self) -> &'static str {
        self.usage().id()
    }
    fn display_name(&self) -> &'static str {
        self.usage().display_name()
    }
    fn cache_key(&self) -> String {
        self.reader.binding().cache_key(self.reader.service())
    }
    fn reset(&self) -> Option<&dyn UsageReset> {
        self.usage().reset()
    }
    async fn query(&self) -> UsageSnapshot {
        self.usage().query().await
    }
}

/// Existing local-home configuration gets a stable identity without exposing its path in cache keys.
pub(crate) fn local_context(
    service: &str,
    home: &std::path::Path,
    legacy: Arc<ProviderContext>,
) -> ResourceContext {
    let path = crate::config::resolved_data_path(home);
    ResourceContext {
        binding: ResourceBinding {
            account: format!("{service}:{}", path.display()),
            revision: String::new(),
        },
        connection: ResourceConnection::LocalSubscription(legacy),
    }
}

pub(crate) async fn collect_with_deadline(
    readers: &[Arc<dyn ResourceReader>],
    timeout: std::time::Duration,
) -> Vec<ResourceSnapshot> {
    collect_with_updates(readers, timeout, |_, _| {}).await
}

const MAX_CONCURRENT_QUERIES: usize = 8;

/// Publish completed queries immediately while retaining configuration order in the result.
/// Dropping this future cancels the entire query set, including slow readers.
pub(crate) async fn collect_with_updates(
    readers: &[Arc<dyn ResourceReader>],
    timeout: std::time::Duration,
    mut update: impl FnMut(usize, &ResourceSnapshot),
) -> Vec<ResourceSnapshot> {
    let mut tasks = tokio::task::JoinSet::new();
    let mut indices = std::collections::HashMap::new();
    let mut waiting = readers.iter().enumerate();
    let mut snapshots = Vec::with_capacity(readers.len());
    loop {
        // Bound both executing queries and spawned tasks. A queued reader gets its
        // full timeout after admission, regardless of how many accounts are configured.
        while tasks.len() < MAX_CONCURRENT_QUERIES {
            let Some((index, reader)) = waiting.next() else {
                break;
            };
            let reader = Arc::clone(reader);
            let task = tasks.spawn(
                async move {
                    crate::background::query(async {
                        tokio::time::timeout(timeout, reader.read()).await
                    })
                    .await
                }
                .in_current_span(),
            );
            indices.insert(task.id(), index);
        }
        let Some(result) = tasks.join_next_with_id().await else {
            break;
        };
        let (id, result) = match result {
            Ok((id, Ok(snapshot))) => (id, snapshot.map_err(|_| "资源查询超时")),
            Ok((id, Err(error))) => (id, Err(error)),
            Err(error) => (error.id(), Err("资源查询任务中断")),
        };
        let index = indices.remove(&id).expect("registered query task");
        let snapshot = result.unwrap_or_else(|message| {
                let reader = &readers[index];
                tracing::warn!(event = "resource_query_failed", service = reader.service(),
                    target = %reader.binding().cache_key(reader.service()), task = %id, reason = message);
                ResourceSnapshot {
                    binding: reader.binding().clone(),
                    service: reader.service().into(),
                    scope: reader.scope(),
                    state: crate::resources::ResourceState::TransientFailure,
                    metrics: Vec::new(),
                    queried_at: None,
                    message: Some(message.into()),
                }
            });
        update(index, &snapshot);
        snapshots.push((index, snapshot));
    }
    snapshots.sort_by_key(|(index, _)| *index);
    snapshots
        .into_iter()
        .map(|(_, snapshot)| snapshot)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscription_preserves_known_reset_credits_and_does_not_invent_zero() {
        for count in [Some(0), Some(3), None] {
            let http = Arc::new(crate::http::FakeHttp::new());
            let mut body = serde_json::json!({"rate_limit":{"primary_window":{
                "used_percent":25,"limit_window_seconds":18000}}});
            if let Some(available) = count {
                body["rate_limit_reset_credits"] = serde_json::json!({"available_count":available});
            }
            http.route(
                "https://chatgpt.com/backend-api/wham/usage",
                Ok(crate::http::HttpResponse {
                    status: 200,
                    headers: Default::default(),
                    body: serde_json::to_vec(&body).unwrap(),
                }),
            );
            let credentials = serde_json::json!({"auth_mode":"chatgpt","tokens":{
                "access_token":"synthetic-token","account_id":"synthetic-account"}})
            .to_string();
            let context = Arc::new(ProviderContext {
                claude_dir: "/nonexistent".into(),
                codex_dir: "/nonexistent".into(),
                grok_dir: "/nonexistent".into(),
                http,
                keychain: Arc::new(move |_, _| Some(credentials.clone())),
                now: Arc::new(|| DateTime::UNIX_EPOCH),
            });
            let reader = builtin_registry()
                .create(
                    "chatgpt_subscription",
                    local_context("chatgpt_subscription", &context.codex_dir, context.clone()),
                )
                .unwrap();
            let snapshot = reader.read().await;
            assert_eq!(snapshot.state, ResourceState::Ready);
            let actual = snapshot.metrics.iter().find_map(|metric| match metric {
                ResourceMetric::ResetCredits { available } => Some(*available),
                _ => None,
            });
            assert_eq!(actual, count);
        }
    }

    struct PendingReader {
        binding: ResourceBinding,
        pending: bool,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    }

    struct Cancelled(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Cancelled {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl ResourceReader for PendingReader {
        fn service(&self) -> &str {
            "synthetic"
        }
        fn binding(&self) -> &ResourceBinding {
            &self.binding
        }
        async fn read(&self) -> ResourceSnapshot {
            let _cancel = Cancelled(self.cancelled.clone());
            if self.pending {
                std::future::pending::<()>().await;
            }
            ResourceSnapshot {
                binding: self.binding.clone(),
                service: self.service().into(),
                scope: ResourceScope::Account,
                state: ResourceState::Ready,
                metrics: Vec::new(),
                queried_at: None,
                message: None,
            }
        }
    }

    #[tokio::test]
    async fn streaming_queries_deliver_fast_result_and_cancel_slow_reader() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        let cancelled = Arc::new(AtomicBool::new(false));
        let readers: Vec<Arc<dyn ResourceReader>> = [true, false]
            .into_iter()
            .enumerate()
            .map(|(index, pending)| {
                Arc::new(PendingReader {
                    binding: ResourceBinding {
                        account: format!("account-{index}"),
                        revision: String::new(),
                    },
                    pending,
                    cancelled: if pending {
                        cancelled.clone()
                    } else {
                        Arc::new(AtomicBool::new(false))
                    },
                }) as Arc<dyn ResourceReader>
            })
            .collect();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            collect_with_updates(&readers, Duration::from_secs(30), |index, _| {
                tx.send(index).unwrap();
            })
            .await
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap(),
            Some(1)
        );
        assert!(!task.is_finished());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !cancelled.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    struct GatedReader {
        binding: ResourceBinding,
        started: tokio::sync::mpsc::UnboundedSender<()>,
        gate: Arc<tokio::sync::Semaphore>,
    }
    #[async_trait]
    impl ResourceReader for GatedReader {
        fn service(&self) -> &str {
            "synthetic"
        }
        fn binding(&self) -> &ResourceBinding {
            &self.binding
        }
        async fn read(&self) -> ResourceSnapshot {
            self.started.send(()).unwrap();
            self.gate.acquire().await.unwrap().forget();
            ResourceSnapshot {
                binding: self.binding.clone(),
                service: self.service().into(),
                scope: ResourceScope::Account,
                state: ResourceState::Ready,
                metrics: Vec::new(),
                queried_at: None,
                message: None,
            }
        }
    }

    #[tokio::test]
    async fn large_query_sets_bound_concurrency_without_losing_order() {
        use std::time::Duration;
        let count = MAX_CONCURRENT_QUERIES * 2 + 1;
        let (started, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let readers: Vec<Arc<dyn ResourceReader>> = (0..count)
            .map(|index| {
                Arc::new(GatedReader {
                    binding: ResourceBinding {
                        account: index.to_string(),
                        revision: String::new(),
                    },
                    started: started.clone(),
                    gate: gate.clone(),
                }) as Arc<dyn ResourceReader>
            })
            .collect();
        let task =
            tokio::spawn(
                async move { collect_with_deadline(&readers, Duration::from_secs(5)).await },
            );
        for _ in 0..MAX_CONCURRENT_QUERIES {
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), rx.recv())
                .await
                .is_err()
        );
        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), rx.recv())
                .await
                .is_err()
        );
        gate.add_permits(count - 1);
        let snapshots = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshots.len(), count);
        for (index, snapshot) in snapshots.iter().enumerate() {
            assert_eq!(snapshot.binding.account, index.to_string());
            assert_eq!(snapshot.state, ResourceState::Ready);
        }
    }

    #[tokio::test]
    async fn registered_subscription_reports_missing_configuration_as_resource_state() {
        let legacy = Arc::new(ProviderContext {
            claude_dir: "/nonexistent/usage-resource-test-claude".into(),
            codex_dir: "/nonexistent/usage-resource-test-codex".into(),
            grok_dir: "/nonexistent/usage-resource-test-grok".into(),
            http: Arc::new(crate::http::FakeHttp::new()),
            keychain: Arc::new(|_, _| None),
            now: Arc::new(|| DateTime::UNIX_EPOCH),
        });
        let context = local_context("chatgpt_subscription", &legacy.codex_dir, legacy.clone());
        let binding = context.binding.clone();
        let reader = builtin_registry()
            .create("chatgpt_subscription", context)
            .unwrap();
        let snapshot = reader.read().await;
        assert_eq!(snapshot.state, ResourceState::NeedsConfiguration);
        assert_eq!(snapshot.binding, binding);
        assert!(snapshot.metrics.is_empty());
        assert!(reader.legacy_usage().unwrap().reset().is_some());
    }
}
