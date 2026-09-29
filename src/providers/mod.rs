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
    async fn query(&self) -> UsageSnapshot;
}

pub async fn query_with_timeout(provider: &dyn UsageProvider, timeout: Duration) -> UsageSnapshot {
    tokio::time::timeout(timeout, provider.query())
        .await
        .unwrap_or_else(|_| {
            snapshot_transient(provider.id(), provider.display_name(), "用量查询超时")
        })
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
    factories: Vec<(String, ProviderFactoryFn)>,
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
        let provider_id = provider_id.trim();
        if provider_id.is_empty() {
            return Err(ProviderError("提供方 id 不能为空".into()));
        }
        if let Some(existing) = self.factories.iter_mut().find(|(id, _)| id == provider_id) {
            existing.1 = factory;
        } else {
            self.factories.push((provider_id.to_string(), factory));
        }
        Ok(())
    }

    pub fn ids(&self) -> Vec<String> {
        self.factories.iter().map(|(id, _)| id.clone()).collect()
    }

    pub fn contains(&self, provider_id: &str) -> bool {
        self.factories.iter().any(|(id, _)| id == provider_id)
    }

    pub fn create(
        &self,
        provider_id: &str,
        ctx: Arc<ProviderContext>,
    ) -> Result<Box<dyn UsageProvider>, ProviderError> {
        let Some((_, factory)) = self.factories.iter().find(|(id, _)| id == provider_id) else {
            return Err(ProviderError(
                UnknownProviderError {
                    missing: vec![provider_id.to_string()],
                    known: self.ids(),
                }
                .to_string(),
            ));
        };
        let provider = factory(ctx);
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
    factory
        .register(ClaudeProvider::ID, |ctx| Box::new(ClaudeProvider::new(ctx)))
        .expect("claude");
    factory
        .register(CodexProvider::ID, |ctx| Box::new(CodexProvider::new(ctx)))
        .expect("codex");
    factory
        .register(GrokProvider::ID, |ctx| Box::new(GrokProvider::new(ctx)))
        .expect("grok");
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
