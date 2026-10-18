//! LMFPD adapter for the fixed official Codex subscription channel.
use super::*;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::Duration;
// Read only bounded response metadata and whitelisted quota fields, never copy
// upstream error prose (which may echo private request data) into application logs.
fn small_json(path: &Path) -> Option<Value> {
    (fs::metadata(path).ok()?.len() <= 64 * 1024).then_some(())?;
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}
pub(super) fn failure_details(report: &mut Report) {
    let trace = report.directory.join("samples/trace");
    let Ok(tasks) = fs::read_dir(trace) else {
        return;
    };
    for task in tasks.flatten().take(3) {
        let Ok(attempts) = fs::read_dir(task.path()) else {
            continue;
        };
        for attempt in attempts.flatten().take(1) {
            let Ok(files) = fs::read_dir(attempt.path()) else {
                continue;
            };
            for file in files.flatten().take(12) {
                let name = file.file_name();
                let Some(prefix) = name.to_str().and_then(|s| s.strip_suffix(".response.json"))
                else {
                    continue;
                };
                let Some(status) = small_json(&file.path())
                    .and_then(|v| v["status"].as_u64())
                    .filter(|s| (400..=599).contains(s))
                else {
                    continue;
                };
                report.http_status = Some(status as u16);
                let body = small_json(&attempt.path().join(format!("{prefix}.body.txt")));
                if status == 429
                    && body
                        .as_ref()
                        .is_some_and(|v| v["error"]["type"] == "usage_limit_reached")
                {
                    report.quota_resets_at = body.and_then(|v| v["error"]["resets_at"].as_i64());
                    report.message = match report
                        .quota_resets_at
                        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                    {
                        Some(time) => format!(
                            "官方订阅额度已用尽（HTTP 429）；预计 {} 恢复。本轮不重试，下小时再检查。",
                            time.with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M:%S %Z")
                        ),
                        None => "官方订阅额度已用尽（HTTP 429）；本轮不重试，下小时再检查。".into(),
                    };
                } else {
                    report.message = format!(
                        "官方订阅采样失败（HTTP {status}）；原始证据已保留，本轮不重试。401/403 时请检查所选账号的官方登录。"
                    );
                }
                return;
            }
        }
    }
}
pub(super) fn resolve_fpd(manager: &Manager, explicit: Option<&Path>) -> Result<PathBuf> {
    let local = manager
        .root()
        .join("fingerprint/tools/node_modules/.bin/fpd");
    let path = explicit.map(Path::to_path_buf).unwrap_or_else(|| {
        if local.is_file() {
            local
        } else {
            PathBuf::from("fpd")
        }
    });
    if path.components().count() > 1 || path.is_absolute() {
        return path
            .canonicalize()
            .map_err(|_| Error::message("找不到 fpd；请先安装 lmfpd，或使用 --fpd 指定"));
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(&path))
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
        .ok_or_else(|| Error::message("找不到 fpd；请先安装 lmfpd，或使用 --fpd 指定"))
}
pub(super) fn command(fpd: &Path, home: &Path) -> Command {
    let mut command = Command::new(fpd);
    clear_auth_env(&mut command);
    for name in [
        "API_KEY",
        "MODEL",
        "BASE_URL",
        "USAGE_CODEX_RUN_CONTEXT",
        "USAGE_CODEX_NOTIFY_CONTEXT",
        "NODE_OPTIONS",
        "NODE_PATH",
    ] {
        command.env_remove(name);
    }
    command
        .env("CODEX_HOME", home)
        .env("FPD_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null());
    command
}
pub(super) fn summarize(report: &mut Report, value: &Value) -> Result<()> {
    if value["schema"] != "fpd-detection-v1" || value["cancelled"] == true {
        return Err(Error::message("检测器未产生有效的离线报告"));
    }
    let analysis = &value["analysis"];
    let candidates = analysis["results"]
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::message("本轮有效样本不足，无法评分；原始记录已保留"))?;
    report.accepted = analysis["used_outputs"].as_u64().unwrap_or(0) as usize;
    report.reference_contains_model = candidates.iter().any(|v| v["model"] == MODEL);
    report.closest_candidate = candidates[0]["model"].as_str().map(str::to_owned);
    report.status = if report.accepted == 3 {
        ReportStatus::Completed
    } else {
        ReportStatus::Partial
    };
    report.message = if report.reference_contains_model {
        "候选与置信度仅是参考库内的相对排名，不是模型身份认证。"
    } else {
        "参考库尚未收录 gpt-6.1-sol；已保留样本和相近候选，不能据此判断换模。"
    }
    .into();
    Ok(())
}

pub(super) struct CodexSubscriptionDetector;
impl CodexSubscriptionDetector {
    pub fn detect(
        &self,
        manager: &mut Manager,
        profile: &super::super::Profile,
        path: &Path,
        fpd: &Path,
        cancel: &AtomicBool,
    ) -> Result<Value> {
        let _lease = manager.oauth_run_lease(&profile.id)?;
        let home = manager.materialize(&profile.id, profile.revision)?;
        let auth = oauth::read_auth(&home.join("auth.json"))?;
        if profile.oauth.as_ref().is_none_or(|identity| {
            identity.account_id != auth.identity.account_id
                || identity.user_id != auth.identity.user_id
        }) {
            return Err(Error::message("OAuth 登录与所选账号不符，请重新登录该账号"));
        }
        let collection = path.join("samples");
        let mut sample = command(fpd, &home);
        sample
            .args([
                "sample",
                "--channel",
                "codex-subscription",
                "--subscription",
                "--model",
                MODEL,
                "--label",
                MODEL,
                "--family",
                "gpt",
                "--family-name",
                "GPT",
                "--response-model",
                MODEL,
                "--effort",
                "low",
                "--count",
                "3",
                "--parallel",
                "1",
                "--max-attempts",
                "1",
                "--json",
                "--output-dir",
            ])
            .arg(&collection)
            .current_dir(path);
        let collected =
            crate::credentials::output_with_cancel(sample, Duration::from_secs(900), cancel)
                .ok_or_else(|| Error::message("采样超时、被取消或子进程失败；本轮停止"))?;
        if !collected.status.success() {
            return Err(Error::message(
                "官方订阅采样未完成；可能是登录失效、限流或网络错误。查看样本记录，必要时重新登录；本轮不重试。",
            ));
        }
        let mut analyze = command(fpd, &home);
        analyze
            .args(["--input"])
            .arg(collection.join("result.json"))
            .args(["--json", "--no-update-check"]);
        let scored =
            crate::credentials::output_with_cancel(analyze, Duration::from_secs(60), cancel)
                .ok_or_else(|| Error::message("离线评分超时、被取消或子进程失败"))?;
        if !scored.status.success() {
            return Err(Error::message("指纹离线评分失败；原始样本已保留"));
        }
        let value: Value = serde_json::from_slice(&scored.stdout)
            .map_err(|_| Error::message("检测器返回的 JSON 无效"))?;

        private_write(&path.join("detection.json"), &scored.stdout)?;
        Ok(value)
    }
}
