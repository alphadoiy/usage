//! Provider plugins: account-scoped host, OpenCode compatibility and existing capability bridges.
pub(crate) mod adapters;
pub mod auth;
mod cli;
mod runtime;
pub mod store;
pub use cli::{PluginCommand, dispatch};
pub(crate) use runtime::Runtime;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};
pub type Result<T> = std::result::Result<T, String>;

pub(crate) const HOST: &str = include_str!("host.mjs");
pub(crate) const PROTOCOL: &str = include_str!("protocol.mjs");
pub fn account_endpoint(id: &str) -> String {
    format!("https://usage-plugin.invalid/{id}/v1")
}
pub(crate) fn endpoint_account(endpoint: &str) -> Option<String> {
    let url = reqwest::Url::parse(endpoint).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("usage-plugin.invalid")
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let id = url.path().strip_prefix('/')?.strip_suffix("/v1")?;
    uuid::Uuid::parse_str(id).ok()?;
    Some(id.into())
}
pub async fn call(value: Value) -> Result<Value> {
    call_at(&store::root(), value).await
}
fn control_client() -> Result<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Result<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(3))
                .build()
                .map_err(|_| "无法创建插件连接".into())
        })
        .as_ref()
        .map_err(Clone::clone)
}
pub async fn call_at(root: &Path, mut value: Value) -> Result<Value> {
    value["_root"] = root.to_string_lossy().to_string().into();
    let endpoint = tokio::task::spawn_blocking(crate::gateway::plugin_control)
        .await
        .map_err(|_| "插件网关准备失败")??;
    let client = control_client()?;
    let response = client
        .post(endpoint.0)
        .bearer_auth(endpoint.1)
        .json(&value)
        .timeout(std::time::Duration::from_secs(330))
        .send()
        .await
        .map_err(|_| "插件调用失败或超时")?;
    if !response.status().is_success() {
        return Err(format!(
            "插件调用失败（HTTP {}），详情见 Usage 日志",
            response.status().as_u16()
        ));
    }
    response.json().await.map_err(|_| "插件响应无效".into())
}
#[cfg(test)]
mod tests;
