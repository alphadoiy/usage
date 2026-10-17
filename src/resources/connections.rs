//! Persistable connection descriptions and the application query boundary.
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::*;
use crate::registry::RegistryError;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionConfig {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    pub service: String,
    pub base_url: String,
    /// Name of an environment variable; the credential itself is never saved here.
    pub credential_env: String,
    #[serde(default)]
    pub revision: String,
    #[serde(default)]
    pub options: Value,
    /// Explicit managed Codex profile IDs. Association, not proof of an effective run binding.
    #[serde(default)]
    pub profile_ids: Vec<String>,
    /// Explicit inference identity expected by this resource connection.
    #[serde(default)]
    pub inference: Option<InferenceBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceBinding {
    pub base_url: String,
    pub credential_env: String,
}
impl ConnectionConfig {
    pub fn validate(&self) -> Result<(), RegistryError> {
        let clean =
            |s: &str| !s.trim().is_empty() && s == s.trim() && !s.chars().any(char::is_control);
        if !clean(&self.id)
            || !clean(&self.service)
            || self.label.as_deref().is_some_and(|s| !clean(s))
            || self.revision.chars().any(char::is_control)
        {
            return Err(RegistryError::Factory(
                "资源 id/service/label 必须非空且不含控制字符或首尾空白".into(),
            ));
        }
        if !valid_env_name(&self.credential_env) {
            return Err(RegistryError::Factory(
                "credential_env 必须为环境变量名称，不能直接填写密钥".into(),
            ));
        }
        for (i, id) in self.profile_ids.iter().enumerate() {
            if uuid::Uuid::parse_str(id).is_err() || self.profile_ids[..i].contains(id) {
                return Err(RegistryError::Factory(
                    "资源 profile_ids 必须为无重复的 Codex 账号 UUID".into(),
                ));
            }
        }
        super::api::validate_base_url(&self.base_url)?;
        if let Some(inference) = &self.inference {
            super::api::validate_base_url(&inference.base_url)?;
            if !valid_env_name(&inference.credential_env) {
                return Err(RegistryError::Factory(
                    "inference.credential_env 必须为环境变量名称".into(),
                ));
            }
        }
        Ok(())
    }
    pub fn binding(&self) -> ResourceBinding {
        // Changes to endpoint, permission scope, conversion or secret reference invalidate caches.
        // The optional explicit revision also permits invalidation after a secret is rotated in place.
        let canonical_base = super::api::validate_base_url(&self.base_url)
            .map(|url| url.to_string())
            .unwrap_or_else(|_| self.base_url.clone());
        let serialized = serde_json::to_vec(&serde_json::json!([
            self.service,
            canonical_base,
            self.credential_env,
            self.revision,
            self.options,
            self.inference,
        ]))
        .expect("JSON values serialize");
        ResourceBinding {
            account: self.id.clone(),
            revision: format!("{:x}", Sha256::digest(serialized)),
        }
    }
    pub fn descriptor(&self) -> ConnectionDescriptor {
        ConnectionDescriptor {
            id: self.id.clone(),
            label: self.label.clone().unwrap_or_else(|| self.id.clone()),
            service: self.service.clone(),
            profile_ids: self.profile_ids.clone(),
        }
    }
}
fn valid_env_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
}

#[derive(Debug, Serialize)]
pub struct ConnectionDescriptor {
    pub id: String,
    pub label: String,
    pub service: String,
    pub profile_ids: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct ResourceReport {
    #[serde(flatten)]
    pub connection: ConnectionDescriptor,
    pub snapshot: ResourceSnapshot,
}

pub struct ResourceService {
    registry: ResourceRegistry,
    http: Arc<dyn crate::http::HttpClient>,
    credentials: Arc<CredentialResolver>,
    now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}
impl ResourceService {
    pub fn new(
        registry: ResourceRegistry,
        http: Arc<dyn crate::http::HttpClient>,
        credentials: Arc<CredentialResolver>,
        now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    ) -> Self {
        Self {
            registry,
            http,
            credentials,
            now,
        }
    }
    pub fn from_environment(registry: ResourceRegistry) -> Self {
        Self::new(
            registry,
            Arc::new(crate::http::ReqwestClient::new()),
            Arc::new(|reference| {
                let name = reference
                    .strip_prefix("env:")
                    .filter(|name| valid_env_name(name))
                    .ok_or("无效的凭据引用")?;
                std::env::var(name).map_err(|_| "环境变量未设置".into())
            }),
            Arc::new(Utc::now),
        )
    }
    pub fn with_credentials(mut self, credentials: Arc<CredentialResolver>) -> Self {
        self.credentials = credentials;
        self
    }
    pub fn prepare(
        &self,
        config: &ConnectionConfig,
    ) -> Result<Arc<dyn ResourceReader>, RegistryError> {
        self.prepare_with_credentials(config, self.credentials.clone())
    }

    pub fn prepare_with_credentials(
        &self,
        config: &ConnectionConfig,
        credentials: Arc<CredentialResolver>,
    ) -> Result<Arc<dyn ResourceReader>, RegistryError> {
        config.validate()?;
        let context = ResourceContext {
            binding: config.binding(),
            connection: ResourceConnection::Api(ApiConnection {
                base_url: config.base_url.clone(),
                credential_ref: format!("env:{}", config.credential_env),
                options: config.options.clone(),
                http: self.http.clone(),
                credentials,
                now: self.now.clone(),
            }),
        };
        let reader = self.registry.create(&config.service, context)?;
        if reader.service() != config.service || reader.binding() != &config.binding() {
            return Err(RegistryError::Factory(
                "资源工厂返回的类型或账号绑定不匹配".into(),
            ));
        }
        Ok(reader)
    }
    pub async fn query(
        &self,
        configs: &[ConnectionConfig],
    ) -> Result<Vec<ResourceReport>, RegistryError> {
        let mut ids = std::collections::HashSet::new();
        let readers = configs
            .iter()
            .map(|config| {
                if !ids.insert(&config.id) {
                    return Err(RegistryError::Duplicate(config.id.clone()));
                }
                self.prepare(config)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let snapshots =
            super::collect_with_deadline(&readers, crate::providers::QUERY_TIMEOUT).await;
        Ok(configs
            .iter()
            .zip(snapshots)
            .map(|(config, snapshot)| ResourceReport {
                connection: config.descriptor(),
                snapshot,
            })
            .collect())
    }
}

pub fn select(
    configs: &[ConnectionConfig],
    ids: &[String],
    profile_id: Option<&str>,
) -> Result<Vec<ConnectionConfig>, RegistryError> {
    for id in ids {
        if !configs.iter().any(|config| config.id == *id) {
            return Err(RegistryError::Unknown {
                id: id.clone(),
                known: configs.iter().map(|c| c.id.clone()).collect(),
            });
        }
    }
    let selected: Vec<_> = configs
        .iter()
        .filter(|config| {
            (ids.is_empty() || ids.contains(&config.id))
                && profile_id.is_none_or(|id| config.profile_ids.iter().any(|p| p == id))
        })
        .cloned()
        .collect();
    if selected.is_empty() && (!ids.is_empty() || profile_id.is_some()) {
        return Err(RegistryError::Factory("所选账号没有匹配的资源连接".into()));
    }
    Ok(selected)
}

pub fn render_text(reports: &[ResourceReport]) -> String {
    if reports.is_empty() {
        return "尚未配置资源连接；在配置文件中添加 [[resources]]".into();
    }
    let mut lines = Vec::new();
    for report in reports {
        let snapshot = &report.snapshot;
        let scope = match &snapshot.scope {
            ResourceScope::Unspecified => "范围未指定".to_owned(),
            ResourceScope::Account => "账号".into(),
            ResourceScope::Token => "令牌".into(),
            ResourceScope::Organization => "组织".into(),
            ResourceScope::Project { id } => format!("项目 {id}"),
            ResourceScope::ApiKey { id } => format!("API Key {id}"),
        };
        lines.push(format!(
            "{} · {} · {}",
            report.connection.label, snapshot.service, scope
        ));
        if snapshot.state != ResourceState::Ready {
            lines.push(format!(
                "  {}",
                snapshot.message.as_deref().unwrap_or("查询失败")
            ));
            continue;
        }
        if snapshot.metrics.is_empty() {
            lines.push("  查询成功，当前范围暂无账单项目".into());
        }
        for metric in &snapshot.metrics {
            lines.push(match metric {
                ResourceMetric::Balance { remaining } => {
                    format!(
                        "  余额 {} {}{}",
                        remaining.amount,
                        remaining.currency,
                        if snapshot.service == "new_api_token" {
                            "（接口返回）"
                        } else {
                            ""
                        }
                    )
                }
                ResourceMetric::Cost {
                    used,
                    period_start,
                    period_end,
                } => format!(
                    "  费用 {} {} · {} 至 {} (UTC)",
                    used.amount,
                    used.currency,
                    period_start.format("%Y-%m-%d %H:%M"),
                    period_end.format("%Y-%m-%d %H:%M")
                ),
                ResourceMetric::Allowance {
                    remaining,
                    used,
                    unit,
                    unlimited,
                } => format!(
                    "  {} · 已用 {} · {}",
                    unit,
                    used.as_deref().unwrap_or("未知"),
                    if *unlimited {
                        match remaining {
                            Some(value) => format!("剩余 {value} · 令牌不限额"),
                            None => "令牌额度不限量（不代表账号余额无限）".into(),
                        }
                    } else {
                        format!("剩余 {}", remaining.as_deref().unwrap_or("未知"))
                    }
                ),
                ResourceMetric::QuotaWindow {
                    name, used_percent, ..
                } => format!("  {name} · 已用 {used_percent:.0}%"),
                ResourceMetric::Spending { used, currency, .. } => format!(
                    "  支出 {} {}",
                    used.map(|v| v.to_string()).unwrap_or_else(|| "未知".into()),
                    currency.as_deref().unwrap_or("")
                ),
            });
        }
    }
    lines.join("\n")
}
