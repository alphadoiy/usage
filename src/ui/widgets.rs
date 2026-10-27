//! 用量卡片。圆环用 Canvas，进度条用 Braille 点阵。

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Circle, Line as CanvasLine};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::labels::fmt_percent;
use crate::service_status::{ServiceStatus, Source, State};
use crate::theme;
use crate::view::{BadgeKind, BarSpec, CardView, RingSpec, ViewKind};

const CARD_BG: Color = Color::Rgb(12, 12, 13);
const BORDER: Color = Color::Rgb(95, 95, 99);
const TRACK: Color = Color::Rgb(85, 85, 90);
const TEXT: Color = Color::Rgb(242, 242, 247);
const DIM: Color = Color::Rgb(142, 142, 147);
const WARNING: Color = Color::Rgb(255, 159, 10);
const ERROR: Color = Color::Rgb(255, 69, 58);

/// A wide glyph drawn underneath a popup can straddle its edge. Clear its outside
/// half too, otherwise Terminal::diff skips the popup border as a continuation cell.
pub(crate) fn clear_popup_edges(area: Rect, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    for y in area.y..area.bottom() {
        if area.x > buf.area.x && buf[(area.x - 1, y)].symbol().width() > 1 {
            buf[(area.x - 1, y)].set_symbol(" ");
        }
        if area.right() < buf.area.right() && buf[(area.right() - 1, y)].symbol().width() > 1 {
            buf[(area.right(), y)].set_symbol(" ");
        }
    }
}

/// Fit whole hint groups. Keep help and back reachable without clipping a key label in half.
pub(crate) fn fit_hints(text: &str, width: u16) -> String {
    let mut groups: Vec<_> = text
        .split(" · ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let back = groups.last().copied().filter(|s| s.starts_with("Esc"));
    if back.is_some() {
        groups.pop();
    }
    let mut out = String::new();
    for group in groups {
        let candidate = if out.is_empty() {
            group.to_string()
        } else {
            format!("{out} · {group}")
        };
        let reserved = back.map_or(0, |s| s.width() + 3);
        if candidate.width() + reserved <= usize::from(width) {
            out = candidate;
        }
    }
    if let Some(back) = back {
        let candidate = if out.is_empty() {
            back.to_string()
        } else {
            format!("{out} · {back}")
        };
        if candidate.width() <= usize::from(width) {
            out = candidate;
        }
    }
    out
}

pub fn accent(provider_id: &str) -> Color {
    let (red, green, blue) = theme::accent(provider_id);
    Color::Rgb(red, green, blue)
}

pub fn preferred_height(view: &CardView, width: u16) -> u16 {
    let notes = view.lines.len() as u16;
    let inner = match view.kind {
        ViewKind::Ring if !view.rings.is_empty() => 10,
        ViewKind::Bar | ViewKind::Statusline if !view.bars.is_empty() => {
            let rows_per_tier = if width.saturating_sub(4) < 34 { 3 } else { 2 };
            (view.bars.len() as u16)
                .saturating_mul(rows_per_tier)
                .saturating_sub(1)
        }
        _ => 1,
    };
    2 + inner + notes + reset_note_gap(view) + u16::from(view.account_email.is_some())
}

fn reset_note_gap(view: &CardView) -> u16 {
    u16::from(
        (!view.bars.is_empty() || !view.rings.is_empty())
            && view.lines.iter().any(|line| line.starts_with("主动重置 ")),
    )
}

pub fn render_card(area: Rect, buf: &mut Buffer, provider_id: &str, title: &str, view: &CardView) {
    render_card_with_status(area, buf, provider_id, title, view, None, None);
}

pub fn render_card_with_status(
    area: Rect,
    buf: &mut Buffer,
    provider_id: &str,
    title: &str,
    view: &CardView,
    source: Option<Source>,
    status: Option<&ServiceStatus>,
) {
    let color = accent(provider_id);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .style(Style::new().bg(CARD_BG))
        .border_style(BORDER)
        .title_top(Line::from(format!(" {title} ")).style(color).bold())
        .title_top(
            Line::from(format!(" {} ", view.badge))
                .right_aligned()
                .style(badge_color(provider_id, view.badge_kind)),
        );
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let body = render_service_footer(inset_horizontal(inner), buf, source, status);
    render_body(body, buf, provider_id, view);
}

pub fn render_loading(area: Rect, buf: &mut Buffer, provider_id: &str, title: &str) {
    render_loading_with_status(area, buf, provider_id, title, None, None);
}

pub fn render_loading_with_status(
    area: Rect,
    buf: &mut Buffer,
    provider_id: &str,
    title: &str,
    source: Option<Source>,
    status: Option<&ServiceStatus>,
) {
    let color = accent(provider_id);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .style(Style::new().bg(CARD_BG))
        .border_style(BORDER)
        .title_top(Line::from(format!(" {title} ")).style(color).bold());
    let inner = block.inner(area);
    block.render(area, buf);
    let body = render_service_footer(inset_horizontal(inner), buf, source, status);
    Paragraph::new("正在查询官方用量…")
        .style(Style::new().fg(DIM))
        .render(body, buf);
}

fn render_service_footer(
    area: Rect,
    buf: &mut Buffer,
    source: Option<Source>,
    status: Option<&ServiceStatus>,
) -> Rect {
    let Some(source) = source else {
        return area;
    };
    let height = area.height.min(3);
    let body = Rect {
        height: area.height.saturating_sub(height),
        ..area
    };
    let footer = Rect {
        y: area.y + body.height,
        height,
        ..area
    };
    let (label, detail, color) = if let Some(status) = status {
        let label = if status.error.is_some() {
            if status.stale
                && let Some(time) = status.fetched_at
            {
                format!(
                    "● 上次{} {} · 更新失败",
                    status.label(),
                    time.with_timezone(&chrono::Local).format("%H:%M")
                )
            } else if status.fetched_at.is_some() {
                format!("● 官方{} · 详情失败", status.label())
            } else {
                "○ 官方状态 查询失败".into()
            }
        } else {
            format!(
                "● 官方{} {}",
                status.label(),
                status
                    .fetched_at
                    .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
                    .unwrap_or_default()
            )
        };
        let color = if status.error.is_some() {
            WARNING
        } else {
            match status.state {
                State::Operational => Color::Rgb(48, 209, 88),
                State::Maintenance | State::Degraded => WARNING,
                State::PartialOutage | State::MajorOutage => ERROR,
                State::Unknown => DIM,
            }
        };
        (label, status.summary(), color)
    } else {
        (
            "○ 官方状态 查询中…".into(),
            source
                .page_url
                .trim_start_matches("https://")
                .trim_end_matches('/')
                .into(),
            DIM,
        )
    };
    let skip = u16::from(height >= 3);
    Paragraph::new(vec![
        Line::from(label).style(color),
        Line::from(detail).style(DIM),
    ])
    .render(
        Rect {
            y: footer.y + skip,
            height: footer.height.saturating_sub(skip),
            ..footer
        },
        buf,
    );
    body
}

fn inset_horizontal(area: Rect) -> Rect {
    if area.width >= 4 {
        Rect {
            x: area.x + 1,
            width: area.width - 2,
            ..area
        }
    } else {
        area
    }
}

fn render_body(mut area: Rect, buf: &mut Buffer, provider_id: &str, view: &CardView) {
    if let Some(email) = &view.account_email
        && area.height > 0
    {
        Paragraph::new(format!("邮箱  {email}"))
            .style(Style::new().fg(DIM))
            .render(Rect { height: 1, ..area }, buf);
        area.y += 1;
        area.height -= 1;
    }
    let note_rows = view.lines.len() as u16;
    let note_height = note_rows.min(area.height);
    if view.bars.is_empty() && view.rings.is_empty() {
        let notes_area = Rect {
            y: area.y + area.height.saturating_sub(note_height) / 2,
            height: note_height,
            ..area
        };
        render_notes(notes_area, buf, provider_id, &view.lines);
        return;
    }
    let gap = reset_note_gap(view).min(area.height.saturating_sub(note_height));
    let meter_area = Rect {
        height: area.height.saturating_sub(note_height + gap),
        ..area
    };
    let notes_area = Rect {
        y: area.y + meter_area.height + gap,
        height: note_height,
        ..area
    };
    render_meters(meter_area, buf, provider_id, view);
    render_notes(notes_area, buf, provider_id, &view.lines);
}

fn render_meters(area: Rect, buf: &mut Buffer, provider_id: &str, view: &CardView) {
    if area.height == 0 {
        return;
    }
    match view.kind {
        ViewKind::Ring => render_rings(area, buf, provider_id, &view.rings),
        ViewKind::Bar => render_rows(area, buf, provider_id, &view.bars, MeterKind::Braille),
        ViewKind::Statusline => render_rows(area, buf, provider_id, &view.bars, MeterKind::Dots),
    }
}

fn render_rings(area: Rect, buf: &mut Buffer, provider_id: &str, rings: &[RingSpec]) {
    if rings.is_empty() || area.width == 0 {
        return;
    }
    let constraints = vec![Constraint::Fill(1); rings.len()];
    let columns = Layout::horizontal(constraints).split(area);
    for (spec, column) in rings.iter().zip(columns.iter()) {
        RingGauge {
            percent: spec.percent,
            label: spec.label.clone(),
            color: meter_color(provider_id, spec.percent),
        }
        .render(*column, buf);
    }
}

#[derive(Clone, Copy)]
enum MeterKind {
    Braille,
    Dots,
}

#[derive(Clone, Copy)]
struct MeterColumns {
    label: u16,
    detail: u16,
}

fn render_rows(area: Rect, buf: &mut Buffer, provider_id: &str, bars: &[BarSpec], kind: MeterKind) {
    if bars.is_empty() || area.height == 0 {
        return;
    }
    let stacked = area.width < 34;
    let stride = if stacked { 3 } else { 2 };
    let label_width = bars
        .iter()
        .map(|bar| UnicodeWidthStr::width(bar.label.as_str()) as u16)
        .max()
        .unwrap_or(0)
        .clamp(6, if area.width >= 50 { 10 } else { 8 });
    let detail_width = bars
        .iter()
        .filter(|bar| !bar.detail.is_empty())
        .map(|bar| UnicodeWidthStr::width(format!("⟳ {}", bar.detail).as_str()) as u16)
        .max()
        .unwrap_or(0)
        .min(if area.width >= 50 { 12 } else { 9 });
    let columns = MeterColumns {
        label: label_width,
        detail: detail_width,
    };
    for (index, spec) in bars.iter().enumerate() {
        let y = area.y.saturating_add((index as u16).saturating_mul(stride));
        if y >= area.bottom() {
            break;
        }
        let row = Rect {
            y,
            height: area
                .bottom()
                .saturating_sub(y)
                .min(if stacked { 2 } else { 1 }),
            ..area
        };
        if stacked {
            render_stacked_meter(
                row,
                buf,
                spec,
                accent(provider_id),
                meter_color(provider_id, spec.percent),
                kind,
            );
        } else {
            render_meter_row(
                row,
                buf,
                spec,
                accent(provider_id),
                meter_color(provider_id, spec.percent),
                kind,
                columns,
            );
        }
    }
}

fn render_stacked_meter(
    area: Rect,
    buf: &mut Buffer,
    spec: &BarSpec,
    label_color: Color,
    color: Color,
    kind: MeterKind,
) {
    let header = Rect { height: 1, ..area };
    let [label, _gap1, percent, _gap2, detail] = Layout::horizontal([
        Constraint::Length(6),
        Constraint::Length(1),
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(header);
    Paragraph::new(spec.label.as_str())
        .style(Style::new().fg(label_color).bold())
        .render(label, buf);
    Paragraph::new(fmt_percent(spec.percent))
        .style(Style::new().fg(color).bold())
        .render(percent, buf);
    if !spec.detail.is_empty() {
        Paragraph::new(format!("⟳ {}", spec.detail))
            .style(Style::new().fg(DIM))
            .render(detail, buf);
    }
    if area.height > 1 {
        let meter = Rect {
            y: area.y + 1,
            height: 1,
            width: if matches!(kind, MeterKind::Dots) {
                area.width.min(20)
            } else {
                area.width
            },
            ..area
        };
        match kind {
            MeterKind::Braille => BrailleBar {
                percent: spec.percent,
                color,
            }
            .render(meter, buf),
            MeterKind::Dots => DotBar {
                percent: spec.percent,
                color,
            }
            .render(meter, buf),
        }
    }
}

fn render_meter_row(
    area: Rect,
    buf: &mut Buffer,
    spec: &BarSpec,
    label_color: Color,
    color: Color,
    kind: MeterKind,
    columns: MeterColumns,
) {
    // Give wide glyphs and the reset icon their own cells. The dots view uses
    // one empty cell between circles because some fonts draw ● past its cell.
    let (label_width, meter_width, gap, detail_width) = match (kind, area.width) {
        (MeterKind::Braille, 50..) => (
            columns.label,
            Constraint::Fill(1),
            3,
            Constraint::Length(columns.detail),
        ),
        (MeterKind::Braille, 34..) => (
            columns.label,
            Constraint::Fill(1),
            2,
            Constraint::Length(columns.detail),
        ),
        (MeterKind::Braille, _) => (6, Constraint::Length(4), 1, Constraint::Fill(1)),
        (MeterKind::Dots, 50..) => (10, Constraint::Length(16), 2, Constraint::Fill(1)),
        (MeterKind::Dots, 34..) => (8, Constraint::Length(12), 1, Constraint::Fill(1)),
        (MeterKind::Dots, _) => (6, Constraint::Length(8), 1, Constraint::Fill(1)),
    };
    let [label, _gap1, meter, _gap2, percent, _gap3, detail] = Layout::horizontal([
        Constraint::Length(label_width),
        Constraint::Length(gap),
        meter_width,
        Constraint::Length(gap),
        Constraint::Length(5),
        Constraint::Length(gap),
        detail_width,
    ])
    .areas(area);
    Paragraph::new(spec.label.as_str())
        .style(Style::new().fg(label_color).bold())
        .render(label, buf);
    match kind {
        MeterKind::Braille => BrailleBar {
            percent: spec.percent,
            color,
        }
        .render(meter, buf),
        MeterKind::Dots => DotBar {
            percent: spec.percent,
            color,
        }
        .render(meter, buf),
    }
    Paragraph::new(fmt_percent(spec.percent))
        .style(Style::new().fg(color).bold())
        .render(percent, buf);
    if !spec.detail.is_empty() {
        Paragraph::new(format!("⟳ {}", spec.detail))
            .style(Style::new().fg(DIM))
            .render(detail, buf);
    }
}

struct DotBar {
    percent: f64,
    color: Color,
}

fn dot_cells(percent: f64, color: Color, dots: u16) -> impl Iterator<Item = (&'static str, Style)> {
    let ratio = if percent.is_finite() {
        (percent / 100.0).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = (ratio * f64::from(dots)).round() as u16;
    (0..dots).map(move |index| {
        let active = index < filled;
        (
            if active { "●" } else { "○" },
            Style::new().fg(if active { color } else { TRACK }),
        )
    })
}

/// Inline variant of the existing dot meter for scrollable provider information.
pub(crate) fn quota_dots(provider_id: &str, used_percent: f64) -> Vec<Span<'static>> {
    dot_cells(used_percent, meter_color(provider_id, used_percent), 8)
        .flat_map(|(symbol, style)| [Span::styled(symbol, style), Span::raw(" ")])
        .collect()
}

impl Widget for DotBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        for (index, (symbol, style)) in
            dot_cells(self.percent, self.color, area.width / 2).enumerate()
        {
            buf[(area.x + index as u16 * 2, area.y)]
                .set_symbol(symbol)
                .set_style(style);
        }
    }
}

/// Filled and unfilled cells share the same four-row Braille glyph so their
/// top and bottom edges line up in a compact, one-row meter.
struct BrailleBar {
    percent: f64,
    color: Color,
}

impl Widget for BrailleBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let ratio = if self.percent.is_finite() {
            (self.percent / 100.0).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let filled = (ratio * f64::from(area.width)).round() as u16;
        for offset in 0..area.width {
            let style = Style::new()
                .fg(if offset < filled { self.color } else { TRACK })
                .bg(CARD_BG);
            buf[(area.x + offset, area.y)]
                .set_symbol("⣿")
                .set_style(style);
        }
    }
}

fn render_notes(area: Rect, buf: &mut Buffer, provider_id: &str, lines: &[String]) {
    if area.height == 0 {
        return;
    }
    let rows = Layout::vertical(vec![Constraint::Length(1); lines.len()]).split(area);
    for (line, row) in lines.iter().zip(rows.iter()) {
        Paragraph::new(line.as_str())
            .style(Style::new().fg(note_color(provider_id, line)))
            .render(*row, buf);
    }
}

struct RingGauge {
    percent: f64,
    label: String,
    color: Color,
}

impl Widget for RingGauge {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 6 || area.height < 4 {
            return;
        }
        let [canvas_area, caption_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
        let color = self.color;
        let ratio = (self.percent / 100.0).clamp(0.0, 1.0);
        // 终端格子大约宽是高的一半，把 x 轴拉长，圆才不会被压扁。
        let aspect = f64::from(canvas_area.width) / (f64::from(canvas_area.height) * 2.0);
        let aspect = aspect.max(1.0);
        Canvas::default()
            .x_bounds([-aspect, aspect])
            .y_bounds([-1.0, 1.0])
            .paint(move |ctx| {
                for radius in [0.72, 0.82, 0.92] {
                    ctx.draw(&Circle::new(0.0, 0.0, radius, TRACK));
                    draw_arc(ctx, ratio, radius, color);
                }
            })
            .render(canvas_area, buf);
        Paragraph::new(format!("{}  {}", fmt_percent(self.percent), self.label))
            .centered()
            .style(Style::new().fg(color).bold())
            .render(caption_area, buf);
    }
}

fn draw_arc(
    ctx: &mut ratatui::widgets::canvas::Context<'_>,
    ratio: f64,
    radius: f64,
    color: Color,
) {
    if ratio <= 0.0 {
        return;
    }
    let steps = (120.0 * ratio).ceil() as i32;
    let steps = steps.max(1);
    let mut previous: Option<(f64, f64)> = None;
    for step in 0..=steps {
        let t = ratio * f64::from(step) / f64::from(steps);
        // 12 点方向顺时针。Canvas 的 y 轴向上。
        let theta = std::f64::consts::FRAC_PI_2 - t * std::f64::consts::TAU;
        let point = (radius * theta.cos(), radius * theta.sin());
        if let Some(start) = previous {
            ctx.draw(&CanvasLine::new(start.0, start.1, point.0, point.1, color));
        }
        previous = Some(point);
    }
}

fn meter_color(provider_id: &str, percent: f64) -> Color {
    let (red, green, blue) = theme::meter(provider_id, percent);
    Color::Rgb(red, green, blue)
}

fn badge_color(provider_id: &str, kind: BadgeKind) -> Color {
    if provider_id == "grok" {
        return match kind {
            BadgeKind::Ok | BadgeKind::Muted => DIM,
            BadgeKind::Warn | BadgeKind::Error => TEXT,
        };
    }
    match kind {
        BadgeKind::Ok | BadgeKind::Muted => DIM,
        BadgeKind::Warn => WARNING,
        BadgeKind::Error => ERROR,
    }
}

fn note_color(provider_id: &str, line: &str) -> Color {
    if line.starts_with("主动重置 ") {
        return if line.contains("重置 0 次") {
            DIM
        } else {
            accent(provider_id)
        };
    }
    if provider_id == "grok" {
        return if line.starts_with("超额") || line.starts_with("没有返回") {
            DIM
        } else {
            TEXT
        };
    }
    if line.starts_with("网络波动") {
        WARNING
    } else if line.starts_with("超额") || line.starts_with("没有返回") {
        DIM
    } else {
        ERROR
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::*;
    use crate::model::{QuotaTier, snapshot_ok};
    use crate::view::{BadgeKind, BarSpec, CardView, ViewKind};

    #[test]
    fn popup_clears_wide_glyphs_straddling_its_edges() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 3));
        buffer.set_string(1, 1, "界", Style::default());
        buffer.set_string(9, 1, "界", Style::default());
        clear_popup_edges(Rect::new(2, 0, 8, 3), &mut buffer);
        assert_eq!(buffer[(1, 1)].symbol(), " ");
        assert_eq!(buffer[(10, 1)].symbol(), " ");
    }

    #[test]
    fn hint_groups_fit_without_partial_keys_and_keep_back_available() {
        let text = "? 操作 · ↑/↓ 选择 · Space 勾选 · Esc 返回";
        for width in 0..90 {
            let fitted = fit_hints(text, width);
            assert!(fitted.width() <= usize::from(width));
            for group in fitted.split(" · ").filter(|group| !group.is_empty()) {
                assert!(text.split(" · ").any(|original| original == group));
            }
            if usize::from(width) >= "? 操作 · Esc 返回".width() {
                assert!(fitted.contains("? 操作") && fitted.contains("Esc 返回"));
            }
        }
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let area = buffer.area();
        let mut lines = Vec::new();
        for y in 0..area.height {
            let mut line = String::new();
            for x in 0..area.width {
                line.push_str(buffer[(x, y)].symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    fn compact(text: &str) -> String {
        text.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    #[test]
    fn ring_style_draws_only_canvas_rings() {
        let now = Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap();
        let snapshot = snapshot_ok(
            "claude",
            "Claude Code",
            vec![
                QuotaTier {
                    name: "five_hour".into(),
                    utilization: 42.0,
                    resets_at: Some(now + chrono::Duration::hours(2)),
                },
                QuotaTier {
                    name: "seven_day".into(),
                    utilization: 18.0,
                    resets_at: Some(now + chrono::Duration::days(4)),
                },
            ],
            None,
            now,
        );
        let view = crate::view::builtin_styles()
            .build("ring", &snapshot, now, None)
            .unwrap();
        assert_eq!(view.rings.len(), 2);
        assert!(view.bars.is_empty());

        let area = Rect::new(0, 0, 90, 14);
        let mut buffer = Buffer::empty(area);
        render_card(area, &mut buffer, "claude", "Claude Code", &view);
        let text = buffer_text(&buffer);
        let flat = compact(&text);
        assert!(flat.contains("ClaudeCode"), "{text}");
        assert!(flat.contains("5小时"), "{text}");
        assert!(flat.contains("7天"), "{text}");
        assert!(text.contains("42%"), "{text}");
        assert!(
            text.chars()
                .any(|ch| ('\u{2800}'..='\u{28FF}').contains(&ch)),
            "Canvas ring should use braille\n{text}"
        );
    }

    #[test]
    fn bar_style_uses_braille_only() {
        let view = CardView {
            account_email: None,
            badge: "12:00:00".into(),
            badge_kind: BadgeKind::Ok,
            kind: ViewKind::Bar,
            rings: Vec::new(),
            bars: vec![BarSpec {
                label: "5小时".into(),
                percent: 50.0,
                detail: "2h0m".into(),
            }],
            lines: Vec::new(),
        };
        let area = Rect::new(0, 0, 70, preferred_height(&view, 70));
        let mut buffer = Buffer::empty(area);
        render_card(area, &mut buffer, "codex", "Codex", &view);
        let text = buffer_text(&buffer);
        assert!(text.contains("Codex"), "{text}");
        assert!(
            text.contains('⣿'),
            "Braille bar should draw filled dots\n{text}"
        );
        assert_eq!(area.height, 3);
        let meter_cells = (0..area.width)
            .filter(|&x| buffer[(x, 1)].symbol() == "⣿")
            .collect::<Vec<_>>();
        assert!(meter_cells.len() >= 30, "bar is still too short\n{text}");
        let first = meter_cells[0];
        let last = *meter_cells.last().unwrap();
        assert_eq!(buffer[(first, 1)].fg, accent("codex"));
        assert_eq!(buffer[(first + meter_cells.len() as u16 / 2, 1)].fg, TRACK);
        assert_eq!(buffer[(first, 1)].bg, CARD_BG);
        let percent_x = (last + 1..area.width)
            .find(|&x| buffer[(x, 1)].symbol() == "5")
            .unwrap();
        let reset_x = (percent_x + 1..area.width)
            .find(|&x| buffer[(x, 1)].symbol() == "⟳")
            .unwrap();
        assert!(percent_x >= last + 3);
        assert!(reset_x >= percent_x + 6);
        assert!(text.contains("⟳ 2h0m"), "{text}");
        assert!(!text.contains('█'), "block gauge should be absent\n{text}");
        assert!(text.contains("50%"), "{text}");
    }

    #[test]
    fn account_email_has_its_own_row_above_usage() {
        let now = Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap();
        let mut snapshot = snapshot_ok(
            "codex",
            "Codex",
            vec![QuotaTier {
                name: "five_hour".into(),
                utilization: 50.0,
                resets_at: None,
            }],
            None,
            now,
        );
        snapshot.account_email = Some("user@example.com".into());
        let view = crate::view::build_view(&snapshot, "bar", now, None).unwrap();
        for (width, meter_row) in [(45, 2), (28, 3)] {
            let area = Rect::new(0, 0, width, preferred_height(&view, width));
            let mut buffer = Buffer::empty(area);
            render_card(area, &mut buffer, "codex", "Codex", &view);
            let text = buffer_text(&buffer);
            let rows = text.lines().collect::<Vec<_>>();
            assert!(rows[1].contains("user@example.com"), "{text}");
            assert!(rows[meter_row].contains('⣿'), "{text}");
        }
    }

    #[test]
    fn reset_note_has_space_after_braille_bars() {
        let view = CardView {
            account_email: None,
            badge: "12:00:00".into(),
            badge_kind: BadgeKind::Ok,
            kind: ViewKind::Bar,
            rings: Vec::new(),
            bars: vec![
                BarSpec {
                    label: "5小时".into(),
                    percent: 50.0,
                    detail: "2h0m".into(),
                },
                BarSpec {
                    label: "7天".into(),
                    percent: 23.0,
                    detail: "4d0h".into(),
                },
            ],
            lines: vec!["主动重置 2 次 · x".into()],
        };
        let area = Rect::new(0, 0, 70, preferred_height(&view, 70));
        assert_eq!(area.height, 7);
        let mut buffer = Buffer::empty(area);
        render_card(area, &mut buffer, "codex", "Codex", &view);
        assert_eq!(buffer[(12, 3)].symbol(), "⣿");
        assert!((2..area.width - 2).all(|x| buffer[(x, 4)].symbol() == " "));
        assert_eq!(buffer[(2, 5)].symbol(), "主");
        let text = buffer_text(&buffer);
        assert!(compact(&text).contains("主动重置2次·x"), "{text}");
    }

    #[test]
    fn braille_bar_expands_with_available_card_width() {
        let view = CardView {
            account_email: None,
            badge: "12:00:00".into(),
            badge_kind: BadgeKind::Ok,
            kind: ViewKind::Bar,
            rings: Vec::new(),
            bars: vec![BarSpec {
                label: "5小时".into(),
                percent: 50.0,
                detail: "4h35m".into(),
            }],
            lines: Vec::new(),
        };
        let lengths = [50, 70].map(|width| {
            let area = Rect::new(0, 0, width, preferred_height(&view, width));
            let mut buffer = Buffer::empty(area);
            render_card(area, &mut buffer, "codex", "Codex", &view);
            (0..width)
                .filter(|&x| buffer[(x, 1)].symbol() == "⣿")
                .count()
        });
        assert!(lengths[0] >= 18, "50-column card: {} cells", lengths[0]);
        assert!(lengths[1] >= 35, "70-column card: {} cells", lengths[1]);
        assert!(lengths[1] > lengths[0]);
    }

    #[test]
    fn braille_bar_fill_and_track_share_one_grid() {
        let area = Rect::new(0, 0, 5, 1);
        let mut buffer = Buffer::empty(area);
        BrailleBar {
            percent: 50.0,
            color: accent("codex"),
        }
        .render(area, &mut buffer);
        assert_eq!(buffer_text(&buffer), "⣿⣿⣿⣿⣿");
        assert_eq!(buffer[(0, 0)].fg, accent("codex"));
        assert_eq!(buffer[(2, 0)].fg, accent("codex"));
        assert_eq!(buffer[(3, 0)].fg, TRACK);
        assert_eq!(buffer[(3, 0)].bg, CARD_BG);
    }

    #[test]
    fn two_braille_bars_fit_narrow_card() {
        let view = CardView {
            account_email: None,
            badge: "12:00:00".into(),
            badge_kind: BadgeKind::Ok,
            kind: ViewKind::Bar,
            rings: Vec::new(),
            bars: vec![
                BarSpec {
                    label: "5小时".into(),
                    percent: 50.0,
                    detail: "2h0m".into(),
                },
                BarSpec {
                    label: "7天".into(),
                    percent: 23.0,
                    detail: "4d0h".into(),
                },
            ],
            lines: Vec::new(),
        };
        for width in [32, 40] {
            let area = Rect::new(0, 0, width, preferred_height(&view, width));
            let mut buffer = Buffer::empty(area);
            render_card(area, &mut buffer, "codex", "Codex", &view);
            let text = buffer_text(&buffer);
            let rows = text.lines().collect::<Vec<_>>();
            if width == 32 {
                assert_eq!(area.height, 7);
                assert!(compact(rows[1]).contains("5小时"), "{text}");
                assert!(
                    rows[1].contains("50%") && rows[1].contains("⟳ 2h0m"),
                    "{text}"
                );
                assert!(rows[4].contains("7天") && rows[4].contains("23%"), "{text}");
                assert!(rows[4].contains("⟳ 4d0h"), "{text}");
                assert_eq!(buffer[(2, 2)].symbol(), "⣿");
                assert_eq!(buffer[(2, 5)].symbol(), "⣿");
            } else {
                assert_eq!(area.height, 5);
                assert!(compact(rows[1]).contains("5小时"), "{text}");
                assert!(
                    rows[1].contains("50%") && rows[1].contains("⟳ 2h0m"),
                    "{text}"
                );
                assert!(rows[3].contains("7天") && rows[3].contains("23%"), "{text}");
                assert!(rows[3].contains("⟳ 4d0h"), "{text}");
                assert_eq!(buffer[(12, 1)].symbol(), "⣿");
                assert_eq!(buffer[(12, 3)].symbol(), "⣿");
            }
        }
    }

    #[test]
    fn statusline_style_is_compact_for_every_provider() {
        let now = Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap();
        for (id, name) in [
            ("claude", "Claude Code"),
            ("codex", "Codex"),
            ("grok", "Grok"),
        ] {
            let snapshot = snapshot_ok(
                id,
                name,
                vec![
                    QuotaTier {
                        name: "five_hour".into(),
                        utilization: 57.0,
                        resets_at: Some(now + chrono::Duration::hours(2)),
                    },
                    QuotaTier {
                        name: "seven_day".into(),
                        utilization: 24.0,
                        resets_at: None,
                    },
                ],
                None,
                now,
            );
            let view = crate::view::build_view(&snapshot, "statusline", now, None).unwrap();
            assert_eq!(view.kind, ViewKind::Statusline);
            assert!(view.rings.is_empty());
            let area = Rect::new(0, 0, 60, preferred_height(&view, 60));
            assert_eq!(area.height, 5);
            let mut buffer = Buffer::empty(area);
            render_card(area, &mut buffer, id, name, &view);
            let text = buffer_text(&buffer);
            let flat = compact(&text);
            assert!(flat.contains("5小时") && flat.contains("7天"), "{text}");
            assert!(text.contains('●') && text.contains('○'), "{text}");
            assert!(text.contains("57%") && text.contains("24%"), "{text}");
            assert_eq!(buffer[(14, 1)].symbol(), "●");
            assert_eq!(buffer[(15, 1)].symbol(), " ");
            assert_eq!(buffer[(16, 1)].symbol(), "●");
            assert_eq!(buffer[(39, 1)].symbol(), "⟳");
            assert_eq!(buffer[(40, 1)].symbol(), " ");
            assert_eq!(buffer[(14, 2)].symbol(), " ");
            assert!(!text.contains('⣿'), "{text}");
            let narrow = Rect::new(0, 0, 32, preferred_height(&view, 32));
            let mut narrow_buffer = Buffer::empty(narrow);
            render_card(narrow, &mut narrow_buffer, id, name, &view);
            let narrow_text = buffer_text(&narrow_buffer);
            assert!(narrow_text.contains("57%"), "{narrow_text}");
            assert!(narrow_text.contains("⟳ 2h0m"), "{narrow_text}");
        }
    }

    #[test]
    fn ring_widget_keeps_caption() {
        let area = Rect::new(0, 0, 24, 10);
        let mut buffer = Buffer::empty(area);
        RingGauge {
            percent: 73.0,
            label: "5小时".into(),
            color: meter_color("claude", 73.0),
        }
        .render(area, &mut buffer);
        let text = buffer_text(&buffer);
        assert!(text.contains("73%"), "{text}");
        assert!(compact(&text).contains("5小时"), "{text}");
    }

    #[test]
    fn provider_colors_apply_to_meters_and_grok_status() {
        assert_eq!(accent("codex"), Color::Rgb(96, 165, 250));
        assert_eq!(meter_color("codex", 50.0), accent("codex"));
        assert_eq!(meter_color("codex", 75.0), WARNING);
        assert_eq!(accent("grok"), TEXT);
        assert_eq!(meter_color("grok", 95.0), TEXT);
        assert_eq!(badge_color("grok", BadgeKind::Error), TEXT);
        assert_eq!(note_color("grok", "查询失败"), TEXT);
    }
}
