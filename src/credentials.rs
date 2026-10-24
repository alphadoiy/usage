//! 从本机 Keychain 或 CLI 凭据文件读取令牌。只读，不刷新、不写回。

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
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

pub fn clean_email(value: &str) -> Option<String> {
    let email = value.trim();
    if email.len() > 254
        || email.contains(char::is_whitespace)
        || email.contains(char::is_control)
        || email.matches('@').count() != 1
        || email.starts_with('@')
        || email.ends_with('@')
    {
        return None;
    }
    Some(email.to_string())
}

fn email_from_jwt(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    parts.next()?;
    let payload = parts.next()?;
    parts.next()?;
    if parts.next().is_some() || payload.len() > 8192 {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims.get("email")?.as_str().and_then(clean_email)
}

pub fn codex_email_from_credentials(content: &str) -> Option<String> {
    let root: Value = serde_json::from_str(content).ok()?;
    if root.get("auth_mode")?.as_str()? != "chatgpt" {
        return None;
    }
    let tokens = root.get("tokens")?;
    tokens
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(email_from_jwt)
        .or_else(|| {
            tokens
                .get("access_token")
                .and_then(Value::as_str)
                .and_then(email_from_jwt)
        })
}

pub fn grok_email_from_auth(content: &str) -> Option<String> {
    let root: Value = serde_json::from_str(content).ok()?;
    let entry = select_grok_entry(root.as_object()?)?;
    entry
        .get("email")
        .and_then(Value::as_str)
        .and_then(clean_email)
        .or_else(|| {
            entry
                .get("id_token")
                .and_then(Value::as_str)
                .and_then(email_from_jwt)
        })
        .or_else(|| {
            entry
                .get("key")
                .and_then(Value::as_str)
                .and_then(email_from_jwt)
        })
}

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

pub(crate) fn output_with_timeout(command: Command, timeout: Duration) -> Option<Output> {
    output_with_cancel(command, timeout, &std::sync::atomic::AtomicBool::new(false))
}

pub(crate) fn output_with_cancel(
    mut command: Command,
    timeout: Duration,
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<Output> {
    use std::sync::atomic::Ordering;
    if cancel.load(Ordering::Acquire) {
        return None;
    }
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // These captured-output probes run independently of the foreground terminal.
        // Own a group so cancellation also stops wrappers' children and pipe holders.
        command.process_group(0);
    }
    let mut child = command.spawn().ok()?;
    let started = Instant::now();
    let result = (|| {
        let mut stdout = child.stdout.take()?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: stdout owns this valid pipe fd; only its read end is changed.
            // Nonblocking reads also let the reader honor the deadline when a
            // descendant keeps the pipe open after the immediate child exits.
            unsafe {
                let fd = stdout.as_raw_fd();
                let flags = libc::fcntl(fd, libc::F_GETFL);
                if flags == -1 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
                    return None;
                }
            }
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("usage-child-output".into())
            .spawn(move || {
                let output = (|| {
                    const MAX_OUTPUT: usize = 4 * 1024 * 1024;
                    let mut output = Vec::new();
                    let mut buffer = [0u8; 8192];
                    while started.elapsed() < timeout {
                        match stdout.read(&mut buffer) {
                            Ok(0) => return Some(output),
                            Ok(n) if output.len() + n <= MAX_OUTPUT => {
                                output.extend_from_slice(&buffer[..n]);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                #[cfg(unix)]
                                {
                                    use std::os::fd::AsRawFd;
                                    let mut ready = libc::pollfd {
                                        fd: stdout.as_raw_fd(),
                                        events: libc::POLLIN,
                                        revents: 0,
                                    };
                                    let wait = timeout
                                        .saturating_sub(started.elapsed())
                                        .as_millis()
                                        .min(20)
                                        as i32;
                                    // SAFETY: stdout owns the descriptor and ready is a valid
                                    // single-element poll array. Wait for readiness instead
                                    // of adding a scheduling delay to every pipe-sized chunk.
                                    if unsafe { libc::poll(&mut ready, 1, wait) } < 0
                                        && std::io::Error::last_os_error().kind()
                                            != std::io::ErrorKind::Interrupted
                                    {
                                        return None;
                                    }
                                }
                                #[cfg(not(unix))]
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            _ => return None,
                        }
                    }
                    None
                })();
                let _ = sender.send(output);
            })
            .ok()?;
        let mut output = None;
        let status = loop {
            if started.elapsed() >= timeout || cancel.load(Ordering::Acquire) {
                return None;
            }
            if output.is_none() {
                match receiver.try_recv() {
                    Ok(Some(bytes)) => output = Some(bytes),
                    Ok(None) | Err(mpsc::TryRecvError::Disconnected) => return None,
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            // Keep the group leader unreaped while descendants hold its stdout.
            // On cancellation the PID still belongs to this command's group.
            if output.is_some()
                && let Some(status) = child.try_wait().ok()?
            {
                break status;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = output?;
        Some(Output {
            status,
            stdout,
            stderr: Vec::new(),
        })
    })();
    if result.is_none() {
        #[cfg(unix)]
        // SAFETY: this child was started in its own process group above. Never target
        // the caller's foreground group; descendants can outlive the direct child.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    result
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
    use serde_json::json;

    fn jwt(email: &str) -> String {
        let payload = URL_SAFE_NO_PAD.encode(json!({ "email": email }).to_string());
        format!("header.{payload}.signature")
    }

    #[test]
    fn codex_email_comes_from_current_chatgpt_token() {
        let content = json!({
            "auth_mode": "chatgpt",
            "tokens": { "id_token": jwt(" user@example.com "), "access_token": jwt("other@example.com") }
        })
        .to_string();
        assert_eq!(
            codex_email_from_credentials(&content).as_deref(),
            Some("user@example.com")
        );
        let api_key = content.replace("chatgpt", "apikey");
        assert_eq!(codex_email_from_credentials(&api_key), None);
        assert_eq!(codex_email_from_credentials("{}"), None);
    }

    #[test]
    fn grok_email_uses_the_selected_auth_entry() {
        let content = json!({
            "https://accounts.x.ai/sign-in": { "key": jwt("legacy@example.com") },
            "https://auth.x.ai::client": {
                "key": "opaque-token",
                "id_token": jwt("oidc@example.com")
            }
        })
        .to_string();
        assert_eq!(
            grok_email_from_auth(&content).as_deref(),
            Some("oidc@example.com")
        );
        assert_eq!(grok_email_from_auth("{}"), None);
    }

    #[test]
    fn invalid_email_is_not_shown() {
        assert_eq!(clean_email("not-an-email"), None);
        assert_eq!(clean_email("a@b.com\nsecret"), None);
        assert_eq!(email_from_jwt("opaque-token"), None);
    }

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

    #[cfg(unix)]
    #[test]
    fn child_output_drains_large_pipes_and_rejects_unbounded_output() {
        let mut large = Command::new("python3");
        large.args(["-c", "import sys; sys.stdout.write('x'*1048576)"]);
        let output = output_with_timeout(large, Duration::from_secs(2)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1024 * 1024);
        let mut unlimited = Command::new("python3");
        unlimited.args(["-c", "import sys; sys.stdout.write('x'*(5*1048576))"]);
        assert!(output_with_timeout(unlimited, Duration::from_secs(2)).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_stops_descendants_of_captured_output_commands() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let root =
            std::env::temp_dir().join(format!("usage-probe-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let ready = root.join("ready");
        let survived = root.join("survived");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "( : > \"$1\"; sleep 0.4; : > \"$2\" ) & wait",
                "fixture",
            ])
            .arg(&ready)
            .arg(&survived);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            output_with_cancel(command, Duration::from_secs(3), &worker_cancel)
        });
        let until = Instant::now() + Duration::from_secs(2);
        while !ready.exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        let started = ready.exists();
        cancel.store(true, Ordering::Release);
        assert!(worker.join().unwrap().is_none());
        assert!(
            started,
            "the descendant must be running before cancellation"
        );
        std::thread::sleep(Duration::from_millis(450));
        assert!(
            !survived.exists(),
            "the descendant continued after cancellation"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn completed_child_does_not_wait_forever_for_descendant_stdout() {
        let mut command = Command::new("python3");
        command.args([
            "-c",
            "import os,time; pid=os.fork(); time.sleep(0.7) if pid==0 else None",
        ]);
        let start = Instant::now();
        assert!(output_with_timeout(command, Duration::from_millis(100)).is_none());
        assert!(start.elapsed() < Duration::from_millis(600));
    }
}
