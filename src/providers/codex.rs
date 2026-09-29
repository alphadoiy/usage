//! Codex 官方用量。仅 `auth_mode = chatgpt` 时查询 wham/usage。

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use super::{ProviderContext, UsageProvider};
use crate::credentials::{CODEX_KEYCHAIN_SERVICE, parse_codex_credentials};
use crate::http::{TransportError, body_excerpt, is_transient_status};
use crate::model::{
    CredentialStatus, QuotaTier, UsageSnapshot, snapshot_error, snapshot_not_found, snapshot_ok,
    snapshot_transient,
};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

pub struct CodexProvider {
    ctx: Arc<ProviderContext>,
}

impl CodexProvider {
    pub const ID: &'static str = "codex";
    pub const NAME: &'static str = "Codex";

    pub fn new(ctx: Arc<ProviderContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl UsageProvider for CodexProvider {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn display_name(&self) -> &'static str {
        Self::NAME
    }

    async fn query(&self) -> UsageSnapshot {
        let now = (self.ctx.now)();
        let ctx = Arc::clone(&self.ctx);
        let (token, account_id, status, message) = match tokio::task::spawn_blocking(move || {
            read_credentials(&ctx, now)
        })
        .await
        {
            Ok(credentials) => credentials,
            Err(error) => {
                return snapshot_transient(Self::ID, Self::NAME, format!("凭据读取中断: {error}"));
            }
        };
        match status {
            CredentialStatus::NotFound => snapshot_not_found(Self::ID, Self::NAME, message),
            CredentialStatus::ParseError => snapshot_error(
                Self::ID,
                Self::NAME,
                CredentialStatus::ParseError,
                message.unwrap_or_else(|| "无法解析 Codex 凭据".into()),
                now,
            ),
            CredentialStatus::Expired | CredentialStatus::Valid => {
                let Some(token) = token else {
                    return snapshot_error(
                        Self::ID,
                        Self::NAME,
                        CredentialStatus::ParseError,
                        message.unwrap_or_else(|| "无法解析 Codex 凭据".into()),
                        now,
                    );
                };
                let result = query_usage(&self.ctx, &token, account_id.as_deref(), now).await;
                if status == CredentialStatus::Expired && !result.success {
                    snapshot_error(
                        Self::ID,
                        Self::NAME,
                        CredentialStatus::Expired,
                        message.unwrap_or_else(|| "Codex 登录已过期，请重新登录".into()),
                        now,
                    )
                } else {
                    result
                }
            }
        }
    }
}

pub fn parse_codex_usage(body: &Value, now: DateTime<Utc>) -> UsageSnapshot {
    let Some(root) = body.as_object() else {
        return snapshot_error(
            CodexProvider::ID,
            CodexProvider::NAME,
            CredentialStatus::Valid,
            "用量响应不是 JSON 对象",
            now,
        );
    };
    let mut tiers = Vec::new();
    if let Some(rate_limit) = root.get("rate_limit").and_then(Value::as_object) {
        for key in ["primary_window", "secondary_window"] {
            if let Some(tier) = window_tier(rate_limit.get(key)) {
                tiers.push(tier);
            }
        }
    }
    snapshot_ok(CodexProvider::ID, CodexProvider::NAME, tiers, None, now)
}

pub fn window_seconds_to_tier_name(seconds: i64) -> String {
    match seconds {
        18_000 => "five_hour".into(),
        604_800 => "seven_day".into(),
        2_592_000 => "30_day".into(),
        seconds => {
            let hours = seconds / 3600;
            if hours >= 24 {
                format!("{}_day", hours / 24)
            } else {
                format!("{hours}_hour")
            }
        }
    }
}

fn read_credentials(
    ctx: &ProviderContext,
    now: DateTime<Utc>,
) -> (
    Option<String>,
    Option<String>,
    CredentialStatus,
    Option<String>,
) {
    if let Some(raw) = (ctx.keychain)(CODEX_KEYCHAIN_SERVICE, None) {
        return parse_codex_credentials(&raw, now);
    }
    let path = ctx.codex_dir.join("auth.json");
    if !path.exists() {
        return (None, None, CredentialStatus::NotFound, None);
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_codex_credentials(&content, now),
        Err(error) => (
            None,
            None,
            CredentialStatus::ParseError,
            Some(format!("无法读取 Codex 凭据: {error}")),
        ),
    }
}

async fn query_usage(
    ctx: &ProviderContext,
    token: &str,
    account_id: Option<&str>,
    now: DateTime<Utc>,
) -> UsageSnapshot {
    let mut headers = vec![
        ("Authorization", format!("Bearer {token}")),
        ("User-Agent", "codex-cli".into()),
        ("Accept", "application/json".into()),
    ];
    if let Some(account_id) = account_id {
        headers.push(("ChatGPT-Account-Id", account_id.to_string()));
    }
    let response = match ctx.http.request("GET", USAGE_URL, &headers, None).await {
        Ok(response) => response,
        Err(TransportError(error)) => {
            return snapshot_transient(
                CodexProvider::ID,
                CodexProvider::NAME,
                format!("网络错误: {error}"),
            );
        }
    };
    if response.status == 401 || response.status == 403 {
        return snapshot_error(
            CodexProvider::ID,
            CodexProvider::NAME,
            CredentialStatus::Expired,
            format!("鉴权失败 (HTTP {})，请重新登录 Codex", response.status),
            now,
        );
    }
    if is_transient_status(response.status) {
        return snapshot_transient(
            CodexProvider::ID,
            CodexProvider::NAME,
            format!("服务暂时不可用 (HTTP {})", response.status),
        );
    }
    if !(200..300).contains(&response.status) {
        return snapshot_error(
            CodexProvider::ID,
            CodexProvider::NAME,
            CredentialStatus::Valid,
            format!(
                "接口错误 (HTTP {}): {}",
                response.status,
                body_excerpt(&response.body)
            ),
            now,
        );
    }
    let payload: Value = match serde_json::from_slice(&response.body) {
        Ok(value) => value,
        Err(error) => {
            return snapshot_error(
                CodexProvider::ID,
                CodexProvider::NAME,
                CredentialStatus::Valid,
                format!("用量响应无法解析: {error}"),
                now,
            );
        }
    };
    parse_codex_usage(&payload, now)
}

fn window_tier(value: Option<&Value>) -> Option<QuotaTier> {
    let value = value?.as_object()?;
    let used = value.get("used_percent")?.as_f64()?;
    let name = value
        .get("limit_window_seconds")
        .and_then(Value::as_i64)
        .map(window_seconds_to_tier_name)
        .unwrap_or_else(|| "unknown".into());
    let resets_at = value
        .get("reset_at")
        .and_then(Value::as_i64)
        .and_then(|ts| Utc.timestamp_opt(ts, 0).single());
    Some(QuotaTier {
        name,
        utilization: used,
        resets_at,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;
    use crate::credentials::parse_codex_credentials;
    use crate::model::CredentialStatus;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap()
    }

    #[test]
    fn window_names() {
        assert_eq!(window_seconds_to_tier_name(18_000), "five_hour");
        assert_eq!(window_seconds_to_tier_name(604_800), "seven_day");
        assert_eq!(window_seconds_to_tier_name(2_592_000), "30_day");
        assert_eq!(window_seconds_to_tier_name(3 * 3600), "3_hour");
        assert_eq!(window_seconds_to_tier_name(2 * 86_400), "2_day");
    }

    #[test]
    fn primary_and_secondary_windows() {
        let quota = parse_codex_usage(
            &json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 41.5,
                        "limit_window_seconds": 18000,
                        "reset_at": 1_780_000_000
                    },
                    "secondary_window": {
                        "used_percent": 12,
                        "limit_window_seconds": 604800,
                        "reset_at": 1_790_000_000
                    }
                }
            }),
            now(),
        );
        let tiers: Vec<_> = quota
            .tiers
            .iter()
            .map(|tier| (tier.name.as_str(), tier.utilization))
            .collect();
        assert_eq!(tiers, vec![("five_hour", 41.5), ("seven_day", 12.0)]);
    }

    #[test]
    fn api_key_mode_is_not_found() {
        let (token, _, status, message) = parse_codex_credentials(
            &json!({"auth_mode": "apikey", "tokens": {"access_token": "x"}}).to_string(),
            now(),
        );
        assert!(token.is_none());
        assert_eq!(status, CredentialStatus::NotFound);
        assert!(message.unwrap().contains("ChatGPT"));
    }
}
