//! Conservative launcher-side resolution; unsupported layers never claim a resource binding.
use std::path::Path;

use super::{AuthMode, config};
use crate::runs::{IdentityState, LaunchIdentity};

pub(super) struct Resolution {
    pub identity: LaunchIdentity,
    pub key: Option<String>,
    pub credential_env: Option<String>,
    pub gateway_supported: bool,
}

enum CommandPosition {
    Root,
    Exec,
    Prompt,
}

pub(super) fn resolve(
    saved: &str,
    mode: AuthMode,
    cwd: &Path,
    resume: bool,
    args: &[String],
    managed_key: Option<&str>,
    credentials: &dyn Fn(&str) -> Option<String>,
) -> Resolution {
    let mut identity = LaunchIdentity {
        state: IdentityState::Unresolved,
        agent: "codex".into(),
        auth_mode: mode.log_name().into(),
        endpoint: None,
        model: None,
        model_provider: None,
        credential_source: None,
        reason: None,
    };
    let mut credential_env = None;
    let mut gateway_supported = false;
    let resolved = (|| -> Result<Option<String>, &'static str> {
        if resume {
            return Err("会话恢复可能沿用会话内配置，尚未取得 Agent 确认");
        }
        if mode == AuthMode::Oauth {
            return Err("OAuth 运行尚未提供可核对的 API 资源身份");
        }
        // Avoid reimplementing Codex project trust, root markers and layered model defaults.
        for directory in cwd.ancestors() {
            if directory.join(".codex/config.toml").exists() {
                return Err("存在项目配置层，尚未取得 Agent 的实际配置确认");
            }
            if directory.join(".git").exists() {
                break;
            }
        }
        let mut document: toml::Value = toml::from_str(saved).map_err(|_| "保存的配置无法解析")?;
        let mut model = None;
        let mut iter = args.iter();
        let mut position = CommandPosition::Root;
        while let Some(arg) = iter.next() {
            if arg == "--" {
                break;
            }
            if !arg.starts_with('-') {
                position = match position {
                    CommandPosition::Root if matches!(arg.as_str(), "exec" | "e") => {
                        CommandPosition::Exec
                    }
                    CommandPosition::Root
                        if matches!(
                            arg.as_str(),
                            "resume"
                                | "fork"
                                | "review"
                                | "login"
                                | "logout"
                                | "app-server"
                                | "serve"
                                | "proxy"
                                | "debug"
                                | "mcp"
                                | "sandbox"
                        ) =>
                    {
                        return Err("此子命令的配置来源尚未解析");
                    }
                    CommandPosition::Exec
                        if matches!(arg.as_str(), "resume" | "fork" | "review" | "help") =>
                    {
                        return Err("此子命令的配置来源尚未解析");
                    }
                    _ => CommandPosition::Prompt,
                };
                continue;
            }
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(a, b)| (a, Some(b)));
            if matches!(
                flag,
                "--profile"
                    | "-p"
                    | "--oss"
                    | "--local-provider"
                    | "--cd"
                    | "-C"
                    | "--worktree"
                    | "-w"
            ) || (flag.starts_with("-p") && !flag.starts_with("--"))
            {
                return Err("profile、目录或本地提供方覆盖尚未解析");
            }
            if matches!(flag, "--config" | "-c")
                || (flag.starts_with("-c") && !flag.starts_with("--"))
            {
                let value = if flag != "-c" && flag.starts_with("-c") && !flag.starts_with("--") {
                    &arg[2..]
                } else {
                    inline
                        .or_else(|| iter.next().map(String::as_str))
                        .ok_or("配置覆盖缺少值")?
                };
                let (key, value) = value.split_once('=').ok_or("配置覆盖缺少等号")?;
                apply_override(&mut document, key, value)?;
            } else if matches!(flag, "--model" | "-m")
                || (flag.starts_with("-m") && !flag.starts_with("--"))
            {
                model = Some(
                    if flag != "-m" && flag.starts_with("-m") && !flag.starts_with("--") {
                        arg[2..].to_string()
                    } else {
                        inline
                            .or_else(|| iter.next().map(String::as_str))
                            .ok_or("模型覆盖缺少值")?
                            .to_string()
                    },
                );
            } else if [
                "--image",
                "-i",
                "--sandbox",
                "-s",
                "--ask-for-approval",
                "-a",
                "--enable",
                "--disable",
                "--add-dir",
                "--color",
                "--output-last-message",
                "-o",
                "--output-schema",
            ]
            .contains(&flag)
            {
                if inline.is_none() {
                    iter.next().ok_or("启动选项缺少值")?;
                }
            } else if ![
                "--no-daemon",
                "--json",
                "--full-auto",
                "--dangerously-bypass-approvals-and-sandbox",
                "--skip-git-repo-check",
                "--ephemeral",
                "--no-alt-screen",
                "--search",
                "--yolo",
                "-h",
                "--help",
                "-V",
                "--version",
            ]
            .contains(&flag)
            {
                return Err("存在尚未识别的启动选项");
            }
        }
        if document.get("profile").is_some() {
            return Err("保存的配置选择了尚未解析的原生 profile");
        }
        if document
            .get("models")
            .and_then(|models| models.get("new_thread"))
            .is_some()
            || document
                .get("forced_login_method")
                .is_some_and(|method| method.as_str() != Some("api"))
        {
            return Err("存在尚未解析的模型默认值或登录方式覆盖");
        }
        let provider = document
            .get("model_provider")
            .and_then(toml::Value::as_str)
            .ok_or("缺少明确的模型提供方")?;
        let table = document
            .get("model_providers")
            .and_then(|p| p.get(provider))
            .ok_or("模型提供方配置不存在")?;
        if table.as_table().is_none_or(|fields| {
            fields.keys().any(|field| {
                ![
                    "name",
                    "base_url",
                    "env_key",
                    "env_key_instructions",
                    "wire_api",
                    "requires_openai_auth",
                    "request_max_retries",
                    "stream_max_retries",
                    "stream_idle_timeout_ms",
                    "supports_websockets",
                ]
                .contains(&field.as_str())
            })
        }) {
            return Err("提供方含尚未解析的认证或路由字段");
        }
        if table
            .get("requires_openai_auth")
            .and_then(toml::Value::as_bool)
            == Some(true)
        {
            return Err("提供方使用了尚未解析的认证方式");
        }
        let base = table
            .get("base_url")
            .and_then(toml::Value::as_str)
            .ok_or("缺少明确的推理端点")?;
        config::validate_url(base).map_err(|_| "推理端点格式不适合记录")?;
        let selected_model = model
            .or_else(|| {
                document
                    .get("model")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string)
            })
            .filter(|m| !m.trim().is_empty() && !m.chars().any(char::is_control))
            .ok_or("缺少明确的模型")?;
        let env = table
            .get("env_key")
            .and_then(toml::Value::as_str)
            .ok_or("缺少明确的凭据引用")?;
        let (key, source) = if env == config::KEY_ENV {
            (managed_key.map(str::to_string), "managed_revision")
        } else {
            // These inherited names are explicitly removed by the managed launcher.
            let key = if super::CLEARED_AUTH_ENV.contains(&env) {
                None
            } else {
                credentials(env)
            };
            (key, "environment")
        };
        let key = key
            .filter(|k| !k.is_empty())
            .ok_or("运行采用的凭据不可用")?;
        identity.endpoint = Some(base.to_string());
        identity.model = Some(selected_model);
        identity.model_provider = Some(provider.into());
        identity.credential_source = Some(source.into());
        gateway_supported = table
            .get("wire_api")
            .and_then(toml::Value::as_str)
            .is_none_or(|api| matches!(api, "responses" | "chat"))
            && table
                .get("supports_websockets")
                .and_then(toml::Value::as_bool)
                != Some(true)
            && !args.iter().any(|a| {
                a == "--enable"
                    || a.starts_with("--enable=")
                    || a == "-c"
                    || a == "--config"
                    || a.starts_with("--config=")
                    || (a.starts_with("-c") && !a.starts_with("--"))
            })
            && ![
                "responses_websockets",
                "responses_websockets_v2",
                "respect_system_proxy",
            ]
            .iter()
            .any(|key| {
                document
                    .get("features")
                    .and_then(|f| f.get(key))
                    .and_then(toml::Value::as_bool)
                    == Some(true)
            });
        credential_env = Some(env.into());
        Ok(Some(key))
    })();
    match resolved {
        Ok(key) => {
            identity.state = IdentityState::Resolved;
            Resolution {
                identity,
                key,
                credential_env,
                gateway_supported,
            }
        }
        Err(reason) => {
            identity.reason = Some(reason.into());
            Resolution {
                identity,
                key: None,
                credential_env: None,
                gateway_supported: false,
            }
        }
    }
}

fn apply_override(document: &mut toml::Value, key: &str, raw: &str) -> Result<(), &'static str> {
    // Parse a sentinel separately to distinguish a dotted path from a table-valued assignment.
    let sentinel: toml::Value =
        toml::from_str(&format!("{key}=0")).map_err(|_| "配置覆盖路径无法解析")?;
    let mut cursor = &sentinel;
    let mut path = Vec::new();
    while let Some(table) = cursor.as_table() {
        if table.len() != 1 {
            return Err("配置覆盖路径不唯一");
        }
        let (part, value) = table.iter().next().ok_or("配置覆盖路径为空")?;
        path.push(part);
        cursor = value;
    }
    if cursor.as_integer() != Some(0) || path.is_empty() {
        return Err("配置覆盖路径无效");
    }
    if ["profile", "profiles"].contains(&path[0].as_str()) {
        return Err("原生 profile 覆盖尚未解析");
    }
    let parsed: toml::Value = toml::from_str(&format!("value={raw}"))
        .or_else(|_| toml::from_str(&format!("value={}", toml::Value::String(raw.into()))))
        .map_err(|_| "配置覆盖值无法解析")?;
    let table = parsed
        .as_table()
        .filter(|t| t.len() == 1)
        .ok_or("配置覆盖值不唯一")?;
    let value = table.get("value").ok_or("配置覆盖值不存在")?.clone();
    let mut target = document;
    for part in &path[..path.len() - 1] {
        let table = target.as_table_mut().ok_or("配置覆盖路径与已有字段冲突")?;
        target = table
            .entry((*part).clone())
            .or_insert_with(|| toml::Value::Table(Default::default()));
    }
    target
        .as_table_mut()
        .ok_or("配置覆盖路径不是表")?
        .insert(path[path.len() - 1].clone(), value);
    Ok(())
}

#[cfg(test)]
mod gateway_tests {
    use super::*;
    #[test]
    fn observation_requires_resolved_byok_http_and_unambiguous_overrides() {
        let path =
            std::env::temp_dir().join(format!("usage-gateway-resolution-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(path.join(".git")).unwrap();
        let config = config::new_config("https://upstream.invalid/v1", "m", None).unwrap();
        let resolve = |text: &str, resume, args: &[String]| {
            super::resolve(
                text,
                AuthMode::Byok,
                &path,
                resume,
                args,
                Some("synthetic"),
                &|_| None,
            )
        };
        assert!(resolve(&config, false, &[]).gateway_supported);
        assert!(resolve(&config, false, &["--model".into(), "other".into()]).gateway_supported);
        assert!(!resolve(&config, true, &[]).gateway_supported);
        assert!(!resolve(&config, false, &["-c".into(), "model='other'".into()]).gateway_supported);
        let mut websocket: toml::Value = toml::from_str(&config).unwrap();
        websocket["model_providers"]["usage_byok"]
            .as_table_mut()
            .unwrap()
            .insert("supports_websockets".into(), toml::Value::Boolean(true));
        let websocket = toml::to_string(&websocket).unwrap();
        assert!(!resolve(&websocket, false, &[]).gateway_supported);
        std::fs::create_dir_all(path.join(".codex")).unwrap();
        std::fs::write(path.join(".codex/config.toml"), "model='project'").unwrap();
        assert!(!resolve(&config, false, &[]).gateway_supported);
        std::fs::remove_dir_all(path).unwrap();
    }
}
