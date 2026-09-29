//! 命令行入口。不带参数时打开 ratatui 界面，`--once` / `--json` 只查询一次。

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
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
    /// bar (Braille 进度条)、ring (圆环) 或 statusline (圆点状态栏)
    #[arg(long)]
    style: Option<String>,
    /// 逗号分隔的提供方，例如 claude,codex
    #[arg(long, global = true)]
    providers: Option<String>,
    /// 自动刷新秒数，0 表示不自动刷新
    #[arg(long)]
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

pub async fn run() -> i32 {
    let cli = Cli::parse();
    match dispatch(cli).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            2
        }
    }
}

async fn dispatch(cli: Cli) -> Result<i32, CliError> {
    match cli.command {
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
    tokio::task::spawn_blocking(move || crate::ui::run(handle, providers, style_config, path))
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
        jobs.spawn(async move {
            (
                index,
                query_with_timeout(provider.as_ref(), QUERY_TIMEOUT).await,
            )
        });
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
    let home = home_dir();
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
        keychain: Arc::new(read_macos_keychain),
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
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::Barrier;

    use super::*;
    use crate::model::snapshot_not_found;

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
