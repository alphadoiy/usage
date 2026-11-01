//! Native process boundaries: argument policy and the lifetime of inherited run leases.
use super::{AuthMode, Error, Result};

/// Serialize integers directly. Going through serde_json::Value would turn
/// arbitrary-precision JSON numbers into private map wrappers in TOML.
pub(super) fn command_hook(command: &str, timeout: u64, background: bool) -> Result<toml::Value> {
    #[derive(serde::Serialize)]
    struct Handler<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        command: &'a str,
        timeout: u64,
        r#async: bool,
    }
    #[derive(serde::Serialize)]
    struct Group<'a> {
        hooks: [Handler<'a>; 1],
    }
    toml::Value::try_from(Group {
        hooks: [Handler {
            kind: "command",
            command,
            timeout,
            r#async: background,
        }],
    })
    .map_err(|_| Error::message("无法创建生命周期 hook"))
}

pub(super) fn validate_run_args(args: &[String], mode: AuthMode) -> Result<()> {
    let mut args = args.iter();
    let mut options = true;
    let mut positional = false;
    while let Some(arg) = args.next() {
        if options && arg == "--" {
            options = false;
            continue;
        }
        if options && arg.starts_with('-') {
            let flag = arg.split('=').next().unwrap_or(arg.as_str());
            if arg == "--remote" || arg.starts_with("--remote=") {
                return Err(Error::message(
                    "托管实例不支持 --remote；它会改变账号和配置来源",
                ));
            }
            if mode == AuthMode::Oauth
                && (arg.starts_with("-c")
                    || arg.starts_with("-p")
                    || ["--config", "--profile", "--oss", "--local-provider"].contains(&flag))
            {
                return Err(oauth_args_error());
            }
            if mode == AuthMode::Byok {
                if ["--profile", "--oss", "--local-provider"].contains(&flag)
                    || (arg.starts_with("-p") && !arg.starts_with("--"))
                {
                    return Err(Error::message(
                        "托管启动使用 Usage 选择的 Provider，不能通过 profile 或本地提供方参数改变配置来源",
                    ));
                }
                let raw = if matches!(arg.as_str(), "-c" | "--config") {
                    Some(
                        args.next()
                            .ok_or_else(|| Error::message("配置覆盖缺少值"))?
                            .as_str(),
                    )
                } else {
                    arg.strip_prefix("--config=")
                        .or_else(|| arg.strip_prefix("-c").filter(|_| !arg.starts_with("--")))
                };
                if let Some(raw) = raw {
                    let (key, _) = raw
                        .split_once('=')
                        .ok_or_else(|| Error::message("配置覆盖缺少等号"))?;
                    if super::config::protected_override(key) {
                        let field = key.trim().split('.').next().unwrap_or_default();
                        return Err(Error::message(format!(
                            "托管启动不能覆盖 {field}：账号通道、认证和状态目录由 Usage 管理；请通过 Usage 编辑 Provider"
                        )));
                    }
                    continue;
                }
            }
            // Skip values even when they resemble auth flags or subcommand names.
            if matches!(
                arg.as_str(),
                "--model"
                    | "-m"
                    | "--image"
                    | "-i"
                    | "--cd"
                    | "-C"
                    | "--sandbox"
                    | "-s"
                    | "--ask-for-approval"
                    | "-a"
                    | "--enable"
                    | "--disable"
                    | "--add-dir"
                    | "--color"
                    | "--output-last-message"
                    | "-o"
                    | "--output-schema"
                    | "--config"
                    | "-c"
                    | "--profile"
                    | "-p"
            ) {
                args.next();
            }
        } else if !positional {
            if mode == AuthMode::Oauth
                && ["login", "logout", "app-server", "serve", "proxy"].contains(&arg.as_str())
            {
                return Err(oauth_args_error());
            }
            positional = true;
        }
    }
    Ok(())
}

/// Explicit runtime overrides outrank native project layers. User model/effort flags may
/// follow these defaults; account overrides are rejected before materializing the Home.
pub(super) fn pin_managed_config(command: &mut std::process::Command, text: &str) -> Result<()> {
    let doc: toml::Value = toml::from_str(text).map_err(|_| Error::message("无法读取运行配置"))?;
    let account = super::config::account_settings(&doc);
    for (key, value) in account.as_table().expect("account table") {
        // Profiles/projects are configuration containers, not individual launch settings.
        if !matches!(key.as_str(), "profiles" | "projects" | "profile") {
            command.args(["-c", &format!("{key}={value}")]);
        }
    }
    for key in ["model", "model_reasoning_effort", "models"] {
        if let Some(value) = doc.get(key) {
            command.args(["-c", &format!("{key}={value}")]);
        }
    }
    // Project features must not silently switch a managed HTTP route to a different
    // transport. Other project features and all trust/permission decisions remain native.
    for key in [
        "responses_websockets",
        "responses_websockets_v2",
        "respect_system_proxy",
    ] {
        let value = doc
            .get("features")
            .and_then(|v| v.get(key))
            .cloned()
            .unwrap_or(toml::Value::Boolean(false));
        command.args(["-c", &format!("features.{key}={value}")]);
    }
    Ok(())
}

/// Codex splits `-c` keys on dots without TOML quoting, so a quoted segment would name a new,
/// incomplete provider. Only bare-key provider IDs can be addressed by a dotted override.
pub(super) fn provider_override(provider: &str, field: &str) -> Option<String> {
    (!provider.is_empty()
        && provider
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    .then(|| format!("model_providers.{provider}.{field}"))
}

fn oauth_args_error() -> Error {
    Error::message(
        "OAuth 托管启动不能覆盖 profile / -c 或运行认证服务命令；请通过账号编辑配置、通过登录功能重新认证",
    )
}

/// Keep the shared flock alive in the Codex process even if its launcher is killed.
#[cfg(unix)]
pub(super) fn inherit_run_lease(
    command: &mut std::process::Command,
    lease: &std::fs::File,
) -> std::io::Result<()> {
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    let inherited = lease.try_clone()?;
    // SAFETY: the closure owns its fd and performs only async-signal-safe fcntl calls
    // after fork. Only the child's descriptor becomes inheritable; the parent's
    // remains CLOEXEC, so unrelated concurrent commands cannot acquire this lease.
    unsafe {
        command.pre_exec(move || {
            let fd = inherited.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags == -1 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(test)]
mod managed_policy_tests {
    use super::*;

    #[test]
    fn managed_accounts_reject_channel_overrides_without_echoing_values() {
        for args in [
            vec!["-c", "model_provider='openai'"],
            vec!["--config=model_providers={}"],
            vec!["-cmodel_providers.usage_byok.base_url='https://private.invalid'"],
            vec!["-c", "forced_login_method='chatgpt'"],
            vec!["-c", "sqlite_home='/private'"],
            vec!["--profile=other"],
            vec!["--oss"],
        ] {
            let result = validate_run_args(
                &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                AuthMode::Byok,
            );
            let error = result.expect_err(&format!("{args:?}")).to_string();
            assert!(!error.contains("private"));
        }
    }

    #[test]
    fn managed_accounts_allow_preferences_and_respect_argument_boundaries() {
        for args in [
            vec!["-c", "model='chosen'", "--enable", "shell_snapshot"],
            vec!["-cmodel_reasoning_effort='low'"],
            vec!["--image", "--profile", "--", "-cmodel_provider=other"],
            vec!["exec", "--output-last-message", "--oss", "hello"],
        ] {
            validate_run_args(
                &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                AuthMode::Byok,
            )
            .unwrap();
        }
    }
}
