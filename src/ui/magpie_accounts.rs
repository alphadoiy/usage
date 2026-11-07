//! Magpie's account list owns its viewport; summary and model controls stay fixed.
use ratatui::{
    buffer::Buffer,
    crossterm::event::KeyCode,
    layout::Rect,
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::overview::{allowance_amount, clean, wrap};
use crate::provider_catalog::{Models, Provider, QueryState};
use crate::resources::{AccountStatus, ResourceAccount, ResourceMetric, ResourceState};

pub(super) struct State {
    selected: Option<String>,
    details: bool,
    detail_scroll: u16,
}
impl Default for State {
    fn default() -> Self {
        Self {
            selected: None,
            details: true,
            detail_scroll: 0,
        }
    }
}

pub(super) fn accounts(provider: &Provider) -> Vec<&ResourceAccount> {
    provider
        .usage
        .sources
        .iter()
        .filter(|s| s.service == "magpie_quotas")
        .filter_map(|s| match &s.state {
            QueryState::Result(s) if s.state == ResourceState::Ready => Some(&s.metrics),
            _ => None,
        })
        .flatten()
        .filter_map(|m| match m {
            ResourceMetric::Account { account } => Some(account),
            _ => None,
        })
        .collect()
}

impl State {
    fn index(&self, accounts: &[&ResourceAccount]) -> usize {
        self.selected
            .as_ref()
            .and_then(|id| accounts.iter().position(|a| &a.id == id))
            .unwrap_or(0)
    }
    pub(super) fn reconcile(&mut self, provider: &Provider) {
        let accounts = accounts(provider);
        if let Some(account) = accounts.get(self.index(&accounts)) {
            self.selected = Some(account.id.clone());
        }
    }
    pub(super) fn key(&mut self, key: KeyCode, provider: &Provider, size: (u16, u16)) -> bool {
        match key {
            KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k') => {
                let accounts = accounts(provider);
                if accounts.is_empty() {
                    return true;
                }
                let index = self.index(&accounts);
                let next = if matches!(key, KeyCode::Up | KeyCode::Char('k')) {
                    index.saturating_sub(1)
                } else {
                    (index + 1).min(accounts.len() - 1)
                };
                self.selected = Some(accounts[next].id.clone());
                self.detail_scroll = 0;
            }
            KeyCode::Enter => {
                self.details = !self.details;
                self.detail_scroll = 0;
            }
            KeyCode::PageDown | KeyCode::PageUp if self.details => {
                let accounts = accounts(provider);
                let lines = accounts
                    .get(self.index(&accounts))
                    .map_or(0, |a| details(a, size.0).len());
                let height = detail_height(size.1, lines);
                let max = lines
                    .saturating_sub(usize::from(height))
                    .min(u16::MAX as usize) as i32;
                let delta = if key == KeyCode::PageDown { 3 } else { -3 };
                self.detail_scroll = (i32::from(self.detail_scroll) + delta).clamp(0, max) as u16;
            }
            KeyCode::Esc if self.details => self.details = false,
            _ => return false,
        }
        true
    }
}

fn clip(text: &str, width: usize) -> String {
    let text = clean(text);
    if text.width() <= width {
        return text;
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for part in text.graphemes(true) {
        if out.width() + part.width() + 1 > width {
            break;
        }
        out.push_str(part);
    }
    out.push('…');
    out
}

fn metric_name(metric: &ResourceMetric) -> &str {
    match metric {
        ResourceMetric::AllowanceWindow { name, .. }
        | ResourceMetric::QuotaWindow { name, .. }
        | ResourceMetric::ReportedBalance { name, .. } => name,
        ResourceMetric::Allowance { name, .. } => name.as_deref().unwrap_or("额度"),
        _ => "额度",
    }
}
fn total(metric: &ResourceMetric) -> bool {
    let name = metric_name(metric);
    name.contains('总') || name.to_lowercase().starts_with("total")
}
fn numeric(metric: &ResourceMetric) -> bool {
    matches!(
        metric,
        ResourceMetric::AllowanceWindow { .. }
            | ResourceMetric::Allowance { .. }
            | ResourceMetric::Balance { .. }
    )
}
fn primary(account: &ResourceAccount) -> Option<&ResourceMetric> {
    account
        .metrics
        .iter()
        .find(|m| total(m) && numeric(m))
        .or_else(|| account.metrics.iter().find(|m| numeric(m)))
        .or_else(|| {
            account.metrics.iter().find(
                |m| matches!(m, ResourceMetric::ReportedBalance { name, .. } if name == "余额"),
            )
        })
        .or_else(|| {
            account
                .metrics
                .iter()
                .find(|m| matches!(m, ResourceMetric::QuotaWindow { .. }))
        })
        .or_else(|| account.metrics.first())
}
fn percent(metric: &ResourceMetric) -> Option<f64> {
    match metric {
        ResourceMetric::QuotaWindow { used_percent, .. } => Some(*used_percent),
        ResourceMetric::AllowanceWindow { used, limit, .. } => used
            .parse::<f64>()
            .ok()
            .zip(limit.parse::<f64>().ok())
            .filter(|(_, limit)| *limit > 0.0)
            .map(|(u, l)| u / l * 100.0)
            .filter(|p| p.is_finite()),
        _ => None,
    }
}
fn balance(metric: &ResourceMetric) -> String {
    match metric {
        ResourceMetric::AllowanceWindow {
            remaining, unit, ..
        } => format!(
            "{} {}",
            allowance_amount(
                Some(remaining),
                if unit == "积分" { "credits" } else { unit }
            ),
            clean(unit)
        ),
        ResourceMetric::Allowance {
            remaining,
            unit,
            unlimited,
            ..
        } => {
            if *unlimited {
                "不限量".into()
            } else {
                format!(
                    "{} {}",
                    allowance_amount(
                        remaining.as_deref(),
                        if unit == "积分" { "credits" } else { unit }
                    ),
                    clean(unit)
                )
            }
        }
        ResourceMetric::ReportedBalance { display, .. } => clean(display),
        ResourceMetric::Balance { remaining } => format!(
            "{} {}",
            clean(&remaining.amount),
            clean(&remaining.currency)
        ),
        ResourceMetric::QuotaWindow { used_percent, .. } => {
            format!("已用 {}", crate::labels::fmt_percent(*used_percent))
        }
        _ => "未返回余额".into(),
    }
}
fn status(account: &ResourceAccount) -> Option<(&'static str, Color)> {
    match account.status {
        AccountStatus::Failed => Some(("查询失败", Color::Yellow)),
        AccountStatus::Pending => Some(("待查询", Color::DarkGray)),
        AccountStatus::Cached => Some(("缓存", Color::Yellow)),
        AccountStatus::Ready => None,
    }
}
fn amount(account: &ResourceAccount) -> (String, Color) {
    if matches!(
        account.status,
        AccountStatus::Failed | AccountStatus::Pending
    ) {
        let (label, color) = status(account).unwrap();
        return (label.into(), color);
    }
    let metric = primary(account);
    let color = match metric.and_then(percent) {
        Some(p) if p >= 95.0 => Color::Red,
        Some(p) if p >= 80.0 => Color::Yellow,
        _ => Color::White,
    };
    let mut text = metric.map_or_else(|| "未返回余额".into(), balance);
    let extra = account
        .metrics
        .iter()
        .filter(|m| numeric(m))
        .count()
        .saturating_sub(1);
    if extra > 0 {
        text.push_str(&format!(" · +{extra}项"));
    }
    if account.status == AccountStatus::Cached {
        text.push_str(" · 缓存");
    }
    (text, color)
}
fn short_identity(value: &str, width: usize) -> String {
    let value = clean(value);
    if value.width() <= width {
        return value;
    }
    let tail_width = width.saturating_sub(1) / 2;
    let mut tail = Vec::new();
    let mut used = 0;
    for part in value.graphemes(true).rev() {
        if used + part.width() > tail_width {
            break;
        }
        used += part.width();
        tail.push(part);
    }
    format!(
        "{}{}",
        clip(&value, width.saturating_sub(used)),
        tail.into_iter().rev().collect::<String>()
    )
}

fn label(account: &ResourceAccount, all: &[&ResourceAccount], width: usize) -> String {
    let duplicate_vendor = all
        .iter()
        .filter(|a| a.provider == account.provider)
        .count()
        > 1;
    match account.user.as_deref() {
        Some(user) if duplicate_vendor || user.width() <= 16 => {
            let user = short_identity(user, 14.min(width / 2));
            let name = clip(&account.name, width.saturating_sub(user.width() + 3));
            format!("{name} · {user}")
        }
        _ => clip(&account.name, width),
    }
}

fn quantity(metric: &ResourceMetric) -> Option<(rust_decimal::Decimal, String)> {
    let (value, unit) = match metric {
        ResourceMetric::AllowanceWindow {
            remaining, unit, ..
        } => (remaining.as_str(), unit.as_str()),
        ResourceMetric::Allowance {
            remaining: Some(value),
            unit,
            ..
        } => (value.as_str(), unit.as_str()),
        ResourceMetric::ReportedBalance { display, .. } => {
            let text = display.trim();
            let split =
                text.find(|c: char| !c.is_ascii_digit() && !matches!(c, '.' | ',' | '-' | '+'))?;
            (&text[..split], text[split..].trim())
        }
        _ => return None,
    };
    Some((value.replace(',', "").parse().ok()?, unit.to_lowercase()))
}

pub(super) fn details(account: &ResourceAccount, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(clean(&account.name)).fg(Color::Cyan).bold()];
    if let Some(user) = &account.user {
        lines.push(Line::from(format!("账号 {}", clean(user))).dim());
    }
    if let Some((label, color)) = status(account) {
        lines.push(Line::from(label).fg(color));
    }
    if matches!(
        account.status,
        AccountStatus::Failed | AccountStatus::Pending
    ) {
        return wrap(lines, width);
    }
    let primary = primary(account);
    let has_breakdown = primary.is_some_and(|p| {
        total(p)
            && quantity(p).is_some_and(|(_, unit)| {
                account.metrics.iter().any(|m| {
                    !total(m) && numeric(m) && quantity(m).is_some_and(|(_, other)| unit == other)
                })
            })
    });
    for metric in &account.metrics {
        // A reported balance is often a duplicate of a structured balance window.
        if matches!(metric, ResourceMetric::ReportedBalance { name, .. } if name == "余额")
            && quantity(metric).is_some_and(|value| {
                account
                    .metrics
                    .iter()
                    .any(|other| numeric(other) && quantity(other).as_ref() == Some(&value))
            })
        {
            continue;
        }
        if has_breakdown && primary.is_some_and(|p| std::ptr::eq(metric, p)) {
            continue;
        }
        let mut spans = vec![
            Span::raw(format!("{}  ", clean(metric_name(metric)))).dim(),
            Span::raw(balance(metric)).bold(),
        ];
        if let Some(percent) = percent(metric) {
            let label = crate::labels::fmt_percent(percent);
            let used = spans.iter().map(|s| s.content.width()).sum::<usize>();
            let room = usize::from(width).saturating_sub(used + label.width() + 3);
            let bar = if room >= 4 { room.min(8) } else { 0 };
            let filled = ((percent.clamp(0.0, 100.0) / 100.0) * bar as f64).round() as usize;
            let text = if bar > 0 {
                format!(
                    "  {}{} {label}",
                    "━".repeat(filled),
                    "─".repeat(bar - filled)
                )
            } else {
                format!(" · {label}")
            };
            spans.push(Span::raw(text).fg(if percent >= 95.0 {
                Color::Red
            } else {
                Color::Cyan
            }));
        }
        lines.push(Line::from(spans));
    }
    wrap(lines, width)
}

fn today(provider: &Provider) -> Line<'static> {
    let source = provider
        .usage
        .sources
        .iter()
        .find(|s| s.service == "magpie_usage");
    if let Some(source) = source {
        match &source.state {
            QueryState::Result(s) if s.state == ResourceState::Ready => {
                if let Some(ResourceMetric::RequestUsage {
                    requests,
                    failed_requests,
                    input_tokens,
                    output_tokens,
                    ..
                }) = s.metrics.first()
                {
                    let tokens = u128::from(*input_tokens) + u128::from(*output_tokens);
                    return Line::from(format!(
                        "今日 · 请求 {requests} 次 · {tokens} Token{}",
                        if *failed_requests > 0 {
                            format!(" · 失败 {failed_requests} 次")
                        } else {
                            String::new()
                        }
                    ));
                }
            }
            QueryState::Loading => return Line::from("今日用量 · 查询中…").dim(),
            _ => return Line::from("今日用量 · 查询失败").yellow(),
        }
    }
    Line::from("今日用量 · 暂无数据").dim()
}

fn text(area: Rect, buf: &mut Buffer, lines: Vec<Line<'static>>) {
    Paragraph::new(lines).render(area, buf);
}
fn row(area: Rect, y: u16, height: u16) -> Rect {
    Rect::new(
        area.x,
        area.y + y,
        area.width,
        height.min(area.height.saturating_sub(y)),
    )
}

fn detail_height(height: u16, lines: usize) -> u16 {
    let available = height.saturating_sub(6);
    if available >= 6 {
        lines.min(usize::from(available / 2)).min(8) as u16
    } else {
        0
    }
}

pub(super) fn model_scroll_max(provider: &Provider, height: u16) -> u16 {
    let count = match &provider.models {
        Models::Ready(items) => items.len(),
        _ => 1,
    };
    count
        .saturating_sub(usize::from(height.saturating_sub(5)))
        .min(u16::MAX as usize) as u16
}

pub(super) fn render(
    area: Rect,
    buf: &mut Buffer,
    provider: &Provider,
    state: &State,
    models: bool,
    scroll: u16,
) {
    if area.is_empty() {
        return;
    }
    text(
        row(area, 0, 1),
        buf,
        vec![Line::from("Magpie · 内置").bold()],
    );
    text(
        row(area, 1, 2),
        buf,
        wrap(vec![today(provider)], area.width),
    );
    if area.height < 4 {
        return;
    }
    let footer = area.height.saturating_sub(2);
    let model = provider
        .configured_model
        .as_deref()
        .filter(|m| !m.is_empty() && *m != "默认模型")
        .unwrap_or("尚未选择");
    let model_count = match &provider.models {
        Models::Ready(m) => format!("{} 个", m.len()),
        Models::Loading => "查询中".into(),
        Models::Unavailable(_) => "查询失败".into(),
    };
    let suffix = format!(" · {model_count} · M");
    let model_width = usize::from(area.width).saturating_sub(5 + suffix.width());
    text(
        row(area, footer, 1),
        buf,
        vec![
            Line::from(clip(
                &format!("模型 {}{suffix}", clip(model, model_width)),
                usize::from(area.width),
            ))
            .dim(),
        ],
    );
    let hint = if models {
        "↑↓ 滚动 · Enter 返回账号 · e 配置"
    } else {
        "↑↓ 账号 · Enter 详情 · e 配置 · M 模型 · v 请求"
    };
    text(
        row(area, footer + 1, 1),
        buf,
        vec![Line::from(clip(hint, usize::from(area.width))).dim()],
    );
    if area.height < 7 {
        return;
    }
    if models {
        let body = row(area, 3, footer.saturating_sub(3));
        let lines = match &provider.models {
            Models::Ready(items) => items
                .iter()
                .map(|m| Line::from(clip(m, usize::from(area.width))))
                .collect::<Vec<_>>(),
            Models::Loading => vec![Line::from("正在读取模型…").dim()],
            Models::Unavailable(reason) => {
                wrap(vec![Line::from(clean(reason)).yellow()], body.width)
            }
        };
        let max = lines
            .len()
            .saturating_sub(usize::from(body.height))
            .min(u16::MAX as usize) as u16;
        Paragraph::new(lines)
            .scroll((scroll.min(max), 0))
            .render(body, buf);
        return;
    }
    let accounts = accounts(provider);
    let index = state.index(&accounts).min(accounts.len().saturating_sub(1));
    let available = footer.saturating_sub(4);
    let detail_lines = accounts
        .get(index)
        .filter(|_| state.details)
        .map(|a| details(a, area.width))
        .unwrap_or_default();
    let detail_height = detail_height(area.height, detail_lines.len());
    let list_height = available.saturating_sub(detail_height + u16::from(detail_height > 0));
    let row_height = if area.width < 64 && list_height >= 2 {
        2
    } else {
        1
    };
    let slots = usize::from(list_height / row_height).max(1);
    let start = index
        .saturating_sub(slots / 2)
        .min(accounts.len().saturating_sub(slots));
    let end = (start + slots).min(accounts.len());
    let title = if accounts.is_empty() {
        "账号".into()
    } else {
        format!("账号 {}–{} / {}", start + 1, end, accounts.len())
    };
    text(
        row(area, 3, 1),
        buf,
        vec![Line::from(format!("{title} · 剩余")).cyan()],
    );
    if accounts.is_empty() {
        let source = provider
            .usage
            .sources
            .iter()
            .find(|s| s.service == "magpie_quotas");
        let message = match source.map(|s| &s.state) {
            Some(QueryState::Loading) => "正在读取账号额度…",
            Some(QueryState::Result(s)) if s.state == ResourceState::Ready => "暂无可查询账号",
            Some(_) => "额度查询失败 · r 重试",
            None => "尚未连接额度查询",
        };
        text(
            row(area, 4, list_height),
            buf,
            vec![Line::from(message).dim()],
        );
    }
    for (offset, account) in accounts[start..end].iter().enumerate() {
        let y = 4 + offset as u16 * row_height;
        if y >= 4 + list_height {
            break;
        }
        let selected = start + offset == index;
        let style = if selected {
            super::widgets::selected_row()
        } else {
            Style::default()
        };
        let marker = if selected { "› " } else { "  " };
        let (amount, color) = amount(account);
        if row_height == 1 {
            let left = usize::from(area.width).saturating_mul(2) / 5;
            let name = label(account, &accounts, left.saturating_sub(2));
            let value = clip(&amount, usize::from(area.width).saturating_sub(left + 1));
            let padding = usize::from(area.width).saturating_sub(2 + name.width() + value.width());
            text(
                row(area, y, 1),
                buf,
                vec![
                    Line::from(vec![
                        Span::raw(marker).cyan(),
                        Span::raw(name),
                        Span::raw(" ".repeat(padding)),
                        Span::raw(value).fg(color).bold(),
                    ])
                    .style(style),
                ],
            );
        } else {
            text(
                row(area, y, 1),
                buf,
                vec![
                    Line::from(format!(
                        "{marker}{}",
                        label(
                            account,
                            &accounts,
                            usize::from(area.width).saturating_sub(2)
                        )
                    ))
                    .style(style),
                ],
            );
            text(
                row(area, y + 1, 1),
                buf,
                vec![
                    Line::from(format!(
                        "    {}",
                        clip(&amount, usize::from(area.width).saturating_sub(4))
                    ))
                    .fg(color)
                    .style(style),
                ],
            );
        }
    }
    if detail_height > 0 {
        let body = row(area, footer - detail_height, detail_height);
        let max = detail_lines
            .len()
            .saturating_sub(usize::from(detail_height))
            .min(u16::MAX as usize) as u16;
        let offset = state.detail_scroll.min(max);
        if max > 0 {
            text(
                row(area, footer - detail_height - 1, 1),
                buf,
                vec![
                    Line::from(clip(
                        &format!(
                            "详情 {}–{} / {} · PgUp/PgDn",
                            offset + 1,
                            offset + detail_height,
                            detail_lines.len()
                        ),
                        usize::from(area.width),
                    ))
                    .dim(),
                ],
            );
        }
        Paragraph::new(detail_lines)
            .scroll((offset, 0))
            .render(body, buf);
    }
}

#[cfg(test)]
mod tests;
