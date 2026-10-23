use super::{AuthMode, Error, Manager, Result, config, oauth};
use clap::{Args, Subcommand};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[derive(Debug, Subcommand)]
pub enum CodexCommand {
    /// GPT 官方订阅模型指纹测试与每小时调度
    Fingerprint {
        #[command(subcommand)]
        command: super::fingerprint::FingerprintCommand,
    },
    #[command(hide = true)]
    ResumeLocal {
        #[arg(long)]
        home: PathBuf,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        session: String,
        #[arg(long, default_value = "codex")]
        binary: PathBuf,
    },
    #[command(hide = true)]
    Event,
    /// 查看某次运行的进程与会话事件（不包含对话内容）
    RunEvents {
        run: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// 管理仅对 usage 启动会话生效的 Bark 通知
    Bark {
        #[command(subcommand)]
        command: super::bark::BarkCommand,
    },
    #[command(hide = true)]
    Notify {
        #[arg(long, value_enum)]
        event: super::bark::Event,
        payload: Option<String>,
    },
    /// 使用官方 Codex CLI 登录 ChatGPT OAuth（浏览器或设备码）
    Login {
        /// 新账号名称；重新登录时可省略
        #[arg(required_unless_present = "account")]
        name: Option<String>,
        /// 重新登录此账号；登录身份必须匹配
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        device_auth: bool,
        #[arg(long, default_value = "codex")]
        binary: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
    },
    /// 导入官方 auth.json 或 Cockpit 单账号 JSON（只读来源）
    ImportAuth {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// 列出已保存的 Codex 账号配置（不显示密钥）
    #[command(visible_aliases=["accounts", "presets"])]
    List {
        #[arg(long)]
        json: bool,
    },
    /// 查看启动记录及其实际使用的账号和配置版本
    Runs {
        profile: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 保存 BYOK 账号配置；密钥从 stdin、环境变量或文件读取
    Add {
        name: String,
        #[arg(long, required_unless_present = "config")]
        base_url: Option<String>,
        #[arg(long, required_unless_present = "config")]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        /// 导入完整 Codex TOML
        #[arg(long)]
        config: Option<PathBuf>,
        #[command(flatten)]
        key: KeyInput,
    },
    /// 编辑配置，保存为新版本；本地编辑会暂停跟随 CC Switch
    Edit {
        profile: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
        #[command(flatten)]
        key: KeyInput,
    },
    /// 复制为独立账号配置
    Copy { profile: String, name: String },
    /// 从列表移除；配置版本和历史会话仍然保留
    Remove { profile: String },
    /// 解除 CC Switch 来源绑定，保留本地配置；再次导入会创建新账号配置
    Detach { profile: String },
    /// 查看完整配置（已脱敏）
    Show {
        profile: String,
        #[arg(long)]
        revision: Option<i64>,
    },
    /// 导出不含密钥的 Codex TOML
    Export {
        profile: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// 读取 CC Switch 的 Codex BYOK 供应商
    Import {
        #[arg(long)]
        source: Option<PathBuf>,
        /// 持续单向跟随源配置；每次启动前同步
        #[arg(long)]
        follow: bool,
        /// 只导入这些 CC Switch 供应商 ID（可重复）
        #[arg(long = "provider")]
        providers: Vec<String>,
        /// 只预览，不读取/写入本机密钥存储
        #[arg(long)]
        dry_run: bool,
    },
    /// 同步已启用跟随的账号配置
    Sync {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t=15, value_parser=clap::value_parser!(u64).range(1..))]
        interval: u64,
    },
    /// 开启或暂停指定账号配置的跟随
    Follow {
        profile: String,
        #[arg(long)]
        off: bool,
    },
    /// 使用独立 CODEX_HOME 打开 Codex，可重复启动多个终端
    Run {
        profile: String,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long)]
        revision: Option<i64>,
        #[arg(long)]
        resume: Option<String>,
        #[arg(long, default_value = "codex")]
        binary: PathBuf,
        /// 在 Ghostty 右侧分屏运行
        #[arg(long)]
        split: bool,
        #[arg(long)]
        worktree: bool,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// 校验配置；--request 会向所选供应商发送一个小型付费测试请求
    Test {
        profile: String,
        #[arg(long)]
        request: bool,
    },
}

impl CodexCommand {
    /// Fixed command names only; never format the enum (it contains user input).
    pub(crate) fn log_name(&self) -> &'static str {
        match self {
            Self::Fingerprint { .. } => "codex.fingerprint",
            Self::ResumeLocal { .. } => "codex.resume_local",
            Self::Event => "codex.event",
            Self::RunEvents { .. } => "codex.run_events",
            Self::Bark { .. } => "codex.bark",
            Self::Notify { .. } => "codex.notify",
            Self::Login { .. } => "codex.login",
            Self::ImportAuth { .. } => "codex.import_auth",
            Self::List { .. } => "codex.list",
            Self::Runs { .. } => "codex.runs",
            Self::Add { .. } => "codex.add",
            Self::Edit { .. } => "codex.edit",
            Self::Copy { .. } => "codex.copy",
            Self::Remove { .. } => "codex.remove",
            Self::Detach { .. } => "codex.detach",
            Self::Show { .. } => "codex.show",
            Self::Export { .. } => "codex.export",
            Self::Import { .. } => "codex.import",
            Self::Sync { .. } => "codex.sync",
            Self::Follow { .. } => "codex.follow",
            Self::Run { .. } => "codex.run",
            Self::Test { .. } => "codex.test",
        }
    }
}

#[derive(Debug, Default, Args)]
#[group(multiple = false)]
pub struct KeyInput {
    #[arg(long)]
    key_env: Option<String>,
    #[arg(long)]
    key_file: Option<PathBuf>,
    #[arg(long)]
    key_stdin: bool,
}
impl KeyInput {
    fn provided(&self) -> bool {
        self.key_env.is_some() || self.key_file.is_some() || self.key_stdin
    }
    fn read(&self) -> Result<String> {
        let key = if let Some(name) = &self.key_env {
            std::env::var(name).map_err(|_| Error::message("密钥环境变量未设置"))?
        } else if let Some(path) = &self.key_file {
            fs_text(path)?
        } else {
            if !self.key_stdin {
                return Err(Error::message(
                    "请使用 --key-env、--key-file 或 --key-stdin 提供 API Key",
                ));
            }
            let mut key = String::new();
            io::stdin().take(65537).read_to_string(&mut key)?;
            key
        };
        if key.len() > 65536 || key.trim().is_empty() {
            return Err(Error::message("API Key 为空或过长"));
        }
        Ok(key.trim().to_string())
    }
}
fn fs_text(path: &Path) -> Result<String> {
    if std::fs::metadata(path)?.len() > 1024 * 1024 {
        return Err(Error::message("配置文件过大"));
    }
    Ok(std::fs::read_to_string(path)?)
}

pub async fn dispatch(command: CodexCommand, usage_config: Option<PathBuf>) -> Result<i32> {
    let command = match command {
        CodexCommand::Fingerprint { command } => {
            return super::fingerprint::dispatch(command).await;
        }
        CodexCommand::Login {
            name,
            account,
            device_auth,
            binary,
            config: file,
            model,
        } => {
            let text = file.as_deref().map(fs_text).transpose()?;
            let text = if model.is_some() {
                Some(oauth::patch_config(
                    text.as_deref().unwrap_or(""),
                    model.as_deref(),
                    None,
                )?)
            } else {
                text
            };
            let adapter = crate::hub::shared()
                .authentication(crate::auth::AuthSpec {
                    service: "codex_oauth".into(),
                    context: crate::auth::AuthContext {
                        root: super::default_root(),
                        account,
                    },
                })
                .map_err(|error| Error::message(error.to_string()))?;
            let result = crate::auth::run_in_terminal(
                adapter.as_ref(),
                crate::auth::AuthRequest {
                    name,
                    config: text,
                    program: binary,
                    method: if device_auth {
                        crate::auth::AuthMethod::DEVICE_CODE.id
                    } else {
                        crate::auth::AuthMethod::BROWSER.id
                    }
                    .into(),
                },
            )
            .await;
            let p = match result {
                Ok(account) => account,
                Err(crate::auth::AuthError::Cancelled) => return Ok(130),
                Err(error) => return Err(Error::message(error.to_string())),
            };
            println!("已保存 OAuth 账号：{}（{}）", p.name, p.id);
            return Ok(0);
        }
        CodexCommand::ResumeLocal {
            home,
            cwd,
            session,
            binary,
        } => {
            use crate::agents::RunningAgent;
            #[cfg(unix)]
            let _interrupts =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let outcome = super::start_local_codex(
                &home,
                &crate::agents::SpawnRequest {
                    cwd,
                    program: binary,
                    args: Vec::new(),
                    resume: Some(session),
                    usage_config,
                },
            )?
            .wait()
            .map_err(Error::message)?;
            return Ok(outcome.status.code().unwrap_or(1));
        }
        CodexCommand::Run {
            profile,
            cwd,
            revision,
            resume,
            binary,
            split,
            worktree,
            mut args,
        } => {
            if worktree {
                args.push("--worktree".into());
            }
            if split {
                let mut command =
                    super::launch_shell_command(&profile, &cwd, revision, resume.as_deref())?;
                command = super::with_usage_config(command, usage_config.as_deref())?;
                command.push_str(" --binary ");
                command.push_str(&super::shell_quote(&binary.to_string_lossy()));
                if !args.is_empty() {
                    command.push_str(" -- ");
                    command.push_str(
                        &args
                            .iter()
                            .map(|s| super::shell_quote(s))
                            .collect::<Vec<_>>()
                            .join(" "),
                    );
                }
                crate::sessions::command_in_ghostty(&cwd, &command).map_err(Error::message)?;
            } else {
                // Preserve the launcher parent when SIGINT reaches the whole foreground group.
                #[cfg(unix)]
                let _interrupts =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
                let outcome = super::managed::spawn(
                    &super::default_root(),
                    &profile,
                    revision,
                    &crate::agents::SpawnRequest {
                        cwd,
                        program: binary,
                        args,
                        resume,
                        usage_config,
                    },
                )
                .map_err(Error::message)?;
                return Ok(outcome.status.code().unwrap_or(130));
            }
            return Ok(0);
        }
        CodexCommand::Event => {
            super::runtime::callback();
            return Ok(0);
        }
        CodexCommand::Notify { event, payload } => {
            super::bark::callback(event, payload).await;
            return Ok(0);
        }
        CodexCommand::Bark { command } => {
            super::bark::dispatch(&super::default_root(), command).await?;
            return Ok(0);
        }
        command => command,
    };
    let mut manager = Manager::open(super::default_root())?;
    match command {
        CodexCommand::Fingerprint { .. }
        | CodexCommand::ResumeLocal { .. }
        | CodexCommand::Event
        | CodexCommand::Notify { .. }
        | CodexCommand::Bark { .. }
        | CodexCommand::Run { .. }
        | CodexCommand::Login { .. } => {
            unreachable!()
        }
        CodexCommand::RunEvents { run, limit, json } => {
            let events = manager.run_events(&run, limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&events)
                        .map_err(|_| Error::message("序列化失败"))?
                );
            } else {
                for event in events {
                    println!(
                        "{}  {}  {}  {}",
                        event.observed_at,
                        event.kind,
                        event.session_id.as_deref().unwrap_or("—"),
                        event.model.as_deref().unwrap_or("—")
                    );
                }
            }
        }
        CodexCommand::ImportAuth {
            source,
            name,
            account,
            config: file,
        } => {
            let text = file.as_deref().map(fs_text).transpose()?;
            let p = manager.import_oauth(
                name.as_deref(),
                text.as_deref(),
                &source,
                account.as_deref(),
            )?;
            println!("已保存 OAuth 账号：{}（{}）；来源未修改", p.name, p.id);
        }
        CodexCommand::List { json } => {
            let profiles = manager.list()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&profiles)
                        .map_err(|_| Error::message("序列化失败"))?
                );
            } else {
                for p in profiles {
                    println!(
                        "{}  {}  v{}  {}  {}  {}",
                        p.id,
                        p.name,
                        p.revision,
                        p.model,
                        p.base_url,
                        if p.auth_mode == AuthMode::Oauth {
                            p.oauth
                                .as_ref()
                                .map(|o| format!("OAuth · {}", o.label()))
                                .unwrap_or_default()
                        } else if p.follow {
                            "跟随 CC Switch".into()
                        } else {
                            "BYOK · 独立".into()
                        }
                    );
                }
            }
        }
        CodexCommand::Runs { profile, json } => {
            let profile_id = profile
                .as_deref()
                .map(|selector| manager.resolve(selector).map(|p| p.id))
                .transpose()?;
            let runs = manager.runs(profile_id.as_deref())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&runs)
                        .map_err(|_| Error::message("序列化失败"))?
                );
            } else {
                for run in runs {
                    println!(
                        "{}  {}  v{}  {}  {}  {}",
                        run.id,
                        run.profile_id,
                        run.revision,
                        run.cwd.display(),
                        run.started_at,
                        run.finished_at
                            .map(|_| format!("已结束 ({:?})", run.exit_code))
                            .unwrap_or_else(|| "未记录结束（进程状态未知）".into())
                    );
                }
            }
        }
        CodexCommand::Add {
            name,
            base_url,
            model,
            effort,
            config: file,
            key,
            ..
        } => {
            let config = match file {
                Some(path) => fs_text(&path)?,
                None => config::new_config(
                    base_url.as_deref().unwrap_or_default(),
                    model.as_deref().unwrap_or_default(),
                    effort.as_deref(),
                )?,
            };
            let profile = manager.add(&name, &config, &key.read()?)?;
            println!("已保存：{}（{}）", profile.name, profile.id);
        }
        CodexCommand::Edit {
            profile,
            name,
            base_url,
            model,
            effort,
            config: file,
            key,
            ..
        } => {
            let current = manager.resolve(&profile)?;
            let original = manager.revision(&current.id, current.revision)?;
            let use_editor = file.is_none()
                && name.is_none()
                && base_url.is_none()
                && model.is_none()
                && effort.is_none()
                && !key.provided();
            let mut text = match file {
                Some(path) => fs_text(&path)?,
                None => original.config.clone(),
            };
            if use_editor {
                text = edit_text(&text)?;
            }
            text = if current.auth_mode == AuthMode::Oauth {
                if base_url.is_some() || key.provided() {
                    return Err(Error::message("OAuth 账号不能设置 Base URL / API Key"));
                }
                oauth::patch_config(&text, model.as_deref(), effort.as_deref())?
            } else {
                config::patch(
                    &text,
                    base_url.as_deref(),
                    model.as_deref(),
                    effort.as_deref(),
                )?
            };
            let secret = if key.provided() {
                Some(key.read()?)
            } else {
                None
            };
            let p = manager.update(
                &profile,
                current.revision,
                &text,
                secret.as_deref(),
                name.as_deref(),
            )?;
            println!("已保存 {} v{}；原版本仍可恢复", p.name, p.revision);
        }
        CodexCommand::Copy { profile, name, .. } => {
            let p = manager.copy(&profile, &name)?;
            println!("已复制：{}（{}）", p.name, p.id);
        }
        CodexCommand::Remove { profile } => {
            manager.remove(&profile)?;
            println!("已移出列表，配置及历史仍然保留");
        }
        CodexCommand::Detach { profile } => {
            manager.detach(&profile)?;
            println!("已解除来源绑定，配置及历史保持原样");
        }
        CodexCommand::Show { profile, revision } => {
            let p = manager.resolve(&profile)?;
            print!(
                "{}",
                manager
                    .revision(&p.id, revision.unwrap_or(p.revision))?
                    .config
            );
        }
        CodexCommand::Export { profile, output } => {
            manager.export(&profile, &output)?;
            println!("已导出（不含密钥）：{}", output.display());
        }
        CodexCommand::Import {
            source,
            follow,
            providers,
            dry_run,
            ..
        } => {
            let report = manager.import_ccswitch(
                &source.unwrap_or_else(super::ccswitch::default_path),
                follow,
                &providers,
                dry_run,
            )?;
            if dry_run {
                println!("导入预览（未保存）：");
            }
            print!("{report}");
        }
        CodexCommand::Sync {
            dry_run,
            watch,
            interval,
            ..
        } => loop {
            print!("{}", manager.sync(dry_run)?);
            if !watch {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {},
                _ = tokio::signal::ctrl_c() => break,
            }
        },
        CodexCommand::Follow { profile, off } => {
            manager.follow(&profile, !off)?;
            println!(
                "{}",
                if off {
                    "已暂停跟随"
                } else {
                    "已开启跟随"
                }
            );
        }
        CodexCommand::Test { profile, request } => {
            let p = manager.resolve(&profile)?;
            let saved = manager.revision(&p.id, p.revision)?;
            if p.auth_mode == AuthMode::Oauth {
                oauth::normalize_config(&saved.config)?;
                let auth = oauth::read_auth(&manager.home(&p.id, p.revision)?.join("auth.json"))?;
                if p.oauth.as_ref().is_none_or(|o| {
                    o.account_id != auth.identity.account_id || o.user_id != auth.identity.user_id
                }) {
                    return Err(Error::message("OAuth 登录文件与账号身份不匹配；请重新登录"));
                }
            } else {
                config::normalize(&saved.config)?;
            }
            println!("配置有效：{} · {} · {}", p.name, p.model, p.base_url);
            if request {
                let status = manager.test_connection(&p.id).await?;
                println!("Responses 测试 HTTP {status}");
                if !(200..300).contains(&status) {
                    return Ok(1);
                }
            }
        }
    }
    Ok(0)
}

pub fn edit_text(text: &str) -> Result<String> {
    let directory = std::env::temp_dir().join(format!("usage-codex-edit-{}", uuid::Uuid::new_v4()));
    super::private_dir(&directory)?;
    let path = directory.join("config.toml");
    super::private_write(&path, text.as_bytes())?;
    let result = (|| {
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into());
        let parts =
            shlex::split(&editor).ok_or_else(|| Error::message("无法解析 EDITOR/VISUAL"))?;
        let (program, args) = parts
            .split_first()
            .ok_or_else(|| Error::message("EDITOR 为空"))?;
        let status = std::process::Command::new(program)
            .args(args)
            .arg(&path)
            .status()?;
        if !status.success() {
            return Err(Error::message("编辑器已取消，配置未保存"));
        }
        fs_text(&path)
    })();
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(directory);
    result
}
