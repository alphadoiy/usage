//! 命令行入口。不带参数时打开 ratatui 界面，`--once` / `--json` 只查询一次。

use std::path::PathBuf;
use std::sync::Arc;

use tracing::Instrument;

use chrono::{Datelike, Duration, Local, NaiveDate, Utc};
use clap::{Parser, Subcommand};
use tokio::runtime::Handle;
use tokio::task::JoinSet;

use crate::config::{
    AppConfig, ConfigError, default_config_path, home_dir, load_config, with_overrides,
    write_default_config,
};
use crate::credentials::read_macos_keychain;
use crate::http::ReqwestClient;
use crate::model::{CredentialStatus, UsageSnapshot, snapshot_transient};
use crate::plain::render_dashboard;
use crate::providers::{
    ProviderContext, QUERY_TIMEOUT, UsageProvider, builtin_factory, query_with_timeout,
};
use crate::statusline;
use crate::view::builtin_styles;

#[derive(Parser)]
#[command(name = "usage", about = "查看 Claude Code、Codex 和 Grok 的官方用量")]
struct Cli {
    /// 配置文件路径。默认 ~/.config/usage/config.toml
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    /// --once 文本样式：bar (Braille 进度条)、ring (圆环) 或 statusline (圆点状态栏)
    #[arg(long)]
    style: Option<String>,
    /// 逗号分隔的提供方，例如 claude,codex
    #[arg(long, global = true)]
    providers: Option<String>,
    /// 自动刷新秒数，0 表示不自动刷新
    #[arg(long, global = true)]
    refresh: Option<u64>,
    /// 查询一次，用文本打印后退出
    #[arg(long)]
    once: bool,
    /// 查询一次并输出 JSON
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 本地 HTTP/SSE 请求观测网关
    Gateway {
        #[command(subcommand)]
        command: crate::gateway::GatewayCommand,
    },
    /// 查询配置的 API 余额、原始额度与官方费用
    Resources {
        /// 资源连接 ID；省略时查询全部连接
        ids: Vec<String>,
        /// 仅查询显式关联到此 Codex 账号的连接（名称或 UUID）
        #[arg(long)]
        profile: Option<String>,
        /// 只查询启动时与此运行匹配且配置未变更的连接
        #[arg(long, conflicts_with = "profile")]
        run: Option<uuid::Uuid>,
        /// 只列出连接，不请求网络；--run 仍需校验凭据
        #[arg(long)]
        list: bool,
        #[arg(long)]
        json: bool,
    },
    /// 查询 OpenAI、Claude、Grok 的官方服务状态（无需登录）
    Status {
        /// 输出状态、受影响组件和未解决事件的 JSON
        #[arg(long)]
        json: bool,
    },
    /// 查看最近的结构化日志；不初始化日志系统
    Logs {
        /// 只输出日志目录
        #[arg(long)]
        path: bool,
        /// 输出最后 N 条事件
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=10000))]
        tail: u32,
        /// 筛选操作 ID（UUID），跨日志文件查找
        #[arg(long)]
        operation: Option<uuid::Uuid>,
    },
    /// Codex BYOK/OAuth 账号配置、CC Switch 导入/同步和多实例启动
    Codex {
        #[command(subcommand)]
        command: Option<crate::codex_manager::cli::CodexCommand>,
    },
    /// TUI 贡献看板：年度热力图、每日明细和月度趋势
    #[command(visible_alias = "dashboard")]
    History {
        /// 年份，默认今年
        #[arg(long, value_parser = clap::value_parser!(i32).range(1970..=9999))]
        year: Option<i32>,
        /// 时间范围（本地日历日期）；不指定则按年份查询
        #[arg(long, value_parser = ["today", "7d", "30d", "month"], conflicts_with_all = ["year", "from", "to"])]
        range: Option<String>,
        /// 起始日期 YYYY-MM-DD（含当天）
        #[arg(long)]
        from: Option<NaiveDate>,
        /// 结束日期 YYYY-MM-DD（含当天）
        #[arg(long)]
        to: Option<NaiveDate>,
        /// 项目路径包含的文字
        #[arg(long)]
        project: Option<String>,
        /// 模型名称包含的文字
        #[arg(long)]
        model: Option<String>,
        /// 只统计指定实例 ID
        #[arg(long)]
        instance: Option<String>,
        /// 导出当前筛选范围的 CSV
        #[arg(long, conflicts_with_all = ["json", "once"])]
        csv: bool,
        /// 显示月度汇总
        #[arg(long)]
        monthly: bool,
        /// 输出文本明细后退出
        #[arg(long, conflicts_with = "json")]
        once: bool,
        /// 输出按日、按月聚合的 JSON 后退出
        #[arg(long)]
        json: bool,
    },
    /// 输出适合 Claude Code 等 agent 的轻量状态栏，读取可选的 stdin JSON
    Statusline {
        /// 用量缓存秒数；0 表示每次查询
        #[arg(long, default_value_t = 60)]
        cache_seconds: u64,
        /// 不输出 ANSI 颜色
        #[arg(long)]
        no_color: bool,
    },
    /// 列出已注册的提供方
    Providers,
    /// 列出可视化样式
    Styles,
    /// 写出默认配置文件
    InitConfig {
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
}

impl Cli {
    fn interactive(&self) -> bool {
        match &self.command {
            None => !self.once && !self.json,
            Some(Command::Codex { command: None }) => true,
            Some(Command::History {
                csv, once, json, ..
            }) => !csv && !once && !json && !self.once && !self.json,
            _ => false,
        }
    }
}

pub async fn run() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            return code;
        }
    };
    if matches!(
        cli.command,
        Some(Command::Logs { .. })
            | Some(Command::Codex {
                command: Some(crate::codex_manager::cli::CodexCommand::Bark {
                    command: crate::codex_manager::bark::BarkCommand::Logs { .. }
                })
            })
    ) {
        return match dispatch(cli).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("{error}");
                2
            }
        };
    }
    let command = match &cli.command {
        Some(Command::Gateway { .. }) => "gateway",
        Some(Command::Resources { .. }) => "resources",
        Some(Command::Status { .. }) => "status",
        Some(Command::Codex {
            command: Some(command),
        }) => command.log_name(),
        Some(Command::Codex { command: None }) => "codex.manage",
        Some(Command::History { .. }) => "history",
        Some(Command::Statusline { .. }) => "statusline",
        Some(Command::Providers) => "providers",
        Some(Command::Styles) => "styles",
        Some(Command::InitConfig { .. }) => "init_config",
        Some(Command::Logs { .. }) => "logs",
        None => "dashboard",
    };
    let logging_guard = match crate::logging::init(if command == "statusline" {
        "warn"
    } else {
        "info"
    }) {
        Ok(guard) => guard,
        Err(_) => {
            eprintln!("无法初始化文件日志；当前命令继续运行");
            None
        }
    };
    let mut op = crate::logging::Operation::new("cli.dispatch");
    op.span().record("command", command);
    op.span().in_scope(|| {
        tracing::info!(
            event = "process_started",
            version = env!("CARGO_PKG_VERSION")
        )
    });
    op.begin("dispatch");
    let result = crate::logging::instrument(op.span(), dispatch(cli)).await;
    let code = match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");

            2
        }
    };
    if code != 0 && logging_guard.is_some() {
        eprintln!(
            "日志目录：{}（操作 ID：{}）",
            crate::logging::directory().display(),
            op.id()
        );
    }
    op.span()
        .in_scope(|| tracing::info!(event = "process_exited", exit_code = code));
    op.finish(code == 0);
    code
}

async fn dispatch(cli: Cli) -> Result<i32, CliError> {
    let _runtime = cli
        .interactive()
        .then(crate::app_runtime::AppRuntime::start);
    match cli.command {
        Some(Command::Gateway { command }) => {
            crate::gateway::execute(command).await.map_err(CliError)?;
            Ok(0)
        }
        Some(Command::Resources {
            ids,
            profile,
            run,
            list,
            json,
        }) => {
            use crate::resources::connections::{ResourceService, render_text, select};
            let path = cli.config.clone().unwrap_or_else(default_config_path);
            let config = load_config(&path, cli.config.is_some())?;
            let profile_id = profile
                .map(|selector| {
                    crate::codex_manager::Manager::open_default()
                        .and_then(|manager| manager.resolve(&selector))
                        .map(|profile| profile.id)
                        .map_err(|error| CliError(error.to_string()))
                })
                .transpose()?;
            let (selected, credentials) = if let Some(run) = run {
                let pinned = crate::codex_manager::Manager::open_default()
                    .and_then(|manager| {
                        manager.resources_for_run(&run.to_string(), &config.resources, &ids)
                    })
                    .map_err(|error| CliError(error.to_string()))?;
                let credentials = pinned.credential_resolver();
                tracing::info!(event="resource_run_selected", run_id=%run, resource_count=pinned.configs.len());
                (pinned.configs, Some(credentials))
            } else {
                (
                    select(&config.resources, &ids, profile_id.as_deref())
                        .map_err(|error| CliError(error.to_string()))?,
                    None,
                )
            };
            let mut service =
                ResourceService::from_environment(crate::hub::Hub::default().resources);
            if let Some(credentials) = credentials {
                service = service.with_credentials(credentials);
            }
            if list {
                // Validate factories/options without resolving any secret.
                for connection in &selected {
                    service
                        .prepare(connection)
                        .map_err(|error| CliError(error.to_string()))?;
                }
                let descriptors: Vec<_> =
                    selected.iter().map(|config| config.descriptor()).collect();
                if json || cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&descriptors)
                            .map_err(|error| CliError(error.to_string()))?
                    );
                } else if descriptors.is_empty() {
                    println!("尚未配置资源连接；在配置文件中添加 [[resources]]");
                } else {
                    for item in descriptors {
                        println!("{} · {} · {}", item.id, item.label, item.service);
                    }
                }
                return Ok(0);
            }
            let reports = service
                .query(&selected)
                .await
                .map_err(|error| CliError(error.to_string()))?;
            if json || cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&reports)
                        .map_err(|error| CliError(error.to_string()))?
                );
            } else {
                println!("{}", render_text(&reports));
            }
            Ok(i32::from(reports.iter().any(|report| {
                report.snapshot.state != crate::resources::ResourceState::Ready
            })))
        }
        Some(Command::Status { json }) => {
            let sources = crate::service_status::select(
                cli.providers.as_deref().unwrap_or("openai,claude,grok"),
            )
            .map_err(CliError)?;
            let snapshots =
                crate::service_status::collect(Arc::new(ReqwestClient::new()), &sources).await;
            if json || cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&snapshots)
                        .map_err(|e| CliError(e.to_string()))?
                );
            } else {
                println!("{}", crate::service_status::render_text(&snapshots));
            }
            Ok(i32::from(snapshots.iter().any(|s| s.error.is_some())))
        }
        Some(Command::Logs {
            path,
            tail,
            operation,
        }) => {
            if path {
                println!("{}", crate::logging::directory().display());
            } else {
                let operation = operation.map(|id| id.to_string());
                for line in crate::logging::recent(tail as usize, operation.as_deref())
                    .map_err(|_| CliError("无法读取日志目录".into()))?
                {
                    println!("{line}");
                }
            }
            Ok(0)
        }
        Some(Command::Codex { command }) => {
            if let Some(command) = command {
                crate::codex_manager::cli::dispatch(command, cli.config)
                    .await
                    .map_err(|e| CliError(e.to_string()))
            } else {
                let handle = Handle::current();
                {
                    let span = tracing::Span::current();
                    tokio::task::spawn_blocking(move || {
                        span.in_scope(|| crate::ui::run_codex_with_config(handle, cli.config))
                    })
                }
                .await
                .map_err(|e| CliError(e.to_string()))?
                .map_err(|e| CliError(e.to_string()))?;
                Ok(0)
            }
        }
        Some(Command::History {
            year,
            range,
            from,
            to,
            project,
            model,
            instance,
            csv,
            monthly,
            once,
            json,
        }) => {
            let path = cli.config.clone().unwrap_or_else(default_config_path);
            let config = load_config(&path, cli.config.is_some())?;
            let config = with_overrides(config, None, cli.providers.as_deref(), cli.refresh)?;
            let mut query = history_query(range.as_deref(), from, to, project, model, year)?;
            if let Some(id) = &instance
                && !config
                    .data_instances()
                    .iter()
                    .any(|i| i.id == *id && config.providers.contains(&i.provider))
            {
                return Err(CliError(format!("当前来源中没有实例：{id}")));
            }
            query.instance = instance;
            let year = year.unwrap_or_else(|| Local::now().year());
            if once || json || csv || cli.once || cli.json {
                let sources = config.providers.clone();
                let span = tracing::Span::current();
                let history = tokio::task::spawn_blocking(move || {
                    span.in_scope(|| crate::history::HistoryCache::default().load(&config))
                })
                .await
                .map_err(|e| CliError(format!("历史读取失败: {e}")))?;
                let report = history.report_query(year, &sources, &query);
                if csv {
                    if cli.json {
                        return Err(CliError("--csv 不能与 --json 同时使用".into()));
                    }
                    println!(
                        "date,total_tokens,input_tokens,output_tokens,cached_read_tokens,calls"
                    );
                    for (date, t) in if monthly {
                        &report.monthly
                    } else {
                        &report.daily
                    } {
                        println!(
                            "{date},{},{},{},{},{}",
                            t.total(),
                            t.input,
                            t.output,
                            t.cached,
                            t.calls
                        );
                    }
                    for warning in &report.warnings {
                        eprintln!("{warning}");
                    }
                } else if json || cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report)
                            .map_err(|e| CliError(e.to_string()))?
                    );
                } else {
                    println!(
                        "{year} · {} · {} · {} Tokens",
                        report.providers.join(" + "),
                        report.timezone,
                        report.totals.total()
                    );
                    println!("日期          总 Token       输入       输出       缓存   调用/记录");
                    let rows = if monthly {
                        report.monthly
                    } else {
                        report.daily
                    };
                    for (date, tokens) in rows {
                        println!(
                            "{date:<10} {:>12} {:>10} {:>10} {:>10} {:>10}",
                            tokens.total(),
                            tokens.input,
                            tokens.output,
                            tokens.cached,
                            tokens.calls
                        );
                    }
                    for warning in report.warnings {
                        eprintln!("{warning}");
                    }
                }
            } else {
                let handle = Handle::current();
                let span = tracing::Span::current();
                tokio::task::spawn_blocking(move || {
                    span.in_scope(|| {
                        crate::ui::run_history_with_query(
                            handle, config, path, year, monthly, query,
                        )
                    })
                })
                .await
                .map_err(|e| CliError(format!("界面任务失败: {e}")))?
                .map_err(|e| CliError(e.to_string()))?;
            }
            Ok(0)
        }
        Some(Command::Statusline {
            cache_seconds,
            no_color,
        }) => {
            print_statusline(cli.config, cli.providers, cache_seconds, no_color).await?;
            Ok(0)
        }
        Some(Command::Providers) => {
            print_providers().await?;
            Ok(0)
        }
        Some(Command::Styles) => {
            print_styles();
            Ok(0)
        }
        Some(Command::InitConfig { path, force }) => {
            let path = path.unwrap_or_else(default_config_path);
            write_default_config(&path, force)?;
            println!("已写入 {}", path.display());
            Ok(0)
        }
        None => query(cli).await,
    }
}

fn history_query(
    range: Option<&str>,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    project: Option<String>,
    model: Option<String>,
    year: Option<i32>,
) -> Result<crate::history::HistoryQuery, CliError> {
    let today = Local::now().date_naive();
    let (from, to) = match range {
        Some("today") => (Some(today), Some(today)),
        Some("7d") => (Some(today - Duration::days(6)), Some(today)),
        Some("30d") => (Some(today - Duration::days(29)), Some(today)),
        Some("month") => (today.with_day(1), Some(today)),
        _ if from.is_some() || to.is_some() => (
            Some(from.unwrap_or_else(|| {
                NaiveDate::from_ymd_opt(year.unwrap_or_else(|| to.unwrap_or(today).year()), 1, 1)
                    .unwrap()
            })),
            Some(to.unwrap_or(today)),
        ),
        _ => (None, None),
    };
    if from.zip(to).is_some_and(|(from, to)| from > to) {
        return Err(CliError("起始日期不能晚于结束日期".into()));
    }
    if from
        .zip(to)
        .is_some_and(|(from, to)| (to - from).num_days() > 36600)
    {
        return Err(CliError("一次最多查询 100 年历史".into()));
    }
    Ok(crate::history::HistoryQuery {
        from,
        to,
        project,
        model,
        instance: None,
    })
}

async fn print_statusline(
    config_path: Option<PathBuf>,
    selected_providers: Option<String>,
    cache_seconds: u64,
    no_color: bool,
) -> Result<(), CliError> {
    let path = config_path.clone().unwrap_or_else(default_config_path);
    let config = load_config(&path, config_path.is_some())?;
    let config = with_overrides(config, None, selected_providers.as_deref(), None)?;
    let providers = builtin_factory()
        .create_many(&config.providers, make_context(&config))
        .map_err(|error| CliError(error.to_string()))?;
    let providers: Vec<Arc<dyn UsageProvider>> = providers.into_iter().map(Arc::from).collect();
    let input = statusline::read_stdin();
    let now = Utc::now();
    let snapshots = statusline::collect(
        &providers,
        &input,
        &statusline::cache_path(),
        cache_seconds,
        now,
    )
    .await;
    println!("{}", statusline::render(&input, &snapshots, now, !no_color));
    Ok(())
}

async fn query(cli: Cli) -> Result<i32, CliError> {
    let config_path = cli.config.clone().unwrap_or_else(default_config_path);
    let config = load_config(&config_path, cli.config.is_some())?;
    let config = with_overrides(
        config,
        cli.style.as_deref(),
        cli.providers.as_deref(),
        cli.refresh,
    )?;
    let ctx = make_context(&config);
    let providers = builtin_factory()
        .create_many(&config.providers, ctx)
        .map_err(|error| CliError(error.to_string()))?;
    let providers: Vec<Arc<dyn UsageProvider>> = providers.into_iter().map(Arc::from).collect();
    if cli.once || cli.json {
        return Ok(print_once(&providers, &config, cli.json).await);
    }
    let handle = Handle::current();
    let path = config_path;
    let style_config = config;
    {
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            span.in_scope(|| crate::ui::run(handle, providers, style_config, path))
        })
    }
    .await
    .map_err(|error| CliError(format!("界面任务失败: {error}")))?
    .map_err(|error| CliError(error.to_string()))?;
    Ok(0)
}

async fn print_once(
    providers: &[Arc<dyn UsageProvider>],
    config: &AppConfig,
    as_json: bool,
) -> i32 {
    let snapshots = query_all(providers).await;
    if as_json {
        let values: Vec<_> = snapshots
            .iter()
            .map(|snapshot| snapshot.to_json())
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&values).unwrap_or_else(|_| "[]".into())
        );
    } else {
        println!(
            "{}",
            render_dashboard(&snapshots, &config.style, Utc::now())
        );
    }
    if snapshots.iter().any(|snapshot| snapshot.success)
        || (!snapshots.is_empty()
            && snapshots
                .iter()
                .all(|snapshot| snapshot.credential_status == CredentialStatus::NotFound))
    {
        0
    } else {
        1
    }
}

async fn query_all(providers: &[Arc<dyn UsageProvider>]) -> Vec<UsageSnapshot> {
    let mut jobs = JoinSet::new();
    for (index, provider) in providers.iter().enumerate() {
        let provider = Arc::clone(provider);
        jobs.spawn(
            async move {
                (
                    index,
                    query_with_timeout(provider.as_ref(), QUERY_TIMEOUT).await,
                )
            }
            .in_current_span(),
        );
    }
    let mut results = vec![None; providers.len()];
    while let Some(completion) = jobs.join_next().await {
        if let Ok((index, snapshot)) = completion {
            results[index] = Some(snapshot);
        }
    }
    results
        .into_iter()
        .enumerate()
        .map(|(index, result)| {
            result.unwrap_or_else(|| {
                snapshot_transient(
                    providers[index].id(),
                    providers[index].display_name(),
                    "查询任务中断",
                )
            })
        })
        .collect()
}

async fn print_providers() -> Result<(), CliError> {
    let ctx = make_context(&AppConfig::default());
    let factory = builtin_factory();
    for provider_id in factory.registry.ids() {
        let provider = factory
            .create(&provider_id, Arc::clone(&ctx))
            .map_err(|error| CliError(error.to_string()))?;
        println!("{:<10} {}", provider.id(), provider.display_name());
    }
    Ok(())
}

fn print_styles() {
    let styles = builtin_styles();
    for style_id in styles.ids() {
        println!("{:<10} {}", style_id, styles.label(&style_id));
    }
}

pub fn make_context(config: &AppConfig) -> Arc<ProviderContext> {
    make_context_with_keychain(config, Arc::new(read_macos_keychain))
}

fn make_context_with_keychain(
    config: &AppConfig,
    keychain: Arc<crate::providers::KeychainReader>,
) -> Arc<ProviderContext> {
    let home = home_dir();
    let custom_codex_home = config.codex_dir.as_ref().is_some_and(|dir| {
        crate::config::resolved_data_path(dir)
            != crate::config::resolved_data_path(&home.join(".codex"))
    });
    Arc::new(ProviderContext {
        claude_dir: config
            .claude_dir
            .clone()
            .unwrap_or_else(|| home.join(".claude")),
        codex_dir: config
            .codex_dir
            .clone()
            .unwrap_or_else(|| home.join(".codex")),
        grok_dir: config
            .grok_dir
            .clone()
            .unwrap_or_else(|| home.join(".grok")),
        http: Arc::new(ReqwestClient::new()),
        keychain: Arc::new(move |service, account| {
            // A custom home must not silently query another account's default Keychain item.
            if custom_codex_home && service == crate::credentials::CODEX_KEYCHAIN_SERVICE {
                None
            } else {
                keychain(service, account)
            }
        }),
        now: Arc::new(Utc::now),
    })
}

#[derive(Debug)]
struct CliError(String);

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<ConfigError> for CliError {
    fn from(error: ConfigError) -> Self {
        Self(error.0)
    }
}

impl std::error::Error for CliError {}

#[cfg(test)]
mod tests {
    #[test]
    fn status_accepts_provider_selection_and_json_on_either_side() {
        use clap::Parser;
        for args in [
            vec!["usage", "status", "--providers", "openai,claude", "--json"],
            vec!["usage", "--json", "--providers", "codex", "status"],
        ] {
            let cli = super::Cli::try_parse_from(args).unwrap();
            assert!(matches!(cli.command, Some(super::Command::Status { .. })));
        }
    }
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::Barrier;

    use super::*;
    use crate::model::snapshot_not_found;

    #[test]
    fn custom_codex_home_never_falls_back_to_the_default_keychain_account() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reads = Arc::new(AtomicUsize::new(0));
        let reader: Arc<crate::providers::KeychainReader> = {
            let reads = reads.clone();
            Arc::new(move |_, _| {
                reads.fetch_add(1, Ordering::SeqCst);
                Some("fixture-default-account".into())
            })
        };
        let config = AppConfig {
            codex_dir: Some(
                std::env::temp_dir().join(format!("usage-custom-home-{}", uuid::Uuid::new_v4())),
            ),
            ..Default::default()
        };
        let custom = make_context_with_keychain(&config, reader.clone());
        assert!((custom.keychain)(crate::credentials::CODEX_KEYCHAIN_SERVICE, None).is_none());
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!((custom.keychain)(crate::credentials::CLAUDE_KEYCHAIN_SERVICE, None).is_some());
        let default = make_context_with_keychain(&AppConfig::default(), reader);
        assert!((default.keychain)(crate::credentials::CODEX_KEYCHAIN_SERVICE, None).is_some());
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn parses_tui_dashboard_alias_and_history_exports() {
        let cli = Cli::try_parse_from([
            "usage",
            "dashboard",
            "--year",
            "2026",
            "--monthly",
            "--providers",
            "codex,grok",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::History {
                year: Some(2026),
                monthly: true,
                once: false,
                json: false,
                ..
            })
        ));
        assert_eq!(cli.providers.as_deref(), Some("codex,grok"));
        assert!(Cli::try_parse_from(["usage", "history", "--year", "0"]).is_err());
        assert!(Cli::try_parse_from(["usage", "history", "--once", "--json"]).is_err());
    }

    #[test]
    fn history_ranges_are_inclusive_and_invalid_dates_fail() {
        let q = history_query(Some("7d"), None, None, None, None, None).unwrap();
        assert_eq!((q.to.unwrap() - q.from.unwrap()).num_days(), 6);
        assert!(
            history_query(
                None,
                NaiveDate::from_ymd_opt(2026, 10, 2),
                NaiveDate::from_ymd_opt(2026, 10, 1),
                None,
                None,
                None
            )
            .is_err()
        );
        assert!(Cli::try_parse_from(["usage", "history", "--from", "2026-02-30"]).is_err());
        assert!(
            Cli::try_parse_from(["usage", "history", "--range", "7d", "--year", "2026"]).is_err()
        );
        assert!(Cli::try_parse_from(["usage", "history", "--csv", "--json"]).is_err());
        let cli = Cli::try_parse_from([
            "usage",
            "history",
            "--range",
            "month",
            "--project",
            "usage",
            "--model",
            "gpt",
            "--refresh",
            "0",
            "--csv",
        ])
        .unwrap();
        assert_eq!(cli.refresh, Some(0));
    }
    struct WaitingProvider {
        id: &'static str,
        barrier: Arc<Barrier>,
        delay: Duration,
    }

    #[async_trait]
    impl UsageProvider for WaitingProvider {
        fn id(&self) -> &'static str {
            self.id
        }

        fn display_name(&self) -> &'static str {
            self.id
        }

        async fn query(&self) -> UsageSnapshot {
            self.barrier.wait().await;
            tokio::time::sleep(self.delay).await;
            snapshot_not_found(self.id, self.id, None)
        }
    }

    #[tokio::test]
    async fn once_queries_concurrently_and_keeps_provider_order() {
        let barrier = Arc::new(Barrier::new(3));
        let providers: Vec<Arc<dyn UsageProvider>> = [("claude", 30), ("codex", 5), ("grok", 15)]
            .into_iter()
            .map(|(id, milliseconds)| {
                Arc::new(WaitingProvider {
                    id,
                    barrier: Arc::clone(&barrier),
                    delay: Duration::from_millis(milliseconds),
                }) as Arc<dyn UsageProvider>
            })
            .collect();

        let snapshots = tokio::time::timeout(Duration::from_secs(2), query_all(&providers))
            .await
            .expect("all providers must start together");
        assert_eq!(
            snapshots
                .iter()
                .map(|snapshot| snapshot.tool.as_str())
                .collect::<Vec<_>>(),
            ["claude", "codex", "grok"]
        );
    }

    struct PanickingProvider;

    #[async_trait]
    impl UsageProvider for PanickingProvider {
        fn id(&self) -> &'static str {
            "broken"
        }

        fn display_name(&self) -> &'static str {
            "Broken"
        }

        async fn query(&self) -> UsageSnapshot {
            panic!("provider panic")
        }
    }

    #[tokio::test]
    async fn once_reports_failed_task_as_transient_snapshot() {
        let providers: Vec<Arc<dyn UsageProvider>> = vec![Arc::new(PanickingProvider)];
        let snapshots = query_all(&providers).await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].tool, "broken");
        assert!(snapshots[0].transient);
    }
}

#[cfg(test)]
mod app_runtime_tests {
    use super::*;
    #[test]
    fn automatic_paid_checks_only_belong_to_interactive_invocations() {
        for (args, expected) in [
            (vec!["usage"], true),
            (vec!["usage", "--once"], false),
            (vec!["usage", "--json"], false),
            (vec!["usage", "history"], true),
            (vec!["usage", "history", "--once"], false),
            (vec!["usage", "codex"], true),
            (
                vec!["usage", "codex", "fingerprint", "status", "account"],
                false,
            ),
            (vec!["usage", "providers"], false),
        ] {
            assert_eq!(Cli::try_parse_from(args).unwrap().interactive(), expected);
        }
    }
}
