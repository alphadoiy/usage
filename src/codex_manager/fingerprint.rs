//! Official subscription fingerprint checks: domain model and CLI entry point.
use super::{AuthMode, Error, Manager, Result, clear_auth_env, oauth, private_dir, private_write};
use clap::Subcommand;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};
mod codex;
pub(crate) mod runtime;
mod service;
mod store;
pub(crate) use service::FingerprintService;
#[cfg(test)]
mod tests;
const MODEL: &str = "gpt-6.1-sol";
const INTERVAL: i64 = 3600;
const RETAIN: usize = 168;
#[derive(Debug, Subcommand)]
pub enum FingerprintCommand {
    /// 采集 3 条官方订阅样本并本地评分；固定 gpt-6.1-sol / low
    Run {
        profile: String,
        /// 一小时内已尝试过则跳过；供定时任务使用
        #[arg(long)]
        due: bool,
        #[arg(long)]
        fpd: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// 查看最近一次测试、报告位置和定时任务配置
    Status {
        profile: String,
        #[arg(long)]
        json: bool,
    },
    /// 启用 Usage 应用内每小时测试；退出 Usage 后停止
    Enable {
        profile: String,
        #[arg(long)]
        fpd: Option<PathBuf>,
    },
    /// 停止每小时任务，保留已有测试结果
    Disable { profile: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReportStatus {
    Running,
    Completed,
    Partial,
    Failed,
    Interrupted,
    #[serde(other)]
    Unknown,
}
impl std::fmt::Display for ReportStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Unknown => "unknown",
        })
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Report {
    pub(crate) model: String,
    pub(crate) channel: String,
    pub(crate) started_at: i64,
    pub(crate) finished_at: Option<i64>,
    pub(crate) status: ReportStatus,
    pub(crate) directory: PathBuf,
    pub(crate) accepted: usize,
    pub(crate) reference_contains_model: bool,
    pub(crate) closest_candidate: Option<String>,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) http_status: Option<u16>,
    #[serde(default)]
    pub(crate) quota_resets_at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Snapshot {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub latest: Option<Report>,
    pub history: Vec<Report>,
    pub warnings: Vec<String>,
}
fn account(manager: &Manager, selector: &str) -> Result<super::Profile> {
    let profile = manager.resolve(selector)?;
    if profile.auth_mode != AuthMode::Oauth {
        return Err(Error::message("指纹测试仅支持 ChatGPT 官方 OAuth 订阅账号"));
    }
    Ok(profile)
}
#[cfg(test)]
fn directory(manager: &Manager, id: &str) -> PathBuf {
    store::Store::new(manager.root().to_owned()).account_dir(id)
}
fn print_report(report: &Report, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).map_err(|_| Error::message("无法编码指纹报告"))?
        );
    } else {
        println!(
            "{} · 官方订阅 · {} · {}/3 样本\n{}\n报告：{}",
            report.model,
            report.status,
            report.accepted,
            report.message,
            report.directory.display()
        );
        if let Some(candidate) = &report.closest_candidate {
            println!("参考库最近候选：{candidate}");
        }
    }
    Ok(())
}

pub async fn dispatch(command: FingerprintCommand) -> Result<i32> {
    tokio::task::spawn_blocking(move || {
        let service = FingerprintService::from_environment();
        match command {
            FingerprintCommand::Run {
                profile,
                due,
                fpd,
                json,
            } => match service.run(
                &profile,
                due,
                fpd.as_deref(),
                &std::sync::atomic::AtomicBool::new(false),
            )? {
                Some(report) => {
                    print_report(&report, json)?;
                    Ok(i32::from(report.status == ReportStatus::Failed))
                }
                None => {
                    if json {
                        println!("{{\"status\":\"skipped\"}}");
                    } else {
                        println!("已有测试运行中，或距离上次尝试不足一小时，已跳过。");
                    }
                    Ok(0)
                }
            },
            FingerprintCommand::Status { profile, json } => {
                let snapshot = service.status(&profile)?;
                if let Some(report) = snapshot.latest {
                    print_report(&report, json)?;
                } else if json {
                    println!("null");
                } else {
                    println!("尚无测试记录。");
                }
                if !json {
                    println!(
                        "定时任务：{}",
                        if snapshot.enabled {
                            "已启用 · 仅 Usage 打开期间每小时测试"
                        } else {
                            "未启用"
                        }
                    );
                    for warning in snapshot.warnings {
                        println!("提示：{warning}");
                    }
                }
                Ok(0)
            }
            FingerprintCommand::Enable { profile, fpd } => {
                let name = service.enable(&profile, fpd.as_deref())?;
                println!("已启用：{name} · {MODEL} · Usage 打开期间每小时测试，退出后停止。");
                Ok(0)
            }
            FingerprintCommand::Disable { profile } => {
                service.disable(&profile)?;
                println!("已停用应用内定时测试；历史结果保留。");
                Ok(0)
            }
        }
    })
    .await
    .map_err(|_| Error::message("指纹后台任务失败"))?
}
