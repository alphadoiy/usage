//! Bark delivery is injected through session flags, never written to a Codex home.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Error, Result};
use crate::logging;
mod content;
mod presence;
mod source;
#[cfg(all(test, unix))]
mod tests;

pub(super) const CONTEXT_ENV: &str = "USAGE_CODEX_BARK_CONTEXT";
const MAX_INPUT: u64 = 1024 * 1024;

#[derive(Subcommand, Debug)]
pub enum BarkCommand {
    /// 保存 Bark 推送 URL；只对 usage 启动的 Codex 注入通知
    Configure {
        #[command(flatten)]
        url: UrlInput,
        /// 不提醒本轮完成
        #[arg(long)]
        no_completion: bool,
        /// 不注入请求审批 hook
        #[arg(long)]
        no_approval: bool,
    },
    /// 查看通知开关与服务器（不显示设备密钥）
    Status,
    /// 关闭通知，保留 Bark 配置；对运行中的会话也生效
    Disable,
    /// 重新启用已保存的 Bark 配置
    Enable,
    /// 向已配置的设备发送测试推送
    Test,
    /// 查看跨通知进程的 Bark 日志；不初始化日志系统
    Logs {
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=10000))]
        tail: u32,
        /// 筛选某次 usage 启动实例的运行 ID
        #[arg(long)]
        run: Option<uuid::Uuid>,
    },
}

#[derive(Args, Debug)]
#[group(required = true, multiple = false)]
pub struct UrlInput {
    /// 从 stdin 读取 Bark App 中的推送 URL
    #[arg(long)]
    url_stdin: bool,
    /// 从指定环境变量读取 URL，命令行不携带设备密钥
    #[arg(long)]
    url_env: Option<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Event {
    Complete,
    Approval,
}

#[derive(Serialize, Deserialize)]
struct Settings {
    enabled: bool,
    server: String,
    device_key: String,
    completion: bool,
    approval: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Context {
    root: PathBuf,
    #[serde(default)]
    home: Option<PathBuf>,
    account: String,
    run_id: String,
    previous_notify: Vec<String>,
}

fn settings_path(root: &Path) -> PathBuf {
    root.join("bark.json")
}

fn read_settings(root: &Path) -> Result<Option<Settings>> {
    let path = settings_path(root);
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|_| Error::message("Bark 配置损坏，请重新 configure")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(Error::message("无法读取 Bark 配置")),
    }
}

fn save(root: &Path, settings: &Settings) -> Result<()> {
    super::private_dir(root)?;
    super::private_write(
        &settings_path(root),
        &serde_json::to_vec_pretty(settings).map_err(|_| Error::message("无法保存 Bark 配置"))?,
    )
}

fn from_url(text: &str, completion: bool, approval: bool) -> Result<Settings> {
    let mut url = reqwest::Url::parse(text.trim())
        .map_err(|_| Error::message("Bark URL 无效；使用 App 中的 https://服务器/设备密钥 URL"))?;
    if !["https", "http"].contains(&url.scheme())
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::message(
            "Bark URL 必须是 HTTP(S)，且不能包含用户名或密码",
        ));
    }
    let device_key = url
        .path_segments()
        .and_then(|mut segments| segments.next())
        .filter(|key| !key.is_empty() && !key.contains('%'))
        .ok_or_else(|| Error::message("Bark URL 缺少设备密钥"))?
        .to_string();
    // Bark's copied test URL may include a sample title/body. Never retain these.
    url.set_path("/push");
    url.set_query(None);
    url.set_fragment(None);
    Ok(Settings {
        enabled: true,
        server: url.to_string(),
        device_key,
        completion,
        approval,
    })
}

pub async fn dispatch(root: &Path, command: BarkCommand) -> Result<()> {
    match command {
        BarkCommand::Logs { tail, run } => {
            let run = run.map(|id| id.to_string());
            for line in logging::recent_bark(tail as usize, run.as_deref())? {
                println!("{line}");
            }
        }
        BarkCommand::Configure {
            url,
            no_completion,
            no_approval,
        } => {
            if no_completion && no_approval {
                return Err(Error::message(
                    "至少启用一种通知；关闭全部通知请使用 bark disable",
                ));
            }
            let text = if let Some(name) = url.url_env {
                std::env::var(name).map_err(|_| Error::message("指定环境变量不存在或不是文本"))?
            } else {
                let mut text = String::new();
                std::io::stdin().take(4097).read_to_string(&mut text)?;
                if text.len() > 4096 {
                    return Err(Error::message("Bark URL 过长"));
                }
                text
            };
            save(root, &from_url(&text, !no_completion, !no_approval)?)?;
            println!("Bark 已启用；仅 usage 启动的 Codex 会话注入通知。未发送推送。");
            if !no_approval {
                println!(
                    "首次启动后用 /hooks 信任 usage 的 PermissionRequest hook，才能收到审批请求提醒。"
                );
            }
        }
        BarkCommand::Status => match read_settings(root)? {
            Some(settings) => {
                println!(
                    "Bark：{} · 完成 {} · 请求审批 {}",
                    if settings.enabled {
                        "已启用"
                    } else {
                        "已关闭"
                    },
                    settings.completion,
                    settings.approval
                );
                println!("服务器：{}", settings.server);
                println!("自动通知：仅锁屏或所有显示器熄屏时发送");
                println!("当前状态：{}", presence::current().label());
            }
            None => println!("Bark：未配置"),
        },
        BarkCommand::Enable | BarkCommand::Disable => {
            let mut settings = read_settings(root)?.ok_or_else(|| {
                Error::message("请先使用 usage codex bark configure 保存推送 URL")
            })?;
            settings.enabled = matches!(command, BarkCommand::Enable);
            save(root, &settings)?;
            println!(
                "Bark：{}",
                if settings.enabled {
                    "已启用"
                } else {
                    "已关闭"
                }
            );
        }
        BarkCommand::Test => {
            let settings = read_settings(root)?.ok_or_else(|| Error::message("Bark 尚未配置"))?;
            send(
                &settings,
                "usage · 测试通知",
                "Codex 通知通道已连接",
                "usage-test",
                false,
            )
            .await?;
            println!("Bark 已接受测试推送；请检查 iPhone / Apple Watch。");
        }
    }
    Ok(())
}

/// Only the child receives this environment and these one-off configuration flags.
pub(super) fn inject(
    command: &mut Command,
    root: &Path,
    home: &Path,
    account: &str,
    run_id: &str,
    saved_config: &str,
) -> Result<()> {
    command.env_remove(CONTEXT_ENV);
    let Some(settings) = read_settings(root)?.filter(|s| s.enabled) else {
        return Ok(());
    };
    let executable = std::env::current_exe()?;
    let config: toml::Value =
        toml::from_str(saved_config).map_err(|_| Error::message("无法读取原通知配置"))?;
    let previous_notify: Vec<String> = config
        .get("notify")
        .map(|value| value.clone().try_into())
        .transpose()
        .map_err(|_| Error::message("账号的 notify 必须是字符串数组"))?
        .unwrap_or_default();
    let context = Context {
        root: root.to_path_buf(),
        home: Some(home.to_path_buf()),
        account: account.into(),
        run_id: run_id.into(),
        previous_notify,
    };
    command.env(
        CONTEXT_ENV,
        serde_json::to_string(&context).map_err(|_| Error::message("无法创建通知上下文"))?,
    );
    // Keep the completion wrapper even for approval-only delivery so an account's
    // existing notify command remains active and is invoked exactly once.
    if settings.completion || !context.previous_notify.is_empty() {
        let argv = vec![
            executable.to_string_lossy().into_owned(),
            "codex".into(),
            "notify".into(),
            "--event".into(),
            "complete".into(),
        ];
        let value =
            toml::Value::try_from(argv).map_err(|_| Error::message("无法创建通知启动参数"))?;
        command.args(["-c", &format!("notify={value}")]);
    }
    if settings.approval {
        let callback = format!(
            "{} codex notify --event approval",
            super::shell_quote(&executable.to_string_lossy())
        );
        let groups = toml::Value::try_from(vec![json!({"hooks":[{
            "type":"command", "command":callback, "async":true, "timeout":8
        }]})])
        .map_err(|_| Error::message("无法创建审批通知 hook"))?;
        command.args(["-c", &format!("hooks.PermissionRequest={groups}")]);
    }
    tracing::info!(
        event = "bark_injected",
        completion = settings.completion,
        approval = settings.approval
    );
    Ok(())
}

/// Callback errors never interfere with Codex or decide a permission request.
pub async fn callback(event: Event, payload: Option<String>) {
    if let Err(_error) = callback_inner(event, payload).await {
        // reqwest errors can contain the destination URL; never print them.
        tracing::warn!(event = "bark_delivery_failed");
    }
    if matches!(event, Event::Approval) {
        println!("{{}}");
    }
}

async fn callback_inner(event: Event, payload: Option<String>) -> Result<()> {
    let Some(context) = std::env::var(CONTEXT_ENV).ok() else {
        tracing::info!(event = "bark_ignored", reason = "context_missing");
        return Ok(());
    };
    let mut context: Context =
        serde_json::from_str(&context).map_err(|_| Error::message("通知上下文无效"))?;
    // Running sessions started before this field was added already inherit the
    // managed CODEX_HOME. Keep their callbacks compatible with the new filter.
    if context.home.is_none() {
        context.home = std::env::var_os("CODEX_HOME").map(PathBuf::from);
    }
    uuid::Uuid::parse_str(&context.run_id).map_err(|_| Error::message("通知运行 ID 无效"))?;
    let payload = if let Some(payload) = payload {
        payload
    } else {
        let mut payload = String::new();
        std::io::stdin()
            .take(MAX_INPUT + 1)
            .read_to_string(&mut payload)?;
        payload
    };
    if payload.len() as u64 > MAX_INPUT {
        return Err(Error::message("通知事件过大"));
    }
    handle_event(&context, event, &payload, presence::current).await
}

async fn handle_event(
    context: &Context,
    event: Event,
    payload: &str,
    read_presence: impl Fn() -> presence::Presence,
) -> Result<()> {
    let mut op = logging::Operation::new("codex.bark_notify");
    op.link_run(&context.run_id);
    let span = op.span();
    let result = logging::instrument(
        span,
        handle_event_logged(context, event, payload, read_presence, &mut op),
    )
    .await;
    op.finish(result.is_ok());
    result
}

async fn handle_event_logged(
    context: &Context,
    event: Event,
    payload: &str,
    read_presence: impl Fn() -> presence::Presence,
    op: &mut logging::Operation,
) -> Result<()> {
    op.begin("event_decode");
    let data: Value = serde_json::from_str(payload).map_err(|_| Error::message("通知事件无效"))?;
    let kind = match event {
        Event::Complete if data["type"] == "agent-turn-complete" => "complete",
        Event::Approval if data["hook_event_name"] == "PermissionRequest" => "approval",
        _ => {
            tracing::info!(event = "bark_ignored", reason = "unsupported_event");
            return Ok(());
        }
    };
    let thread = data
        .get("thread-id")
        .or_else(|| data.get("thread_id"))
        .or_else(|| data.get("session_id"))
        .and_then(Value::as_str)
        .unwrap_or(&context.run_id);
    let turn = data
        .get("turn-id")
        .or_else(|| data.get("turn_id"))
        .and_then(Value::as_str);
    let mut hash = Sha256::new();
    hash.update(kind);
    hash.update(thread);
    hash.update(if matches!(event, Event::Complete) {
        turn.unwrap_or(payload)
    } else {
        payload
    });
    let event_id = format!("{:x}", hash.finalize());
    tracing::info!(
        event = "bark_event_received",
        kind,
        event_id,
        callback_type = data["type"].as_str().or(data["hook_event_name"].as_str()),
        thread_id = thread,
        turn_id = turn,
        client = data["client"].as_str(),
        account = context.account,
        project = content::project(&data),
        payload_bytes = payload.len(),
        tool_name = data["tool_name"].as_str(),
        agent_id = data["agent_id"].as_str(),
        agent_type = data["agent_type"].as_str(),
        input_message_count = data["input-messages"].as_array().map(Vec::len)
    );
    if matches!(event, Event::Complete) {
        let reply = content::reply(&data).unwrap_or("");
        tracing::info!(
            event = "bark_completion_content",
            assistant_chars = reply.chars().count(),
            assistant_preview = content::excerpt(reply, 200)
        );
    }
    if matches!(event, Event::Complete)
        && let Some((program, args)) = context.previous_notify.split_first()
    {
        let mut previous = Command::new(program);
        previous.args(args).arg(payload).env_remove(CONTEXT_ENV);
        op.begin("previous_notify");
        let output = crate::credentials::output_with_timeout(previous, Duration::from_secs(10));
        if output.is_none_or(|output| !output.status.success()) {
            tracing::warn!(event = "bark_previous_notify_failed");
        }
    }
    let Some(settings) = op
        .step("settings_read", || read_settings(&context.root))?
        .filter(|s| s.enabled)
    else {
        tracing::info!(event = "bark_suppressed", reason = "disabled");
        return Ok(());
    };
    if (matches!(event, Event::Complete) && !settings.completion)
        || (matches!(event, Event::Approval) && !settings.approval)
    {
        tracing::info!(event = "bark_suppressed", reason = "event_disabled");
        return Ok(());
    }
    if matches!(event, Event::Complete) {
        op.begin("thread_source_check");
        let origin = source::classify(context.home.as_deref(), thread);
        tracing::info!(event = "bark_thread_source", origin = origin.label());
        if origin != source::Origin::User {
            tracing::info!(event = "bark_suppressed", reason = origin.reason());
            return Ok(());
        }
    }
    op.begin("presence_check");
    let state = read_presence();
    if state != presence::Presence::Away {
        tracing::info!(
            event = "bark_suppressed",
            reason = if state == presence::Presence::Present {
                "screen_active"
            } else {
                "screen_state_unknown"
            }
        );
        return Ok(());
    }
    let (title, body) = content::notification(event, &data, &context.account);
    tracing::info!(
        event = "bark_notification_prepared",
        title,
        body_preview = content::excerpt(&body, 300),
        body_chars = body.chars().count()
    );
    op.begin("bark_request");
    let result = send(
        &settings,
        &title,
        &body,
        &format!("usage-codex-{}", context.run_id),
        matches!(event, Event::Approval),
    )
    .await;
    if result.is_err() {
        op.fail("bark_request");
    }
    result
}

async fn send(
    settings: &Settings,
    title: &str,
    body: &str,
    group: &str,
    attention: bool,
) -> Result<()> {
    let started = Instant::now();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Error::message("无法创建 Bark HTTP 客户端"))?;
    tracing::info!(event = "bark_request_started", timeout_seconds = 5);
    let response = client
        .post(&settings.server)
        .json(&json!({
            "device_key":settings.device_key, "title":title, "body":body, "group":group,
            "level": if attention { "timeSensitive" } else { "active" },
        }))
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(
                event = "bark_request_failed",
                reason = if error.is_timeout() {
                    "timeout"
                } else if error.is_connect() {
                    "connection"
                } else {
                    "transport"
                },
                elapsed_ms = started.elapsed().as_millis() as u64
            );
            Error::message("Bark 请求失败")
        })?;
    tracing::info!(
        event = "bark_response_received",
        http_status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis() as u64
    );
    if !response.status().is_success() {
        tracing::warn!(
            event = "bark_request_failed",
            reason = "http_status",
            http_status = response.status().as_u16()
        );
        return Err(Error::message("Bark 返回 HTTP 错误"));
    }
    let response: Value = response.json().await.map_err(|_| {
        tracing::warn!(event = "bark_request_failed", reason = "invalid_response");
        Error::message("Bark 响应无效")
    })?;
    if response["code"] != 200 {
        tracing::warn!(
            event = "bark_request_failed",
            reason = "backend_rejected",
            backend_code = response["code"].as_i64()
        );
        return Err(Error::message("Bark 未接受推送"));
    }
    tracing::info!(
        event = "bark_delivered",
        attention,
        elapsed_ms = started.elapsed().as_millis() as u64
    );
    Ok(())
}
