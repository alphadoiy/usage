//! 提供方工厂和注册表。
//!
//! 新增一个工具时实现 [`UsageProvider`]，再 `factory.register(id, 工厂)`。
//! 查询和界面都通过工厂创建实例。

mod claude;
mod codex;
mod grok;
mod grok_proto;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

pub use claude::ClaudeProvider;
pub use codex::CodexProvider;
pub use grok::GrokProvider;

use crate::http::HttpClient;
use crate::model::{UsageSnapshot, snapshot_transient};

/// Bounds the whole provider query, including credential loading and HTTP.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(20);

#[async_trait]
pub trait UsageProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    /// Identity of the configured query target, separate from its tool type.
    fn cache_key(&self) -> String {
        self.id().into()
    }
    async fn query(&self) -> UsageSnapshot;

    fn reset(&self) -> Option<&dyn UsageReset> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetPreview {
    pub available: u32,
    pub next_expires_at: Option<DateTime<Utc>>,
    pub account_label: Option<String>,
}

#[async_trait]
pub trait UsageReset: Send + Sync {
    async fn preview_reset(&self) -> Result<ResetPreview, ProviderError>;
    async fn consume_reset(&self) -> Result<(), ProviderError>;
}

pub async fn query_with_timeout(provider: &dyn UsageProvider, timeout: Duration) -> UsageSnapshot {
    let mut op = crate::logging::Operation::new("provider.query");
    op.begin("query");
    let span = op.span();
    let snapshot = crate::logging::instrument(span, async {
        let provider_name = match provider.id() {
            "claude" => "claude",
            "codex" => "codex",
            "grok" => "grok",
            _ => "other",
        };
        let result = tokio::time::timeout(timeout, provider.query()).await;
        let timed_out = result.is_err();
        let snapshot = result.unwrap_or_else(|_| {
            snapshot_transient(provider.id(), provider.display_name(), "用量查询超时")
        });
        if snapshot.success {
            tracing::debug!(
                event = "provider_query_result", provider = provider_name,
                successful = true, credential_status = ?snapshot.credential_status
            );
        } else {
            tracing::warn!(
                event = "provider_query_result", provider = provider_name,
                successful = false, timed_out, credential_status = ?snapshot.credential_status,
                transient = snapshot.transient
            );
        }
        snapshot
    })
    .await;
    op.finish(snapshot.success);
    snapshot
}

pub type KeychainReader = dyn Fn(&str, Option<&str>) -> Option<String> + Send + Sync;

pub struct ProviderContext {
    pub claude_dir: PathBuf,
    pub codex_dir: PathBuf,
    pub grok_dir: PathBuf,
    pub http: Arc<dyn HttpClient>,
    pub keychain: Arc<KeychainReader>,
    pub now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

pub type ProviderFactoryFn =
    Arc<dyn Fn(Arc<ProviderContext>) -> Box<dyn UsageProvider> + Send + Sync>;

#[derive(Debug)]
pub struct ProviderError(pub String);

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for ProviderError {}

#[derive(Debug)]
pub struct UnknownProviderError {
    pub missing: Vec<String>,
    pub known: Vec<String>,
}

impl std::fmt::Display for UnknownProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let known = if self.known.is_empty() {
            "（无）".to_string()
        } else {
            self.known.join(", ")
        };
        write!(
            formatter,
            "未知提供方: {}。已注册: {known}",
            self.missing.join(", ")
        )
    }
}

impl std::error::Error for UnknownProviderError {}

#[derive(Default)]
pub struct ProviderRegistry {
    factories: crate::registry::FactoryRegistry<Arc<ProviderContext>, Box<dyn UsageProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        provider_id: &str,
        factory: ProviderFactoryFn,
    ) -> Result<(), ProviderError> {
        self.factories
            .register(provider_id, move |ctx| Ok(factory(ctx)))
            .map_err(|error| ProviderError(error.to_string()))
    }

    pub fn ids(&self) -> Vec<String> {
        self.factories.ids()
    }

    pub fn contains(&self, provider_id: &str) -> bool {
        self.factories.contains(provider_id)
    }

    pub fn create(
        &self,
        provider_id: &str,
        ctx: Arc<ProviderContext>,
    ) -> Result<Box<dyn UsageProvider>, ProviderError> {
        let provider = self
            .factories
            .create(provider_id, ctx)
            .map_err(|error| ProviderError(error.to_string()))?;
        if provider.id() != provider_id {
            return Err(ProviderError(format!(
                "提供方 {provider_id} 的工厂返回了 id={}",
                provider.id()
            )));
        }
        Ok(provider)
    }
}

pub struct ProviderFactory {
    pub registry: ProviderRegistry,
}

impl ProviderFactory {
    pub fn new() -> Self {
        Self {
            registry: ProviderRegistry::new(),
        }
    }

    pub fn register(
        &mut self,
        provider_id: &str,
        factory: impl Fn(Arc<ProviderContext>) -> Box<dyn UsageProvider> + Send + Sync + 'static,
    ) -> Result<(), ProviderError> {
        self.registry.register(provider_id, Arc::new(factory))
    }

    pub fn create(
        &self,
        provider_id: &str,
        ctx: Arc<ProviderContext>,
    ) -> Result<Box<dyn UsageProvider>, ProviderError> {
        self.registry.create(provider_id, ctx)
    }

    pub fn create_many(
        &self,
        provider_ids: &[String],
        ctx: Arc<ProviderContext>,
    ) -> Result<Vec<Box<dyn UsageProvider>>, UnknownProviderError> {
        let unknown: Vec<String> = provider_ids
            .iter()
            .filter(|provider_id| !self.registry.contains(provider_id))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err(UnknownProviderError {
                missing: unknown,
                known: self.registry.ids(),
            });
        }
        provider_ids
            .iter()
            .map(|provider_id| {
                self.registry
                    .create(provider_id, Arc::clone(&ctx))
                    .map_err(|error| UnknownProviderError {
                        missing: vec![error.0],
                        known: self.registry.ids(),
                    })
            })
            .collect()
    }
}

impl Default for ProviderFactory {
    fn default() -> Self {
        Self::new()
    }
}

pub fn builtin_factory() -> ProviderFactory {
    let mut factory = ProviderFactory::new();
    let resources = crate::hub::shared().resources.clone();
    for (id, service) in [
        ("claude", "claude_subscription"),
        ("codex", "chatgpt_subscription"),
        ("grok", "grok_subscription"),
    ] {
        let resources = Arc::clone(&resources);
        factory
            .register(id, move |ctx| {
                let home = match id {
                    "claude" => &ctx.claude_dir,
                    "codex" => &ctx.codex_dir,
                    _ => &ctx.grok_dir,
                };
                let context = crate::resources::local_context(service, home, Arc::clone(&ctx));
                let reader = resources
                    .create(service, context)
                    .expect("内置资源工厂有效");
                Box::new(crate::resources::LegacyUsageAdapter::new(reader))
            })
            .expect("内置工具 ID 唯一");
    }
    factory
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    struct SlowProvider;

    #[async_trait]
    impl UsageProvider for SlowProvider {
        fn id(&self) -> &'static str {
            "slow"
        }

        fn display_name(&self) -> &'static str {
            "Slow"
        }

        async fn query(&self) -> UsageSnapshot {
            tokio::time::sleep(Duration::from_secs(1)).await;
            unreachable!("timed out query must be cancelled")
        }
    }

    #[tokio::test]
    async fn query_deadline_returns_transient_snapshot() {
        let snapshot = query_with_timeout(&SlowProvider, Duration::from_millis(10)).await;
        assert_eq!(snapshot.tool, "slow");
        assert_eq!(snapshot.error.as_deref(), Some("用量查询超时"));
        assert!(snapshot.transient);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_credential_reader_does_not_hold_the_deadline() {
        let mut ctx = empty_ctx();
        let context = Arc::get_mut(&mut ctx).expect("unique context");
        context.claude_dir = PathBuf::from("/nonexistent/usage-test-credentials");
        context.keychain = Arc::new(|_, _| {
            std::thread::sleep(Duration::from_millis(100));
            None
        });
        let provider = ClaudeProvider::new(ctx);
        let snapshot = query_with_timeout(&provider, Duration::from_millis(5)).await;
        assert!(snapshot.transient);
        assert_eq!(snapshot.error.as_deref(), Some("用量查询超时"));
    }

    #[test]
    fn builtin_order_and_unknown_provider() {
        let factory = builtin_factory();
        assert_eq!(factory.registry.ids(), ["claude", "codex", "grok"]);
        for id in ["claude", "codex", "grok"] {
            let provider = factory.create(id, empty_ctx()).unwrap();
            assert_eq!(provider.reset().is_some(), id == "codex");
        }
        let Err(error) = factory.create_many(&["claude".into(), "missing".into()], empty_ctx())
        else {
            panic!("expected unknown provider");
        };
        assert!(error.to_string().contains("missing"));
        assert!(error.to_string().contains("claude"));
    }

    #[test]
    fn factory_rejects_mismatched_id() {
        struct Other;
        #[async_trait]
        impl UsageProvider for Other {
            fn id(&self) -> &'static str {
                "other"
            }
            fn display_name(&self) -> &'static str {
                "Other"
            }
            async fn query(&self) -> UsageSnapshot {
                crate::model::snapshot_not_found(self.id(), self.display_name(), None)
            }
        }
        let mut factory = ProviderFactory::new();
        factory.register("claude", |_| Box::new(Other)).unwrap();
        let Err(error) = factory.create("claude", empty_ctx()) else {
            panic!("expected mismatched id");
        };
        assert!(error.to_string().contains("other"));
    }

    #[test]
    fn builtin_cache_identity_tracks_home_instead_of_tool_type() {
        let factory = builtin_factory();
        let a = empty_ctx();
        let mut b = empty_ctx();
        Arc::get_mut(&mut b).unwrap().codex_dir = PathBuf::from("/nonexistent/usage-other-account");
        let first = factory.create("codex", a.clone()).unwrap();
        let same = factory.create("codex", a).unwrap();
        let second = factory.create("codex", b).unwrap();
        assert_eq!(first.id(), second.id());
        assert_eq!(first.cache_key(), same.cache_key());
        assert_ne!(first.cache_key(), second.cache_key());
        assert!(!second.cache_key().contains("usage-other-account"));
    }

    fn empty_ctx() -> Arc<ProviderContext> {
        Arc::new(ProviderContext {
            claude_dir: PathBuf::from("."),
            codex_dir: PathBuf::from("."),
            grok_dir: PathBuf::from("."),
            http: Arc::new(crate::http::FakeHttp::new()),
            keychain: Arc::new(|_, _| None),
            now: Arc::new(|| DateTime::UNIX_EPOCH),
        })
    }
}
