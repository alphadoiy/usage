//! Provider inventory with independent model, quota and balance capabilities.
mod codex;
pub mod presets;
mod usage;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
pub use usage::{ProviderUsage, QueryState, ResetHandle, UsageSource};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;

use crate::codex_manager::{AuthMode, Manager};
use crate::config::AppConfig;
use crate::http::HttpClient;
use crate::resources::CredentialResolver;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OfficialApi,
    CompatibleApi,
    Subscription,
    SubscriptionService,
    Plugin,
}
impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::OfficialApi => "官方 API · BYOK",
            Self::CompatibleApi => "兼容 API · BYOK",
            Self::Subscription => "官方订阅 · OAuth",
            Self::SubscriptionService => "订阅服务",
            Self::Plugin => "扩展服务",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Models {
    Loading,
    Ready(Vec<String>),
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub agent: Option<String>,
    pub kind: ProviderKind,
    pub configured_model: Option<String>,
    pub models: Models,
    pub usage: ProviderUsage,
    pub operations: Vec<crate::provider_actions::OperationHandle>,
    pub performance: Option<crate::performance::Binding>,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub providers: Vec<Provider>,
    pub(crate) model_keys: HashMap<String, String>,
    pub(crate) refreshing_models: HashSet<String>,
    pub(crate) refreshing_resources: HashSet<String>,
}
impl Catalog {
    pub fn apply_models(&mut self, id: &str, models: Models) -> bool {
        let Some(provider) = self.providers.iter_mut().find(|p| p.id == id) else {
            return false;
        };
        if matches!(models, Models::Loading) {
            return false;
        }
        let refreshing = self.refreshing_models.remove(id);
        if !matches!(provider.models, Models::Loading) && !refreshing {
            return false;
        }
        provider.models = models;
        true
    }
}

pub struct Plan {
    pub catalog: Catalog,
    pub queries: Vec<ModelQuery>,
    pub resources: Vec<Arc<dyn crate::resources::ResourceReader>>,
}

impl Plan {
    /// Reuse only values whose query identity is unchanged. A new plan always
    /// rediscovers local configuration, but stable model lists need no network.
    pub(crate) fn reuse(&mut self, previous: &Catalog, refresh_models: bool) {
        self.queries.retain(|query| {
            let cached = previous.providers.iter().find(|p| p.id == query.id);
            if self
                .catalog
                .model_keys
                .get(&query.id)
                .is_some_and(|key| Some(key) == previous.model_keys.get(&query.id))
                && let Some(cached) = cached
                && !matches!(cached.models, Models::Loading)
            {
                self.catalog
                    .providers
                    .iter_mut()
                    .find(|p| p.id == query.id)
                    .unwrap()
                    .models = cached.models.clone();
                if refresh_models {
                    self.catalog.refreshing_models.insert(query.id.clone());
                }
                return refresh_models;
            }
            true
        });
        for provider in &mut self.catalog.providers {
            let Some(cached) = previous.providers.iter().find(|p| p.id == provider.id) else {
                continue;
            };
            for source in &mut provider.usage.sources {
                if !matches!(source.state, QueryState::Loading) {
                    continue;
                }
                if let Some(old) = cached.usage.sources.iter().find(|old| {
                    old.target == source.target
                        && old.binding == source.binding
                        && old.service == source.service
                }) && let QueryState::Result(_) = &old.state
                {
                    source.state = old.state.clone();
                    self.catalog
                        .refreshing_resources
                        .insert(source.target.clone());
                }
            }
        }
    }
}

enum Protocol {
    OpenAi,
    Anthropic,
    Codex(codex::AccountModels),
    Plugin { root: PathBuf, account: String },
}

// Credentials only live in query plans; never Debug/Serialize them or put them in UI state.
pub struct ModelQuery {
    id: String,
    url: reqwest::Url,
    key: String,
    protocol: Protocol,
}

fn endpoint(base: &str) -> Result<reqwest::Url, &'static str> {
    let mut url = crate::http::validate_api_base_url(base).map_err(|error| match error {
        crate::http::BaseUrlError::Invalid => "模型接口地址无效",
        crate::http::BaseUrlError::Policy => {
            "模型接口地址需为无凭据和查询参数的 HTTPS 地址（本机回环可用 HTTP）"
        }
    })?;
    let path = url.path().trim_end_matches('/');
    let path = if path.is_empty() {
        "/v1/models".into()
    } else {
        format!("{path}/models")
    };
    url.set_path(&path);
    Ok(url)
}

impl ModelQuery {
    fn cache_key(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.url.as_str());
        hash.update([0]);
        hash.update(&self.key);
        if let Protocol::Plugin { root, .. } = &self.protocol {
            hash.update(root.as_os_str().as_encoded_bytes());
        }
        if let Protocol::Codex(account) = &self.protocol {
            hash.update([0]);
            hash.update(&account.account_id);
            hash.update([0]);
            hash.update(account.cached_version.to_string());
        }
        format!("{:x}", hash.finalize())
    }

    pub(crate) fn new(id: String, base: &str, key: String) -> Result<Self, &'static str> {
        if key.trim().is_empty() || key.chars().any(char::is_control) {
            return Err("未提供有效的推理 API Key");
        }
        let url = endpoint(base)?;
        let protocol = if url.host_str() == Some("api.anthropic.com") {
            Protocol::Anthropic
        } else {
            Protocol::OpenAi
        };
        Ok(Self {
            id,
            url,
            key,
            protocol,
        })
    }

    async fn read(&self, http: &dyn HttpClient) -> Models {
        let result = tokio::time::timeout(Duration::from_secs(12), self.fetch(http)).await;
        match result {
            Ok(Ok(models)) => Models::Ready(models),
            Ok(Err(error)) => Models::Unavailable(error.into()),
            Err(_) => Models::Unavailable("模型列表查询超时".into()),
        }
    }

    async fn fetch(&self, http: &dyn HttpClient) -> Result<Vec<String>, &'static str> {
        if let Protocol::Plugin { root, account } = &self.protocol {
            let models = crate::plugins::call_at(
                root,
                serde_json::json!({"operation":"models","account":account}),
            )
            .await
            .map_err(|_| "插件模型目录查询失败")?;
            return models
                .as_array()
                .ok_or("插件模型目录格式无效")?
                .iter()
                .map(|m| {
                    m["id"]
                        .as_str()
                        .filter(|s| {
                            !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
                        })
                        .map(str::to_owned)
                        .ok_or("插件模型 ID 无效")
                })
                .collect();
        }
        if let Protocol::Codex(account) = &self.protocol {
            return account.fetch(http, &self.key).await;
        }
        #[derive(Deserialize)]
        struct Model {
            id: String,
        }
        #[derive(Deserialize)]
        struct Response {
            data: Vec<Model>,
            #[serde(default)]
            has_more: bool,
            last_id: Option<String>,
        }
        let headers = match &self.protocol {
            Protocol::OpenAi => vec![("Authorization", format!("Bearer {}", self.key))],
            Protocol::Anthropic => vec![
                ("x-api-key", self.key.clone()),
                ("anthropic-version", "2023-06-01".into()),
            ],
            Protocol::Codex(_) | Protocol::Plugin { .. } => unreachable!(),
        };
        let mut url = self.url.clone();
        let mut models = BTreeSet::new();
        let mut cursors = BTreeSet::new();
        for _ in 0..20 {
            let body = model_response(http, &url, &headers, 2 * 1024 * 1024).await?;
            let page: Response =
                serde_json::from_slice(&body).map_err(|_| "模型列表响应格式无效")?;
            for model in page.data {
                if model.id.is_empty()
                    || model.id.len() > 256
                    || model.id.chars().any(char::is_control)
                {
                    return Err("模型列表包含无效的模型名称");
                }
                models.insert(model.id);
            }
            if models.len() > 5000 {
                return Err("模型列表超过展示上限");
            }
            if !page.has_more {
                return Ok(models.into_iter().collect());
            }
            let cursor = page
                .last_id
                .filter(|id| !id.is_empty())
                .ok_or("模型列表分页缺少游标")?;
            if !cursors.insert(cursor.clone()) {
                return Err("模型列表分页游标重复");
            }
            url = self.url.clone();
            url.query_pairs_mut().append_pair("after_id", &cursor);
        }
        Err("模型列表分页超过上限")
    }
}

async fn model_response(
    http: &dyn HttpClient,
    url: &reqwest::Url,
    headers: &[(&str, String)],
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    let response = http
        .request("GET", url.as_str(), headers, None)
        .await
        .map_err(|_| "模型列表网络请求失败")?;
    match response.status {
        200 => {}
        401 | 403 => return Err("模型列表认证失败或权限不足"),
        404 | 405 | 501 => return Err("此端点未提供模型列表接口"),
        429 => return Err("模型列表查询受到限流"),
        _ => return Err("模型列表接口暂不可用"),
    }
    if response.body.len() > limit {
        return Err("模型列表响应过大");
    }
    Ok(response.body)
}

fn kind(base: &str) -> ProviderKind {
    let official = crate::resources::balance_endpoint(base).is_some()
        || reqwest::Url::parse(base).ok().is_some_and(|url| {
            url.scheme() == "https"
                && url.port_or_known_default() == Some(443)
                && ["api.openai.com", "api.anthropic.com", "api.x.ai"]
                    .contains(&url.host_str().unwrap_or_default())
        });
    if official {
        ProviderKind::OfficialApi
    } else {
        ProviderKind::CompatibleApi
    }
}

pub struct CatalogService {
    root: PathBuf,
    pub(crate) http: Arc<dyn HttpClient>,
    credentials: Arc<CredentialResolver>,
    plugins: crate::plugins::store::Store,
}
impl CatalogService {
    pub fn new(
        root: PathBuf,
        http: Arc<dyn HttpClient>,
        credentials: Arc<CredentialResolver>,
    ) -> Self {
        Self {
            root,
            http,
            credentials,
            plugins: crate::plugins::store::Store::new(crate::plugins::store::root()),
        }
    }
    pub fn with_plugin_root(mut self, root: PathBuf) -> Self {
        self.plugins = crate::plugins::store::Store::new(root);
        self
    }
    pub fn from_environment() -> Self {
        Self::new(
            crate::codex_manager::default_root(),
            Arc::new(crate::http::ReqwestClient::same_origin_redirects()),
            Arc::new(|reference| {
                let name = reference.strip_prefix("env:").ok_or("无效的凭据引用")?;
                std::env::var(name).map_err(|_| "推理凭据环境变量未设置".into())
            }),
        )
    }
    pub fn load(&self, config: &AppConfig) -> Result<Plan, String> {
        let mut plan = Plan {
            catalog: Catalog::default(),
            queries: Vec::new(),
            resources: Vec::new(),
        };
        let plugin_accounts = self.plugins.accounts().unwrap_or_default();
        let plugins: HashMap<_, _> = self
            .plugins
            .plugins()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.enabled)
            .map(|p| (p.id.clone(), p))
            .collect();
        let available_accounts: HashSet<_> = plugin_accounts
            .iter()
            .filter(|a| plugins.contains_key(&a.plugin))
            .map(|a| a.id.as_str())
            .collect();
        let mut plugin_bindings: HashMap<String, Vec<String>> = HashMap::new();
        if let Some(manager) =
            Manager::open_read_only(self.root.clone()).map_err(|_| "读取 Provider 账号库失败")?
        {
            for profile in manager.list().map_err(|_| "读取 Provider 配置失败")? {
                let plugin_account = crate::plugins::endpoint_account(&profile.base_url);
                let models = if let Some(account) = &plugin_account {
                    plugin_bindings
                        .entry(account.clone())
                        .or_default()
                        .push(profile.id.clone());
                    if available_accounts.contains(account.as_str()) {
                        Models::Loading
                    } else {
                        unavailable_plugin()
                    }
                } else {
                    match manager.model_catalog_query(&profile) {
                        Ok(query) => {
                            plan.queries.push(query);
                            Models::Loading
                        }
                        Err(_) => unreadable_query(&profile),
                    }
                };
                let provider_kind = if let Some(account_id) = &plugin_account {
                    plugin_accounts
                        .iter()
                        .find(|a| &a.id == account_id)
                        .and_then(|a| plugins.get(&a.plugin))
                        .and_then(|p| presets::for_package(&p.package))
                        .map_or(ProviderKind::Plugin, |preset| preset.kind)
                } else if profile.auth_mode == AuthMode::Oauth {
                    ProviderKind::Subscription
                } else {
                    kind(&profile.base_url)
                };
                plan.catalog.providers.push(Provider {
                    id: profile.id.clone(),
                    name: profile.name.clone(),
                    agent: Some("Codex".into()),
                    kind: provider_kind,
                    configured_model: Some(profile.model.clone()),
                    models,
                    operations: Vec::new(),
                    usage: ProviderUsage::default(),
                    performance: manager.performance_binding(&profile, profile.revision).ok(),
                });
                self.add_profile_usage(&mut plan, &manager, &profile, config);
            }
        }
        let mut descriptors: HashMap<String, Option<serde_json::Value>> = HashMap::new();
        for account in plugin_accounts {
            let Some(plugin) = plugins.get(&account.plugin) else {
                continue;
            };
            let preset = presets::for_package(&plugin.package);
            let existing = plugin_bindings.remove(&account.id).unwrap_or_default();
            let descriptor = descriptors.entry(plugin.id.clone()).or_insert_with(|| {
                crate::plugins::store::read::<serde_json::Value>(
                    &self
                        .plugins
                        .root
                        .join("descriptors")
                        .join(format!("{}.json", plugin.id)),
                )
                .ok()
                .filter(|d| d["revision"].as_u64() == Some(plugin.revision))
            });
            let supports_usage = descriptor
                .as_ref()
                .and_then(|d| {
                    d["providers"]
                        .as_array()?
                        .iter()
                        .find(|p| p["id"] == account.provider)
                        .map(|p| p["capabilities"]["usage"] == true)
                })
                .unwrap_or(true);
            let ids = if existing.is_empty() {
                let id = format!("plugin:{}", account.id);
                plan.catalog.providers.push(Provider {
                    id: id.clone(),
                    name: preset.map_or_else(
                        || account.name.clone(),
                        |p| p.account_name(&account.provider, &account.name),
                    ),
                    agent: None,
                    kind: preset.map_or(ProviderKind::Plugin, |p| p.kind),
                    configured_model: None,
                    models: Models::Loading,
                    operations: Vec::new(),
                    usage: ProviderUsage::default(),
                    performance: None,
                });
                vec![id]
            } else {
                existing
            };
            for id in ids {
                let query = self.plugin_query(id.clone(), plugin, &account.id);
                if supports_usage {
                    let reader = crate::plugins::adapters::resource(
                        &self.plugins.root,
                        &account,
                        format!(
                            "{}:{}:{}",
                            plugin.version,
                            plugin.revision,
                            plugin.entry.display()
                        ),
                    );
                    let label =
                        preset.map_or_else(|| "账号额度".into(), |p| format!("{} 额度", p.name));
                    usage::attach_reader(&mut plan, &id, label, reader);
                } else if let Some(p) = plan.catalog.providers.iter_mut().find(|p| p.id == id) {
                    p.usage.hint = Some("此服务暂未提供用量查询".into());
                }
                if let Some(checkin) = crate::plugins::capabilities::checkin(
                    &plugin.package,
                    &account.provider,
                    descriptor.as_ref(),
                ) && let Some(provider) = plan.catalog.providers.iter_mut().find(|p| p.id == id)
                {
                    provider.operations.push(
                        crate::provider_actions::registry()
                            .create(
                                "plugin_checkin",
                                crate::provider_actions::Context {
                                    root: self.plugins.root.clone(),
                                    account: account.id.clone(),
                                    automatic_status: checkin.automatic_status,
                                    day_offset_seconds: checkin.day_offset_seconds,
                                },
                            )
                            .expect("discovered plugin account operation"),
                    );
                }
                plan.queries.push(query);
            }
        }
        for connection in &config.resources {
            let Some(inference) = &connection.inference else {
                continue;
            };
            // Billing services for an existing managed provider are not extra inference providers.
            if connection
                .profile_ids
                .iter()
                .any(|id| plan.catalog.providers.iter().any(|p| &p.id == id))
            {
                continue;
            }
            let id = format!("resource:{}", connection.id);
            let query = (self.credentials)(&format!("env:{}", inference.credential_env))
                .map_err(|_| "推理凭据环境变量未设置")
                .and_then(|key| ModelQuery::new(id.clone(), &inference.base_url, key));
            let models = match query {
                Ok(query) => {
                    plan.queries.push(query);
                    Models::Loading
                }
                Err(error) => Models::Unavailable(error.into()),
            };
            let performance = inference
                .performance_instance
                .as_deref()
                .map(|instance_id| {
                    let instance = config
                        .instances
                        .iter()
                        .find(|i| i.id == instance_id)
                        .ok_or("性能日志实例不存在")?;
                    let agent = crate::agents::builtin_registry()
                        .create(
                            &instance.provider,
                            crate::agents::AgentContext::Local {
                                home: instance.path.clone(),
                            },
                        )
                        .map_err(|_| "性能日志 Agent 无效")?;
                    let source = agent.request_source().ok_or("此 Agent 尚不支持性能日志")?;
                    Ok::<_, &str>(crate::performance::local_binding(
                        &id,
                        agent.kind().id(),
                        &instance.path,
                        source,
                    ))
                })
                .transpose()?;
            plan.catalog.providers.push(Provider {
                id,
                name: connection
                    .label
                    .clone()
                    .unwrap_or_else(|| connection.id.clone()),
                agent: None,
                kind: kind(&inference.base_url),
                configured_model: None,
                models,
                operations: Vec::new(),
                usage: ProviderUsage::default(),
                performance,
            });
        }
        self.add_configured_usage(&mut plan, config);
        plan.catalog.model_keys = plan
            .queries
            .iter()
            .map(|query| (query.id.clone(), query.cache_key()))
            .collect();
        Ok(plan)
    }
}

fn unreadable_query(profile: &crate::codex_manager::Profile) -> Models {
    Models::Unavailable(if profile.auth_mode == AuthMode::Oauth {
        "无法读取此 OAuth 账号的模型查询凭据，请重新登录".into()
    } else {
        "无法读取此 Provider 的模型查询凭据".into()
    })
}
fn unavailable_plugin() -> Models {
    Models::Unavailable("插件账号不可用或插件已停用".into())
}

/// Where an Agent form lists models: a saved Provider, or an endpoint typed into the form.
pub enum ModelSource {
    Saved(String),
    Endpoint { base: String, key: String },
}

impl CatalogService {
    /// Cached by package version, plugin revision and account, so a plugin update refetches.
    fn plugin_query(
        &self,
        id: String,
        plugin: &crate::plugins::store::Plugin,
        account: &str,
    ) -> ModelQuery {
        ModelQuery {
            id,
            url: reqwest::Url::parse(&crate::plugins::account_endpoint(account))
                .expect("plugin endpoint"),
            key: format!(
                "{}:{}:{}:{}",
                plugin.id, plugin.version, plugin.revision, account
            ),
            protocol: Protocol::Plugin {
                root: self.plugins.root.clone(),
                account: account.into(),
            },
        }
    }

    /// The query the home board makes for one Provider, outside a full refresh.
    pub async fn models(self: Arc<Self>, source: ModelSource) -> Models {
        let http = self.http.clone();
        let query = tokio::task::spawn_blocking(move || match source {
            ModelSource::Endpoint { base, key } => ModelQuery::new("form".into(), &base, key)
                .map_err(|e| Models::Unavailable(e.into())),
            // Only this Provider's credentials are read, through the same construction as `load`.
            ModelSource::Saved(id) => {
                let manager = Manager::open_read_only(self.root.clone())
                    .ok()
                    .flatten()
                    .ok_or_else(|| Models::Unavailable("读取 Provider 账号库失败".into()))?;
                let profile = manager
                    .list()
                    .ok()
                    .and_then(|profiles| profiles.into_iter().find(|p| p.id == id))
                    .ok_or_else(|| Models::Unavailable("Provider 不存在".into()))?;
                match crate::plugins::endpoint_account(&profile.base_url) {
                    Some(account) => self
                        .plugins
                        .account(&account)
                        .ok()
                        .and_then(|a| {
                            self.plugins
                                .plugins()
                                .ok()?
                                .into_iter()
                                .find(|p| p.enabled && p.id == a.plugin)
                        })
                        .map(|plugin| self.plugin_query(profile.id.clone(), &plugin, &account))
                        .ok_or_else(unavailable_plugin),
                    None => manager
                        .model_catalog_query(&profile)
                        .map_err(|_| unreadable_query(&profile)),
                }
            }
        })
        .await;
        match query {
            Ok(Ok(query)) => query.read(http.as_ref()).await,
            Ok(Err(models)) => models,
            Err(_) => Models::Unavailable("模型列表查询任务中断".into()),
        }
    }
}

/// Bound concurrency and keep identity on failures; dropping the future cancels all requests.
pub async fn collect_models(
    http: Arc<dyn HttpClient>,
    queries: Vec<ModelQuery>,
    mut update: impl FnMut(String, Models),
) {
    let mut tasks = JoinSet::new();
    let mut ids = HashMap::new();
    let mut pending = queries.into_iter();
    loop {
        while tasks.len() < 4 {
            let Some(query) = pending.next() else {
                break;
            };
            let id = query.id.clone();
            let http = http.clone();
            let task = tasks.spawn(async move { query.read(http.as_ref()).await });
            ids.insert(task.id(), id);
        }
        let Some(result) = tasks.join_next_with_id().await else {
            break;
        };
        let (task, models) = match result {
            Ok(value) => value,
            Err(error) => (
                error.id(),
                Models::Unavailable("模型列表查询任务中断".into()),
            ),
        };
        if let Some(id) = ids.remove(&task) {
            update(id, models);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{FakeHttp, HttpResponse, TransportError};
    use crate::resources::connections::{ConnectionConfig, InferenceBinding};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("usage-catalog-{}", uuid::Uuid::new_v4())))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn response(code: u16, body: &str) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status: code,
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
        })
    }
    fn query(base: &str) -> ModelQuery {
        ModelQuery::new("provider-a".into(), base, "synthetic-api-key".into()).unwrap()
    }
    fn config() -> AppConfig {
        AppConfig {
            resources: vec![ConnectionConfig {
                id: "relay".into(),
                label: Some("Relay".into()),
                service: "new_api_account".into(),
                base_url: "https://billing.invalid".into(),
                credential_env: "BILLING_ONLY".into(),
                revision: String::new(),
                options: json!({"user_id": 1}),
                profile_ids: Vec::new(),
                inference: Some(InferenceBinding {
                    base_url: "https://relay.invalid/proxy/v1".into(),
                    credential_env: "INFERENCE_ONLY".into(),
                    performance_instance: None,
                }),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn performance_only_uses_an_explicit_instance_and_factory_source_without_creating_home() {
        let f = Fixture::new();
        let service = CatalogService::new(
            f.0.join("accounts"),
            Arc::new(FakeHttp::new()),
            Arc::new(|_| Err("synthetic-unset".into())),
        );
        let mut config = config();
        let unbound = service.load(&config).unwrap();
        assert!(unbound.catalog.providers[0].performance.is_none());
        let resource_identity = config.resources[0].binding();
        config.instances.push(crate::config::DataInstance {
            id: "work-claude".into(),
            provider: "claude".into(),
            path: f.0.join("claude-home"),
        });
        config.resources[0]
            .inference
            .as_mut()
            .unwrap()
            .performance_instance = Some("work-claude".into());
        let plan = service.load(&config).unwrap();
        assert_eq!(config.resources[0].binding(), resource_identity);
        let binding = plan.catalog.providers[0].performance.as_ref().unwrap();
        assert_eq!(binding.provider, "resource:relay");
        assert_eq!(binding.source.id, "claude_log_v1");
        assert_eq!(binding.home, f.0.join("claude-home"));
        assert!(
            !binding.home.exists(),
            "binding factories must do no filesystem writes"
        );
        config.resources[0]
            .inference
            .as_mut()
            .unwrap()
            .performance_instance = Some("missing".into());
        assert!(service.load(&config).is_err());
    }

    #[tokio::test]
    async fn refresh_reuses_models_and_balances_only_for_unchanged_query_identities() {
        let f = Fixture::new();
        let http = Arc::new(FakeHttp::new());
        http.route(
            "https://relay.invalid/proxy/v1/models",
            response(200, r#"{"data":[{"id":"cached-model"}]}"#),
        );
        http.route(
            "https://billing.invalid/api/user/self",
            response(
                200,
                r#"{"success":true,"data":{"id":1,"quota":2500000,"used_quota":10}}"#,
            ),
        );
        let key = Arc::new(Mutex::new("synthetic-inference".to_string()));
        let current_key = key.clone();
        let service = CatalogService::new(
            f.0.clone(),
            http.clone(),
            Arc::new(move |reference| {
                Ok(if reference == "env:INFERENCE_ONLY" {
                    current_key.lock().unwrap().clone()
                } else {
                    "synthetic-billing".into()
                })
            }),
        );
        let mut config = config();
        let mut initial = service.load(&config).unwrap();
        collect_models(http.clone(), initial.queries, |id, models| {
            assert!(initial.catalog.apply_models(&id, models));
        })
        .await;
        let target = initial.resources[0]
            .binding()
            .cache_key(initial.resources[0].service());
        let snapshot = initial.resources[0].read().await;
        assert_eq!(snapshot.state, crate::resources::ResourceState::Ready);
        assert!(initial.catalog.apply_resource(&target, snapshot.clone()));

        config.resources[0].label = Some("Renamed provider".into());
        let mut automatic = service.load(&config).unwrap();
        automatic.reuse(&initial.catalog, false);
        assert!(automatic.queries.is_empty());
        assert_eq!(automatic.resources.len(), 1);
        assert_eq!(automatic.catalog.providers[0].name, "Renamed provider");
        assert_eq!(
            automatic.catalog.providers[0].models,
            Models::Ready(vec!["cached-model".into()])
        );
        assert!(
            matches!(&automatic.catalog.providers[0].usage.sources[0].state,
            QueryState::Result(result) if result.metrics == snapshot.metrics
                && result.queried_at == snapshot.queried_at)
        );
        let updated = automatic.resources[0].read().await;
        assert!(automatic.catalog.apply_resource(&target, updated.clone()));
        assert!(!automatic.catalog.apply_resource(&target, updated));
        assert_eq!(
            http.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.url.ends_with("/models"))
                .count(),
            1
        );
        assert_eq!(
            http.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.url.ends_with("/self"))
                .count(),
            2
        );

        let mut manual = service.load(&config).unwrap();
        manual.reuse(&initial.catalog, true);
        assert_eq!(manual.queries.len(), 1);
        assert_eq!(
            manual.catalog.providers[0].models,
            initial.catalog.providers[0].models
        );
        let id = manual.catalog.providers[0].id.clone();
        assert!(
            manual
                .catalog
                .apply_models(&id, Models::Unavailable("query failed".into()))
        );
        assert!(
            !manual
                .catalog
                .apply_models(&id, Models::Ready(vec!["duplicate".into()]))
        );
        let mut failed = snapshot.clone();
        failed.state = crate::resources::ResourceState::TransientFailure;
        failed.metrics.clear();
        assert!(manual.catalog.apply_resource(&target, failed));
        assert!(
            matches!(&manual.catalog.providers[0].usage.sources[0].state,
            QueryState::Result(result) if result.metrics.is_empty())
        );

        *key.lock().unwrap() = "rotated-inference".into();
        let mut rotated = service.load(&config).unwrap();
        rotated.reuse(&initial.catalog, false);
        assert_eq!(rotated.queries.len(), 1);
        assert_eq!(rotated.catalog.providers[0].models, Models::Loading);
        assert!(!format!("{:?}", rotated.catalog).contains("rotated-inference"));
        *key.lock().unwrap() = "synthetic-inference".into();

        config.resources[0].inference.as_mut().unwrap().base_url = "https://new.invalid/v1".into();
        let mut changed = service.load(&config).unwrap();
        changed.reuse(&initial.catalog, false);
        assert_eq!(changed.queries.len(), 1);
        assert_eq!(changed.catalog.providers[0].models, Models::Loading);
        assert!(matches!(
            changed.catalog.providers[0].usage.sources[0].state,
            QueryState::Loading
        ));
        assert!(!changed.catalog.apply_resource(&target, snapshot));
    }

    #[tokio::test]
    async fn configured_inference_uses_only_inference_key_and_keeps_empty_lists_distinct() {
        let f = Fixture::new();
        let http = Arc::new(FakeHttp::new());
        http.route(
            "https://relay.invalid/proxy/v1/models",
            response(200, r#"{"object":"list","data":[]}"#),
        );
        let service = CatalogService::new(
            f.0.clone(),
            http.clone(),
            Arc::new(|reference| {
                assert_eq!(reference, "env:INFERENCE_ONLY");
                Ok("synthetic-inference".into())
            }),
        );
        let mut plan = service.load(&config()).unwrap();
        assert!(!f.0.exists());
        assert_eq!(plan.catalog.providers[0].kind, ProviderKind::CompatibleApi);
        collect_models(http.clone(), plan.queries, |id, models| {
            assert!(plan.catalog.apply_models(&id, models));
        })
        .await;
        assert_eq!(plan.catalog.providers[0].models, Models::Ready(Vec::new()));
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "GET");
        assert!(calls[0].body.is_none());
        assert_eq!(
            calls[0].headers,
            vec![("Authorization".into(), "Bearer synthetic-inference".into())]
        );
        assert!(!format!("{:?}", plan.catalog).contains("synthetic-inference"));
    }

    #[tokio::test]
    async fn managed_catalog_is_independent_of_runs_and_pins_each_accounts_saved_key() {
        struct KeyHttp;
        #[async_trait]
        impl HttpClient for KeyHttp {
            async fn request(
                &self,
                method: &str,
                url: &str,
                headers: &[(&str, String)],
                body: Option<Vec<u8>>,
            ) -> Result<HttpResponse, TransportError> {
                assert_eq!(method, "GET");
                assert_eq!(url, "https://relay.invalid/v1/models");
                assert!(body.is_none());
                let model = match headers[0].1.as_str() {
                    "Bearer synthetic-a" => "model-for-a",
                    "Bearer synthetic-b" => "model-for-b",
                    other => panic!("unexpected fixture credential: {other}"),
                };
                response(
                    200,
                    &json!({"data":[{"id":model},{"id":model}]}).to_string(),
                )
            }
        }
        let f = Fixture::new();
        let mut manager = Manager::open(f.0.clone()).unwrap();
        let text = crate::codex_manager::config::new_config(
            "https://relay.invalid/v1",
            "configured-only",
            None,
        )
        .unwrap();
        let a = manager.add("A", &text, "synthetic-a").unwrap();
        let b = manager.add("B", &text, "synthetic-b").unwrap();
        // Inventory/model discovery must not need a launch or even a runs table.
        rusqlite::Connection::open(f.0.join("manager.sqlite3"))
            .unwrap()
            .execute_batch("DROP TABLE run_contexts; DROP TABLE runs;")
            .unwrap();
        let service = CatalogService::new(
            f.0.clone(),
            Arc::new(KeyHttp),
            Arc::new(|_| panic!("unexpected env lookup")),
        );
        let mut plan = service.load(&AppConfig::default()).unwrap();
        manager
            .update(&a.id, a.revision, &text, Some("rotated-after-plan"), None)
            .unwrap();
        collect_models(service.http.clone(), plan.queries, |id, models| {
            plan.catalog.apply_models(&id, models);
        })
        .await;
        for (id, model) in [(a.id, "model-for-a"), (b.id, "model-for-b")] {
            let provider = plan.catalog.providers.iter().find(|p| p.id == id).unwrap();
            assert_eq!(provider.models, Models::Ready(vec![model.into()]));
            assert_eq!(
                provider.configured_model.as_deref(),
                Some("configured-only")
            );
        }
        assert!(!format!("{:?}", plan.catalog).contains("synthetic-"));
    }

    #[tokio::test]
    async fn single_provider_models_match_the_board_query_for_saved_and_typed_endpoints() {
        struct KeyHttp;
        #[async_trait]
        impl HttpClient for KeyHttp {
            async fn request(
                &self,
                _method: &str,
                url: &str,
                headers: &[(&str, String)],
                _body: Option<Vec<u8>>,
            ) -> Result<HttpResponse, TransportError> {
                let model = match (url, headers[0].1.as_str()) {
                    ("https://relay.invalid/v1/models", "Bearer synthetic-a") => "model-for-a",
                    ("https://typed.invalid/v1/models", "Bearer typed-key") => "typed-model",
                    other => panic!("unexpected fixture request: {other:?}"),
                };
                response(200, &json!({"data":[{"id":model}]}).to_string())
            }
        }
        let f = Fixture::new();
        let mut manager = Manager::open(f.0.clone()).unwrap();
        let text = crate::codex_manager::config::new_config(
            "https://relay.invalid/v1",
            "configured-only",
            None,
        )
        .unwrap();
        let a = manager.add("A", &text, "synthetic-a").unwrap();
        let plugin_text = crate::codex_manager::config::new_config(
            &crate::plugins::account_endpoint(&uuid::Uuid::new_v4().to_string()),
            "model",
            None,
        )
        .unwrap();
        let orphan = manager.add("Plugin", &plugin_text, "inference").unwrap();
        let service = Arc::new(
            CatalogService::new(
                f.0.clone(),
                Arc::new(KeyHttp),
                Arc::new(|_| panic!("unexpected env lookup")),
            )
            .with_plugin_root(f.0.join("plugins")),
        );
        let models = |source| service.clone().models(source);
        assert_eq!(
            models(ModelSource::Saved(a.id)).await,
            Models::Ready(vec!["model-for-a".into()])
        );
        assert_eq!(
            models(ModelSource::Endpoint {
                base: "https://typed.invalid/v1".into(),
                key: "typed-key".into(),
            })
            .await,
            Models::Ready(vec!["typed-model".into()])
        );
        assert_eq!(
            models(ModelSource::Saved(orphan.id)).await,
            unavailable_plugin()
        );
        assert!(matches!(
            models(ModelSource::Saved("missing".into())).await,
            Models::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn native_anthropic_lists_all_pages_with_its_own_authentication() {
        let http = FakeHttp::new();
        http.route(
            "https://api.anthropic.com/v1/models?after_id=first",
            response(
                200,
                r#"{"data":[{"id":"second"}],"has_more":false,"last_id":"second"}"#,
            ),
        );
        http.route(
            "https://api.anthropic.com/v1/models",
            response(
                200,
                r#"{"data":[{"id":"first"}],"has_more":true,"last_id":"first"}"#,
            ),
        );
        assert_eq!(
            query("https://api.anthropic.com").read(&http).await,
            Models::Ready(vec!["first".into(), "second".into()])
        );
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        for call in calls.iter() {
            assert!(
                call.headers
                    .contains(&("x-api-key".into(), "synthetic-api-key".into()))
            );
            assert!(
                call.headers
                    .contains(&("anthropic-version".into(), "2023-06-01".into()))
            );
            assert!(!call.headers.iter().any(|(key, _)| key == "Authorization"));
        }
    }

    #[tokio::test]
    async fn failures_never_echo_response_secrets_or_return_partial_models_as_complete() {
        for (code, body, expected) in [
            (401, r#"{"error":"synthetic-api-key"}"#, "认证失败"),
            (404, "synthetic-api-key", "未提供模型列表接口"),
            (429, "synthetic-api-key", "限流"),
            (200, r#"{"error":"synthetic-api-key"}"#, "格式无效"),
            (
                200,
                r#"{"data":[{"id":"first"}],"has_more":true,"last_id":"loop"}"#,
                "游标重复",
            ),
        ] {
            let http = FakeHttp::new();
            http.route("https://api.openai.com/v1/models", response(code, body));
            let Models::Unavailable(message) = query("https://api.openai.com/v1").read(&http).await
            else {
                panic!("accepted malformed or partial list");
            };
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("synthetic-api-key"));
        }
        for base in [
            "https://user:key@relay.invalid/v1",
            "https://relay.invalid/v1?key=secret",
            "http://relay.invalid/v1",
        ] {
            assert!(ModelQuery::new("a".into(), base, "fixture-key".into()).is_err());
        }
        assert_eq!(kind("https://api.openai.com/v1"), ProviderKind::OfficialApi);
        assert_eq!(
            kind("https://api.openai.com.attacker.invalid/v1"),
            ProviderKind::CompatibleApi
        );
    }

    fn oauth_profile(
        manager: &mut Manager,
        root: &std::path::Path,
        workspace: &str,
        version: &str,
    ) -> (crate::codex_manager::Profile, PathBuf, String) {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let claims = json!({"sub":"fixture-user","email":"fixture@example.test",
            "https://api.openai.com/auth":{"chatgpt_account_id":workspace,"chatgpt_user_id":"fixture-user","chatgpt_plan_type":"plus"}});
        let jwt = format!("e30.{}.fixture", URL_SAFE_NO_PAD.encode(claims.to_string()));
        let source = root.join(format!("{workspace}.json"));
        std::fs::write(
            &source,
            json!({"auth_mode":"chatgpt","tokens":{"id_token":jwt,
            "access_token":jwt,"refresh_token":"synthetic-refresh","account_id":workspace},
            "last_refresh":"2026-10-17T00:00:00Z"})
            .to_string(),
        )
        .unwrap();
        let profile = manager
            .import_oauth(Some(workspace), None, &source, None)
            .unwrap();
        let home = manager.home(&profile.id, profile.revision).unwrap();
        std::fs::write(
            home.join("models_cache.json"),
            json!({"client_version":version}).to_string(),
        )
        .unwrap();
        (profile, home, jwt)
    }

    #[tokio::test]
    async fn oauth_reset_uses_selected_account_even_with_explicit_billing_resources() {
        let f = Fixture::new();
        let mut manager = Manager::open(f.0.clone()).unwrap();
        let (first, _, first_token) = oauth_profile(&mut manager, &f.0, "reset-first", "999.0.0");
        let (second, _, second_token) =
            oauth_profile(&mut manager, &f.0, "reset-second", "999.0.0");
        let http = Arc::new(FakeHttp::new());
        let reset_url = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
        http.route(reset_url, response(200, r#"{"available_count":2}"#));
        http.route(format!("{reset_url}/consume"), response(200, "{}"));
        let mut config = config();
        config.resources[0].profile_ids = vec![second.id.clone()];
        let plan = CatalogService::new(
            f.0.clone(),
            http.clone(),
            Arc::new(|_| Ok("synthetic-billing-token".into())),
        )
        .load(&config)
        .unwrap();
        assert!(http.calls.lock().unwrap().is_empty());
        for id in [&first.id, &second.id] {
            assert!(
                plan.catalog
                    .providers
                    .iter()
                    .find(|p| &p.id == id)
                    .unwrap()
                    .usage
                    .reset
                    .is_some()
            );
        }
        let selected = plan
            .catalog
            .providers
            .iter()
            .find(|p| p.id == second.id)
            .unwrap();
        assert_eq!(
            selected.usage.sources.len(),
            2,
            "billing must not remove subscription/reset"
        );
        let provider = selected.usage.reset.as_ref().unwrap().provider();
        let reset = provider.reset().unwrap();
        assert_eq!(
            reset
                .preview_reset()
                .await
                .unwrap()
                .account_label
                .as_deref(),
            Some("reset-second")
        );
        reset.consume_reset().await.unwrap();
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 3, "preview, pre-submit check and one consume");
        assert_eq!(calls.iter().filter(|call| call.method == "POST").count(), 1);
        for call in calls.iter() {
            assert!(
                call.headers
                    .contains(&("Authorization".into(), format!("Bearer {second_token}")))
            );
            assert!(call.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("chatgpt-account-id") && value == "reset-second"
            }));
            assert!(
                !call
                    .headers
                    .iter()
                    .any(|(_, value)| value.contains(&first_token)
                        || value.contains("synthetic-billing-token"))
            );
        }
    }

    #[tokio::test]
    async fn oauth_inventory_never_queries_api_models_with_chatgpt_tokens() {
        let f = Fixture::new();
        let mut manager = Manager::open(f.0.clone()).unwrap();
        let (_, home, jwt) = oauth_profile(&mut manager, &f.0, "fixture-workspace", "999.0.0");
        let http = Arc::new(FakeHttp::new());
        http.route("https://chatgpt.com/backend-api/codex/models?client_version=999.0.0", response(200, r#"{"models":[{"slug":"plan-model","visibility":"list"},{"slug":"internal","visibility":"hide"}]}"#));
        http.route("https://chatgpt.com/backend-api/wham/usage",response(200,r#"{"rate_limit":{"primary_window":{"used_percent":25,"limit_window_seconds":18000}}}"#));
        let service = CatalogService::new(
            f.0.clone(),
            http.clone(),
            Arc::new(|_| panic!("OAuth tried API credentials")),
        );
        let mut plan = service.load(&AppConfig::default()).unwrap();
        assert_eq!(plan.queries.len(), 1);
        assert!(http.calls.lock().unwrap().is_empty());
        assert_eq!(plan.catalog.providers[0].kind, ProviderKind::Subscription);
        collect_models(
            http.clone(),
            std::mem::take(&mut plan.queries),
            |id, models| {
                assert!(plan.catalog.apply_models(&id, models));
            },
        )
        .await;
        assert_eq!(
            plan.catalog.providers[0].models,
            Models::Ready(vec!["plan-model".into()])
        );
        assert_eq!(plan.resources.len(), 1);
        let usage = plan.resources[0].read().await;
        assert_eq!(usage.state, crate::resources::ResourceState::Ready);
        assert!(usage.metrics.iter().any(|m|matches!(m,crate::resources::ResourceMetric::QuotaWindow{used_percent,..} if *used_percent==25.0)));
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].url,
            "https://chatgpt.com/backend-api/codex/models?client_version=999.0.0"
        );
        assert!(
            calls[0]
                .headers
                .contains(&("Authorization".into(), format!("Bearer {jwt}")))
        );
        assert!(
            calls[0]
                .headers
                .contains(&("version".into(), "999.0.0".into()))
        );
        assert!(
            calls[0]
                .headers
                .contains(&("ChatGPT-Account-Id".into(), "fixture-workspace".into()))
        );
        drop(calls);
        let mut auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join("auth.json")).unwrap()).unwrap();
        auth["tokens"]["account_id"] = json!("different-workspace");
        std::fs::write(home.join("auth.json"), auth.to_string()).unwrap();
        let rejected = service.load(&AppConfig::default()).unwrap();
        assert!(rejected.queries.is_empty());
        assert!(
            matches!(&rejected.catalog.providers[0].models, Models::Unavailable(message) if message.contains("重新登录"))
        );
        assert_eq!(http.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn oauth_models_and_cache_refresh_follow_each_account_and_rotated_token() {
        let f = Fixture::new();
        let mut manager = Manager::open(f.0.clone()).unwrap();
        let (a, a_home, a_token) = oauth_profile(&mut manager, &f.0, "workspace-a", "999.0.1");
        let (b, _, b_token) = oauth_profile(&mut manager, &f.0, "workspace-b", "999.0.2");
        let http = Arc::new(FakeHttp::new());
        for (version, model) in [("999.0.1", "a-plan-model"), ("999.0.2", "b-plan-model")] {
            http.route(
                format!("https://chatgpt.com/backend-api/codex/models?client_version={version}"),
                response(200, &json!({"models":[{"slug":model}]}).to_string()),
            );
        }
        let service = CatalogService::new(
            f.0.clone(),
            http.clone(),
            Arc::new(|_| panic!("OAuth used API credentials")),
        );
        let mut plan = service.load(&AppConfig::default()).unwrap();
        collect_models(
            http.clone(),
            std::mem::take(&mut plan.queries),
            |id, models| {
                assert!(plan.catalog.apply_models(&id, models));
            },
        )
        .await;
        for (profile, model, token, workspace) in [
            (&a, "a-plan-model", a_token, "workspace-a"),
            (&b, "b-plan-model", b_token, "workspace-b"),
        ] {
            assert_eq!(
                plan.catalog
                    .providers
                    .iter()
                    .find(|p| p.id == profile.id)
                    .unwrap()
                    .models,
                Models::Ready(vec![model.into()])
            );
            let calls = http.calls.lock().unwrap();
            let call = calls
                .iter()
                .find(|c| {
                    c.headers
                        .contains(&("ChatGPT-Account-Id".into(), workspace.into()))
                })
                .unwrap();
            assert!(
                call.headers
                    .contains(&("Authorization".into(), format!("Bearer {token}")))
            );
            assert!(
                call.url
                    .starts_with("https://chatgpt.com/backend-api/codex/models?")
            );
        }
        let mut stable = service.load(&AppConfig::default()).unwrap();
        stable.reuse(&plan.catalog, false);
        assert!(stable.queries.is_empty());
        assert_eq!(http.calls.lock().unwrap().len(), 2);

        let auth_path = a_home.join("auth.json");
        let mut auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&auth_path).unwrap()).unwrap();
        auth["tokens"]["access_token"] = json!("rotated-access-token");
        std::fs::write(auth_path, auth.to_string()).unwrap();
        let mut changed = service.load(&AppConfig::default()).unwrap();
        changed.reuse(&plan.catalog, false);
        assert_eq!(changed.queries.len(), 1);
        assert_eq!(changed.queries[0].id, a.id);
        collect_models(
            http.clone(),
            std::mem::take(&mut changed.queries),
            |id, models| {
                assert!(changed.catalog.apply_models(&id, models));
            },
        )
        .await;
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(
            calls[2]
                .headers
                .contains(&("Authorization".into(), "Bearer rotated-access-token".into()))
        );
        assert!(
            calls[2]
                .headers
                .contains(&("ChatGPT-Account-Id".into(), "workspace-a".into()))
        );
        assert_eq!(
            changed
                .catalog
                .providers
                .iter()
                .find(|p| p.id == b.id)
                .unwrap()
                .models,
            Models::Ready(vec!["b-plan-model".into()])
        );
    }

    #[tokio::test]
    async fn fast_and_panicking_queries_are_isolated_and_cancellation_drops_slow_queries() {
        struct ControlledHttp {
            active: Arc<AtomicUsize>,
        }
        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        #[async_trait]
        impl HttpClient for ControlledHttp {
            async fn request(
                &self,
                _: &str,
                url: &str,
                _: &[(&str, String)],
                _: Option<Vec<u8>>,
            ) -> Result<HttpResponse, TransportError> {
                if url.contains("panic.invalid") {
                    panic!("synthetic transport panic");
                }
                if url.contains("slow.invalid") {
                    self.active.fetch_add(1, Ordering::SeqCst);
                    let _guard = Guard(self.active.clone());
                    std::future::pending::<()>().await;
                }
                response(200, r#"{"data":[{"id":"available"}]}"#)
            }
        }
        let active = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(Mutex::new(Vec::new()));
        let output = results.clone();
        let http = Arc::new(ControlledHttp {
            active: active.clone(),
        });
        let queries = ["slow", "panic", "fast"]
            .into_iter()
            .map(|id| {
                ModelQuery::new(
                    id.into(),
                    &format!("https://{id}.invalid/v1"),
                    "synthetic-key".into(),
                )
                .unwrap()
            })
            .collect();
        let task = tokio::spawn(async move {
            collect_models(http, queries, |id, models| {
                output.lock().unwrap().push((id, models))
            })
            .await;
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if results.lock().unwrap().len() == 2 && active.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) > 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let results = results.lock().unwrap();
        assert!(
            results
                .iter()
                .any(|(id, models)| id == "fast" && matches!(models, Models::Ready(_)))
        );
        assert!(
            results
                .iter()
                .any(|(id, models)| id == "panic" && matches!(models, Models::Unavailable(_)))
        );
    }
}
