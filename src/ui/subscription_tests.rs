//! Account-bound subscription checks. Official status pages have no dependency on this view.
use crate::codex_manager::fingerprint::{Report, ReportStatus, Snapshot};
use crate::provider_catalog::{Provider, ProviderKind};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use std::collections::HashMap;

fn display(report: Option<&Report>) -> (&'static str, Color, &'static str) {
    match report.map(|r| r.status) {
        Some(ReportStatus::Completed) => ("●", Color::Green, "测试完成"),
        Some(ReportStatus::Partial) => ("▲", Color::Yellow, "评分部分完成"),
        Some(ReportStatus::Running) => ("◆", Color::Cyan, "测试中"),
        Some(ReportStatus::Interrupted) => ("○", Color::DarkGray, "已中断"),
        Some(ReportStatus::Failed) if report.is_some_and(|r| r.http_status == Some(429)) => (
            "▲",
            Color::Yellow,
            if report.is_some_and(|r| r.quota_resets_at.is_some()) {
                "额度耗尽"
            } else {
                "限流"
            },
        ),
        Some(ReportStatus::Failed) => ("×", Color::Red, "测试失败"),
        Some(ReportStatus::Unknown) => ("○", Color::DarkGray, "状态未知"),
        None => ("○", Color::DarkGray, "等待测试"),
    }
}
fn ranking(report: &Report) -> Option<String> {
    let candidate = super::overview::clean(report.closest_candidate.as_deref()?);
    let confidence = match report.confidence {
        Some(value) => format!("库内置信度 {:.2}%", value * 100.0),
        None if report.status == ReportStatus::Completed => "置信度未校准".into(),
        None => "样本不足，仅部分排名".into(),
    };
    Some(format!("最接近：{candidate} · {confidence}"))
}
pub(super) fn supports(provider: &Provider) -> bool {
    provider.kind == ProviderKind::Subscription
}
#[derive(Default)]
pub(super) struct Tests {
    loaded: bool,
    snapshots: HashMap<String, Snapshot>,
    error: Option<String>,
}
impl Tests {
    pub fn set(&mut self, result: Result<Vec<Snapshot>, String>) -> bool {
        let (snapshots, error) = match result {
            Ok(snapshots) => (
                snapshots
                    .into_iter()
                    .filter(|s| !s.id.is_empty())
                    .map(|s| (s.id.clone(), s))
                    .collect(),
                None,
            ),
            Err(error) => (HashMap::new(), Some(error)),
        };
        let changed = !self.loaded || self.snapshots != snapshots || self.error != error;
        self.loaded = true;
        self.snapshots = snapshots;
        self.error = error;
        changed
    }
    fn snapshot(&self, provider: &Provider) -> Option<&Snapshot> {
        supports(provider)
            .then(|| self.snapshots.get(&provider.id))
            .flatten()
    }
    pub fn summary(&self, provider: &Provider) -> Vec<Line<'static>> {
        if !supports(provider) {
            return Vec::new();
        }
        if !self.loaded {
            return vec![Line::from("○ 订阅测试 · 读取中… · t 详情").dim()];
        }
        if self.error.is_some() {
            return vec![Line::from("○ 订阅测试 · 记录读取失败 · t 详情").dim()];
        }
        let Some(snapshot) = self.snapshot(provider) else {
            return vec![Line::from("○ 订阅测试 · 未启用 · t 详情").dim()];
        };
        let (symbol, color, label) = if snapshot.blocked_reason.is_some() {
            ("○", Color::DarkGray, "暂不测试")
        } else {
            display(snapshot.latest.as_ref())
        };
        let mut lines = vec![Line::from(format!("{symbol} 订阅测试 · {label} · t 详情")).fg(color)];
        if let Some(reason) = &snapshot.blocked_reason {
            lines.push(Line::from(format!("  {}", super::overview::clean(reason))).dim());
        } else if let Some(report) = &snapshot.latest
            && let Some(result) = ranking(report)
        {
            lines.push(Line::from(format!("  {result}")));
        }
        if !snapshot.warnings.is_empty() {
            lines.push(Line::from("▲ 此账号部分测试记录异常 · t 查看").fg(Color::Yellow));
        }
        if snapshot.update_notice.is_some() {
            lines.push(Line::from("▲ lmfpd 有更新 · t 查看版本").fg(Color::Yellow));
        }
        lines
    }
    pub fn details(&self, provider: &Provider, width: u16) -> Vec<Line<'static>> {
        use super::overview::clean;
        if !supports(provider) {
            return Vec::new();
        }
        let mut lines = vec![Line::from(format!("{} · 订阅测试", clean(&provider.name))).bold()];
        if let Some(error) = &self.error {
            lines.push(Line::from(clean(error)).fg(Color::Yellow));
        }
        if let Some(snapshot) = self.snapshot(provider) {
            let (symbol, color, label) = display(snapshot.latest.as_ref());
            if let Some(reason) = &snapshot.blocked_reason {
                lines.push(Line::from("○ 暂不测试").dim());
                lines.push(Line::from(clean(reason)));
            } else {
                lines.push(Line::from(format!("{symbol} {label}")).fg(color));
            }
            if let Some(report) = &snapshot.latest {
                if snapshot.blocked_reason.is_some() {
                    lines.push(Line::from(format!("上次结果：{symbol} {label}")).dim());
                }
                lines.push(Line::from(format!("{} · low", clean(&report.model))).dim());
                if snapshot.blocked_reason.is_none()
                    && matches!(
                        report.status,
                        ReportStatus::Completed | ReportStatus::Partial
                    )
                {
                    if let Some(result) = ranking(report) {
                        lines.push(Line::from(result));
                    } else if let Some(scoring) = &report.scoring {
                        let note = if scoring.complete_rounds == 0 {
                            "每轮样本不足，暂无置信度".into()
                        } else {
                            format!("完整评分 {}/{} 轮", scoring.complete_rounds, scoring.rounds)
                        };
                        lines.push(Line::from(format!(
                            "排名 {}/{} 轮 · {note}",
                            scoring.ranked_rounds, scoring.rounds
                        )));
                    }
                    if let Some(label) = &report.reference_label
                        && Some(label) != report.closest_candidate.as_ref()
                    {
                        lines.push(Line::from(format!("参考组：{}", clean(label))).dim());
                    }
                    lines.push(
                        Line::from(if report.reference_contains_model {
                            "排名仅供参考，不代表服务健康或模型身份。"
                        } else {
                            "参考库未收录此模型，排名不能用于确认模型身份。"
                        })
                        .dim(),
                    );
                } else if report.status == ReportStatus::Running
                    && snapshot.blocked_reason.is_none()
                {
                    lines.push(Line::from("正在采集，完成后自动更新。").dim());
                } else if !matches!(
                    report.status,
                    ReportStatus::Completed | ReportStatus::Partial | ReportStatus::Running
                ) {
                    // Preserve actionable failures (including quota reset times).
                    lines.push(Line::from(clean(&report.message)));
                }
            }
            for warning in &snapshot.warnings {
                lines.push(Line::from(clean(warning)).fg(Color::Yellow));
            }
            if let Some(notice) = &snapshot.update_notice {
                lines.push(Line::from(clean(notice)).fg(Color::Yellow));
                lines.push(Line::from("更新命令见 docs/fingerprint.md").dim());
            }
            lines.push(Line::from(""));
            lines.push(Line::from("最近 24 次测试 · 旧 → 新").bold());
            if snapshot.history.is_empty() {
                lines.push(Line::from("暂无完成记录").dim());
            } else {
                lines.push(Line::from(
                    snapshot
                        .history
                        .iter()
                        .enumerate()
                        .map(|(i, r)| {
                            let (symbol, color, _) = display(Some(r));
                            Span::styled(
                                format!("{}{symbol}", if i == 0 { "" } else { " " }),
                                Style::new().fg(color),
                            )
                        })
                        .collect::<Vec<_>>(),
                ));
                lines.push(Line::from("● 完成  ◆ 测试中  ▲ 部分/限流  × 失败  ○ 其他").dim());
            }
        } else if !self.loaded {
            lines.push(Line::from("正在读取此账号的订阅测试记录…").dim());
        } else if self.error.is_none() {
            lines.push(Line::from("此 Provider 尚未启用订阅测试").dim());
        }
        lines.push(Line::from("t / Esc 返回 · ↑/↓ 滚动 · ←/→ 切换 Provider").dim());
        super::overview::wrap(lines, width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn provider(id: &str) -> Provider {
        Provider {
            operations: Vec::new(),
            id: id.into(),
            name: "Official account".into(),
            agent: None,
            kind: ProviderKind::Subscription,
            configured_model: None,
            models: crate::provider_catalog::Models::Ready(Vec::new()),
            usage: Default::default(),
            performance: None,
        }
    }
    fn text(lines: Vec<Line<'static>>) -> String {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn reports_history_and_engine_updates_belong_only_to_their_provider() {
        let report = Report {
            model: "gpt-6.1-sol".into(),
            channel: "codex-subscription".into(),
            started_at: 1792308647,
            finished_at: Some(1792308651),
            status: ReportStatus::Failed,
            directory: Default::default(),
            accepted: 0,
            scoring: None,
            reference_contains_model: false,
            reference_label: None,
            closest_candidate: None,
            confidence: None,
            message: "预计 17:15 恢复".into(),
            http_status: Some(429),
            quota_resets_at: Some(1792314948),
        };
        let mut tests = Tests::default();
        tests.set(Ok(vec![Snapshot {
            id: "a".into(),
            name: "stale name".into(),
            enabled: true,
            blocked_reason: None,
            latest: Some(report.clone()),
            history: vec![report],
            warnings: Vec::new(),
            update_notice: Some("lmfpd 有更新：0.0.1 → 0.0.2".into()),
        }]));
        assert!(text(tests.summary(&provider("a"))).contains("额度耗尽"));
        assert!(text(tests.summary(&provider("a"))).contains("lmfpd 有更新"));
        assert!(!text(tests.summary(&provider("b"))).contains("额度耗尽"));
        assert!(!text(tests.summary(&provider("b"))).contains("lmfpd"));
        for width in [30, 72] {
            let text = text(tests.details(&provider("a"), width))
                .split_whitespace()
                .collect::<String>();
            assert!(text.contains("最近24次测试"));
            assert!(text.contains("17:15"));
            assert!(!text.contains("stalename"));
        }
        let mut unsupported = provider("a");
        unsupported.kind = ProviderKind::SubscriptionService;
        assert!(tests.summary(&unsupported).is_empty());
        assert!(tests.details(&unsupported, 72).is_empty());
    }
    #[test]
    fn read_failures_stay_in_subscription_providers_and_recover_without_zero_results() {
        let mut tests = Tests::default();
        tests.set(Err("read failure".into()));
        assert!(text(tests.summary(&provider("a"))).contains("记录读取失败"));
        let mut byok = provider("a");
        byok.kind = ProviderKind::CompatibleApi;
        assert!(tests.summary(&byok).is_empty());
        tests.set(Ok(Vec::new()));
        assert!(text(tests.summary(&provider("a"))).contains("未启用"));
        assert!(!text(tests.summary(&provider("a"))).contains("读取失败"));
    }
    #[test]
    fn successful_sampling_is_distinct_from_partial_scoring() {
        let report = Report {
            model: "gpt-6.1-sol".into(),
            channel: "codex-subscription".into(),
            started_at: 1792308647,
            finished_at: Some(1792308651),
            status: ReportStatus::Partial,
            directory: Default::default(),
            accepted: 3,
            scoring: Some(crate::codex_manager::fingerprint::Scoring {
                rounds: 3,
                ranked_rounds: 3,
                complete_rounds: 0,
            }),
            reference_contains_model: true,
            reference_label: Some("gpt-6-astra/6.1-sol".into()),
            closest_candidate: None,
            confidence: None,
            message: "各条件独立评分，不提供总排名或置信度".into(),
            http_status: None,
            quota_resets_at: None,
        };
        let mut tests = Tests::default();
        tests.set(Ok(vec![Snapshot {
            id: "a".into(),
            latest: Some(report.clone()),
            history: vec![report.clone(), report],
            ..Default::default()
        }]));
        let summary_lines = tests.summary(&provider("a"));
        assert_eq!(summary_lines.len(), 1);
        let summary = text(summary_lines);
        assert!(summary.contains("评分部分完成"));
        assert!(!summary.contains("采样"));
        assert!(!summary.contains("轮"));
        let details = text(tests.details(&provider("a"), 120));
        assert!(details.contains("评分部分完成"));
        assert!(details.contains("排名 3/3 轮 · 每轮样本不足，暂无置信度"));
        assert!(details.contains("参考组：gpt-6-astra/6.1-sol"));
        assert!(!details.contains("未收录"));
        assert!(details.contains("▲ ▲"));
        assert!(!details.contains("采样 3/3"));
        assert!(!details.contains("各条件独立评分，不提供总排名或置信度"));
        assert!(!details.contains("0/3"));
        assert!(!details.contains("测试失败"));

        let mut complete = tests.snapshots["a"].clone();
        let report = complete.latest.as_mut().unwrap();
        report.status = ReportStatus::Completed;
        report.closest_candidate = Some("gpt-6-astra/6.1-sol".into());
        report.confidence = Some(0.9983446737190644);
        tests.set(Ok(vec![complete]));
        for output in [
            tests.summary(&provider("a")),
            tests.details(&provider("a"), 120),
        ] {
            let output = text(output);
            assert!(output.contains("最接近：gpt-6-astra/6.1-sol · 库内置信度 99.83%"));
            assert!(!output.contains("每轮样本不足"));
            assert!(!output.contains("每小时测试"));
            assert!(!output.contains("Usage 运行期间"));
            assert!(!output.contains("采样 3/3"));
            assert!(!output.contains("10-18"));
            assert!(
                !output.contains("参考组："),
                "the winner and expected reference group must not be repeated"
            );
        }

        tests.snapshots.get_mut("a").unwrap().blocked_reason =
            Some("参考库未收录 gpt-6.1-sol，已跳过测试以避免消耗订阅额度。".into());
        let summary = text(tests.summary(&provider("a")));
        assert!(summary.contains("暂不测试"));
        assert!(summary.contains("参考库未收录 gpt-6.1-sol"));
        assert!(!summary.contains("评分部分完成"));
        let details = text(tests.details(&provider("a"), 120));
        assert!(details.contains("上次结果：● 测试完成"));
        assert!(details.contains("▲ ▲"));
        assert!(!details.contains("排名 3/3"));
    }
}
