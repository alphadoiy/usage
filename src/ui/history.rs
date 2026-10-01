//! Contribution calendar and daily/monthly token breakdown.
use crate::history::{History, HistoryReport, SOURCES, Tokens, compact};
use chrono::{Datelike, Duration, Local, Months, NaiveDate};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

pub struct HistoryPanel {
    pub history: History,
    pub loading: bool,
    pub selected: NaiveDate,
    pub monthly: bool,
    pub allowed: Vec<String>,
    filter: Option<String>,
}
impl HistoryPanel {
    pub fn new(allowed: Vec<String>, year: i32, monthly: bool) -> Self {
        let today = Local::now().date_naive();
        let selected = if today.year() == year {
            today
        } else {
            NaiveDate::from_ymd_opt(year, 1, 1).unwrap_or(today)
        };
        Self {
            history: History::default(),
            loading: true,
            selected,
            monthly,
            allowed,
            filter: None,
        }
    }
    pub fn sources(&self) -> Vec<String> {
        self.filter
            .as_ref()
            .map_or_else(|| self.allowed.clone(), |s| vec![s.clone()])
    }
    pub fn key(&mut self, key: KeyCode) {
        if self.monthly
            && matches!(
                key,
                KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k')
            )
        {
            let next = if matches!(key, KeyCode::Up | KeyCode::Char('k')) {
                self.selected.checked_sub_months(Months::new(1))
            } else {
                self.selected.checked_add_months(Months::new(1))
            };
            if let Some(date) = next.filter(|date| *date <= Local::now().date_naive()) {
                self.selected = date;
            }
            return;
        }
        let shift = match key {
            KeyCode::Left => -7,
            KeyCode::Right => 7,
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::Down | KeyCode::Char('j') => 1,
            _ => 0,
        };
        if shift != 0 {
            if let Some(date) = self
                .selected
                .checked_add_signed(Duration::days(shift))
                .filter(|date| *date <= Local::now().date_naive())
            {
                self.selected = date;
            }
            return;
        }
        if matches!(key, KeyCode::PageUp | KeyCode::PageDown) {
            let dates: Vec<_> = self
                .history
                .days
                .keys()
                .copied()
                .filter(|date| {
                    date.year() == self.selected.year()
                        && self.history.day(*date, &self.sources()).total() > 0
                })
                .collect();
            let next = if key == KeyCode::PageUp {
                dates.into_iter().rev().find(|date| *date < self.selected)
            } else {
                dates
                    .into_iter()
                    .find(|date| *date > self.selected && *date <= Local::now().date_naive())
            };
            if let Some(date) = next {
                self.selected = date;
            }
            return;
        }
        match key {
            KeyCode::Char('m') => self.monthly = !self.monthly,
            KeyCode::Char('0') => self.filter = None,
            KeyCode::Char(ch @ '1'..='3') => {
                let source = SOURCES[(ch as u8 - b'1') as usize];
                if self.allowed.iter().any(|s| s == source) {
                    self.filter = Some(source.into());
                }
            }
            KeyCode::Tab => {
                self.filter = match self
                    .filter
                    .as_ref()
                    .and_then(|s| self.allowed.iter().position(|a| a == s))
                {
                    None => self.allowed.first().cloned(),
                    Some(i) => self.allowed.get(i + 1).cloned(),
                };
            }
            KeyCode::Char('[') | KeyCode::Char(']') => {
                let year = self.selected.year() + if key == KeyCode::Char('[') { -1 } else { 1 };
                if (1970..=Local::now().year()).contains(&year) {
                    self.selected =
                        NaiveDate::from_ymd_opt(year, self.selected.month(), self.selected.day())
                            .or_else(|| NaiveDate::from_ymd_opt(year, self.selected.month(), 28))
                            .unwrap();
                }
            }
            KeyCode::Char('t') => self.selected = Local::now().date_naive(),
            _ => {}
        }
    }
}

const MUTED: Color = Color::Rgb(130, 138, 153);
const ACCENT: Color = Color::Rgb(144, 202, 249);
// Foreground squares leave the terminal background visible between every cell.
const BLUES: [Color; 5] = [
    Color::Rgb(58, 65, 79),
    Color::Rgb(144, 202, 249),
    Color::Rgb(66, 165, 245),
    Color::Rgb(30, 136, 229),
    Color::Rgb(21, 101, 192),
];
const MAX_RECENT: usize = 7;

struct HeatScale(Vec<u64>);
impl HeatScale {
    fn new(report: &HistoryReport) -> Self {
        let mut values: Vec<_> = report
            .daily
            .values()
            .map(|t| t.total())
            .filter(|v| *v > 0)
            .collect();
        values.sort_unstable();
        values.dedup();
        Self(values)
    }
    fn color(&self, value: u64) -> Color {
        if value == 0 {
            return BLUES[0];
        }
        let level = if self.0.len() <= 1 {
            4
        } else {
            1 + self.0.partition_point(|n| *n < value) * 3 / (self.0.len() - 1)
        };
        BLUES[level.min(4)]
    }
}

pub fn render(area: Rect, buf: &mut Buffer, panel: &HistoryPanel) {
    // Keep content compact even in a large terminal; the extra space stays quiet.
    let pad = if area.width >= 70 { 3 } else { 1 };
    let content = Rect::new(
        area.x.saturating_add(pad),
        area.y,
        area.width.saturating_sub(pad * 2).min(118),
        area.height,
    );
    if content.is_empty() {
        return;
    }
    let sources = panel.sources();
    let report = panel.history.report(panel.selected.year(), &sources);
    let warning_height = u16::from(!panel.history.warnings.is_empty());
    let calendar_height = if panel.monthly && content.height < 28 {
        0
    } else if content.height >= 28 {
        11
    } else if content.height >= 15 {
        10
    } else {
        0
    };
    let [summary, calendar, _gap, bottom, warning] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(calendar_height),
        Constraint::Length(u16::from(calendar_height > 0 && content.height >= 28)),
        Constraint::Fill(1),
        Constraint::Length(warning_height),
    ])
    .areas(content);
    render_summary(summary, buf, panel, &report);
    if calendar_height > 0 {
        render_calendar(calendar, buf, panel, &report);
    }
    if bottom.width >= 98 && bottom.height >= 8 {
        let [detail, list] = Layout::horizontal([Constraint::Length(37), Constraint::Fill(1)])
            .spacing(4)
            .areas(bottom);
        render_detail(detail, buf, panel, &report, false);
        if panel.monthly {
            render_months(list, buf, panel, &report);
        } else {
            render_recent(list, buf, panel, &report);
        }
    } else {
        let detail_height = 3;
        let [detail, list] =
            Layout::vertical([Constraint::Length(detail_height), Constraint::Fill(1)])
                .spacing(1)
                .areas(bottom);
        render_detail(detail, buf, panel, &report, true);
        if panel.monthly {
            render_months(list, buf, panel, &report);
        } else {
            render_recent(list, buf, panel, &report);
        }
    }
    if let Some(message) = panel.history.warnings.first() {
        Paragraph::new(format!(
            "{message}{}",
            if panel.history.warnings.len() > 1 {
                format!(" · 另 {} 条", panel.history.warnings.len() - 1)
            } else {
                String::new()
            }
        ))
        .style(Style::new().fg(Color::Yellow))
        .render(warning, buf);
    }
}

fn render_summary(area: Rect, buf: &mut Buffer, panel: &HistoryPanel, report: &HistoryReport) {
    let mut tabs = vec![Span::styled(
        format!("{}  ", panel.selected.year()),
        Style::new().bold(),
    )];
    for (id, label, key) in [
        (None, "全部", "0"),
        (Some("codex"), "Codex", "1"),
        (Some("claude"), "Claude", "2"),
        (Some("grok"), "Grok", "3"),
    ] {
        if id.is_some_and(|id| !panel.allowed.iter().any(|s| s == id)) {
            continue;
        }
        let selected = panel.filter.as_deref() == id;
        tabs.push(Span::styled(
            format!(" {key} {label} "),
            if selected {
                Style::new().fg(ACCENT).bold().underlined()
            } else {
                Style::new().fg(MUTED)
            },
        ));
        tabs.push(Span::raw(" "));
    }
    let active = report.daily.values().filter(|t| t.total() > 0).count();
    let cache = if report.totals.input == 0 {
        "—".to_string()
    } else {
        format!(
            "{:.0}%",
            report.totals.cached as f64 / report.totals.input as f64 * 100.0
        )
    };
    let stats = if panel.loading {
        "正在读取本地用量日志…".to_string()
    } else {
        format!(
            "{} Tokens   ·   {} 活跃日   ·   缓存 {}",
            compact(report.totals.total()),
            active,
            cache
        )
    };
    Paragraph::new(vec![Line::from(tabs), Line::from(stats).fg(MUTED)]).render(area, buf);
}

struct CalendarWindow {
    start: NaiveDate,
    first: NaiveDate,
    last: NaiveDate,
    offset: usize,
    visible: usize,
}
fn calendar_window(panel: &HistoryPanel, width: u16) -> Option<CalendarWindow> {
    let year = panel.selected.year();
    let first = NaiveDate::from_ymd_opt(year, 1, 1)?;
    let last = NaiveDate::from_ymd_opt(year, 12, 31)?.min(Local::now().date_naive());
    if last < first {
        return None;
    }
    let start = first - Duration::days(i64::from(first.weekday().num_days_from_sunday()));
    let weeks = ((last - start).num_days() / 7 + 1) as usize;
    let visible = (usize::from(width.saturating_sub(4)) / 2).min(weeks);
    if visible == 0 {
        return None;
    }
    let selected_week = ((panel.selected.min(last) - start).num_days() / 7) as usize;
    let offset = selected_week
        .saturating_sub(visible / 2)
        .min(weeks - visible);
    Some(CalendarWindow {
        start,
        first,
        last,
        offset,
        visible,
    })
}
fn render_calendar(area: Rect, buf: &mut Buffer, panel: &HistoryPanel, report: &HistoryReport) {
    let Some(window) = calendar_window(panel, area.width) else {
        Paragraph::new("该年份没有可显示的日期")
            .style(Style::new().fg(MUTED))
            .render(area, buf);
        return;
    };
    let scale = HeatScale::new(report);
    let mut lines = vec![Line::from("每日 Tokens").bold()];
    let mut months = vec![Span::raw("    ")];
    let mut prior = None;
    for week in window.offset..window.offset + window.visible {
        let date = (window.start + Duration::days((week * 7 + 6) as i64))
            .min(window.last)
            .max(window.first);
        months.push(Span::styled(
            if prior == Some(date.month()) {
                "  ".into()
            } else {
                format!("{:<2}", date.month())
            },
            Style::new().fg(MUTED),
        ));
        prior = Some(date.month());
    }
    lines.push(Line::from(months));
    for weekday in 0..7 {
        let mut row = vec![Span::styled(
            match weekday {
                1 => "Mon ",
                3 => "Wed ",
                5 => "Fri ",
                _ => "    ",
            },
            Style::new().fg(MUTED),
        )];
        for week in window.offset..window.offset + window.visible {
            let date = window.start + Duration::days((week * 7 + weekday) as i64);
            if date < window.first || date > window.last {
                row.push(Span::raw("  "));
                continue;
            }
            let value = report.daily.get(&date.to_string()).map_or(0, |t| t.total());
            let style = Style::new().fg(scale.color(value));
            row.push(Span::styled(
                if date == panel.selected { "▣" } else { "■" },
                if date == panel.selected {
                    style.bold().underlined()
                } else {
                    style
                },
            ));
            // This uncolored cell is essential: don't join neighboring days with background fills.
            row.push(Span::raw(" "));
        }
        lines.push(Line::from(row));
    }
    let mut legend = vec![Span::styled("    少 ", Style::new().fg(MUTED))];
    for color in BLUES {
        legend.push(Span::styled("■", Style::new().fg(color)));
        legend.push(Span::raw(" "));
    }
    legend.push(Span::styled("多", Style::new().fg(MUTED)));
    if window.visible < ((window.last - window.start).num_days() / 7 + 1) as usize {
        let from = (window.start + Duration::days((window.offset * 7) as i64)).max(window.first);
        let to = (window.start + Duration::days(((window.offset + window.visible) * 7 - 1) as i64))
            .min(window.last);
        legend.push(Span::styled(
            format!("    {} — {}", from.format("%m-%d"), to.format("%m-%d")),
            Style::new().fg(MUTED),
        ));
    }
    lines.push(Line::from(legend));
    Paragraph::new(lines).render(area, buf);
}

fn selected_tokens(panel: &HistoryPanel, report: &HistoryReport) -> Tokens {
    if panel.monthly {
        report
            .monthly
            .get(&panel.selected.format("%Y-%m").to_string())
            .copied()
            .unwrap_or_default()
    } else {
        panel.history.day(panel.selected, &panel.sources())
    }
}
fn source_color(source: &str) -> Color {
    let (r, g, b) = crate::theme::accent(source);
    Color::Rgb(r, g, b)
}
fn render_detail(
    area: Rect,
    buf: &mut Buffer,
    panel: &HistoryPanel,
    report: &HistoryReport,
    compact_layout: bool,
) {
    let tokens = selected_tokens(panel, report);
    let date = if panel.monthly {
        panel.selected.format("%Y-%m").to_string()
    } else {
        panel.selected.to_string()
    };
    let mut sources = Vec::new();
    for source in panel.sources() {
        let t = if panel.monthly {
            panel
                .history
                .report(panel.selected.year(), std::slice::from_ref(&source))
                .monthly[&date]
        } else {
            panel
                .history
                .day(panel.selected, std::slice::from_ref(&source))
        };
        sources.push(Span::styled(
            format!("{source} {}  ", compact(t.total())),
            Style::new().fg(source_color(&source)),
        ));
    }
    let lines = if compact_layout {
        vec![
            Line::from(vec![
                Span::styled(format!("{date}  "), Style::new().fg(ACCENT).bold()),
                Span::styled(
                    format!("{} Tokens", compact(tokens.total())),
                    Style::new().bold(),
                ),
            ]),
            Line::from(format!(
                "输入 {}  ·  输出 {}  ·  缓存 {}",
                compact(tokens.input),
                compact(tokens.output),
                compact(tokens.cached)
            ))
            .fg(MUTED),
            Line::from(sources),
        ]
    } else {
        let mut lines = vec![
            Line::from(date).fg(ACCENT).bold(),
            Line::from(format!("{} Tokens", compact(tokens.total()))).bold(),
            Line::from(""),
            Line::from(format!(
                "输入 {:>10}   输出 {}",
                compact(tokens.input),
                compact(tokens.output)
            ))
            .fg(MUTED),
            Line::from(format!(
                "缓存 {:>10}   记录 {}",
                compact(tokens.cached),
                compact(tokens.calls)
            ))
            .fg(MUTED),
        ];
        lines.extend(sources.into_iter().map(Line::from));
        lines
    };
    Paragraph::new(lines).render(area, buf);
}

fn recent_rows(
    panel: &HistoryPanel,
    report: &HistoryReport,
    count: usize,
) -> Vec<(NaiveDate, Tokens)> {
    let today = Local::now().date_naive();
    let mut dates: Vec<_> = report
        .daily
        .iter()
        .filter_map(|(date, t)| {
            let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
            (date <= today && (t.total() > 0 || date == panel.selected)).then_some((date, *t))
        })
        .collect();
    let index = dates
        .iter()
        .position(|(date, _)| *date == panel.selected)
        .unwrap_or(dates.len().saturating_sub(1));
    let count = count.min(MAX_RECENT);
    let start = index
        .saturating_sub(count / 2)
        .min(dates.len().saturating_sub(count));
    dates.drain(..start);
    dates.truncate(count);
    dates.reverse();
    dates
}
fn right_label(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    format!("{}{}", " ".repeat(width.saturating_sub(text.width())), text)
}

fn render_recent(area: Rect, buf: &mut Buffer, panel: &HistoryPanel, report: &HistoryReport) {
    if area.height < 3 {
        return;
    }
    let count = usize::from(area.height.saturating_sub(2)).min(MAX_RECENT);
    let mut lines = vec![Line::from("近期用量").bold()];
    let full = area.width >= 61;
    let medium = area.width >= 46;
    let mut header = format!(" 日期       {:>8}", "Tokens");
    if medium {
        header.push_str(&format!(" {:>9} {:>9} {:>9}", "Input", "Output", "Cached"));
    }
    if full || !medium {
        header.push_str(&format!(" {}", right_label("记录", 7)));
    }
    lines.push(Line::from(header).fg(MUTED));
    let rows = recent_rows(panel, report, count);
    if rows.is_empty() {
        lines.push(Line::from("没有用量记录").fg(MUTED));
    }
    for (date, t) in rows {
        let mut text = format!(
            "{}{:<10} {:>8}",
            if date == panel.selected { "›" } else { " " },
            date.format("%m-%d"),
            compact(t.total())
        );
        if medium {
            text.push_str(&format!(
                " {:>9} {:>9} {:>9}",
                compact(t.input),
                compact(t.output),
                compact(t.cached)
            ));
        }
        if full || !medium {
            text.push_str(&format!(" {:>7}", compact(t.calls)));
        }
        lines.push(Line::from(text).style(if date == panel.selected {
            Style::new().fg(ACCENT).bold()
        } else {
            Style::new()
        }));
    }
    Paragraph::new(lines).render(area, buf);
}
fn render_months(area: Rect, buf: &mut Buffer, panel: &HistoryPanel, report: &HistoryReport) {
    if area.height < 3 {
        return;
    }
    let columns = if area.width >= 72 { 3 } else { 2 };
    let rows = 12 / columns;
    let cell_width = usize::from(area.width) / columns;
    let bar_width = cell_width.saturating_sub(13);
    let max = report
        .monthly
        .values()
        .map(|t| t.total())
        .max()
        .unwrap_or(0)
        .max(1);
    let sources = panel.sources();
    let reports: Vec<_> = sources
        .iter()
        .map(|s| {
            panel
                .history
                .report(panel.selected.year(), std::slice::from_ref(s))
        })
        .collect();
    let mut lines = vec![Line::from("月度趋势").bold()];
    for row in 0..rows {
        let mut spans = Vec::new();
        for col in 0..columns {
            let month = row + col * rows + 1;
            let key = format!("{}-{month:02}", panel.selected.year());
            let t = report.monthly[&key];
            spans.push(Span::styled(
                format!(
                    "{}{month:02} ",
                    if month as u32 == panel.selected.month() {
                        "›"
                    } else {
                        " "
                    }
                ),
                Style::new().fg(if month as u32 == panel.selected.month() {
                    ACCENT
                } else {
                    MUTED
                }),
            ));
            let filled = ((t.total() as f64 / max as f64 * bar_width as f64).round() as usize)
                .min(bar_width)
                .max(usize::from(t.total() > 0 && bar_width > 0));
            let mut used = 0;
            let mut cumulative = 0u64;
            for (source, report) in sources.iter().zip(&reports) {
                cumulative = cumulative.saturating_add(report.monthly[&key].total());
                let end = if t.total() == 0 {
                    0
                } else {
                    ((cumulative as f64 / t.total() as f64 * filled as f64).round() as usize)
                        .min(filled)
                };
                spans.push(Span::styled(
                    "━".repeat(end.saturating_sub(used)),
                    Style::new().fg(source_color(source)),
                ));
                used = end;
            }
            spans.push(Span::styled(
                "─".repeat(bar_width.saturating_sub(used)),
                Style::new().fg(BLUES[0]),
            ));
            spans.push(Span::raw(format!(" {:>7} ", compact(t.total()))));
        }
        lines.push(Line::from(spans));
    }
    Paragraph::new(lines).render(area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn panel(monthly: bool) -> HistoryPanel {
        let mut p = HistoryPanel::new(
            SOURCES.iter().map(|s| s.to_string()).collect(),
            2024,
            monthly,
        );
        p.loading = false;
        p.selected = NaiveDate::from_ymd_opt(2024, 9, 20).unwrap();
        for day in 1..=20 {
            let date = NaiveDate::from_ymd_opt(2024, 9, day).unwrap();
            for source in &p.allowed {
                p.history.days.entry(date).or_default().insert(
                    source.clone(),
                    Tokens {
                        input: 900 * day as u64,
                        output: 100 * day as u64,
                        cached: 300 * day as u64,
                        calls: day as u64,
                    },
                );
            }
        }
        p
    }
    fn text(buffer: &Buffer) -> String {
        buffer
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .replace(' ', "")
    }
    #[test]
    fn recent_list_is_bounded_and_excludes_unselected_zero_days() {
        let p = panel(false);
        let report = p.history.report(2024, &p.sources());
        let rows = recent_rows(&p, &report, 100);
        assert_eq!(rows.len(), MAX_RECENT);
        assert_eq!(rows[0].0, p.selected);
        assert!(rows.iter().all(|(_, t)| t.total() > 0));
        let mut p = p;
        p.selected = NaiveDate::from_ymd_opt(2024, 10, 1).unwrap();
        let rows = recent_rows(&p, &report, 100);
        assert_eq!(rows.len(), MAX_RECENT);
        assert_eq!(rows[0].0, p.selected);
        assert_eq!(rows.iter().filter(|(_, t)| t.total() == 0).count(), 1);
    }
    #[test]
    fn heatmap_uses_separate_foreground_squares_and_sparse_labels() {
        let p = panel(false);
        let report = p.history.report(2024, &p.sources());
        let rect = Rect::new(0, 0, 112, 11);
        let mut buf = Buffer::empty(rect);
        render_calendar(rect, &mut buf, &p, &report);
        assert!(text(&buf).contains("Mon"));
        assert!(!text(&buf).contains("Sun"));
        assert!(buf.content().iter().any(|c| c.symbol() == "▣"));
        for y in 2..9 {
            for x in (4..110).step_by(2) {
                let cell = &buf[(x, y)];
                if matches!(cell.symbol(), "■" | "▣") {
                    assert_eq!(cell.bg, Color::Reset);
                    assert_eq!(buf[(x + 1, y)].symbol(), " ");
                    assert_eq!(buf[(x + 1, y)].bg, Color::Reset);
                }
            }
        }
        let scale = HeatScale::new(&report);
        assert_eq!(scale.color(0), BLUES[0]);
        assert_eq!(scale.color(3000), BLUES[1]);
        assert_eq!(scale.color(60000), BLUES[4]);
    }
    #[test]
    fn calendar_has_no_future_cells_and_navigation_stops_at_today() {
        let today = Local::now().date_naive();
        let mut p = HistoryPanel::new(vec!["codex".into()], today.year(), false);
        let window = calendar_window(&p, 112).unwrap();
        assert_eq!(window.last, today);
        p.key(KeyCode::Down);
        assert_eq!(p.selected, today);
        p.key(KeyCode::Char('m'));
        p.key(KeyCode::Down);
        assert_eq!(p.selected, today);
    }
    #[test]
    fn monthly_layout_shows_twelve_months_without_duplicate_table() {
        let p = panel(true);
        for (width, height) in [(120, 35), (80, 22), (40, 18)] {
            let rect = Rect::new(0, 0, width, height);
            let mut buf = Buffer::empty(rect);
            render(rect, &mut buf, &p);
            let content = text(&buf);
            assert!(content.contains("月度趋势"), "{width}x{height}: {content}");
            assert!(content.contains("›09"));
            assert!(content.contains("12"));
            assert!(!content.contains("近期用量"));
            if width >= 80 {
                for source in &p.allowed {
                    assert!(
                        buf.content()
                            .iter()
                            .any(|c| c.symbol() == "━" && c.fg == source_color(source))
                    );
                }
            }
        }
    }
    #[test]
    fn filters_leap_day_activity_jumps_and_small_terminals() {
        let mut p = panel(false);
        p.selected = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        p.key(KeyCode::Char(']'));
        assert_eq!(p.selected.to_string(), "2025-02-28");
        p.key(KeyCode::Char('2'));
        assert_eq!(p.sources(), ["claude"]);
        p.key(KeyCode::Tab);
        assert_eq!(p.sources(), ["grok"]);
        p.key(KeyCode::Tab);
        assert_eq!(p.sources().len(), 3);
        p.selected = NaiveDate::from_ymd_opt(2024, 9, 20).unwrap();
        p.key(KeyCode::PageUp);
        assert_eq!(p.selected.day(), 19);
        p.key(KeyCode::PageDown);
        assert_eq!(p.selected.day(), 20);
        for monthly in [false, true] {
            p.monthly = monthly;
            for (width, height) in [(120, 35), (80, 24), (40, 20), (1, 1), (0, 0)] {
                let rect = Rect::new(0, 0, width, height);
                render(rect, &mut Buffer::empty(rect), &p);
            }
        }
    }
}
