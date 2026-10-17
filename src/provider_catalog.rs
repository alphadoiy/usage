//! Provider inventory with independent model, quota and balance capabilities.
mod usage;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
pub use usage::{ProviderUsage, QueryState, UsageSource};

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
}
impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::OfficialApi => "官方 API · BYOK",
            Self::CompatibleApi => "兼容 API · BYOK",
            Self::Subscription => "官方订阅 · OAuth",
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

#[derive(Debug, Clone, Copy)]
enum Protocol {
    OpenAi,
    Anthropic,
}

// Credentials only live in query plans; never Debug/Serialize them or put them in UI state.
pub struct ModelQuery {
    id: String,
    url: reqwest::Url,
    key: String,
    protocol: Protocol,
}

fn endpoint(base: &str) -> Result<reqwest::Url, &'static str> {
    let mut url = reqwest::Url::parse(base).map_err(|_| "模型接口地址无效")?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("模型接口地址需为无凭据和查询参数的 HTTPS 地址（本机回环可用 HTTP）");
    }
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
        let headers = match self.protocol {
            Protocol::OpenAi => vec![("Authorization", format!("Bearer {}", self.key))],
            Protocol::Anthropic => vec![
                ("x-api-key", self.key.clone()),
                ("anthropic-version", "2023-06-01".into()),
            ],
        };
        let mut url = self.url.clone();
        let mut models = BTreeSet::new();
        let mut cursors = BTreeSet::new();
        for _ in 0..20 {
            let response = http
                .request("GET", url.as_str(), &headers, None)
                .await
                .map_err(|_| "模型列表网络请求失败")?;
            match response.status {
                200 => {}
                401 | 403 => return Err("模型列表认证失败或权限不足"),
                404 | 405 | 501 => return Err("此端点未提供模型列表接口"),
                429 => return Err("模型列表查询受到限流"),
                _ => return Err("模型列表接口暂不可用"),
            }
            if response.body.len() > 2 * 1024 * 1024 {
                return Err("模型列表响应过大");
            }
            let page: Response =
                serde_json::from_slice(&response.body).map_err(|_| "模型列表响应格式无效")?;
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
        }
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
        if let Some(manager) =
            Manager::open_read_only(self.root.clone()).map_err(|_| "读取 Provider 账号库失败")?
        {
            for profile in manager.list().map_err(|_| "读取 Provider 配置失败")? {
                let query = if profile.auth_mode == AuthMode::Byok {
                    manager
                        .model_catalog_query(&profile)
                        .map_err(|_| "无法读取此 Provider 的模型查询凭据".to_owned())
                } else {
                    Err("OAuth 订阅未接入模型列表接口".into())
                };
                let models = match query {
                    Ok(query) => {
                        plan.queries.push(query);
                        Models::Loading
                    }
                    Err(message) => Models::Unavailable(message),
                };
                let provider_kind = if profile.auth_mode == AuthMode::Oauth {
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
                    usage: ProviderUsage::default(),
                });
                self.add_profile_usage(&mut plan, &manager, &profile, config);
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
                usage: ProviderUsage::default(),
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
                }),
            }],
            ..Default::default()
        }
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

    #[tokio::test]
    async fn oauth_inventory_never_queries_api_models_with_chatgpt_tokens() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let f = Fixture::new();
        let mut manager = Manager::open(f.0.clone()).unwrap();
        let claims = json!({"sub":"fixture-user","email":"fixture@example.test","https://api.openai.com/auth":{"chatgpt_account_id":"fixture-workspace","chatgpt_user_id":"fixture-user","chatgpt_plan_type":"plus"}});
        let jwt = format!("e30.{}.fixture", URL_SAFE_NO_PAD.encode(claims.to_string()));
        let source = f.0.join("oauth-source.json");
        std::fs::write(&source,json!({"auth_mode":"chatgpt","tokens":{"id_token":jwt,"access_token":jwt,"refresh_token":"synthetic-refresh","account_id":"fixture-workspace"},"last_refresh":"2026-10-17T00:00:00Z"}).to_string()).unwrap();
        manager
            .import_oauth(Some("Personal"), None, &source, None)
            .unwrap();
        let http = Arc::new(FakeHttp::new());
        http.route("https://chatgpt.com/backend-api/wham/usage",response(200,r#"{"rate_limit":{"primary_window":{"used_percent":25,"limit_window_seconds":18000}}}"#));
        let plan = CatalogService::new(
            f.0.clone(),
            http.clone(),
            Arc::new(|_| panic!("OAuth tried API credentials")),
        )
        .load(&AppConfig::default())
        .unwrap();
        assert!(plan.queries.is_empty());
        assert!(http.calls.lock().unwrap().is_empty());
        assert_eq!(plan.catalog.providers[0].kind, ProviderKind::Subscription);
        assert!(
            matches!(&plan.catalog.providers[0].models,Models::Unavailable(reason) if reason.contains("OAuth"))
        );
        assert_eq!(plan.resources.len(), 1);
        let usage = plan.resources[0].read().await;
        assert_eq!(usage.state, crate::resources::ResourceState::Ready);
        assert!(usage.metrics.iter().any(|m|matches!(m,crate::resources::ResourceMetric::QuotaWindow{used_percent,..} if *used_percent==25.0)));
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0]
                .headers
                .contains(&("ChatGPT-Account-Id".into(), "fixture-workspace".into()))
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
