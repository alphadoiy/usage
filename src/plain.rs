//! `--once` 的纯文本输出。交互界面走 ratatui，这里不依赖终端控件。

use chrono::{DateTime, Utc};
use unicode_width::UnicodeWidthStr;

use crate::labels::fmt_percent;
use crate::model::UsageSnapshot;
use crate::view::{CardView, ViewKind, build_view};

pub fn render_dashboard(snapshots: &[UsageSnapshot], style: &str, now: DateTime<Utc>) -> String {
    snapshots
        .iter()
        .map(|snapshot| {
            let view = build_view(snapshot, style, now, None).unwrap_or_else(|_| CardView {
                account_email: snapshot.account_email.clone(),
                badge: "查询失败".into(),
                badge_kind: crate::view::BadgeKind::Error,
                kind: ViewKind::Bar,
                rings: Vec::new(),
                bars: Vec::new(),
                lines: vec!["未知样式".into()],
            });
            render_card(&snapshot.display_name, &view)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_card(title: &str, view: &CardView) -> String {
    let mut lines = vec![format!("{title}  {}", view.badge)];
    if let Some(email) = &view.account_email {
        lines.push(format!("邮箱  {email}"));
    }
    if view.kind == ViewKind::Ring && !view.rings.is_empty() {
        let captions: Vec<String> = view
            .rings
            .iter()
            .map(|ring| format!("{} {}", fmt_percent(ring.percent), ring.label))
            .collect();
        lines.push(captions.join("   "));
    }
    for bar in &view.bars {
        if view.kind == ViewKind::Statusline {
            let percent = if bar.percent.is_finite() {
                bar.percent.clamp(0.0, 100.0)
            } else {
                0.0
            };
            let filled = ((percent / 100.0) * 8.0).round() as usize;
            let detail = if bar.detail.is_empty() {
                String::new()
            } else {
                format!("  ⟳ {}", bar.detail)
            };
            lines.push(format!(
                "{} {}{} {}{}",
                bar.label,
                "● ".repeat(filled),
                "○ ".repeat(8 - filled),
                fmt_percent(percent),
                detail
            ));
        } else {
            let padding = 10usize.saturating_sub(UnicodeWidthStr::width(bar.label.as_str()));
            let detail = if bar.detail.is_empty() {
                String::new()
            } else {
                format!("  ⟳ {}", bar.detail)
            };
            lines.push(format!(
                "{}{} {}  {}{}",
                bar.label,
                " ".repeat(padding),
                bar_line(bar.percent),
                fmt_percent(bar.percent),
                detail,
            ));
        }
    }
    lines.extend(view.lines.iter().cloned());
    lines.join("\n")
}

fn bar_line(percent: f64) -> String {
    let width = 12;
    let bounded = if percent.is_finite() {
        percent.clamp(0.0, 100.0)
    } else {
        0.0
    };
    let filled = ((bounded / 100.0) * (width * 2) as f64).round() as usize;
    let full = (filled / 2).min(width);
    let half = usize::from(!filled.is_multiple_of(2) && full < width);
    format!(
        "{}{}{}",
        "⣿".repeat(full),
        "⡇".repeat(half),
        "⠿".repeat(width - full - half)
    )
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::model::{QuotaTier, snapshot_ok};

    #[test]
    fn once_output_uses_compact_statusline_style() {
        let now = Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap();
        let mut snapshot = snapshot_ok(
            "codex",
            "Codex",
            vec![QuotaTier {
                name: "five_hour".into(),
                utilization: 57.0,
                resets_at: Some(now + chrono::Duration::hours(2)),
            }],
            None,
            now,
        );
        snapshot.account_email = Some("user@example.com".into());
        let output = render_dashboard(&[snapshot], "statusline", now);
        assert!(output.contains("邮箱  user@example.com"), "{output}");
        assert!(output.contains("●") && output.contains("○"), "{output}");
        assert!(output.contains("⟳ 2h0m"), "{output}");
        assert!(!output.contains('⣿'), "{output}");
    }
}
