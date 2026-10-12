//! Bark delivery is injected through session flags, never written to a Codex home.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clap::{Args, Subcommand, ValueEnum};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Error, Result};

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

#[derive(Serialize, Deserialize)]
struct Context {
    root: PathBuf,
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
        return Ok(());
    };
    let context: Context =
        serde_json::from_str(&context).map_err(|_| Error::message("通知上下文无效"))?;
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
    let data: Value = serde_json::from_str(&payload).map_err(|_| Error::message("通知事件无效"))?;
    let (kind, status) = match event {
        Event::Complete if data["type"] == "agent-turn-complete" => ("complete", "本轮执行结束"),
        Event::Approval if data["hook_event_name"] == "PermissionRequest" => {
            ("approval", "请求权限审批")
        }
        _ => return Ok(()),
    };
    if matches!(event, Event::Complete)
        && let Some((program, args)) = context.previous_notify.split_first()
    {
        let mut previous = Command::new(program);
        previous.args(args).arg(&payload).env_remove(CONTEXT_ENV);
        let _ = crate::credentials::output_with_timeout(previous, Duration::from_secs(10));
    }
    let Some(settings) = read_settings(&context.root)?.filter(|s| s.enabled) else {
        return Ok(());
    };
    if (matches!(event, Event::Complete) && !settings.completion)
        || (matches!(event, Event::Approval) && !settings.approval)
    {
        return Ok(());
    }
    let thread = data
        .get("thread-id")
        .or_else(|| data.get("session_id"))
        .and_then(Value::as_str)
        .unwrap_or(&context.run_id);
    let cwd = data.get("cwd").and_then(Value::as_str).unwrap_or("");
    let project = Path::new(cwd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Codex");
    let title = format!("Codex · {status}");
    let body = format!(
        "账号：{}\n项目：{}\n会话：{}",
        context.account,
        project,
        thread.chars().take(12).collect::<String>()
    );
    // Do not send commands, prompts, full paths, or assistant messages to Bark.
    let mut hash = Sha256::new();
    hash.update(kind);
    hash.update(thread);
    match event {
        Event::Complete => hash.update(
            data.get("turn-id")
                .and_then(Value::as_str)
                .unwrap_or(&payload),
        ),
        Event::Approval => hash.update(&payload),
    }
    let event_id = format!("{:x}", hash.finalize());
    let conn = delivery_database(&context.root)?;
    let claimed = conn.execute(
        "INSERT OR IGNORE INTO deliveries VALUES (?1,?2,unixepoch())",
        params![context.run_id, event_id],
    )?;
    if claimed == 0 {
        return Ok(());
    }
    let result = send(
        &settings,
        &title,
        &body,
        &format!("usage-codex-{}", context.run_id),
        matches!(event, Event::Approval),
    )
    .await;
    if result.is_err() {
        conn.execute(
            "DELETE FROM deliveries WHERE run_id=?1 AND event_id=?2",
            params![context.run_id, event_id],
        )?;
    }
    result
}

fn delivery_database(root: &Path) -> Result<Connection> {
    let path = root.join("bark-deliveries.sqlite3");
    // Create with private permissions before SQLite opens the file.
    super::private_create(&path, b"")?;
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS deliveries (
        run_id TEXT NOT NULL, event_id TEXT NOT NULL, created_at INTEGER NOT NULL,
        PRIMARY KEY(run_id,event_id));
        DELETE FROM deliveries WHERE created_at < unixepoch()-604800;",
    )?;
    Ok(conn)
}

async fn send(
    settings: &Settings,
    title: &str,
    body: &str,
    group: &str,
    attention: bool,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Error::message("无法创建 Bark HTTP 客户端"))?;
    let response = client
        .post(&settings.server)
        .json(&json!({
            "device_key":settings.device_key, "title":title, "body":body, "group":group,
            "level": if attention { "timeSensitive" } else { "active" },
        }))
        .send()
        .await
        .map_err(|_| Error::message("Bark 请求失败"))?;
    if !response.status().is_success() {
        return Err(Error::message("Bark 返回 HTTP 错误"));
    }
    let response: Value = response
        .json()
        .await
        .map_err(|_| Error::message("Bark 响应无效"))?;
    if response["code"] != 200 {
        return Err(Error::message("Bark 未接受推送"));
    }
    tracing::info!(event = "bark_delivered", attention);
    Ok(())
}
