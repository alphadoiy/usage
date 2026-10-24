//! Claude Code 官方用量。
//!
//! 凭据顺序与 cc-switch 一致：macOS Keychain「Claude Code-credentials」，
//! 再回退 ~/.claude/.credentials.json。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{ProviderContext, UsageProvider};
use crate::credentials::{CLAUDE_KEYCHAIN_SERVICE, clean_email, parse_claude_credentials};
use crate::http::{TransportError, body_excerpt, is_transient_status};
use crate::labels::KNOWN_CLAUDE_TIERS;
use crate::model::{
    CredentialStatus, ExtraUsage, QuotaTier, UsageSnapshot, snapshot_error, snapshot_not_found,
    snapshot_ok, snapshot_transient,
};
use crate::timeutil::parse_time;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

pub struct ClaudeProvider {
    ctx: Arc<ProviderContext>,
    profile_cache: Mutex<Option<(String, String)>>,
}

impl ClaudeProvider {
    pub const ID: &'static str = "claude";
    pub const NAME: &'static str = "Claude Code";

    pub fn new(ctx: Arc<ProviderContext>) -> Self {
        Self {
            ctx,
            profile_cache: Mutex::new(None),
        }
    }
}

#[async_trait]
impl UsageProvider for ClaudeProvider {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn display_name(&self) -> &'static str {
        Self::NAME
    }

    async fn query(&self) -> UsageSnapshot {
        let now = (self.ctx.now)();
        let ctx = Arc::clone(&self.ctx);
        let (token, status, message) = match crate::background::blocking(move || {
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
                message.unwrap_or_else(|| "无法解析 Claude 凭据".into()),
                now,
            ),
            CredentialStatus::Expired
            | CredentialStatus::RefreshRequired
            | CredentialStatus::Valid => {
                let Some(token) = token else {
                    return snapshot_error(
                        Self::ID,
                        Self::NAME,
                        status,
                        message.unwrap_or_else(|| "没有可用的 Claude 访问令牌".into()),
                        now,
                    );
                };
                let cached_email = self.profile_cache.lock().ok().and_then(|cache| {
                    cache.as_ref().and_then(|(cached_token, email)| {
                        (cached_token == &token).then(|| email.clone())
                    })
                });
                let (mut result, email) =
                    tokio::join!(query_usage(&self.ctx, &token, now), async {
                        if let Some(email) = cached_email {
                            return Some(email);
                        }
                        let email = tokio::time::timeout(
                            Duration::from_secs(3),
                            query_profile(&self.ctx, &token),
                        )
                        .await
                        .ok()
                        .flatten();
                        if let Some(email) = &email
                            && let Ok(mut cache) = self.profile_cache.lock()
                        {
                            *cache = Some((token.clone(), email.clone()));
                        }
                        email
                    });
                if status == CredentialStatus::Expired && !result.success {
                    snapshot_error(
                        Self::ID,
                        Self::NAME,
                        CredentialStatus::Expired,
                        message
                            .unwrap_or_else(|| "OAuth 令牌已过期，请重新登录 Claude Code".into()),
                        now,
                    )
                } else {
                    result.account_email = email;
                    result
                }
            }
        }
    }
}

async fn query_profile(ctx: &ProviderContext, token: &str) -> Option<String> {
    let response = ctx
        .http
        .request(
            "GET",
            PROFILE_URL,
            &[
                ("Authorization", format!("Bearer {token}")),
                ("anthropic-beta", "oauth-2025-04-20".into()),
                ("Accept", "application/json".into()),
            ],
            None,
        )
        .await
        .ok()?;
    if !(200..300).contains(&response.status) {
        return None;
    }
    let payload: Value = serde_json::from_slice(&response.body).ok()?;
    payload
        .pointer("/account/email")
        .and_then(Value::as_str)
        .and_then(clean_email)
}

pub fn parse_claude_quota(body: &Value, now: DateTime<Utc>) -> UsageSnapshot {
    let Some(root) = body.as_object() else {
        return snapshot_error(
            ClaudeProvider::ID,
            ClaudeProvider::NAME,
            CredentialStatus::Valid,
            "用量响应不是 JSON 对象",
            now,
        );
    };
    let mut tiers = Vec::new();
    for name in KNOWN_CLAUDE_TIERS {
        if let Some((utilization, resets_at)) = usage_window(root.get(*name)) {
            tiers.push(QuotaTier {
                name: (*name).to_string(),
                utilization,
                resets_at: resets_at.as_deref().and_then(parse_time),
            });
        }
    }
    for (key, value) in root {
        if key == "extra_usage" || key == "limits" || KNOWN_CLAUDE_TIERS.contains(&key.as_str()) {
            continue;
        }
        if let Some((utilization, resets_at)) = usage_window(Some(value)) {
            tiers.push(QuotaTier {
                name: key.clone(),
                utilization,
                resets_at: resets_at.as_deref().and_then(parse_time),
            });
        }
    }

    let mut seen = HashSet::new();
    if let Some(limits) = root.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let Some(scoped) = scoped_tier(limit) else {
                continue;
            };
            if !seen.insert(scoped.name.clone()) {
                continue;
            }
            if let Some(existing) = tiers.iter_mut().find(|tier| tier.name == scoped.name) {
                *existing = scoped;
            } else {
                tiers.push(scoped);
            }
        }
    }
    tiers.sort_by_key(|tier| {
        KNOWN_CLAUDE_TIERS
            .iter()
            .position(|name| *name == tier.name)
            .unwrap_or(KNOWN_CLAUDE_TIERS.len())
    });
    snapshot_ok(
        ClaudeProvider::ID,
        ClaudeProvider::NAME,
        tiers,
        extra_usage(root.get("extra_usage")),
        now,
    )
}

fn read_credentials(
    ctx: &ProviderContext,
    now: DateTime<Utc>,
) -> crate::credentials::CredentialRead {
    if let Some(raw) = (ctx.keychain)(CLAUDE_KEYCHAIN_SERVICE, None) {
        return parse_claude_credentials(&raw, now);
    }
    let path = ctx.claude_dir.join(".credentials.json");
    if !path.exists() {
        return (None, CredentialStatus::NotFound, None);
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_claude_credentials(&content, now),
        Err(error) => (
            None,
            CredentialStatus::ParseError,
            Some(format!("无法读取 Claude 凭据: {error}")),
        ),
    }
}

async fn query_usage(ctx: &ProviderContext, token: &str, now: DateTime<Utc>) -> UsageSnapshot {
    let response = match ctx
        .http
        .request(
            "GET",
            USAGE_URL,
            &[
                ("Authorization", format!("Bearer {token}")),
                ("anthropic-beta", "oauth-2025-04-20".into()),
                ("Accept", "application/json".into()),
            ],
            None,
        )
        .await
    {
        Ok(response) => response,
        Err(TransportError(error)) => {
            return snapshot_transient(
                ClaudeProvider::ID,
                ClaudeProvider::NAME,
                format!("网络错误: {error}"),
            );
        }
    };
    if response.status == 401 || response.status == 403 {
        return snapshot_error(
            ClaudeProvider::ID,
            ClaudeProvider::NAME,
            CredentialStatus::Expired,
            format!(
                "鉴权失败 (HTTP {})，请重新登录 Claude Code",
                response.status
            ),
            now,
        );
    }
    if is_transient_status(response.status) {
        return snapshot_transient(
            ClaudeProvider::ID,
            ClaudeProvider::NAME,
            format!("服务暂时不可用 (HTTP {})", response.status),
        );
    }
    if !(200..300).contains(&response.status) {
        return snapshot_error(
            ClaudeProvider::ID,
            ClaudeProvider::NAME,
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
                ClaudeProvider::ID,
                ClaudeProvider::NAME,
                CredentialStatus::Valid,
                format!("用量响应无法解析: {error}"),
                now,
            );
        }
    };
    parse_claude_quota(&payload, now)
}

fn usage_window(value: Option<&Value>) -> Option<(f64, Option<String>)> {
    let value = value?.as_object()?;
    let utilization = number(value.get("utilization")?)?;
    let resets_at = value
        .get("resets_at")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some((utilization, resets_at))
}

fn scoped_tier(limit: &Value) -> Option<QuotaTier> {
    let limit = limit.as_object()?;
    if limit.get("kind").and_then(Value::as_str) != Some("weekly_scoped")
        || limit.get("group").and_then(Value::as_str) != Some("weekly")
    {
        return None;
    }
    let scope = limit.get("scope")?.as_object()?;
    if scope.get("surface").is_some_and(|value| !value.is_null()) {
        return None;
    }
    let display_name = scope.get("model")?.get("display_name")?.as_str()?;
    let tier_name = match display_name.trim().to_ascii_lowercase().as_str() {
        "fable" => "seven_day_fable",
        "opus" => "seven_day_opus",
        "sonnet" => "seven_day_sonnet",
        _ => return None,
    };
    let percent = number(limit.get("percent")?)?;
    if percent < 0.0 {
        return None;
    }
    if limit
        .get("resets_at")
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return None;
    }
    let resets_at = limit
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(parse_time);
    Some(QuotaTier {
        name: tier_name.to_string(),
        utilization: percent,
        resets_at,
    })
}

fn extra_usage(value: Option<&Value>) -> Option<ExtraUsage> {
    let value = value?.as_object()?;
    Some(ExtraUsage {
        is_enabled: value
            .get("is_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        monthly_limit: value.get("monthly_limit").and_then(number),
        used_credits: value.get("used_credits").and_then(number),
        utilization: value.get("utilization").and_then(number),
        currency: value
            .get("currency")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;
    use crate::http::{FakeHttp, HttpResponse, TransportError};
    use crate::model::CredentialStatus;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap()
    }

    fn scoped(model: &str, percent: f64) -> Value {
        json!({
            "kind": "weekly_scoped",
            "group": "weekly",
            "percent": percent,
            "resets_at": "2026-09-12T00:00:00Z",
            "is_active": true,
            "scope": { "model": { "id": null, "display_name": model }, "surface": null }
        })
    }

    #[test]
    fn legacy_windows_and_extra_usage() {
        let quota = parse_claude_quota(
            &json!({
                "five_hour": { "utilization": 12.0, "resets_at": "2026-09-09T15:00:00Z" },
                "seven_day": { "utilization": 25.0, "resets_at": null },
                "seven_day_opus": { "utilization": 8.0 },
                "seven_day_sonnet": null,
                "other_window": { "utilization": 4.0 },
                "extra_usage": {
                    "is_enabled": true,
                    "monthly_limit": 100.0,
                    "used_credits": 9.0,
                    "utilization": 9.0,
                    "currency": "USD"
                }
            }),
            now(),
        );
        assert!(quota.success);
        let tiers: Vec<_> = quota
            .tiers
            .iter()
            .map(|tier| (tier.name.as_str(), tier.utilization))
            .collect();
        assert_eq!(
            tiers,
            vec![
                ("five_hour", 12.0),
                ("seven_day", 25.0),
                ("seven_day_opus", 8.0),
                ("other_window", 4.0),
            ]
        );
        let extra = quota.extra_usage.unwrap();
        assert!(extra.is_enabled);
        assert_eq!(extra.used_credits, Some(9.0));
        assert_eq!(extra.currency.as_deref(), Some("USD"));
    }

    #[test]
    fn scoped_fable_overrides_and_dedupes() {
        let mut inactive = scoped("  fAbLe  ", 0.0);
        inactive["is_active"] = json!(false);
        inactive["resets_at"] = Value::Null;
        let quota = parse_claude_quota(
            &json!({
                "seven_day_fable": { "utilization": 80.0, "resets_at": "2026-09-11T00:00:00Z" },
                "seven_day_opus": { "utilization": 20.0 },
                "seven_day_sonnet": { "utilization": 30.0 },
                "limits": [scoped("Sonnet", 5.0), inactive, scoped("Fable", 90.0), scoped("Opus", 6.0)]
            }),
            now(),
        );
        let tiers: Vec<_> = quota
            .tiers
            .iter()
            .map(|tier| (tier.name.as_str(), tier.utilization))
            .collect();
        assert_eq!(
            tiers,
            vec![
                ("seven_day_fable", 0.0),
                ("seven_day_opus", 6.0),
                ("seven_day_sonnet", 5.0),
            ]
        );
        assert!(quota.tiers[0].resets_at.is_none());
    }

    #[tokio::test]
    async fn keychain_token_is_sent_as_bearer() {
        let http = Arc::new(FakeHttp::new());
        http.route(
            USAGE_URL,
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: json!({"five_hour": {"utilization": 10}})
                    .to_string()
                    .into_bytes(),
            }),
        );
        let ctx = Arc::new(ProviderContext {
            claude_dir: std::env::temp_dir(),
            codex_dir: std::env::temp_dir(),
            grok_dir: std::env::temp_dir(),
            http: http.clone(),
            keychain: Arc::new(|service, _| {
                (service == CLAUDE_KEYCHAIN_SERVICE)
                    .then(|| json!({"claudeAiOauth": {"accessToken": "from-keychain"}}).to_string())
            }),
            now: Arc::new(now),
        });
        let snapshot = ClaudeProvider::new(ctx).query().await;
        assert!(snapshot.success);
        assert_eq!(snapshot.tiers[0].utilization, 10.0);
        let calls = http.calls.lock().unwrap();
        assert_eq!(
            calls[0]
                .headers
                .iter()
                .find(|(k, _)| k == "Authorization")
                .unwrap()
                .1,
            "Bearer from-keychain"
        );
    }

    #[tokio::test]
    async fn profile_email_is_cached_for_the_same_token() {
        let http = Arc::new(FakeHttp::new());
        http.route(
            USAGE_URL,
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: json!({"five_hour": {"utilization": 10}})
                    .to_string()
                    .into_bytes(),
            }),
        );
        http.route(
            PROFILE_URL,
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: json!({"account": {"email": "claude@example.com"}})
                    .to_string()
                    .into_bytes(),
            }),
        );
        let ctx = Arc::new(ProviderContext {
            claude_dir: std::env::temp_dir(),
            codex_dir: std::env::temp_dir(),
            grok_dir: std::env::temp_dir(),
            http: http.clone(),
            keychain: Arc::new(|_, _| {
                Some(json!({"claudeAiOauth": {"accessToken": "token"}}).to_string())
            }),
            now: Arc::new(now),
        });
        let provider = ClaudeProvider::new(ctx);
        for _ in 0..2 {
            let snapshot = provider.query().await;
            assert!(snapshot.success);
            assert_eq!(
                snapshot.account_email.as_deref(),
                Some("claude@example.com")
            );
            assert_eq!(snapshot.to_json()["accountEmail"], "claude@example.com");
        }
        let calls = http.calls.lock().unwrap();
        assert_eq!(
            calls.iter().filter(|call| call.url == PROFILE_URL).count(),
            1
        );
    }

    #[tokio::test]
    async fn network_error_is_transient() {
        let http = Arc::new(FakeHttp::new());
        http.route(USAGE_URL, Err(TransportError("timed out".into())));
        let dir = std::env::temp_dir().join(format!("usage-claude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("claude")).unwrap();
        std::fs::write(
            dir.join("claude/.credentials.json"),
            json!({"claudeAiOauth": {"accessToken": "token"}}).to_string(),
        )
        .unwrap();
        let ctx = Arc::new(ProviderContext {
            claude_dir: dir.join("claude"),
            codex_dir: dir.join("codex"),
            grok_dir: dir.join("grok"),
            http,
            keychain: Arc::new(|_, _| None),
            now: Arc::new(now),
        });
        let snapshot = ClaudeProvider::new(ctx).query().await;
        assert!(snapshot.transient);
        assert_eq!(snapshot.credential_status, CredentialStatus::Valid);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
