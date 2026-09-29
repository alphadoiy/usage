//! 从本机 Keychain 或 CLI 凭据文件读取令牌。只读，不刷新、不写回。

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::model::CredentialStatus;
use crate::timeutil::{is_expired_value, parse_time};

pub const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
pub const CODEX_KEYCHAIN_SERVICE: &str = "Codex Auth";
pub const GROK_OIDC_PREFIX: &str = "https://auth.x.ai::";
pub const GROK_LEGACY_SCOPE: &str = "https://accounts.x.ai/sign-in";
const CODEX_STALE_SECONDS: i64 = 8 * 24 * 3600;
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

pub type CredentialRead = (Option<String>, CredentialStatus, Option<String>);

pub fn read_macos_keychain(service: &str, account: Option<&str>) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let mut command = Command::new("security");
    command.args(["find-generic-password", "-s", service]);
    if let Some(account) = account {
        command.args(["-a", account]);
    }
    command.arg("-w");
    let output = output_with_timeout(command, KEYCHAIN_TIMEOUT)?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn output_with_timeout(mut command: Command, timeout: Duration) -> Option<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    child.wait_with_output().ok()
}

pub fn parse_claude_credentials(content: &str, now: DateTime<Utc>) -> CredentialRead {
    let parsed: Value = match serde_json::from_str(content) {
        Ok(value) => value,
        Err(error) => {
            return (
                None,
                CredentialStatus::ParseError,
                Some(format!("凭据 JSON 无法解析: {error}")),
            );
        }
    };
    let Some(root) = parsed.as_object() else {
        return (
            None,
            CredentialStatus::ParseError,
            Some("凭据 JSON 不是对象".into()),
        );
    };
    let entry = root
        .get("claudeAiOauth")
        .or_else(|| root.get("claude.ai_oauth"));
    let Some(entry) = entry.and_then(Value::as_object) else {
        return (
            None,
            CredentialStatus::ParseError,
            Some("凭据里没有 OAuth 条目".into()),
        );
    };
    let Some(token) = entry
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    else {
        return (
            None,
            CredentialStatus::ParseError,
            Some("accessToken 为空".into()),
        );
    };
    if entry
        .get("expiresAt")
        .is_some_and(|value| is_expired_value(value, now))
    {
        return (
            Some(token.to_string()),
            CredentialStatus::Expired,
            Some("OAuth 令牌已过期".into()),
        );
    }
    (Some(token.to_string()), CredentialStatus::Valid, None)
}

pub fn parse_codex_credentials(
    content: &str,
    now: DateTime<Utc>,
) -> (
    Option<String>,
    Option<String>,
    CredentialStatus,
    Option<String>,
) {
    let parsed: Value = match serde_json::from_str(content) {
        Ok(value) => value,
        Err(error) => {
            return (
                None,
                None,
                CredentialStatus::ParseError,
                Some(format!("Codex 凭据 JSON 无法解析: {error}")),
            );
        }
    };
    let Some(root) = parsed.as_object() else {
        return (
            None,
            None,
            CredentialStatus::ParseError,
            Some("Codex 凭据 JSON 不是对象".into()),
        );
    };
    if root.get("auth_mode").and_then(Value::as_str) != Some("chatgpt") {
        return (
            None,
            None,
            CredentialStatus::NotFound,
            Some("Codex 当前不是 ChatGPT 登录，无法查询官方额度".into()),
        );
    }
    let Some(tokens) = root.get("tokens").and_then(Value::as_object) else {
        return (
            None,
            None,
            CredentialStatus::ParseError,
            Some("Codex 凭据里没有 tokens".into()),
        );
    };
    let Some(token) = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
    else {
        return (
            None,
            None,
            CredentialStatus::ParseError,
            Some("access_token 为空".into()),
        );
    };
    let account = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if root
        .get("last_refresh")
        .and_then(Value::as_str)
        .is_some_and(|text| codex_refresh_is_stale(text, now))
    {
        return (
            Some(token.to_string()),
            account,
            CredentialStatus::Expired,
            Some("Codex 令牌距上次刷新已超过 8 天".into()),
        );
    }
    (
        Some(token.to_string()),
        account,
        CredentialStatus::Valid,
        None,
    )
}

pub fn parse_grok_auth(content: &str, now: DateTime<Utc>) -> CredentialRead {
    let parsed: Value = match serde_json::from_str(content) {
        Ok(value) => value,
        Err(error) => {
            return (
                None,
                CredentialStatus::ParseError,
                Some(format!("Grok 凭据 JSON 无法解析: {error}")),
            );
        }
    };
    let Some(root) = parsed.as_object() else {
        return (
            None,
            CredentialStatus::ParseError,
            Some("Grok auth.json 根节点不是对象".into()),
        );
    };
    let Some(entry) = select_grok_entry(root) else {
        return (
            None,
            CredentialStatus::ParseError,
            Some("Grok auth.json 里没有可用的访问令牌".into()),
        );
    };
    let Some(token) = entry
        .get("key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
    else {
        return (
            None,
            CredentialStatus::ParseError,
            Some("Grok auth.json 里没有可用的访问令牌".into()),
        );
    };
    if entry
        .get("expires_at")
        .and_then(Value::as_str)
        .is_some_and(|text| is_expired_value(&Value::String(text.to_string()), now))
    {
        return (
            Some(token.to_string()),
            CredentialStatus::Expired,
            Some("Grok OAuth 令牌已过期".into()),
        );
    }
    (Some(token.to_string()), CredentialStatus::Valid, None)
}

fn select_grok_entry(
    root: &serde_json::Map<String, Value>,
) -> Option<&serde_json::Map<String, Value>> {
    let mut oidc = None;
    let mut legacy = None;
    for (scope, value) in root {
        let Some(entry) = value.as_object() else {
            continue;
        };
        let has_key = entry
            .get("key")
            .and_then(Value::as_str)
            .is_some_and(|key| !key.is_empty());
        if !has_key {
            continue;
        }
        if scope.starts_with(GROK_OIDC_PREFIX) {
            oidc = Some(entry);
        } else if scope == GROK_LEGACY_SCOPE || scope.contains("/sign-in") {
            legacy = Some(entry);
        }
    }
    oidc.or(legacy)
}

fn codex_refresh_is_stale(last_refresh: &str, now: DateTime<Utc>) -> bool {
    let Some(refreshed) = parse_time(last_refresh) else {
        return false;
    };
    now.timestamp() - refreshed.timestamp() > CODEX_STALE_SECONDS
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn child_output_and_timeout_are_bounded() {
        let mut quick = Command::new("printf");
        quick.arg("ok");
        let output = output_with_timeout(quick, Duration::from_secs(1)).expect("quick command");
        assert_eq!(output.stdout, b"ok");

        let mut stalled = Command::new("sleep");
        stalled.arg("1");
        let started = Instant::now();
        assert!(output_with_timeout(stalled, Duration::from_millis(40)).is_none());
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
