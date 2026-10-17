//! Compact provider inventory and shared terminal-safe text layout.
use ratatui::style::{Color, Stylize};
use ratatui::text::Line;
use unicode_width::UnicodeWidthChar;

use crate::provider_catalog::{Catalog, Models, Provider, ProviderUsage, QueryState};
use crate::resources::{ResourceMetric, ResourceScope, ResourceState};

pub(super) fn clean(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

pub(super) fn selected<'a>(
    catalog: Option<&'a Catalog>,
    id: Option<&str>,
) -> Option<(usize, &'a Provider)> {
    let catalog = catalog?;
    let index = id
        .and_then(|id| catalog.providers.iter().position(|p| p.id == id))
        .unwrap_or(0);
    catalog
        .providers
        .get(index)
        .map(|provider| (index, provider))
}

pub(super) fn lines(
    catalog: Option<&Catalog>,
    loading: bool,
    error: Option<&str>,
    expanded: bool,
    selected_id: Option<&str>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(error) = error {
        lines.push(Line::from(format!("读取失败 · {}", clean(error))).fg(Color::Yellow));
    }
    let Some(catalog) = catalog else {
        if loading {
            lines.push(Line::from("正在读取 Provider…").dim());
        }
        return lines;
    };
    lines.push(
        Line::from(format!(
            "{} 个 Provider{}",
            catalog.providers.len(),
            if loading { " · 更新状态中" } else { "" }
        ))
        .dim(),
    );
    if catalog.providers.is_empty() {
        lines.push(Line::from("尚无 Provider · F4 添加账号").dim());
    }
    if let Some((_, provider)) = selected(Some(catalog), selected_id) {
        lines.push(Line::from(""));
        let agent = provider
            .agent
            .as_ref()
            .map_or(String::new(), |agent| format!(" · {}", clean(agent)));
        lines.push(Line::from(format!("{}{}", clean(&provider.name), agent)).bold());
        lines.push(Line::from(format!("类型：{}", provider.kind.label())).dim());
        lines.extend(usage_lines(&provider.usage, expanded));
        if let Some(model) = &provider.configured_model {
            lines.push(Line::from(format!("默认模型（配置）：{}", clean(model))).dim());
        }
        match &provider.models {
            Models::Loading => lines.push(Line::from("模型：查询中…").dim()),
            Models::Unavailable(reason) => {
                lines.push(Line::from(format!("模型：{}", clean(reason))).fg(Color::Yellow))
            }
            Models::Ready(models) => {
                lines.push(Line::from(format!("接口模型列表 · {} 个", models.len())).dim());
                if models.is_empty() {
                    lines.push(Line::from("接口返回空列表").dim());
                }
                for model in models.iter().take(if expanded { usize::MAX } else { 3 }) {
                    lines.push(Line::from(format!("  • {}", clean(model))));
                }
                if !expanded && models.len() > 3 {
                    lines.push(
                        Line::from(format!("  另 {} 个 · Enter 展开模型", models.len() - 3)).dim(),
                    );
                }
            }
        }
    }
    lines
}

fn usage_lines(usage: &ProviderUsage, expanded: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if usage.sources.is_empty() {
        lines.push(
            Line::from(format!(
                "用量／余额：{}",
                clean(usage.hint.as_deref().unwrap_or("尚未配置查询服务"))
            ))
            .dim(),
        );
    }
    for source in &usage.sources {
        let scope = match &source.state {
            QueryState::Result(snapshot) => match snapshot.scope {
                ResourceScope::Account => "账号",
                ResourceScope::Token => "令牌",
                ResourceScope::Organization => "组织",
                ResourceScope::Project { .. } => "项目",
                ResourceScope::ApiKey { .. } => "API Key",
                ResourceScope::Unspecified => "资源",
            },
            _ => "资源",
        };
        lines.push(Line::from(format!("{} · {}", clean(&source.label), scope)).dim());
        match &source.state {
            QueryState::Loading => lines.push(Line::from("  用量／余额：查询中…").dim()),
            QueryState::Unavailable(message) => lines
                .push(Line::from(format!("  用量／余额：{}", clean(message))).fg(Color::Yellow)),
            QueryState::Result(snapshot) => {
                if snapshot.state != ResourceState::Ready {
                    lines.push(
                        Line::from(format!(
                            "  用量／余额：{}",
                            clean(snapshot.message.as_deref().unwrap_or("查询失败"))
                        ))
                        .fg(Color::Yellow),
                    );
                    continue;
                }
                for metric in &snapshot.metrics {
                    let text = match metric {
                        ResourceMetric::Balance { remaining } => {
                            format!(
                                "余额 {} {}{}",
                                remaining.amount,
                                remaining.currency,
                                if source.service == "new_api_token" {
                                    "（接口返回）"
                                } else {
                                    ""
                                }
                            )
                        }
                        ResourceMetric::Allowance {
                            unit, unlimited, ..
                        } if matches!(unit.as_str(), "new_api_quota" | "sub2api_quota") => {
                            if *unlimited {
                                "令牌不限额".into()
                            } else if snapshot
                                .metrics
                                .iter()
                                .any(|metric| matches!(metric, ResourceMetric::Balance { .. }))
                            {
                                continue;
                            } else {
                                "余额：未配置换算比例或币种".into()
                            }
                        }
                        ResourceMetric::Allowance {
                            remaining,
                            used,
                            unit,
                            unlimited,
                        } => format!(
                            "{} · 已用 {} · {}",
                            unit,
                            used.as_deref().unwrap_or("未知"),
                            if *unlimited {
                                match remaining {
                                    Some(value) => format!("剩余 {value} · 令牌不限额"),
                                    None => "额度不限量（非账号余额）".into(),
                                }
                            } else {
                                format!("剩余 {}", remaining.as_deref().unwrap_or("未知"))
                            }
                        ),
                        ResourceMetric::QuotaWindow {
                            name,
                            used_percent,
                            resets_at,
                        } => {
                            let reset = resets_at
                                .map(|t| {
                                    format!(
                                        " · {} 重置",
                                        t.with_timezone(&chrono::Local).format("%m-%d %H:%M")
                                    )
                                })
                                .unwrap_or_default();
                            format!(
                                "{} · 剩余 {:.0}%{}",
                                crate::labels::tier_label(name),
                                (100.0 - used_percent).clamp(0.0, 100.0),
                                reset
                            )
                        }
                        ResourceMetric::Cost {
                            used,
                            period_start,
                            period_end,
                        } => format!(
                            "费用 {} {} · {} 至 {} (UTC)",
                            used.amount,
                            used.currency,
                            period_start.format("%m-%d"),
                            period_end.format("%m-%d")
                        ),
                        ResourceMetric::Spending {
                            used,
                            limit,
                            currency,
                        } => format!(
                            "支出 {} / {} {}",
                            used.map(|v| v.to_string()).unwrap_or_else(|| "未知".into()),
                            limit
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "未知".into()),
                            currency.as_deref().unwrap_or("")
                        ),
                    };
                    lines.push(Line::from(format!("  {}", clean(&text))));
                }
                if snapshot.metrics.is_empty() {
                    lines.push(Line::from("  接口未返回额度或账单项目").dim());
                }
                if expanded && let Some(time) = snapshot.queried_at {
                    lines.push(
                        Line::from(format!(
                            "  更新 {}",
                            time.with_timezone(&chrono::Local).format("%m-%d %H:%M:%S")
                        ))
                        .dim(),
                    );
                }
            }
        }
    }
    lines
}

/// Hard-wrap styled lines once so viewport scrolling is by terminal rows, including CJK.
pub(super) fn wrap(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(2));
    let mut output = Vec::new();
    for line in lines {
        let mut row = Vec::new();
        let mut used = 0;
        for span in line.spans {
            let mut text = String::new();
            for ch in span.content.chars() {
                let size = ch.width().unwrap_or(0);
                if used + size > width {
                    row.push(ratatui::text::Span::styled(
                        std::mem::take(&mut text),
                        span.style,
                    ));
                    output.push(Line::from(std::mem::take(&mut row)).style(line.style));
                    used = 0;
                }
                text.push(ch);
                used += size;
            }
            row.push(ratatui::text::Span::styled(text, span.style));
        }
        output.push(Line::from(row).style(line.style));
    }
    output
}
