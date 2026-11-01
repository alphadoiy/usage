//! External model supply. Magpie owns authentication, routing and request observation;
//! Usage owns the managed Agent profile and process lifecycle.
mod catalog;
mod cli;
mod resources;
pub use cli::{MagpieCommand, dispatch};
pub(crate) use resources::GatewayReader;

use crate::{
    codex_manager::{self, Manager, Profile},
    http::HttpClient,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::Path, sync::Arc};

pub type Result<T> = std::result::Result<T, String>;
pub const PROVIDER: &str = "magpie";
pub const LOCAL_KEY: &str = "magpie-usage";
pub const BUILTIN_ID: &str = "builtin:magpie";
pub const BUILTIN_NAME: &str = "Magpie（内置）";

/// Saved revisions remain addressable for existing sessions; the product has one Magpie entry.
pub(crate) fn default_profile(manager: &Manager) -> Result<Option<Profile>> {
    let profiles = manager.list().map_err(|e| e.to_string())?;
    Ok(profiles
        .iter()
        .find(|p| p.model_provider == PROVIDER && p.name == BUILTIN_NAME)
        .or_else(|| profiles.iter().find(|p| p.model_provider == PROVIDER))
        .cloned())
}

pub(crate) fn builtin_profile(manager: Option<&Manager>) -> Result<Profile> {
    let saved = manager.map(default_profile).transpose()?.flatten();
    let mut profile = saved.unwrap_or_else(|| Profile {
        id: BUILTIN_ID.into(),
        name: BUILTIN_NAME.into(),
        revision: 0,
        base_url: discover_address(),
        model: "尚未选择模型".into(),
        model_provider: PROVIDER.into(),
        source: None,
        follow: false,
        source_status: "内置 Provider · e 选择模型与路由".into(),
        auth_mode: codex_manager::AuthMode::Byok,
        oauth: None,
    });
    profile.id = BUILTIN_ID.into();
    profile.name = BUILTIN_NAME.into();
    Ok(profile)
}

pub(crate) fn manager_profiles(manager: &Manager) -> Result<Vec<Profile>> {
    let mut profiles = vec![builtin_profile(Some(manager))?];
    profiles.extend(
        manager
            .list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|p| p.model_provider != PROVIDER),
    );
    Ok(profiles)
}

pub(crate) fn builtin_connection(manager: Option<&Manager>) -> Result<Connection> {
    if let Some(manager) = manager
        && let Some(profile) = default_profile(manager)?
    {
        let query = manager
            .model_catalog_query(&profile)
            .map_err(|e| e.to_string())?;
        return Connection::new(&profile.base_url, query.credential());
    }
    Connection::local()
}

/// Not Debug/Serialize: a remote gateway credential may be held here.
#[derive(Clone)]
pub struct Connection {
    base: reqwest::Url,
    key: String,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct Hello {
    pub name: String,
    pub version: String,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub owned_by: String,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<Value>,
    #[serde(default)]
    pub modalities: Value,
    #[serde(default)]
    pub kind: String,
}
impl Model {
    pub fn category(&self) -> &'static str {
        if self.id.starts_with("group/") {
            "自动路由"
        } else if self.id.starts_with("auto/") {
            "自动供应来源"
        } else {
            "指定模型"
        }
    }
    pub fn label(&self) -> String {
        format!("{} · {} · {}", self.category(), self.owned_by, self.id)
    }
}

impl Connection {
    pub fn new(address: &str, key: &str) -> Result<Self> {
        let mut base = crate::http::validate_api_base_url(address)
            .map_err(|_| "Magpie 地址需要 HTTPS 或本机 HTTP，且不能包含凭据或查询参数")?;
        let path = base
            .path()
            .trim_end_matches('/')
            .strip_suffix("/v1")
            .unwrap_or(base.path().trim_end_matches('/'))
            .to_owned();
        base.set_path(&format!("{path}/"));
        if key.trim().is_empty()
            || reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).is_err()
        {
            return Err("Magpie 网关访问凭据无效".into());
        }
        Ok(Self {
            base,
            key: key.into(),
        })
    }
    pub fn local() -> Result<Self> {
        Self::new(&discover_address(), LOCAL_KEY)
    }
    pub fn address(&self) -> &str {
        self.base.as_str()
    }
    pub fn inference_url(&self) -> String {
        self.base.join("v1").expect("relative URL").to_string()
    }
    pub(crate) fn model_query(&self, id: &str) -> Result<crate::provider_catalog::ModelQuery> {
        crate::provider_catalog::ModelQuery::new(id.into(), &self.inference_url(), self.key.clone())
            .map_err(str::to_owned)
    }
    pub fn identity(&self) -> String {
        use sha2::{Digest, Sha256};
        format!(
            "magpie:{:x}",
            Sha256::digest(format!("{}\0{}", self.base, self.key))
        )
    }
    pub async fn get(
        &self,
        http: &dyn HttpClient,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<Value> {
        let mut url = self.base.join(path).map_err(|_| "Magpie 接口地址无效")?;
        if !params.is_empty() {
            url.query_pairs_mut().extend_pairs(params.iter().copied());
        }
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(16),
            http.request(
                "GET",
                url.as_str(),
                &[
                    ("Authorization", format!("Bearer {}", self.key)),
                    ("User-Agent", format!("Usage/{}", env!("CARGO_PKG_VERSION"))),
                ],
                None,
            ),
        )
        .await
        .map_err(|_| "Magpie 查询超时")?
        .map_err(|_| "无法连接 Magpie，请确认服务已启动")?;
        match response.status {
            200 => serde_json::from_slice(&response.body)
                .map_err(|_| "Magpie 返回的数据格式无效".into()),
            401 | 403 => Err("Magpie 认证失败或当前网关密钥没有查询权限".into()),
            404 | 405 => Err("此 Magpie 版本未提供所需接口，请更新 Magpie".into()),
            429 => Err("Magpie 查询受到限流".into()),
            status => Err(format!("Magpie 服务暂不可用（HTTP {status}）")),
        }
    }
    pub async fn hello(&self, http: &dyn HttpClient) -> Result<Hello> {
        let hello: Hello = serde_json::from_value(self.get(http, "api/hello", &[]).await?)
            .map_err(|_| "Magpie 服务身份无效")?;
        if hello.name != "magpie" {
            return Err("此地址不是 Magpie 服务".into());
        }
        Ok(hello)
    }
    pub async fn models(&self, http: &dyn HttpClient) -> Result<Vec<Model>> {
        let value = self.get(http, "v1/models", &[]).await?;
        let mut models: Vec<Model> = serde_json::from_value(
            value
                .get("data")
                .cloned()
                .ok_or("Magpie 模型目录缺少 data")?,
        )
        .map_err(|_| "Magpie 模型目录格式无效")?;
        if models.len() > 5000
            || models
                .iter()
                .any(|m| m.id.is_empty() || m.id.len() > 256 || m.id.chars().any(char::is_control))
        {
            return Err("Magpie 模型目录包含无效模型".into());
        }
        models.retain(|m| {
            (m.kind.is_empty() || m.kind == "model")
                && m.modalities
                    .get("output")
                    .and_then(Value::as_array)
                    .is_none_or(|a| a.iter().any(|v| v == "text"))
        });
        models.sort_by_key(|m| {
            (
                !m.id.starts_with("group/"),
                !m.id.starts_with("auto/"),
                m.owned_by.clone(),
                m.id.clone(),
            )
        });
        models.dedup_by(|a, b| a.id == b.id);
        Ok(models)
    }
    pub fn config(
        &self,
        model: &str,
        effort: Option<&str>,
        account: Option<&str>,
    ) -> Result<String> {
        let mut doc = codex_manager::config::parse(
            &codex_manager::config::new_config(&self.inference_url(), model, effort)
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let provider = doc["model_providers"]
            .as_table_like_mut()
            .ok_or("模型提供方配置无效")?
            .remove("usage_byok")
            .ok_or("缺少模型提供方配置")?;
        doc["model_provider"] = toml_edit::value(PROVIDER);
        doc["model_providers"][PROVIDER] = provider;
        doc["model_providers"][PROVIDER]["name"] = toml_edit::value("Magpie");
        doc["model_providers"][PROVIDER]["http_headers"]["x-openai-actor-authorization"] =
            toml_edit::value("magpie");
        if let Some(account) = account {
            if account.trim().is_empty() || account.chars().any(char::is_control) {
                return Err("Magpie 账号标识无效".into());
            }
            doc["model_providers"][PROVIDER]["http_headers"]["X-Magpie-Account"] =
                toml_edit::value(account);
        }
        Ok(doc.to_string())
    }
    /// Shared by onboarding, CLI and legacy migration; old revisions and sessions survive an update.
    pub fn bind(
        &self,
        manager: &mut Manager,
        name: &str,
        model: &str,
        effort: Option<&str>,
        account: Option<&str>,
        replace: Option<&str>,
    ) -> Result<Profile> {
        let config = self.config(model, effort, account)?;
        match replace {
            Some(id) => {
                let profile = manager.resolve(id).map_err(|e| e.to_string())?;
                if profile.auth_mode != codex_manager::AuthMode::Byok {
                    return Err("原生 OAuth 账号不能迁移为 Magpie，请新建启动配置".into());
                }
                manager
                    .update(
                        &profile.id,
                        profile.revision,
                        &config,
                        Some(&self.key),
                        Some(name),
                    )
                    .map_err(|e| e.to_string())
            }
            None => Err(
                "Magpie 是内置 Provider，不能添加；请用 usage magpie configure 设置模型与路由"
                    .into(),
            ),
        }
    }

    pub fn configure(
        &self,
        manager: &mut Manager,
        model: &str,
        effort: Option<&str>,
        account: Option<&str>,
    ) -> Result<Profile> {
        let config = self.config(model, effort, account)?;
        match default_profile(manager)? {
            Some(profile) => manager
                .update(
                    &profile.id,
                    profile.revision,
                    &config,
                    Some(&self.key),
                    Some(BUILTIN_NAME),
                )
                .map_err(|e| e.to_string()),
            None => manager
                .initialize_magpie(&config, &self.key)
                .map_err(|e| e.to_string()),
        }
    }
    /// Validate the live catalog at launch and materialize Codex's native model catalog.
    /// Uses its own worker/runtime because managed launch is also callable from an async CLI.
    pub(crate) fn prepare_launch(&self, home: &Path, model: &str) -> Result<std::path::PathBuf> {
        let connection = self.clone();
        let models = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "无法准备 Magpie 查询")?
                .block_on(async {
                    let http = crate::http::ReqwestClient::without_redirects();
                    connection.hello(&http).await?;
                    connection.models(&http).await
                })
        })
        .join()
        .map_err(|_| "Magpie 目录查询中断")??;
        if !models.iter().any(|m| m.id == model) {
            return Err(format!("Magpie 已不再提供 {model}，请重新选择模型或路由组"));
        }
        let path = home.join("magpie-models.json");
        let bytes = serde_json::to_vec(&catalog::codex_models(&models)?)
            .map_err(|_| "无法生成 Magpie 模型目录")?;
        crate::private_fs::write(&path, &bytes).map_err(|_| "无法保存实例模型目录")?;
        Ok(path)
    }
}

pub fn is_config(text: &str) -> bool {
    codex_manager::config::parse(text)
        .ok()
        .is_some_and(|doc| doc["model_provider"].as_str() == Some(PROVIDER))
}
pub fn legacy_endpoint(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint)
        .ok()
        .is_some_and(|u| u.host_str() == Some("usage-plugin.invalid"))
}
pub fn discover_address() -> String {
    if let Ok(address) = std::env::var("USAGE_MAGPIE_URL") {
        return address;
    }
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::config::home_dir().join(".config"));
    let port = std::fs::read(root.join("magpie/settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["port"].as_u64())
        .filter(|p| *p > 0 && *p <= 65535)
        .unwrap_or(3425);
    format!("http://127.0.0.1:{port}")
}

pub(crate) fn http() -> Arc<dyn HttpClient> {
    Arc::new(crate::http::ReqwestClient::without_redirects())
}

/// Resolve a connection from a saved profile/card without exposing credentials to UI state.
pub(crate) fn saved_connection(root: &Path, id: &str) -> Result<Connection> {
    let manager = Manager::open_read_only(root.to_path_buf()).map_err(|_| "无法读取启动配置")?;
    if id == BUILTIN_ID {
        return builtin_connection(manager.as_ref());
    }
    let manager = manager.ok_or("尚无启动配置")?;
    for profile in manager.list().map_err(|_| "无法读取启动配置")? {
        if !id.starts_with("magpie:") && profile.id != id {
            continue;
        }
        let saved = manager
            .revision(&profile.id, profile.revision)
            .map_err(|_| "无法读取配置版本")?;
        if !is_config(&saved.config) {
            continue;
        }
        let Ok(query) = manager.model_catalog_query(&profile) else {
            continue;
        };
        let connection = Connection::new(&profile.base_url, query.credential())?;
        if id == profile.id || id == connection.identity() {
            return Ok(connection);
        }
    }
    Err("Magpie 连接已移除或配置已更新，请刷新看板".into())
}

#[cfg(test)]
mod tests;
