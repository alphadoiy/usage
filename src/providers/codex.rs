//! Codex 官方用量。仅 `auth_mode = chatgpt` 时查询 wham/usage。

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use super::{ProviderContext, ProviderError, ResetPreview, UsageProvider, UsageReset};
use crate::credentials::{
    CODEX_KEYCHAIN_SERVICE, codex_email_from_credentials, parse_codex_credentials,
};
use crate::http::{TransportError, body_excerpt, is_transient_status};
use crate::model::{
    CredentialStatus, QuotaTier, UsageSnapshot, snapshot_error, snapshot_not_found, snapshot_ok,
    snapshot_transient,
};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const RESET_CREDITS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const RESET_CONSUME_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";

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

    fn reset(&self) -> Option<&dyn UsageReset> {
        Some(self)
    }

    async fn query(&self) -> UsageSnapshot {
        let now = (self.ctx.now)();
        let ctx = Arc::clone(&self.ctx);
        let (token, account_id, status, message, mut email) =
            match crate::background::blocking(move || read_credentials(&ctx, now)).await {
                Ok(credentials) => credentials,
                Err(error) => {
                    return snapshot_transient(
                        Self::ID,
                        Self::NAME,
                        format!("凭据读取中断: {error}"),
                    );
                }
            };
        let mut result = match status {
            CredentialStatus::NotFound => snapshot_not_found(Self::ID, Self::NAME, message),
            CredentialStatus::ParseError => snapshot_error(
                Self::ID,
                Self::NAME,
                CredentialStatus::ParseError,
                message.unwrap_or_else(|| "无法解析 Codex 凭据".into()),
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
                        message.unwrap_or_else(|| "无法解析 Codex 凭据".into()),
                        now,
                    );
                };
                let (mut result, unauthorized) = query_usage(
                    &self.ctx,
                    &token,
                    account_id.as_deref(),
                    status == CredentialStatus::RefreshRequired,
                    now,
                )
                .await;
                // Codex can rotate auth.json while this request is in flight.
                // Retry one 401 only with a newer token for the same account.
                if unauthorized {
                    let ctx = Arc::clone(&self.ctx);
                    if let Ok((Some(current), current_account, current_status, _, current_email)) =
                        crate::background::blocking(move || read_credentials(&ctx, now)).await
                        && current != token
                    {
                        if current_account == account_id && current_account.is_some() {
                            result = query_usage(
                                &self.ctx,
                                &current,
                                current_account.as_deref(),
                                current_status == CredentialStatus::RefreshRequired,
                                now,
                            )
                            .await
                            .0;
                            email = current_email;
                        } else {
                            email = None;
                            result = snapshot_error(
                                Self::ID,
                                Self::NAME,
                                CredentialStatus::Valid,
                                "Codex 登录账号在查询期间已切换，请刷新后重试",
                                now,
                            );
                        }
                    }
                }
                result
            }
        };
        result.account_email = email;
        result
    }
}

#[async_trait]
impl UsageReset for CodexProvider {
    async fn preview_reset(&self) -> Result<ResetPreview, ProviderError> {
        let (token, account_id) = self.reset_credentials().await?;
        fetch_reset_preview(&self.ctx, &token, &account_id).await
    }

    async fn consume_reset(&self) -> Result<(), ProviderError> {
        let (token, account_id) = self.reset_credentials().await?;
        let preview = fetch_reset_preview(&self.ctx, &token, &account_id).await?;
        if preview.available == 0 {
            return Err(ProviderError("没有可用的主动重置次数".into()));
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "redeem_request_id": uuid::Uuid::new_v4().to_string(),
        }))
        .map_err(|error| ProviderError(format!("无法构建重置请求: {error}")))?;
        let response = self
            .ctx
            .http
            .request(
                "POST",
                RESET_CONSUME_URL,
                &reset_headers(&token, &account_id, true),
                Some(body),
            )
            .await
            .map_err(|TransportError(error)| {
                ProviderError(format!(
                    "重置请求结果未知: {error}。请先刷新额度确认，勿直接重试"
                ))
            })?;
        if is_transient_status(response.status) {
            return Err(ProviderError(format!(
                "重置请求结果未知 (HTTP {})。请先刷新额度确认，勿直接重试",
                response.status
            )));
        }
        check_reset_response(&response, "重置")?;
        Ok(())
    }
}

impl CodexProvider {
    async fn reset_credentials(&self) -> Result<(String, String), ProviderError> {
        let ctx = Arc::clone(&self.ctx);
        let now = (ctx.now)();
        let (token, account_id, status, message, _) =
            tokio::task::spawn_blocking(move || read_credentials(&ctx, now))
                .await
                .map_err(|error| ProviderError(format!("凭据读取中断: {error}")))?;
        if !matches!(
            status,
            CredentialStatus::Valid | CredentialStatus::RefreshRequired
        ) {
            return Err(ProviderError(
                message.unwrap_or_else(|| "请先登录 Codex".into()),
            ));
        }
        let token = token.ok_or_else(|| ProviderError("Codex 访问令牌为空".into()))?;
        let account_id = account_id.ok_or_else(|| {
            ProviderError("Codex 凭据缺少 account_id，无法确认要重置的账号".into())
        })?;
        Ok((token, account_id))
    }
}

fn reset_headers(token: &str, account_id: &str, post: bool) -> Vec<(&'static str, String)> {
    let os = match std::env::consts::OS {
        "macos" => "Mac OS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    let mut headers = vec![
        ("Authorization", format!("Bearer {token}")),
        (
            "User-Agent",
            format!(
                "Codex Desktop/{} ({os}; {})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::ARCH
            ),
        ),
        ("originator", "Codex Desktop".into()),
        ("ChatGPT-Account-Id", account_id.to_string()),
        ("Accept", "application/json".into()),
    ];
    if post {
        headers.push(("Content-Type", "application/json".into()));
    }
    headers
}

async fn fetch_reset_preview(
    ctx: &ProviderContext,
    token: &str,
    account_id: &str,
) -> Result<ResetPreview, ProviderError> {
    let response = ctx
        .http
        .request(
            "GET",
            RESET_CREDITS_URL,
            &reset_headers(token, account_id, false),
            None,
        )
        .await
        .map_err(|TransportError(error)| ProviderError(format!("查询主动重置次数失败: {error}")))?;
    check_reset_response(&response, "查询主动重置次数")?;
    let payload: Value = serde_json::from_slice(&response.body)
        .map_err(|error| ProviderError(format!("重置次数响应无法解析: {error}")))?;
    let mut preview = parse_reset_preview(&payload, (ctx.now)())?;
    preview.account_label = Some(
        account_id
            .chars()
            .filter(|ch| !ch.is_control())
            .take(48)
            .collect(),
    );
    Ok(preview)
}

fn check_reset_response(
    response: &crate::http::HttpResponse,
    action: &str,
) -> Result<(), ProviderError> {
    match response.status {
        200..=299 => Ok(()),
        401 | 403 => Err(ProviderError(auth_failure(response).1.into())),
        status => Err(ProviderError(format!(
            "{action}失败 (HTTP {status}): {}",
            body_excerpt(&response.body)
        ))),
    }
}

pub fn parse_reset_preview(
    payload: &Value,
    now: DateTime<Utc>,
) -> Result<ResetPreview, ProviderError> {
    let data = payload.get("data").unwrap_or(payload);
    let credits = payload
        .get("credits")
        .or_else(|| data.get("credits"))
        .and_then(Value::as_array);
    let available = payload
        .get("available_count")
        .or_else(|| payload.get("availableCount"))
        .or_else(|| data.get("available_count"))
        .or_else(|| data.get("availableCount"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .or_else(|| {
            credits.map(|items| {
                items
                    .iter()
                    .filter(|item| available_credit(item, now).is_some())
                    .count() as u32
            })
        })
        .ok_or_else(|| ProviderError("重置次数响应缺少可用次数".into()))?;
    let next_expires_at = credits
        .into_iter()
        .flatten()
        .filter_map(|item| available_credit(item, now))
        .flatten()
        .min();
    Ok(ResetPreview {
        available,
        next_expires_at,
        account_label: None,
    })
}

fn available_credit(value: &Value, now: DateTime<Utc>) -> Option<Option<DateTime<Utc>>> {
    let record = value.as_object()?;
    let status = record
        .get("status")
        .or_else(|| record.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("available")
        .trim()
        .to_ascii_lowercase();
    if matches!(
        status.as_str(),
        "redeemed" | "used" | "consumed" | "expired"
    ) {
        return None;
    }
    let expires_at = ["expires_at", "expire_at", "expiresAt"]
        .iter()
        .find_map(|key| record.get(*key).and_then(reset_timestamp));
    if expires_at.is_some_and(|moment| moment <= now) {
        return None;
    }
    Some(expires_at)
}

fn reset_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::Number(number) => {
            let stamp = number.as_i64()?;
            if stamp > 1_000_000_000_000 {
                DateTime::from_timestamp_millis(stamp)
            } else {
                DateTime::from_timestamp(stamp, 0)
            }
        }
        Value::String(text) => {
            if let Ok(stamp) = text.parse::<i64>() {
                return reset_timestamp(&Value::from(stamp));
            }
            crate::timeutil::parse_time(text)
        }
        _ => None,
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
    let mut snapshot = snapshot_ok(CodexProvider::ID, CodexProvider::NAME, tiers, None, now);
    snapshot.reset_credits_available = root
        .get("rate_limit_reset_credits")
        .and_then(|value| value.get("available_count"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    snapshot
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
    Option<String>,
) {
    if let Some(raw) = (ctx.keychain)(CODEX_KEYCHAIN_SERVICE, None) {
        return parsed_credentials(&raw, now);
    }
    let path = ctx.codex_dir.join("auth.json");
    if !path.exists() {
        return (None, None, CredentialStatus::NotFound, None, None);
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => parsed_credentials(&content, now),
        Err(error) => (
            None,
            None,
            CredentialStatus::ParseError,
            Some(format!("无法读取 Codex 凭据: {error}")),
            None,
        ),
    }
}

fn parsed_credentials(
    content: &str,
    now: DateTime<Utc>,
) -> (
    Option<String>,
    Option<String>,
    CredentialStatus,
    Option<String>,
    Option<String>,
) {
    let (token, account_id, status, message) = parse_codex_credentials(content, now);
    let email = token
        .as_ref()
        .and_then(|_| codex_email_from_credentials(content));
    (token, account_id, status, message, email)
}

async fn query_usage(
    ctx: &ProviderContext,
    token: &str,
    account_id: Option<&str>,
    refresh_required: bool,
    now: DateTime<Utc>,
) -> (UsageSnapshot, bool) {
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
            return (
                snapshot_transient(
                    CodexProvider::ID,
                    CodexProvider::NAME,
                    format!("网络错误: {error}"),
                ),
                false,
            );
        }
    };
    if response.status == 401 || response.status == 403 {
        let (status, message) = auth_failure(&response);
        return (
            snapshot_error(
                CodexProvider::ID,
                CodexProvider::NAME,
                if status == CredentialStatus::Valid && response.status == 401 && refresh_required {
                    CredentialStatus::RefreshRequired
                } else {
                    status
                },
                if status == CredentialStatus::Valid && response.status == 401 && refresh_required {
                    "访问令牌待刷新；请打开 Codex 完成刷新后重试"
                } else {
                    message
                },
                now,
            ),
            response.status == 401,
        );
    }
    if is_transient_status(response.status) {
        return (
            snapshot_transient(
                CodexProvider::ID,
                CodexProvider::NAME,
                format!("服务暂时不可用 (HTTP {})", response.status),
            ),
            false,
        );
    }
    if !(200..300).contains(&response.status) {
        return (
            snapshot_error(
                CodexProvider::ID,
                CodexProvider::NAME,
                CredentialStatus::Valid,
                format!(
                    "接口错误 (HTTP {}): {}",
                    response.status,
                    body_excerpt(&response.body)
                ),
                now,
            ),
            false,
        );
    }
    let payload: Value = match serde_json::from_slice(&response.body) {
        Ok(value) => value,
        Err(error) => {
            return (
                snapshot_error(
                    CodexProvider::ID,
                    CodexProvider::NAME,
                    CredentialStatus::Valid,
                    format!("用量响应无法解析: {error}"),
                    now,
                ),
                false,
            );
        }
    };
    (parse_codex_usage(&payload, now), false)
}

/// Only an explicit revocation code establishes that this credential generation
/// needs a new sign-in. A plain 401 can mean an expired access token.
fn auth_failure(response: &crate::http::HttpResponse) -> (CredentialStatus, &'static str) {
    let code = (response.body.len() <= 32 * 1024)
        .then(|| serde_json::from_slice::<Value>(&response.body).ok())
        .flatten()
        .and_then(|body| {
            body.pointer("/error/code")
                .or_else(|| body.pointer("/error/error_reason"))
                .or_else(|| body.get("code"))
                .or_else(|| body.get("error_reason"))
                .or_else(|| body.get("error"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    match code.as_deref() {
        Some(
            "token_revoked"
            | "invalid_grant"
            | "refresh_token_expired"
            | "refresh_token_invalidated"
            | "refresh_token_reused",
        ) => (CredentialStatus::Expired, "Codex 登录已失效，请重新登录"),
        Some("token_expired") => (
            CredentialStatus::RefreshRequired,
            "Codex 访问令牌已到期；请打开 Codex 完成刷新后重试",
        ),
        _ if response.status == 403 => (
            CredentialStatus::Valid,
            "Codex 账号无权访问此接口 (HTTP 403)",
        ),
        _ => (
            CredentialStatus::Valid,
            "Codex 认证未通过 (HTTP 401)；请打开 Codex 验证登录后重试",
        ),
    }
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
    use crate::http::{FakeHttp, HttpResponse};
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
    fn usage_includes_reset_credit_count() {
        let snapshot = parse_codex_usage(
            &json!({
                "rate_limit": {"primary_window": {"used_percent": 42, "limit_window_seconds": 18000}},
                "rate_limit_reset_credits": {"available_count": 2}
            }),
            now(),
        );
        assert_eq!(snapshot.reset_credits_available, Some(2));
        assert_eq!(snapshot.to_json()["resetCreditsAvailable"], 2);
        let card = crate::view::build_view(&snapshot, "bar", now(), None).unwrap();
        assert!(card.lines.iter().any(|line| line.contains("主动重置 2 次")));
    }

    #[test]
    fn reset_preview_filters_expired_and_used_records() {
        let preview = parse_reset_preview(
            &json!({
                "data": {"credits": [
                    {"id": "available", "status": "available", "expires_at": "2099-06-25T08:30:00Z"},
                    {"id": "expired", "expires_at": "2020-01-01T00:00:00Z"},
                    {"id": "used", "status": "redeemed", "expires_at": "2099-07-01T00:00:00Z"}
                ]}
            }),
            now(),
        )
        .unwrap();
        assert_eq!(preview.available, 1);
        assert_eq!(
            preview.next_expires_at,
            Some(Utc.with_ymd_and_hms(2099, 6, 25, 8, 30, 0).unwrap())
        );
        assert!(parse_reset_preview(&json!({}), now()).is_err());
    }

    fn reset_test_provider(http: Arc<FakeHttp>) -> CodexProvider {
        let credentials = json!({
            "auth_mode": "chatgpt",
            "tokens": {"access_token": "test-token", "account_id": "account-123"}
        })
        .to_string();
        CodexProvider::new(Arc::new(ProviderContext {
            claude_dir: "/nonexistent/usage-test-claude".into(),
            codex_dir: "/nonexistent/usage-test-codex".into(),
            grok_dir: "/nonexistent/usage-test-grok".into(),
            http,
            keychain: Arc::new(move |_, _| Some(credentials.clone())),
            now: Arc::new(now),
        }))
    }

    fn response(status: u16, body: Value) -> HttpResponse {
        HttpResponse {
            status,
            headers: Default::default(),
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

    #[tokio::test]
    async fn oauth_refresh_hint_never_turns_network_failure_into_expired_login() {
        let http = Arc::new(FakeHttp::new());
        http.route(USAGE_URL, Err(TransportError("offline".into())));
        let credentials = json!({
            "auth_mode":"chatgpt", "last_refresh":"2020-01-01T00:00:00Z",
            "tokens":{"access_token":"opaque-token", "account_id":"workspace"}
        })
        .to_string();
        let provider = CodexProvider::new(Arc::new(ProviderContext {
            claude_dir: "/nonexistent/usage-test-claude".into(),
            codex_dir: "/nonexistent/usage-test-codex".into(),
            grok_dir: "/nonexistent/usage-test-grok".into(),
            http,
            keychain: Arc::new(move |_, _| Some(credentials.clone())),
            now: Arc::new(now),
        }));
        let snapshot = provider.query().await;
        assert!(snapshot.transient);
        assert_eq!(snapshot.credential_status, CredentialStatus::Valid);
        assert!(snapshot.error.unwrap().contains("网络错误"));
    }

    #[tokio::test]
    async fn oauth_401_distinguishes_access_expiry_revocation_and_permission() {
        for (http_status, code, expected) in [
            (401, "token_expired", CredentialStatus::RefreshRequired),
            (401, "refresh_token_invalidated", CredentialStatus::Expired),
            (401, "refresh_token_reused", CredentialStatus::Expired),
            (401, "unknown", CredentialStatus::Valid),
            (403, "unknown", CredentialStatus::Valid),
        ] {
            let http = Arc::new(FakeHttp::new());
            http.route(
                USAGE_URL,
                Ok(response(http_status, json!({"error":{"code":code}}))),
            );
            let provider = reset_test_provider(http);
            let snapshot = provider.query().await;
            assert_eq!(snapshot.credential_status, expected, "{http_status} {code}");
            assert!(!snapshot.transient);
            assert!(!snapshot.success);
        }
        let response = response(401, json!({"error":"invalid_grant"}));
        assert_eq!(auth_failure(&response).0, CredentialStatus::Expired);
    }

    #[tokio::test]
    async fn oauth_401_retries_only_a_rotated_token_for_the_same_account() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct TokenHttp {
            calls: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait]
        impl crate::http::HttpClient for TokenHttp {
            async fn request(
                &self,
                _: &str,
                _: &str,
                headers: &[(&str, String)],
                _: Option<Vec<u8>>,
            ) -> Result<HttpResponse, TransportError> {
                let token = headers
                    .iter()
                    .find(|(name, _)| *name == "Authorization")
                    .unwrap()
                    .1
                    .clone();
                self.calls.lock().unwrap().push(token.clone());
                Ok(if token == "Bearer old" {
                    response(401, json!({"error":{"code":"unknown"}}))
                } else {
                    response(200, json!({"rate_limit":{}}))
                })
            }
        }
        for (next_account, expected_calls, success) in
            [("workspace-a", 2, true), ("workspace-b", 1, false)]
        {
            let reads = Arc::new(AtomicUsize::new(0));
            let keychain_reads = Arc::clone(&reads);
            let http = Arc::new(TokenHttp {
                calls: std::sync::Mutex::new(Vec::new()),
            });
            let provider = CodexProvider::new(Arc::new(ProviderContext {
                claude_dir: "/nonexistent/usage-test-claude".into(),
                codex_dir: "/nonexistent/usage-test-codex".into(),
                grok_dir: "/nonexistent/usage-test-grok".into(),
                http: Arc::clone(&http) as Arc<dyn crate::http::HttpClient>,
                keychain: Arc::new(move |_, _| {
                    let first = keychain_reads.fetch_add(1, Ordering::SeqCst) == 0;
                    Some(
                        json!({"auth_mode":"chatgpt", "tokens": {
                            "access_token": if first {"old"} else {"new"},
                            "account_id": if first {"workspace-a"} else {next_account}
                        }})
                        .to_string(),
                    )
                }),
                now: Arc::new(now),
            }));
            let snapshot = provider.query().await;
            assert_eq!(snapshot.success, success);
            assert_eq!(http.calls.lock().unwrap().len(), expected_calls);
            if !success {
                assert_eq!(snapshot.credential_status, CredentialStatus::Valid);
                assert!(snapshot.error.unwrap().contains("账号在查询期间已切换"));
            }
        }
    }

    #[tokio::test]
    async fn reset_requires_available_credit_before_post() {
        let http = Arc::new(FakeHttp::new());
        http.route(
            RESET_CREDITS_URL,
            Ok(response(200, json!({"available_count": 0}))),
        );
        let provider = reset_test_provider(Arc::clone(&http));
        assert!(provider.reset().is_some());
        let error = provider.consume_reset().await.unwrap_err();
        assert!(error.to_string().contains("没有可用"));
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "GET");
    }

    #[tokio::test]
    async fn reset_refuses_credentials_without_account_id() {
        let http = Arc::new(FakeHttp::new());
        let provider = CodexProvider::new(Arc::new(ProviderContext {
            claude_dir: "/nonexistent/usage-test-claude".into(),
            codex_dir: "/nonexistent/usage-test-codex".into(),
            grok_dir: "/nonexistent/usage-test-grok".into(),
            http: Arc::clone(&http) as Arc<dyn crate::http::HttpClient>,
            keychain: Arc::new(|_, _| {
                Some(
                    json!({"auth_mode": "chatgpt", "tokens": {"access_token": "test-token"}})
                        .to_string(),
                )
            }),
            now: Arc::new(now),
        }));
        let error = provider.consume_reset().await.unwrap_err();
        assert!(error.to_string().contains("account_id"));
        assert!(http.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reset_sends_one_confirmed_consume_request() {
        let http = Arc::new(FakeHttp::new());
        http.route(RESET_CONSUME_URL, Ok(response(200, json!({}))));
        http.route(
            RESET_CREDITS_URL,
            Ok(response(200, json!({"available_count": 2}))),
        );
        let provider = reset_test_provider(Arc::clone(&http));
        let preview = provider.preview_reset().await.unwrap();
        assert_eq!(preview.available, 2);
        provider.consume_reset().await.unwrap();
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls
                .iter()
                .map(|call| call.method.as_str())
                .collect::<Vec<_>>(),
            ["GET", "GET", "POST"]
        );
        assert_eq!(calls[2].url, RESET_CONSUME_URL);
        assert!(
            calls[2]
                .headers
                .iter()
                .any(|(name, value)| name == "ChatGPT-Account-Id" && value == "account-123")
        );
        assert!(
            calls[2]
                .headers
                .iter()
                .any(|(name, value)| name == "originator" && value == "Codex Desktop")
        );
        let body: Value = serde_json::from_slice(calls[2].body.as_ref().unwrap()).unwrap();
        let request_id = body["redeem_request_id"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(request_id).is_ok());
    }

    #[tokio::test]
    async fn failed_consume_does_not_retry_automatically() {
        let http = Arc::new(FakeHttp::new());
        http.route(
            RESET_CONSUME_URL,
            Err(TransportError("connection closed".into())),
        );
        http.route(
            RESET_CREDITS_URL,
            Ok(response(200, json!({"available_count": 1}))),
        );
        let provider = reset_test_provider(Arc::clone(&http));
        let error = provider.consume_reset().await.unwrap_err();
        assert!(error.to_string().contains("结果未知"));
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.iter().filter(|call| call.method == "POST").count(), 1);
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
