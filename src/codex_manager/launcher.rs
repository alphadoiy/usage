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
