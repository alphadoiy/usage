//! Ask native Codex to resolve project trust and configuration layers before a local resume.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::agents::SpawnRequest;
use crate::codex_manager::{Error, Result};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;

pub(super) fn provider_override(
    home: &Path,
    cwd: &Path,
    request: &SpawnRequest,
) -> Result<Option<String>> {
    let Some((config_args, effective_cwd)) = probe_args(cwd, &request.args)? else {
        // Any explicit model setting makes native Codex resume with the entire
        // current model configuration. Preserve those choices without a probe.
        return Ok(None);
    };
    let mut command = Command::new(&request.program);
    command
        .args(config_args)
        .args(["app-server", "--listen", "stdio://"])
        .current_dir(cwd)
        .env("CODEX_HOME", home)
        .env_remove("USAGE_CODEX_RUN_CONTEXT")
        .env_remove("USAGE_CODEX_BARK_CONTEXT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    // Launch can run inside the TUI's Tokio runtime. A separate small runtime
    // keeps the synchronous launcher usable there and bounds both pipe directions.
    let provider = std::thread::Builder::new()
        .name("usage-resume-config".into())
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(read_provider(command, effective_cwd, PROBE_TIMEOUT))
        })?
        .join()
        .map_err(|_| probe_error("配置读取任务异常退出"))??;
    Ok(Some(provider))
}

fn probe_error(reason: &str) -> Error {
    Error::message(format!(
        "无法读取 Codex 有效恢复配置：{reason}；请升级支持 config/read 的 Codex，或显式指定 --profile / -c model_provider=..."
    ))
}

fn probe_args(cwd: &Path, args: &[String]) -> Result<Option<(Vec<String>, PathBuf)>> {
    let mut forwarded = Vec::new();
    let mut effective_cwd = cwd.to_owned();
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if matches!(arg, "--profile" | "--model" | "--oss")
            || arg.starts_with("--profile=")
            || arg.starts_with("--model=")
            || (arg.starts_with('-')
                && !arg.starts_with("--")
                && (arg.starts_with("-p") || arg.starts_with("-m")))
        {
            return Ok(None);
        }
        let config = if matches!(arg, "-c" | "--config") {
            Some(args.next().ok_or_else(|| probe_error("-c 缺少配置值"))?)
        } else {
            arg.strip_prefix("--config=")
                .or_else(|| arg.strip_prefix("-c"))
        };
        if let Some(config) = config {
            if let Some((key, _)) = config.split_once('=') {
                // Parse only the key, including TOML quoted keys; leave values and
                // all layer semantics to the native loader.
                if let Ok(doc) = toml::from_str::<toml::Value>(&format!("{key}=0"))
                    && ["model_provider", "model", "model_reasoning_effort"]
                        .iter()
                        .any(|key| doc.get(*key).is_some())
                {
                    return Ok(None);
                }
            }
            forwarded.extend(["-c".into(), config.into()]);
            continue;
        }
        let directory = if matches!(arg, "-C" | "--cd") {
            Some(args.next().ok_or_else(|| probe_error("--cd 缺少目录"))?)
        } else {
            arg.strip_prefix("--cd=").or_else(|| arg.strip_prefix("-C"))
        };
        if let Some(directory) = directory {
            effective_cwd = cwd.join(directory);
        }
    }
    Ok(Some((forwarded, effective_cwd.canonicalize()?)))
}

async fn read_provider(command: Command, cwd: PathBuf, timeout: Duration) -> Result<String> {
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| probe_error("无法启动配置读取进程"))?;
    let result =
        tokio::time::timeout(timeout, async {
            let mut input = child
                .stdin
                .take()
                .ok_or_else(|| probe_error("无法打开输入管道"))?;
            let output = child
                .stdout
                .take()
                .ok_or_else(|| probe_error("无法打开输出管道"))?;
            let mut output = BufReader::new(output.take(MAX_OUTPUT));
            send(&mut input, json!({"id": 1, "method": "initialize", "params": {
            "clientInfo": {"name": "usage_resume_config", "version": env!("CARGO_PKG_VERSION")}
        }})).await?;
            response(&mut output, 1).await?;
            send(&mut input, json!({"method": "initialized"})).await?;
            send(
                &mut input,
                json!({"id": 2, "method": "config/read", "params": {
                    "cwd": cwd, "includeLayers": false
                }}),
            )
            .await?;
            let result = response(&mut output, 2).await?;
            let config = result
                .get("config")
                .and_then(Value::as_object)
                .ok_or_else(|| probe_error("config/read 响应缺少配置"))?;
            match config.get("model_provider") {
                None | Some(Value::Null) => Ok("openai".into()),
                Some(Value::String(provider)) if !provider.trim().is_empty() => {
                    Ok(provider.clone())
                }
                _ => Err(probe_error("model_provider 格式无效")),
            }
        })
        .await
        .unwrap_or_else(|_| Err(probe_error("配置读取超时")));
    // Closing stdin requests normal app-server shutdown. Keep the group leader
    // unreaped during its grace period: wrappers can exit before their descendants,
    // and retaining the PID prevents this process group's ID from being reused.
    tokio::time::sleep(Duration::from_millis(250)).await;
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: this unreaped child was started in its own process group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    result
}

async fn send(input: &mut tokio::process::ChildStdin, message: Value) -> Result<()> {
    let mut line = serde_json::to_vec(&message).map_err(|_| probe_error("无法生成配置请求"))?;
    line.push(b'\n');
    input
        .write_all(&line)
        .await
        .map_err(|_| probe_error("配置读取进程已退出"))
}

async fn response(output: &mut (impl AsyncBufReadExt + Unpin), id: u64) -> Result<Value> {
    loop {
        let mut line = Vec::new();
        let size = output
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| probe_error("无法读取配置响应"))?;
        if size == 0 {
            return Err(probe_error("配置读取进程退出或响应超过大小限制"));
        }
        let message: Value =
            serde_json::from_slice(&line).map_err(|_| probe_error("配置读取协议不受支持"))?;
        if message.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if message.get("error").is_some() {
            // Server errors/config may contain API keys. Never echo complete JSON.
            return Err(probe_error("Codex 拒绝配置读取请求"));
        }
        return message
            .get("result")
            .cloned()
            .ok_or_else(|| probe_error("配置响应缺少结果"));
    }
}

#[cfg(all(test, unix))]
mod tests;
