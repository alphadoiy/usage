//! Typed provider resources; this path never needs a Run or executes imported JavaScript.
use std::sync::Arc;

use chrono::Utc;

use super::{Catalog, CatalogService, ModelQuery, Plan};
use crate::codex_manager::{AuthMode, Manager, Profile, ccswitch};
use crate::config::AppConfig;
use crate::resources::connections::{ConnectionConfig, ResourceService};
use crate::resources::{CredentialResolver, ResourceBinding, ResourceReader, ResourceSnapshot};

/// Account-scoped reset capability. Holds a credential reader, never a captured token.
#[derive(Clone)]
pub struct ResetHandle(Arc<dyn crate::providers::UsageProvider>);
impl std::fmt::Debug for ResetHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResetHandle").finish_non_exhaustive()
    }
}
impl ResetHandle {
    pub(crate) fn new(provider: Arc<dyn crate::providers::UsageProvider>) -> Option<Self> {
        provider.reset().is_some().then_some(Self(provider))
    }
    pub(crate) fn provider(&self) -> Arc<dyn crate::providers::UsageProvider> {
        self.0.clone()
    }
}

#[derive(Debug, Clone)]
pub enum QueryState {
    Loading,
    Unavailable(String),
    Result(ResourceSnapshot),
}
#[derive(Debug, Clone)]
pub struct UsageSource {
    pub target: String,
    pub label: String,
    pub service: String,
    pub binding: ResourceBinding,
    pub state: QueryState,
}
#[derive(Debug, Clone, Default)]
pub struct ProviderUsage {
    pub sources: Vec<UsageSource>,
    pub hint: Option<String>,
    pub reset: Option<ResetHandle>,
}

impl Catalog {
    pub(crate) fn apply_reset_credits(&mut self, target: &str, available: u32) {
        for source in self.providers.iter_mut().flat_map(|p| &mut p.usage.sources) {
            if source.target == target
                && let QueryState::Result(snapshot) = &mut source.state
            {
                snapshot.metrics.retain(|metric| {
                    !matches!(
                        metric,
                        crate::resources::ResourceMetric::ResetCredits { .. }
                    )
                });
                snapshot
                    .metrics
                    .push(crate::resources::ResourceMetric::ResetCredits { available });
            }
        }
    }
    pub fn apply_resource(&mut self, target: &str, snapshot: ResourceSnapshot) -> bool {
        let refreshing = self.refreshing_resources.remove(target);
        let mut applied = false;
        for source in self.providers.iter_mut().flat_map(|p| &mut p.usage.sources) {
            if source.target != target
                || (!matches!(source.state, QueryState::Loading) && !refreshing)
            {
                continue;
            }
            source.state =
                if source.binding == snapshot.binding && source.service == snapshot.service {
                    QueryState::Result(snapshot.clone())
                } else {
                    QueryState::Unavailable("用量响应身份不匹配".into())
                };
            applied = true;
        }
        applied
    }
}

impl CatalogService {
    pub(super) fn usage_service(&self) -> ResourceService {
        ResourceService::new(
            crate::resources::builtin_registry(),
            self.http.clone(),
            self.credentials.clone(),
            Arc::new(Utc::now),
        )
    }

    pub(super) fn add_configured_usage(&self, plan: &mut Plan, config: &AppConfig) {
        let service = self.usage_service();
        for connection in &config.resources {
            let ids: Vec<_> = plan
                .catalog
                .providers
                .iter()
                .filter(|p| {
                    connection.profile_ids.contains(&p.id)
                        || p.id == format!("resource:{}", connection.id)
                })
                .map(|p| p.id.clone())
                .collect();
            if ids.is_empty() {
                continue;
            }
            let result = service
                .prepare(connection)
                .map_err(|_| "用量资源配置无效或服务尚未支持".into());
            for id in ids {
                attach(plan, &id, connection, result.clone());
            }
        }
    }

    pub(super) fn add_profile_usage(
        &self,
        plan: &mut Plan,
        manager: &Manager,
        profile: &Profile,
        config: &AppConfig,
    ) {
        if profile.auth_mode == AuthMode::Oauth {
            let result = manager
                .home(&profile.id, profile.revision)
                .map_err(|_| "无法解析订阅账号目录".to_owned())
                .and_then(|home| {
                    let binding = ResourceBinding {
                        account: profile.id.clone(),
                        revision: profile.revision.to_string(),
                    };
                    let ctx = crate::providers::ProviderContext {
                        claude_dir: home.clone(),
                        codex_dir: home.clone(),
                        grok_dir: home,
                        http: self.http.clone(),
                        keychain: Arc::new(|_, _| None),
                        now: Arc::new(Utc::now),
                    };
                    crate::resources::builtin_registry()
                        .create(
                            "chatgpt_subscription",
                            crate::resources::ResourceContext {
                                binding,
                                connection: crate::resources::ResourceConnection::LocalSubscription(
                                    Arc::new(ctx),
                                ),
                            },
                        )
                        .map_err(|_| "无法创建订阅用量查询".into())
                });
            let binding = ResourceBinding {
                account: profile.id.clone(),
                revision: profile.revision.to_string(),
            };
            let source = UsageSource {
                target: binding.cache_key("chatgpt_subscription"),
                label: "官方订阅额度".into(),
                service: "chatgpt_subscription".into(),
                binding,
                state: match &result {
                    Ok(_) => QueryState::Loading,
                    Err(e) => QueryState::Unavailable(e.clone()),
                },
            };
            let reset = result.as_ref().ok().and_then(|reader| {
                reader.legacy_usage()?.reset()?;
                ResetHandle::new(Arc::new(crate::resources::LegacyUsageAdapter::new(
                    reader.clone(),
                )))
            });
            if let Ok(reader) = result {
                plan.resources.push(reader);
            }
            let usage = &mut plan
                .catalog
                .providers
                .iter_mut()
                .find(|p| p.id == profile.id)
                .unwrap()
                .usage;
            usage.reset = reset;
            usage.sources.push(source);
            return;
        }
        // Explicit Usage configuration wins over imported CC Switch parameters.
        // OAuth's own subscription/reset capability remains independent of extra billing sources.
        if config
            .resources
            .iter()
            .any(|c| c.profile_ids.contains(&profile.id))
        {
            return;
        }
        let result = (|| {
            let Some((path, provider)) = manager
                .usage_source(&profile.id)
                .map_err(|_| "无法读取 Provider 的来源".to_owned())?
            else {
                return Ok(None);
            };
            let query = manager
                .model_catalog_query(profile)
                .map_err(|_| "无法读取 Provider 凭据".to_owned())?;
            let settings = ccswitch::read_usage(&path, &provider, &profile.base_url, &query.key)
                .map_err(|e| e.to_string())?;
            let Some(settings) = settings else {
                return Ok(None);
            };
            if !settings.enabled {
                return Err("CC Switch 中的用量查询未启用".into());
            }
            let prepared = imported_connection(profile, settings, &query)?;
            let Some((connection, key)) = prepared else {
                return Err("此 CC Switch 用量模板尚未接入，请配置 resources 查询服务".into());
            };
            let credentials: Arc<CredentialResolver> = Arc::new(move |_| Ok(key.clone()));
            let reader = self
                .usage_service()
                .prepare_with_credentials(&connection, credentials)
                .map_err(|_| "CC Switch 用量模板的连接参数不完整或无效".to_owned())?;
            Ok(Some((connection, reader)))
        })();
        match result {
            Ok(Some((connection, reader))) => attach(plan, &profile.id, &connection, Ok(reader)),
            Ok(None) => {}
            Err(message) => {
                plan.catalog
                    .providers
                    .iter_mut()
                    .find(|p| p.id == profile.id)
                    .unwrap()
                    .usage
                    .hint = Some(message)
            }
        }
    }
}

fn attach(
    plan: &mut Plan,
    id: &str,
    config: &ConnectionConfig,
    reader: Result<Arc<dyn ResourceReader>, String>,
) {
    let binding = config.binding();
    let target = binding.cache_key(&config.service);
    let state = match reader {
        Ok(reader) => {
            if !plan
                .resources
                .iter()
                .any(|r| r.binding().cache_key(r.service()) == target)
            {
                plan.resources.push(reader);
            }
            QueryState::Loading
        }
        Err(message) => QueryState::Unavailable(message),
    };
    plan.catalog
        .providers
        .iter_mut()
        .find(|p| p.id == id)
        .unwrap()
        .usage
        .sources
        .push(UsageSource {
            target,
            label: config.label.clone().unwrap_or_else(|| config.id.clone()),
            service: config.service.clone(),
            binding,
            state,
        });
}

fn imported_connection(
    profile: &Profile,
    settings: ccswitch::UsageSettings,
    query: &ModelQuery,
) -> Result<Option<(ConnectionConfig, String)>, String> {
    let base = settings
        .base_url
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&profile.base_url)
        .trim_end_matches('/');
    let api_key = settings
        .api_key
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| query.key.clone());
    let template = settings.template_type.as_deref().or_else(|| {
        if settings.code.contains("{{baseUrl}}/api/usage/token")
            && settings.code.contains("total_available")
            && settings.code.contains("total_used")
        {
            Some("new_api_token")
        } else if settings.access_token.is_some() || settings.user_id.is_some() {
            Some("newapi")
        } else {
            None
        }
    });
    let (service, base_url, key, options) = match template {
        Some("newapi") => {
            let key = settings
                .access_token
                .filter(|s| !s.trim().is_empty())
                .ok_or("New API 账号余额需要 Access Token")?;
            let id = settings
                .user_id
                .as_deref()
                .and_then(|id| id.parse::<u64>().ok())
                .filter(|id| *id > 0)
                .ok_or("New API 账号余额需要有效用户 ID")?;
            // Keep raw quota: imported JavaScript does not establish the site's monetary conversion.
            (
                "new_api_account",
                site_root(base)?,
                key,
                serde_json::json!({"user_id":id}),
            )
        }
        Some("new_api_token") => (
            "new_api_token",
            site_root(base)?,
            api_key,
            legacy_token_conversion(&settings.code),
        ),
        Some("sub2api") => (
            "sub2api_usage",
            site_root(base)?,
            api_key,
            serde_json::json!({}),
        ),
        Some("balance") => {
            let Some((service, base)) = crate::resources::balance_endpoint(base) else {
                return Ok(None);
            };
            (service, base, api_key, serde_json::json!({}))
        }
        _ => return Ok(None),
    };
    Ok(Some((
        ConnectionConfig {
            id: format!("ccswitch:{}", profile.id),
            label: Some("CC Switch 用量查询".into()),
            service: service.into(),
            base_url,
            credential_env: "USAGE_PINNED_USAGE_KEY".into(),
            revision: profile.revision.to_string(),
            options,
            profile_ids: Vec::new(),
            inference: None,
        },
        key,
    )))
}

fn site_root(base: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(base).map_err(|_| "用量查询地址无效")?;
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("用量查询地址不能含凭据或查询参数".into());
    }
    let path = url
        .path()
        .trim_end_matches('/')
        .strip_suffix("/v1")
        .unwrap_or_else(|| url.path().trim_end_matches('/'))
        .to_owned();
    url.set_path(&format!("{path}/"));
    Ok(url.to_string())
}

// Read only the familiar legacy token extractor's literal division and currency.
// No JS evaluation, generic expression parsing, or default quota/currency assumptions.
fn legacy_token_conversion(original: &str) -> serde_json::Value {
    let code = compact_js(original);
    let literal = |text: &str| -> Option<String> {
        let value = rust_decimal::Decimal::from_str_exact(text.trim()).ok()?;
        (value > rust_decimal::Decimal::ZERO).then(|| value.normalize().to_string())
    };
    let ratio = |field: &str| -> Option<String> {
        let marker = format!(".{field}");
        if code.matches(&marker).count() != 1 {
            return None;
        }
        let rest = code.split_once(&marker)?.1;
        let rest = rest
            .strip_prefix('/')
            .or_else(|| rest.strip_prefix("||0)/"))?;
        let divisor: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || ['.', '_'].contains(c))
            .collect();
        if divisor.is_empty() || !rest[divisor.len()..].starts_with([',', ';', ')', '}']) {
            return None;
        }
        if let Some(value) = literal(&divisor) {
            return Some(value);
        }
        // Legacy templates also use `var unit = 500000` and `(data.field || 0) / unit`.
        if code.matches(&format!("{divisor}=")).count() != 1
            || ["+=", "-=", "*=", "/=", "++", "--"]
                .iter()
                .any(|op| code.contains(&format!("{divisor}{op}")))
        {
            return None;
        }
        original.lines().find_map(|line| {
            let line = line.trim();
            let declaration = line
                .strip_prefix("const ")
                .or_else(|| line.strip_prefix("var "))
                .or_else(|| line.strip_prefix("let "))?;
            let (name, value) = declaration.split_once('=')?;
            if name.trim() != divisor {
                return None;
            }
            literal(value.split(';').next()?.split("//").next()?.trim())
        })
    };
    let Some(remaining) = ratio("total_available") else {
        return serde_json::json!({});
    };
    if ratio("total_used").as_deref() != Some(&remaining) {
        return serde_json::json!({});
    }
    let unit = [r#"unit:""#, "unit:'", r#""unit":""#, "'unit':'"]
        .into_iter()
        .find_map(|prefix| {
            let rest = code.split_once(prefix)?.1;
            let unit = rest.get(..3)?;
            (unit.bytes().all(|c| c.is_ascii_uppercase())
                && rest
                    .as_bytes()
                    .get(3)
                    .is_some_and(|c| *c == b'\'' || *c == b'"'))
            .then_some(unit)
        });
    match unit {
        Some(unit) => serde_json::json!({"quota_per_unit":remaining,"currency":unit}),
        None => serde_json::json!({}),
    }
}

fn compact_js(code: &str) -> String {
    let mut output = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut chars = code.chars().peekable();
    while let Some(ch) = chars.next() {
        if let Some(delimiter) = quote {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == delimiter {
                quote = None;
            }
        } else if ['\'', '"', '`'].contains(&ch) {
            quote = Some(ch);
            output.push(ch);
        } else if ch == '/' && chars.peek() == Some(&'/') {
            for next in chars.by_ref() {
                if next == '\n' {
                    break;
                }
            }
        } else if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(next) = chars.next() {
                if next == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
        } else if !ch.is_whitespace() {
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{FakeHttp, HttpResponse};
    use crate::resources::{Money, ResourceMetric, ResourceScope, ResourceState};
    use rusqlite::{Connection, params};
    use serde_json::json;

    struct Fixture {
        root: std::path::PathBuf,
        source: std::path::PathBuf,
        manager: Manager,
    }
    impl Fixture {
        fn new(script: serde_json::Value) -> Self {
            let root =
                std::env::temp_dir().join(format!("usage-provider-usage-{}", uuid::Uuid::new_v4()));
            let mut manager = Manager::open(root.join("manager")).unwrap();
            let source = root.join("source.sqlite3");
            let conn = Connection::open(&source).unwrap();
            conn.execute_batch("CREATE TABLE providers(id TEXT,app_type TEXT,name TEXT,settings_config TEXT,meta TEXT);").unwrap();
            let config = crate::codex_manager::config::new_config(
                "https://relay.invalid/gateway/v1",
                "fixture-model",
                None,
            )
            .unwrap();
            conn.execute(
                "INSERT INTO providers VALUES('source-a','codex','Work',?1,?2)",
                params![
                    json!({"config":config,"auth":{"OPENAI_API_KEY":"fixture-inference-key"}})
                        .to_string(),
                    json!({"usage_script":script}).to_string()
                ],
            )
            .unwrap();
            manager.import_ccswitch(&source, true, &[], false).unwrap();
            Self {
                root,
                source,
                manager,
            }
        }
        fn service(&self, http: Arc<FakeHttp>) -> CatalogService {
            CatalogService::new(
                self.manager.root().into(),
                http,
                Arc::new(|_| panic!("imported query read unrelated env credentials")),
            )
        }
        fn script(&self, script: serde_json::Value) {
            Connection::open(&self.source)
                .unwrap()
                .execute(
                    "UPDATE providers SET meta=?1",
                    [json!({"usage_script":script}).to_string()],
                )
                .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn reply(body: &str) -> Result<HttpResponse, crate::http::TransportError> {
        Ok(HttpResponse {
            status: 200,
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
        })
    }
    fn legacy_code() -> &'static str {
        r#"({request:{url:"{{baseUrl}}/api/usage/token",method:"GET",headers:{Authorization:"Bearer {{apiKey}}"}},extractor:function(response){return {used:response.data.total_used / 500000, remaining: response.data.total_available / 500000,unit:"USD"};}})"#
    }
    fn legacy_settings() -> serde_json::Value {
        json!({"enabled":true,"language":"javascript","baseUrl":"https://relay.invalid/gateway/","code":legacy_code()})
    }

    #[tokio::test]
    async fn legacy_ccswitch_token_usage_works_without_runs_and_preserves_explicit_conversion() {
        let f = Fixture::new(legacy_settings());
        Connection::open(f.manager.root().join("manager.sqlite3"))
            .unwrap()
            .execute_batch("DROP TABLE run_contexts; DROP TABLE runs;")
            .unwrap();
        let http = Arc::new(FakeHttp::new());
        http.route("https://relay.invalid/gateway/api/usage/token",reply(r#"{"code":true,"data":{"object":"token_usage","total_used":1000000,"total_available":2500000,"unlimited_quota":true}}"#));
        let mut plan = f.service(http.clone()).load(&AppConfig::default()).unwrap();
        assert_eq!(plan.resources.len(), 1);
        let snapshot = plan.resources[0].read().await;
        assert_eq!(snapshot.state, ResourceState::Ready);
        assert_eq!(snapshot.scope, ResourceScope::Token);
        assert!(snapshot.metrics.contains(&ResourceMetric::Allowance {
            remaining: Some("2500000".into()),
            used: Some("1000000".into()),
            unit: "new_api_quota".into(),
            unlimited: true,
        }));
        assert!(snapshot.metrics.contains(&ResourceMetric::Balance {
            remaining: Money {
                amount: "5".into(),
                currency: "USD".into()
            }
        }));
        let target = plan.resources[0]
            .binding()
            .cache_key(plan.resources[0].service());
        assert!(plan.catalog.apply_resource(&target, snapshot.clone()));
        assert!(!plan.catalog.apply_resource(&target, snapshot));
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].headers.contains(&(
            "Authorization".into(),
            "Bearer fixture-inference-key".into()
        )));
        assert!(!format!("{:?}", plan.catalog).contains("fixture-inference-key"));
    }

    #[tokio::test]
    async fn imported_account_template_uses_account_credentials_and_checks_returned_identity() {
        let f = Fixture::new(
            json!({"enabled":true,"templateType":"newapi","baseUrl":"https://relay.invalid/gateway/v1","accessToken":"fixture-account-key","userId":"42"}),
        );
        let http = Arc::new(FakeHttp::new());
        http.route(
            "https://relay.invalid/gateway/api/user/self",
            reply(r#"{"success":true,"data":{"id":43,"quota":100,"used_quota":1}}"#),
        );
        let plan = f.service(http.clone()).load(&AppConfig::default()).unwrap();
        assert_eq!(plan.resources.len(), 1);
        let snapshot = plan.resources[0].read().await;
        assert_eq!(snapshot.state, ResourceState::AuthenticationFailed);
        assert!(snapshot.metrics.is_empty());
        let calls = http.calls.lock().unwrap();
        assert!(
            calls[0]
                .headers
                .contains(&("Authorization".into(), "Bearer fixture-account-key".into()))
        );
        assert!(
            calls[0]
                .headers
                .contains(&("New-Api-User".into(), "42".into()))
        );
        assert!(!format!("{:?}", plan.catalog).contains("fixture-account-key"));
    }

    #[test]
    fn disabled_custom_and_changed_source_identities_are_not_queried() {
        let f = Fixture::new(legacy_settings());
        let service = f.service(Arc::new(FakeHttp::new()));
        for settings in [
            json!({"enabled":false,"code":legacy_code()}),
            json!({"enabled":true,"templateType":"custom","code":"throw new Error('must never run')"}),
        ] {
            f.script(settings);
            let plan = service.load(&AppConfig::default()).unwrap();
            assert!(plan.resources.is_empty());
            assert!(plan.catalog.providers[0].usage.hint.is_some());
        }
        f.script(legacy_settings());
        let conn = Connection::open(&f.source).unwrap();
        let raw: String = conn
            .query_row("SELECT settings_config FROM providers", [], |r| r.get(0))
            .unwrap();
        let mut data: serde_json::Value = serde_json::from_str(&raw).unwrap();
        data["auth"]["OPENAI_API_KEY"] = json!("other-inference-key");
        conn.execute(
            "UPDATE providers SET settings_config=?1",
            [data.to_string()],
        )
        .unwrap();
        let plan = service.load(&AppConfig::default()).unwrap();
        assert!(plan.resources.is_empty());
        assert!(
            plan.catalog.providers[0]
                .usage
                .hint
                .as_ref()
                .unwrap()
                .contains("身份已变化")
        );
    }

    #[tokio::test]
    async fn explicit_resources_override_imported_usage_and_shared_readers_are_queried_once() {
        let mut f = Fixture::new(legacy_settings());
        let profile = f.manager.list().unwrap().remove(0);
        let text = crate::codex_manager::config::new_config(
            "https://other.invalid/v1",
            "other-model",
            None,
        )
        .unwrap();
        let second = f.manager.add("Other", &text, "other-fixture-key").unwrap();
        let mut config = AppConfig::default();
        config.resources.push(ConnectionConfig {
            id: "shared-wallet".into(),
            label: Some("Explicit wallet".into()),
            service: "new_api_account".into(),
            base_url: "https://wallet.invalid".into(),
            credential_env: "ACCOUNT_KEY".into(),
            revision: String::new(),
            options: json!({"user_id":42,"quota_per_unit":"500000","currency":"CNY"}),
            profile_ids: vec![profile.id, second.id],
            inference: None,
        });
        let http = Arc::new(FakeHttp::new());
        http.route(
            "https://wallet.invalid/api/user/self",
            reply(r#"{"success":true,"data":{"id":42,"quota":1250000,"used_quota":10}}"#),
        );
        let service = CatalogService::new(
            f.manager.root().into(),
            http.clone(),
            Arc::new(|reference| {
                assert_eq!(reference, "env:ACCOUNT_KEY");
                Ok("explicit-account-key".into())
            }),
        );
        let mut plan = service.load(&config).unwrap();
        assert_eq!(plan.resources.len(), 1);
        assert_eq!(plan.catalog.providers.len(), 2);
        assert!(
            plan.catalog
                .providers
                .iter()
                .all(|p| p.usage.sources.len() == 1)
        );
        let reader = &plan.resources[0];
        let target = reader.binding().cache_key(reader.service());
        let result = reader.read().await;
        let mut mismatch = result.clone();
        mismatch.binding.account = "other-wallet".into();
        assert!(plan.catalog.apply_resource(&target, mismatch));
        assert!(
            matches!(&plan.catalog.providers[0].usage.sources[0].state,QueryState::Unavailable(message) if message.contains("身份不匹配"))
        );
        assert!(!plan.catalog.apply_resource(&target, result));
        assert_eq!(http.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn legacy_conversion_requires_matching_literal_divisions_and_currency() {
        assert_eq!(
            legacy_token_conversion(legacy_code()),
            json!({"quota_per_unit":"500000","currency":"USD"})
        );
        let alias = "var unit = 500000\nreturn { used: (data.total_used || 0) / unit, remaining: (data.total_available || 0) / unit, unit: 'USD' };";
        assert_eq!(
            legacy_token_conversion(alias),
            json!({"quota_per_unit":"500000","currency":"USD"})
        );
        assert_eq!(
            legacy_token_conversion(&legacy_code().replace("USD", "U S D")),
            json!({})
        );
        assert_eq!(
            legacy_token_conversion(&format!("{alias}\nunit *= 2;")),
            json!({})
        );
        for code in [
            legacy_code().replace("/ 500000", "/ ratio"),
            legacy_code().replacen("/ 500000", "/ 500000 * 2", 1),
            legacy_code().replace("USD", "points"),
            legacy_code().replace("total_available / 500000", "total_available / 1000"),
        ] {
            assert_eq!(legacy_token_conversion(&code), json!({}));
        }
    }
}
