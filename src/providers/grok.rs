//! Grok 官方用量。读取 ~/.grok/auth.json，向 GetGrokCreditsConfig 发送空 gRPC-web 帧。

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};

use super::grok_proto::{
    grpc_web_trailer_fields, is_grpc_auth_failure, is_team_billing_unavailable,
    is_transient_grpc_status, parse_billing_payload, percent_decode, tier_name_for_reset,
};
use super::{ProviderContext, UsageProvider};
use crate::credentials::{grok_email_from_auth, parse_grok_auth};
use crate::http::{TransportError, body_excerpt, is_transient_status};
use crate::model::{
    CredentialStatus, QuotaTier, UsageSnapshot, snapshot_error, snapshot_not_found, snapshot_ok,
    snapshot_transient,
};

const BILLING_URL: &str = "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig";
const RELOGIN: &str = "请重新执行 grok login";
const EMPTY_FRAME: &[u8] = &[0, 0, 0, 0, 0];

pub struct GrokProvider {
    ctx: Arc<ProviderContext>,
}

impl GrokProvider {
    pub const ID: &'static str = "grok";
    pub const NAME: &'static str = "Grok";

    pub fn new(ctx: Arc<ProviderContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl UsageProvider for GrokProvider {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn display_name(&self) -> &'static str {
        Self::NAME
    }

    async fn query(&self) -> UsageSnapshot {
        let now = (self.ctx.now)();
        let ctx = Arc::clone(&self.ctx);
        let (token, status, message, email) = match crate::background::blocking(move || {
            read_credentials(&ctx, now)
        })
        .await
        {
            Ok(credentials) => credentials,
            Err(error) => {
                return snapshot_transient(Self::ID, Self::NAME, format!("凭据读取中断: {error}"));
            }
        };
        let mut result = match status {
            CredentialStatus::NotFound => snapshot_not_found(Self::ID, Self::NAME, message),
            CredentialStatus::ParseError => snapshot_error(
                Self::ID,
                Self::NAME,
                CredentialStatus::ParseError,
                message.unwrap_or_else(|| "无法解析 Grok 凭据".into()),
                now,
            ),
            CredentialStatus::Expired
            | CredentialStatus::RefreshRequired
            | CredentialStatus::Valid => {
                let Some(token) = token else {
                    return snapshot_error(
                        Self::ID,
                        Self::NAME,
                        CredentialStatus::ParseError,
                        message.unwrap_or_else(|| "无法解析 Grok 凭据".into()),
                        now,
                    );
                };
                let result = query_usage(&self.ctx, &token, now).await;
                if status == CredentialStatus::Expired && !result.success {
                    let detail = message.unwrap_or_else(|| "Grok OAuth 令牌已过期".into());
                    snapshot_error(
                        Self::ID,
                        Self::NAME,
                        CredentialStatus::Expired,
                        format!("{detail}。{RELOGIN}"),
                        now,
                    )
                } else {
                    result
                }
            }
        };
        result.account_email = email;
        result
    }
}

pub fn grpc_failure(status: i64, message: &str, now: DateTime<Utc>) -> UsageSnapshot {
    if is_transient_grpc_status(status, message) {
        return snapshot_transient(
            GrokProvider::ID,
            GrokProvider::NAME,
            format!("服务暂时不可用 (grpc-status {status})"),
        );
    }
    if is_grpc_auth_failure(status, message) {
        return snapshot_error(
            GrokProvider::ID,
            GrokProvider::NAME,
            CredentialStatus::Expired,
            format!("Grok 凭据被拒绝 (grpc-status {status})。{RELOGIN}"),
            now,
        );
    }
    if is_team_billing_unavailable(status, message) {
        return snapshot_error(
            GrokProvider::ID,
            GrokProvider::NAME,
            CredentialStatus::Valid,
            "Grok 团队用量接口暂未提供",
            now,
        );
    }
    let detail = if message.is_empty() {
        "unknown"
    } else {
        message
    };
    snapshot_error(
        GrokProvider::ID,
        GrokProvider::NAME,
        CredentialStatus::Valid,
        format!("账单接口失败 (grpc-status {status}): {detail}"),
        now,
    )
}

fn read_credentials(
    ctx: &ProviderContext,
    now: DateTime<Utc>,
) -> (
    Option<String>,
    CredentialStatus,
    Option<String>,
    Option<String>,
) {
    let path = ctx.grok_dir.join("auth.json");
    if !path.exists() {
        return (None, CredentialStatus::NotFound, None, None);
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let (token, status, message) = parse_grok_auth(&content, now);
            let email = token.as_ref().and_then(|_| grok_email_from_auth(&content));
            (token, status, message, email)
        }
        Err(error) => (
            None,
            CredentialStatus::ParseError,
            Some(format!("无法读取 Grok 凭据: {error}")),
            None,
        ),
    }
}

async fn query_usage(ctx: &ProviderContext, token: &str, now: DateTime<Utc>) -> UsageSnapshot {
    let response = match ctx
        .http
        .request(
            "POST",
            BILLING_URL,
            &[
                ("Authorization", format!("Bearer {token}")),
                ("Origin", "https://grok.com".into()),
                ("Referer", "https://grok.com/?_s=usage".into()),
                ("Accept", "*/*".into()),
                ("Content-Type", "application/grpc-web+proto".into()),
                ("x-grpc-web", "1".into()),
                ("x-user-agent", "connect-es/2.1.1".into()),
                ("User-Agent", "usage-tui".into()),
            ],
            Some(EMPTY_FRAME.to_vec()),
        )
        .await
    {
        Ok(response) => response,
        Err(TransportError(error)) => {
            return snapshot_transient(
                GrokProvider::ID,
                GrokProvider::NAME,
                format!("网络错误: {error}"),
            );
        }
    };
    if response.status == 401 || response.status == 403 {
        return snapshot_error(
            GrokProvider::ID,
            GrokProvider::NAME,
            CredentialStatus::Expired,
            format!("鉴权失败 (HTTP {})。{RELOGIN}", response.status),
            now,
        );
    }
    if response.status == 408 || is_transient_status(response.status) {
        return snapshot_transient(
            GrokProvider::ID,
            GrokProvider::NAME,
            format!("服务暂时不可用 (HTTP {})", response.status),
        );
    }
    let header_status = response.header("grpc-status").and_then(parse_grpc_code);
    let header_message = response
        .header("grpc-message")
        .map(percent_decode)
        .unwrap_or_default();
    if !(200..300).contains(&response.status) {
        return snapshot_error(
            GrokProvider::ID,
            GrokProvider::NAME,
            CredentialStatus::Valid,
            format!(
                "接口错误 (HTTP {}): {}",
                response.status,
                body_excerpt(&response.body)
            ),
            now,
        );
    }
    if let Some(code) = header_status
        && code != 0
    {
        return grpc_failure(code, &header_message, now);
    }
    let trailers = grpc_web_trailer_fields(&response.body);
    if let Some(code) = trailers
        .get("grpc-status")
        .and_then(|value| parse_grpc_code(value))
        && code != 0
    {
        return grpc_failure(
            code,
            trailers
                .get("grpc-message")
                .map(String::as_str)
                .unwrap_or(""),
            now,
        );
    }
    let snapshot = match parse_billing_payload(&response.body, now.timestamp()) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return snapshot_error(
                GrokProvider::ID,
                GrokProvider::NAME,
                CredentialStatus::Valid,
                format!("用量响应无法解析: {error}"),
                now,
            );
        }
    };
    let resets_at = snapshot
        .resets_at
        .and_then(|ts| Utc.timestamp_opt(ts, 0).single());
    let tier = QuotaTier {
        name: tier_name_for_reset(snapshot.resets_at, now.timestamp()).to_string(),
        utilization: snapshot.used_percent.clamp(0.0, 100.0),
        resets_at,
    };
    snapshot_ok(GrokProvider::ID, GrokProvider::NAME, vec![tier], None, now)
}

fn parse_grpc_code(value: &str) -> Option<i64> {
    let text = value.trim();
    if text.is_empty() {
        None
    } else {
        text.parse().ok()
    }
}
