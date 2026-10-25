//! LMFPD adapter for the fixed official Codex subscription channel.
use super::*;
use std::io::Read;
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
                            "官方订阅额度已用尽（HTTP 429）；预计 {} 恢复。本轮不重试，按 5 小时间隔再检查。",
                            time.with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M:%S %Z")
                        ),
                        None => "官方订阅额度已用尽（HTTP 429）；本轮不重试，按 5 小时间隔再检查。"
                            .into(),
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
    // `analysis` is only an alias for a single round. Never count that alias twice.
    let analyses: Vec<_> = match value["rounds"].as_array().filter(|r| !r.is_empty()) {
        Some(rounds) => rounds.iter().map(|r| &r["analysis"]).collect(),
        None => vec![&value["analysis"]],
    };
    if analyses.len() > 3 {
        return Err(Error::message("离线报告轮数超出本次三样本范围"));
    }
    let mut accepted = 0usize;
    let mut ranked = 0;
    let mut complete = 0;
    let mut reference_contains_model = false;
    let mut reference_label = None;
    for analysis in &analyses {
        let used = analysis["used_outputs"].as_u64().unwrap_or(0);
        if used > 3 {
            return Err(Error::message("离线报告样本计数无效"));
        }
        accepted += used as usize;
        let Some(candidates) = analysis["results"].as_array().filter(|r| !r.is_empty()) else {
            continue;
        };
        ranked += 1;
        if let Some(candidate) = candidates.iter().find(|v| {
            reference::matches_target(
                v["model"].as_str().unwrap_or_default(),
                v["display_name"].as_str(),
            )
        }) {
            reference_contains_model = true;
            reference_label = candidate["display_name"]
                .as_str()
                .or_else(|| candidate["model"].as_str())
                .map(str::to_owned);
        }
        if used == 3 && analysis["decision"] != "partial" && analysis["decision"] != "unscorable" {
            complete += 1;
        }
    }
    if accepted > 3 {
        return Err(Error::message("离线报告重复或超出本次三样本范围"));
    }
    report.accepted = accepted;
    report.scoring = Some(Scoring {
        rounds: analyses.len(),
        ranked_rounds: ranked,
        complete_rounds: complete,
    });
    if ranked == 0 {
        return Err(Error::message(
            "采样已记录，但没有可用的离线评分；原始证据已保留",
        ));
    }
    report.reference_contains_model = reference_contains_model;
    report.reference_label = reference_label;
    // Independent conditions do not have an aggregate winner or confidence.
    report.closest_candidate = if analyses.len() == 1 {
        analyses[0]["results"][0]["display_name"]
            .as_str()
            .or_else(|| analyses[0]["results"][0]["model"].as_str())
            .map(str::to_owned)
    } else {
        None
    };
    report.confidence = if analyses.len() == 1
        && complete == 1
        && analyses[0]["probability_status"] == "reference_calibrated"
    {
        analyses[0]["results"][0]["probability"]
            .as_f64()
            .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
    } else {
        None
    };
    report.status = if accepted == 3 && complete == analyses.len() {
        ReportStatus::Completed
    } else {
        ReportStatus::Partial
    };
    let mut message = format!(
        "采样 {accepted}/3 成功；{ranked}/{} 轮已生成排名，{complete}/{} 轮满足三样本要求。",
        analyses.len(),
        analyses.len()
    );
    if complete < analyses.len() {
        message.push_str("不足三条有效样本的轮次仅提供部分排名，不提供置信度。");
    }
    if reference_contains_model {
        if let Some(label) = &report.reference_label {
            message.push_str(&format!("参考组：{label}。"));
        }
        message.push_str("候选与置信度仅是参考库内的相对排名，不是模型身份认证。");
    } else {
        message.push_str("参考库尚未收录 gpt-6.1-sol；已保留样本和相近候选，不能据此判断换模。");
    }
    report.message = message;
    Ok(())
}

pub(super) struct CodexSubscriptionDetector;
// `sample` provides the subscription-only transport, but its reference batch
// groups answers by nuisance condition. Normal detection scores three answers
// together. Use the documented saved-round format and preserve completion proof.
pub(super) fn detection_input(collection: &Value) -> Result<Value> {
    let invalid = || Error::message("采样批次不是本次官方订阅的三条完整回答；原始样本已保留");
    if collection["schema_version"] != 1
        || collection["purpose"] != "reference"
        || collection["model"]["id"] != MODEL
    {
        return Err(invalid());
    }
    let samples = collection["samples"]
        .as_array()
        .filter(|s| s.len() == 3)
        .ok_or_else(invalid)?;
    let mut ids = std::collections::HashSet::new();
    let mut outputs = Vec::new();
    let mut evidence = Vec::new();
    for sample in samples {
        let id = sample["challenge_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(invalid)?;
        if !ids.insert(id)
            || sample["completion"] != "complete"
            || sample["response_model"] != MODEL
        {
            return Err(invalid());
        }
        let text = sample["text"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(invalid)?;
        let expected = sample["expected_count"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(invalid)?;
        outputs.push(serde_json::json!({"text":text,"expected_count":expected}));
        evidence.push(
            serde_json::json!({"challenge_id":id,"condition":sample["condition"],
            "completion":sample["completion"],"response_model":sample["response_model"]}),
        );
    }
    Ok(
        serde_json::json!({"source":{"path":"samples/result.json","channel":"codex-subscription","model":MODEL},
        "rounds":[{"outputs":outputs,"samples":evidence}]}),
    )
}
pub(super) fn score_saved(
    path: &Path,
    fpd: &Path,
    output: &str,
    cancel: &AtomicBool,
) -> Result<Value> {
    let mut bytes = Vec::new();
    fs::File::open(path.join("samples/result.json"))?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(Error::message("采样批次过大；原始证据已保留"));
    }
    let collection = serde_json::from_slice(&bytes).map_err(|_| Error::message("采样批次无效"))?;
    let input = detection_input(&collection)?;
    let input_path = path.join("detection-input.json");
    store::save(&input_path, &input)?;
    // Offline scoring has no subscription login available, and never calls sample.
    let mut analyze = command(fpd, &path.join("offline-home"));
    analyze
        .args(["--input"])
        .arg(&input_path)
        .args(["--strict", "--json", "--no-update-check"]);
    let scored = crate::credentials::output_with_cancel(analyze, Duration::from_secs(60), cancel)
        .ok_or_else(|| Error::message("离线评分超时、被取消或子进程失败"))?;
    if !scored.status.success() {
        return Err(Error::message("指纹离线评分失败；原始样本已保留"));
    }
    let value: Value = serde_json::from_slice(&scored.stdout)
        .map_err(|_| Error::message("检测器返回的 JSON 无效"))?;
    private_write(&path.join(output), &scored.stdout)?;
    Ok(value)
}
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
        // Sampling uses the account's credentials and explicit model arguments,
        // independently of the saved or mutable configuration used to launch Codex.
        let home = manager.home(&profile.id, profile.revision)?;
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
        score_saved(path, fpd, "detection.json", cancel)
    }
}
