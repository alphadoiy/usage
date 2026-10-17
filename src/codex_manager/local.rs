//! Launch the user's existing Codex home without importing or changing its account.
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::{Error, Result, bark};
use crate::agents::{ProcessHandle, SpawnRequest};

pub fn start_local_codex(home: &Path, request: &SpawnRequest) -> Result<ProcessHandle> {
    crate::logging::operation("codex.local_start", |op| {
        let home = home.canonicalize()?;
        let cwd = request.cwd.canonicalize()?;
        if !home.is_dir() || !cwd.is_dir() {
            return Err(Error::message("Codex Home 或项目路径不是目录"));
        }
        if let Some(id) = &request.resume {
            uuid::Uuid::parse_str(id).map_err(|_| Error::message("无效的会话 ID"))?;
        }
        let mut config = String::new();
        match std::fs::File::open(home.join("config.toml")) {
            Ok(file) => {
                file.take(1024 * 1024 + 1).read_to_string(&mut config)?;
                if config.len() > 1024 * 1024 {
                    return Err(Error::message("Codex 配置文件过大"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut command = Command::new(&request.program);
        // Local launches keep the user's authentication environment. A parent
        // managed run's identity must never leak into this independent process.
        command
            .current_dir(&cwd)
            .env("CODEX_HOME", &home)
            .env_remove("USAGE_CODEX_RUN_CONTEXT");
        let run_id = op.id().to_owned();
        let injected = op.step("bark_inject", || {
            bark::inject(
                &mut command,
                &super::default_root(),
                &home,
                "Codex",
                &run_id,
                &config,
            )
        })?;
        if injected {
            // A reused daemon would retain another launch's notification context.
            let mut help = Command::new(&request.program);
            help.arg("--help")
                .current_dir(&cwd)
                .env("CODEX_HOME", &home);
            let help = crate::credentials::output_with_timeout(help, Duration::from_secs(10))
                .filter(|output| output.status.success())
                .ok_or_else(|| Error::message("Codex CLI 通知能力检测失败或超时"))?;
            if String::from_utf8_lossy(&help.stdout).contains("--no-daemon") {
                command.arg("--no-daemon");
            }
        }
        if let Some(id) = &request.resume {
            command.args(["resume", id]);
        }
        let child = op.step("child_spawn", || {
            command.args(&request.args).spawn().map_err(Error::from)
        })?;
        tracing::info!(
            event = "bark_local_launch",
            child_pid = child.id(),
            managed = false,
            bark_injected = injected,
            resume = request.resume.is_some(),
            session_id = request.resume.as_deref(),
            home = %home.display(),
            cwd = %cwd.display(),
        );
        Ok(ProcessHandle::new(child, None, None))
    })
}

pub fn local_resume_shell_command(home: &Path, cwd: &Path, session: &str) -> Result<String> {
    uuid::Uuid::parse_str(session).map_err(|_| Error::message("无效的会话 ID"))?;
    Ok(super::usage_shell_command(&[
        std::env::current_exe()?.to_string_lossy().into_owned(),
        "codex".into(),
        "resume-local".into(),
        "--home".into(),
        home.to_string_lossy().into_owned(),
        "--cwd".into(),
        cwd.to_string_lossy().into_owned(),
        "--session".into(),
        session.into(),
    ]))
}
