//! Home board layout and public-service status history.
use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};

use crate::service_status::{self, ServiceStatus, State};

pub(super) const SOURCES: [service_status::Source; 3] = [
    service_status::OPENAI,
    service_status::CLAUDE,
    service_status::GROK,
];
const HISTORY_LIMIT: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Focus {
    #[default]
    Providers,
    Connections,
    Projects,
}
impl Focus {
    pub fn next(self, reverse: bool) -> Self {
        match (self, reverse) {
            (Self::Providers, false) | (Self::Projects, true) => Self::Connections,
            (Self::Connections, false) | (Self::Providers, true) => Self::Projects,
            _ => Self::Providers,
        }
    }
}

pub(super) fn areas(area: Rect) -> [Rect; 3] {
    if area.width >= 80 {
        let [top, projects] =
            Layout::vertical([Constraint::Percentage(54), Constraint::Fill(1)]).areas(area);
        let [providers, connections] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Fill(1)]).areas(top);
        [providers, connections, projects]
    } else {
        Layout::vertical([
            Constraint::Length(area.height / 3),
            Constraint::Length((area.height / 3).max(8)),
            Constraint::Fill(1),
        ])
        .areas(area)
    }
}

pub(super) fn board(area: Rect, buf: &mut Buffer, title: &str, active: bool) -> Rect {
    let block = Block::bordered()
        .title(format!(" {title} "))
        .border_style(if active {
            Color::LightBlue
        } else {
            Color::DarkGray
        });
    let inner = block.inner(area);
    block.render(area, buf);
    inner
}

#[derive(Debug, Clone)]
struct Sample {
    checked_at: DateTime<Utc>,
    state: State,
}

#[derive(Default)]
pub(super) struct StatusHistory(HashMap<String, VecDeque<Sample>>);
impl StatusHistory {
    pub fn record(&mut self, status: &ServiceStatus) {
        let samples = self.0.entry(status.provider.clone()).or_default();
        if samples
            .back()
            .is_some_and(|sample| sample.checked_at == status.checked_at)
        {
            return;
        }
        // A failed refresh is unknown, even when the last successful display is retained.
        let state = if status.fetched_at.is_none() || status.stale || status.error.is_some() {
            State::Unknown
        } else {
            status.state
        };
        samples.push_back(Sample {
            checked_at: status.checked_at,
            state,
        });
        while samples.len() > HISTORY_LIMIT {
            samples.pop_front();
        }
    }
}

// Symbols have equal height; their shape and label carry meaning without color.
fn indicator(state: State) -> (&'static str, Color) {
    match state {
        State::Operational => ("●", Color::Green),
        State::Maintenance => ("◆", Color::Cyan),
        State::Degraded => ("▲", Color::Yellow),
        State::PartialOutage => ("!", Color::LightRed),
        State::MajorOutage => ("×", Color::Red),
        State::Unknown => ("○", Color::DarkGray),
    }
}

fn current(status: Option<&ServiceStatus>, pending: bool) -> (State, &str) {
    match status {
        Some(s) if s.stale => (State::Unknown, "更新失败"),
        Some(s) if s.fetched_at.is_none() => (State::Unknown, "查询失败"),
        Some(s) if s.error.is_some() => (State::Unknown, "详情不完整"),
        Some(s) => (s.state, s.label()),
        None => (State::Unknown, if pending { "查询中" } else { "尚未查询" }),
    }
}

fn status_line(name: &str, status: Option<&ServiceStatus>, pending: bool) -> Line<'static> {
    let (state, label) = current(status, pending);
    let (symbol, color) = indicator(state);
    Line::from(vec![
        Span::styled(format!("{symbol} "), Style::new().fg(color)),
        Span::raw(format!("{name} · ")),
        Span::styled(label.to_owned(), Style::new().fg(color)),
    ])
}

fn time(value: DateTime<Utc>) -> String {
    value
        .with_timezone(&chrono::Local)
        .format("%m-%d %H:%M:%S")
        .to_string()
}

fn list_lines(
    area: Rect,
    statuses: &HashMap<String, ServiceStatus>,
    pending: bool,
    selected: usize,
    active: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, source) in SOURCES.iter().enumerate() {
        let status = statuses.get(source.id);
        let name = if area.width < 32 && source.id == "grok" {
            "Grok"
        } else {
            source.name
        };
        let mut line = status_line(name, status, pending);
        line.spans.insert(
            0,
            Span::raw(if active && selected == index {
                "> "
            } else {
                "  "
            }),
        );
        if active && selected == index {
            line = line.bold();
        }
        lines.push(line);
        if area.height >= 8 {
            let detail = status.map_or_else(
                || "等待状态源数据".into(),
                |s| {
                    if s.stale {
                        format!(
                            "上次：{} · {}",
                            s.label(),
                            s.fetched_at.map(time).unwrap_or_default()
                        )
                    } else if let Some(incident) = s.incidents.first() {
                        super::overview::clean(&incident.name)
                    } else {
                        format!("查询 {}", time(s.checked_at))
                    }
                },
            );
            lines.push(Line::from(format!("    {detail}")).dim());
        }
    }
    lines.push(
        Line::from(if pending {
            "状态更新中…"
        } else {
            "Enter 查看事件与历史"
        })
        .dim(),
    );
    lines.push(Line::from("来源：官方状态页").dim());
    lines
}

fn detail_lines(
    area: Rect,
    source: service_status::Source,
    status: Option<&ServiceStatus>,
    history: &StatusHistory,
    pending: bool,
) -> Vec<Line<'static>> {
    use super::overview::clean;
    let mut lines = vec![status_line(source.name, status, pending).bold()];
    if let Some(s) = status {
        lines.push(Line::from(format!("最近查询：{}", time(s.checked_at))).dim());
        if let Some(fetched) = s.fetched_at.filter(|_| s.stale || s.error.is_some()) {
            let prefix = if s.stale {
                "上次报告"
            } else {
                "已获取报告"
            };
            lines.push(Line::from(format!("{prefix}：{} · {}", s.label(), time(fetched))).dim());
        }
        if let Some(error) = &s.error {
            lines.push(Line::from(format!("状态源查询异常：{}", clean(error))).fg(Color::Yellow));
        }
        if !s.description.is_empty() {
            lines.push(Line::from(clean(&s.description)));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from("事件").bold());
    if let Some(s) = status {
        if s.stale {
            lines.push(Line::from("以下为上次报告，当前事件待更新").fg(Color::Yellow));
        } else if s.error.is_some() {
            lines.push(Line::from("事件详情可能不完整").fg(Color::Yellow));
        }
        if s.incidents.is_empty() {
            lines.push(
                Line::from(if s.stale || s.error.is_some() || s.fetched_at.is_none() {
                    "暂无可确认的事件信息"
                } else {
                    "状态源未报告未解决事件"
                })
                .dim(),
            );
        }
        for incident in &s.incidents {
            lines.push(Line::from(format!(
                "• {} · {}",
                clean(&incident.name),
                clean(&incident.status)
            )));
            if !incident.url.is_empty() {
                lines.push(Line::from(clean(&incident.url)).dim());
            }
        }
        let affected: Vec<_> = s
            .components
            .iter()
            .filter(|c| c.state != State::Operational)
            .collect();
        if !affected.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from("受影响服务（报告值）").bold());
            for component in affected {
                let (symbol, color) = indicator(component.state);
                lines.push(
                    Line::from(format!(
                        "{symbol} {} · {}",
                        clean(&component.name),
                        component.state.label()
                    ))
                    .fg(color),
                );
            }
        }
    } else {
        lines.push(Line::from("等待状态源数据").dim());
    }
    lines.push(Line::from(""));
    lines.push(Line::from("最近 24 次查询 · 旧 → 新").bold());
    if let Some(samples) = history
        .0
        .get(source.id)
        .filter(|samples| !samples.is_empty())
    {
        lines.push(Line::from(
            samples
                .iter()
                .map(|sample| {
                    let (symbol, color) = indicator(sample.state);
                    Span::styled(symbol, Style::new().fg(color))
                })
                .collect::<Vec<_>>(),
        ));
    } else {
        lines.push(Line::from("暂无查询记录").dim());
    }
    lines.push(Line::from("● 正常  ◆ 维护  ▲ 降级  ! 部分故障  × 严重故障  ○ 未知").dim());
    lines.push(Line::from("每个符号代表一次查询，非固定时间间隔").dim());
    lines.push(Line::from(""));
    lines.push(Line::from(source.page_url).dim());
    lines.push(Line::from("来源为官方状态页，非本机 API 探测").dim());
    super::overview::wrap(lines, area.width)
}

struct TestDisplay {
    symbol: &'static str,
    color: Color,
    label: &'static str,
    priority: u8,
}
fn fingerprint_display(report: Option<&crate::codex_manager::fingerprint::Report>) -> TestDisplay {
    use crate::codex_manager::fingerprint::ReportStatus;
    let (symbol, color, label, priority) = match report.map(|r| r.status) {
        Some(ReportStatus::Completed) => ("●", Color::Green, "测试完成", 0),
        Some(ReportStatus::Partial) => ("▲", Color::Yellow, "样本不完整", 3),
        Some(ReportStatus::Running) => ("◆", Color::Cyan, "测试中", 1),
        Some(ReportStatus::Interrupted) => ("○", Color::DarkGray, "已中断", 2),
        Some(ReportStatus::Failed) if report.is_some_and(|r| r.http_status == Some(429)) => (
            "▲",
            Color::Yellow,
            if report.is_some_and(|r| r.quota_resets_at.is_some()) {
                "额度耗尽"
            } else {
                "限流"
            },
            3,
        ),
        Some(ReportStatus::Failed) => ("×", Color::Red, "测试失败", 4),
        Some(ReportStatus::Unknown) => ("○", Color::DarkGray, "状态未知", 2),
        None => ("○", Color::DarkGray, "等待测试", 2),
    };
    TestDisplay {
        symbol,
        color,
        label,
        priority,
    }
}
fn fingerprint_details(
    area: Rect,
    snapshots: &[crate::codex_manager::fingerprint::Snapshot],
    error: Option<&str>,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from("GPT 官方订阅 · 本机指纹测试").bold(),
        Line::from("仅 Usage 打开期间每小时测试 · 退出后停止").dim(),
    ];
    if let Some(error) = error {
        lines.push(Line::from(error.to_owned()).fg(Color::Yellow));
    }
    for snapshot in snapshots {
        let TestDisplay {
            symbol,
            color,
            label,
            ..
        } = fingerprint_display(snapshot.latest.as_ref());
        lines.push(Line::from(""));
        lines.push(
            Line::from(format!(
                "{symbol} {} · {label}",
                super::overview::clean(&snapshot.name)
            ))
            .fg(color)
            .bold(),
        );
        lines.push(
            Line::from(if snapshot.enabled {
                "调度已启用 · gpt-6.1-sol · low · 3 条样本"
            } else {
                "调度已停用 · 以下为历史结果"
            })
            .dim(),
        );
        if let Some(report) = &snapshot.latest {
            if let Some(at) = DateTime::from_timestamp(report.started_at, 0) {
                lines.push(Line::from(format!(
                    "最近测试：{} · {}/3 样本",
                    time(at),
                    report.accepted
                )));
            }
            lines.push(Line::from(super::overview::clean(&report.message)));
            if let Some(model) = &report.closest_candidate {
                lines.push(Line::from(format!(
                    "参考库最近候选：{}（非身份认证）",
                    super::overview::clean(model)
                )));
            }
        }
        for warning in &snapshot.warnings {
            lines.push(Line::from(super::overview::clean(warning)).fg(Color::Yellow));
        }
        if let Some(notice) = &snapshot.update_notice {
            lines.push(Line::from(notice.clone()).fg(Color::Yellow));
            lines.push(Line::from("手动更新当前 lmfpd 安装；更新命令见 docs/fingerprint.md").dim());
        }
        lines.push(Line::from("最近 24 次测试 · 旧 → 新").bold());
        if snapshot.history.is_empty() {
            lines.push(Line::from("暂无完成记录").dim());
        } else {
            lines.push(Line::from(
                snapshot
                    .history
                    .iter()
                    .map(|r| {
                        let TestDisplay { symbol, color, .. } = fingerprint_display(Some(r));
                        Span::styled(symbol, Style::new().fg(color))
                    })
                    .collect::<Vec<_>>(),
            ));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from("● 完成  ◆ 测试中  ▲ 限流/部分结果  × 失败  ○ 未测试/中断").dim());
    lines.push(Line::from("每个符号代表一次本机测试；采样消耗订阅额度。").dim());
    lines.push(Line::from("指纹排名仅供参考，不代表官方服务健康度或模型身份认证。").dim());
    super::overview::wrap(lines, area.width)
}

#[derive(Default)]
pub(super) struct Connections {
    fingerprints: Vec<crate::codex_manager::fingerprint::Snapshot>,
    fingerprint_error: Option<String>,
    selected: usize,
    details: bool,
    scroll: u16,
}

impl Connections {
    pub fn has_fingerprints(&self) -> bool {
        !self.fingerprints.is_empty() || self.fingerprint_error.is_some()
    }
    pub fn set_fingerprints(
        &mut self,
        result: Result<Vec<crate::codex_manager::fingerprint::Snapshot>, String>,
    ) -> bool {
        let (snapshots, error) = match result {
            Ok(s) => (s, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let changed = self.fingerprints != snapshots || self.fingerprint_error != error;
        self.fingerprints = snapshots;
        self.fingerprint_error = error;
        self.selected = self
            .selected
            .min(SOURCES.len() - usize::from(!self.has_fingerprints()));
        changed
    }
    pub fn is_details(&self) -> bool {
        self.details
    }

    pub fn open(&mut self) {
        self.details = true;
        self.scroll = 0;
    }

    pub fn back(&mut self) {
        self.details = false;
        self.scroll = 0;
    }

    pub fn move_by(
        &mut self,
        delta: i16,
        area: Rect,
        statuses: &HashMap<String, ServiceStatus>,
        history: &StatusHistory,
        pending: bool,
    ) {
        if self.details {
            let max = self
                .lines(area, statuses, history, pending, true)
                .len()
                .saturating_sub(area.height as usize)
                .min(u16::MAX as usize) as i32;
            self.scroll = (i32::from(self.scroll) + i32::from(delta)).clamp(0, max) as u16;
        } else {
            self.selected = (self.selected as i32 + i32::from(delta)).clamp(
                0,
                SOURCES.len() as i32 - 1 + i32::from(self.has_fingerprints()),
            ) as usize;
        }
    }

    fn lines(
        &self,
        area: Rect,
        statuses: &HashMap<String, ServiceStatus>,
        history: &StatusHistory,
        pending: bool,
        active: bool,
    ) -> Vec<Line<'static>> {
        if self.details {
            if let Some(source) = SOURCES.get(self.selected).copied() {
                detail_lines(area, source, statuses.get(source.id), history, pending)
            } else {
                fingerprint_details(area, &self.fingerprints, self.fingerprint_error.as_deref())
            }
        } else if self.has_fingerprints() {
            let compact = Rect { height: 7, ..area };
            let mut lines = list_lines(compact, statuses, pending, self.selected, active);
            lines.truncate(SOURCES.len());
            let report = self
                .fingerprints
                .iter()
                .filter_map(|s| s.latest.as_ref())
                .max_by_key(|r| fingerprint_display(Some(r)).priority);
            let has_warning = self.fingerprints.iter().any(|s| !s.warnings.is_empty());
            let TestDisplay {
                symbol,
                color,
                label,
                ..
            } = if self.fingerprint_error.is_some() {
                TestDisplay {
                    symbol: "○",
                    color: Color::DarkGray,
                    label: "记录读取失败",
                    priority: 2,
                }
            } else if has_warning {
                TestDisplay {
                    symbol: "▲",
                    color: Color::Yellow,
                    label: "部分记录异常",
                    priority: 3,
                }
            } else {
                fingerprint_display(report)
            };
            let prefix = if active && self.selected == SOURCES.len() {
                "> "
            } else {
                "  "
            };
            lines.push(Line::from(format!("{prefix}{symbol} GPT 订阅测试 · {label}")).fg(color));
            if area.height >= 8
                && let Some(at) = report.and_then(|r| DateTime::from_timestamp(r.started_at, 0))
            {
                lines.push(Line::from(format!("    最近测试 {}", time(at))).dim());
            }
            if self.fingerprints.iter().any(|s| s.update_notice.is_some()) {
                lines.push(Line::from("▲ lmfpd 有更新 · Enter 查看版本").fg(Color::Yellow));
            } else {
                lines.push(Line::from("Enter 查看事件与历史").dim());
            }
            lines.push(Line::from("厂商：官方状态页 · 订阅：本机测试").dim());
            lines
        } else {
            list_lines(area, statuses, pending, self.selected, active)
        }
    }

    pub fn render(
        &self,
        area: Rect,
        buf: &mut Buffer,
        statuses: &HashMap<String, ServiceStatus>,
        history: &StatusHistory,
        pending: bool,
        active: bool,
    ) {
        let lines = self.lines(area, statuses, history, pending, active);
        let max = lines
            .len()
            .saturating_sub(area.height as usize)
            .min(u16::MAX as usize) as u16;
        Paragraph::new(lines)
            .scroll((self.scroll.min(max), 0))
            .render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(index: i64) -> ServiceStatus {
        let time = DateTime::from_timestamp(index, 0).unwrap();
        ServiceStatus {
            provider: "openai".into(),
            display_name: "OpenAI".into(),
            page_url: service_status::OPENAI.page_url.into(),
            state: State::Operational,
            description: String::new(),
            components: Vec::new(),
            incidents: Vec::new(),
            fetched_at: Some(time),
            checked_at: time,
            stale: false,
            error: None,
        }
    }

    #[test]
    fn history_is_bounded_and_failed_or_incomplete_samples_never_look_healthy() {
        let mut history = StatusHistory::default();
        for index in 0..30 {
            history.record(&status(index));
        }
        let samples = &history.0["openai"];
        assert_eq!(samples.len(), 24);
        assert_eq!(samples[0].checked_at.timestamp(), 6);
        history.record(&status(29));
        assert_eq!(history.0["openai"].len(), 24);
        let mut failure = status(30);
        failure.fetched_at = None;
        failure.error = Some("offline".into());
        let merged = ServiceStatus::merge(Some(status(29)), failure);
        assert_eq!(merged.state, State::Operational);
        assert!(merged.stale);
        history.record(&merged);
        assert_eq!(history.0["openai"].back().unwrap().state, State::Unknown);
        let mut partial = status(31);
        partial.error = Some("incident details unavailable".into());
        history.record(&partial);
        assert_eq!(history.0["openai"].back().unwrap().state, State::Unknown);
        assert_eq!(indicator(State::Unknown), ("○", Color::DarkGray));
    }

    #[test]
    fn default_layout_exposes_every_official_source_on_narrow_and_wide_terminals() {
        for width in [28, 52, 80, 120] {
            let area = Rect::new(0, 0, width, 22);
            let [providers, connections, projects] = areas(area);
            assert!(!providers.intersects(connections));
            assert!(!projects.intersects(connections));
            assert!(!projects.intersects(providers));
            let mut buffer = Buffer::empty(area);
            let inner = board(connections, &mut buffer, "官方 API", false);
            Connections::default().render(
                inner,
                &mut buffer,
                &HashMap::new(),
                &StatusHistory::default(),
                true,
                false,
            );
            let text = buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            for source in ["OpenAI", "Claude", "Grok"] {
                assert!(text.contains(source), "missing {source} at {width}: {text}");
            }
        }
    }

    #[test]
    fn list_distinguishes_service_outages_from_failed_updates_and_details_preserve_context() {
        let mut outage = status(1);
        outage.provider = "claude".into();
        outage.state = State::PartialOutage;
        outage.incidents.push(service_status::Incident {
            name: "API requests failing".into(),
            status: "investigating".into(),
            url: "https://status.example/incident".into(),
        });
        let mut failed = status(2);
        failed.fetched_at = None;
        failed.error = Some("network unavailable".into());
        let stale = ServiceStatus::merge(Some(outage.clone()), failed);
        let statuses = HashMap::from([("claude".into(), stale)]);
        let area = Rect::new(0, 0, 40, 12);
        let lines = list_lines(area, &statuses, false, 1, true);
        let row = lines
            .iter()
            .find(|line| line.to_string().contains("Claude"))
            .unwrap();
        assert!(row.to_string().contains("○ Claude · 更新失败"));
        assert!(!row.to_string().contains("部分故障"));
        assert_eq!(row.spans[1].style.fg, Some(Color::DarkGray));
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("上次：部分故障"))
        );

        let mut history = StatusHistory::default();
        history.record(&outage);
        history.record(&statuses["claude"]);
        let details = detail_lines(
            area,
            service_status::CLAUDE,
            statuses.get("claude"),
            &history,
            false,
        )
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        assert!(details.contains("network unavailable"));
        assert!(details.contains("以下为上次报告"));
        assert!(details.contains("API requests failing"));
        assert!(details.contains("!○"));
        assert!(
            details.find("API requests failing").unwrap() < details.find("最近 24 次查询").unwrap()
        );
        assert!(!details.contains("状态源未报告未解决事件"));

        let statuses = HashMap::from([("claude".into(), outage)]);
        let lines = list_lines(area, &statuses, false, 1, true);
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("! Claude · 部分故障"))
        );
    }
}

#[cfg(test)]
mod fingerprint_tests {
    use super::*;
    use crate::codex_manager::fingerprint::{Report, ReportStatus, Snapshot};
    #[test]
    fn engine_update_is_visible_without_claiming_damaged_records() {
        let notice = "lmfpd 有更新：0.0.1 → 0.0.2";
        let mut c = Connections::default();
        c.set_fingerprints(Ok(vec![Snapshot {
            name: "Official".into(),
            update_notice: Some(notice.into()),
            ..Default::default()
        }]));
        let area = Rect::new(0, 0, 72, 7);
        let text = c
            .lines(
                area,
                &HashMap::new(),
                &StatusHistory::default(),
                false,
                true,
            )
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("lmfpd 有更新"));
        assert!(!text.contains("记录异常"));
        c.selected = SOURCES.len();
        c.open();
        let text = c
            .lines(
                area,
                &HashMap::new(),
                &StatusHistory::default(),
                false,
                true,
            )
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(notice));
    }
    #[test]
    fn local_test_row_details_and_history_do_not_claim_official_outage_or_identity() {
        let r = Report {
            model: "gpt-6.1-sol".into(),
            channel: "codex-subscription".into(),
            started_at: 1792308647,
            finished_at: Some(1792308651),
            status: ReportStatus::Failed,
            directory: Default::default(),
            accepted: 0,
            reference_contains_model: false,
            closest_candidate: None,
            message: "官方订阅额度已用尽，预计 17:15 恢复".into(),
            http_status: Some(429),
            quota_resets_at: Some(1792314948),
        };
        let mut c = Connections::default();
        let snapshot = Snapshot {
            update_notice: None,
            id: "official".into(),
            warnings: Vec::new(),
            name: "Official account".into(),
            enabled: true,
            latest: Some(r.clone()),
            history: vec![r],
        };
        assert!(c.set_fingerprints(Ok(vec![snapshot])));
        for width in [30, 72] {
            let area = Rect::new(0, 0, width, 12);
            let lines = c.lines(
                area,
                &HashMap::new(),
                &StatusHistory::default(),
                false,
                true,
            );
            let text = lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("GPT 订阅测试 · 额度耗尽"));
            assert!(text.contains("厂商：官方状态页 · 订阅：本机测试"));
            assert!(!text.contains("严重故障"));
            c.selected = SOURCES.len();
            c.open();
            let lines = c.lines(
                area,
                &HashMap::new(),
                &StatusHistory::default(),
                false,
                true,
            );
            let text = lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("");
            assert!(text.contains("最近 24 次测试"));
            assert!(text.contains("▲"));
            assert!(text.contains("17:15"));
            assert!(text.contains("不代表官方服务健康度或模型身份认证"));
            let mut buf = Buffer::empty(area);
            c.render(
                area,
                &mut buf,
                &HashMap::new(),
                &StatusHistory::default(),
                false,
                true,
            );
            c.back();
        }
        c.set_fingerprints(Err("本机测试记录读取失败".into()));
        let text = c
            .lines(
                Rect::new(0, 0, 72, 10),
                &HashMap::new(),
                &StatusHistory::default(),
                false,
                true,
            )
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("○ GPT 订阅测试 · 记录读取失败"));
        c.set_fingerprints(Ok(Vec::new()));
        assert_eq!(c.selected, SOURCES.len() - 1);
    }
}
