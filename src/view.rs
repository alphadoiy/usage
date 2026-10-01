//! 把用量快照编排成圆环、进度条或圆点状态栏。样式本身也走注册表。

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::labels::{fmt_percent, tier_label};
use crate::model::{CredentialStatus, ExtraUsage, QuotaTier, UsageSnapshot};
use crate::timeutil::{format_clock, format_compact_reset};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeKind {
    Ok,
    Muted,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    Bar,
    Ring,
    Statusline,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RingSpec {
    pub label: String,
    pub percent: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BarSpec {
    pub label: String,
    pub percent: f64,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CardView {
    pub account_email: Option<String>,
    pub badge: String,
    pub badge_kind: BadgeKind,
    pub kind: ViewKind,
    pub rings: Vec<RingSpec>,
    pub bars: Vec<BarSpec>,
    pub lines: Vec<String>,
}

type StyleBuilder = fn(&UsageSnapshot, DateTime<Utc>, Option<&str>) -> CardView;

pub struct StyleRegistry {
    order: Vec<String>,
    builders: HashMap<String, StyleBuilder>,
    labels: HashMap<String, String>,
}

impl StyleRegistry {
    pub fn new() -> Self {
        Self {
            order: Vec::new(),
            builders: HashMap::new(),
            labels: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: &str, label: &str, builder: StyleBuilder) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        if !self.builders.contains_key(name) {
            self.order.push(name.to_string());
        }
        self.builders.insert(name.to_string(), builder);
        self.labels.insert(name.to_string(), label.to_string());
    }

    pub fn ids(&self) -> Vec<String> {
        self.order.clone()
    }

    pub fn label(&self, name: &str) -> String {
        self.labels
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.builders.contains_key(name)
    }

    pub fn build(
        &self,
        name: &str,
        snapshot: &UsageSnapshot,
        now: DateTime<Utc>,
        warning: Option<&str>,
    ) -> Result<CardView, UnknownStyleError> {
        let Some(builder) = self.builders.get(name) else {
            return Err(UnknownStyleError {
                name: name.to_string(),
                known: self.ids(),
            });
        };
        Ok(builder(snapshot, now, warning))
    }
}

impl Default for StyleRegistry {
    fn default() -> Self {
        builtin_styles()
    }
}

#[derive(Debug)]
pub struct UnknownStyleError {
    pub name: String,
    pub known: Vec<String>,
}

impl std::fmt::Display for UnknownStyleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let known = if self.known.is_empty() {
            "（无）".to_string()
        } else {
            self.known.join(", ")
        };
        write!(formatter, "未知样式: {}。可选: {known}", self.name)
    }
}

impl std::error::Error for UnknownStyleError {}

pub fn builtin_styles() -> StyleRegistry {
    let mut registry = StyleRegistry::new();
    registry.register("bar", "Braille 进度条", bars_only);
    registry.register("ring", "圆环", rings_only);
    registry.register("statusline", "圆点状态栏", compact_statusline);
    registry
}

pub fn build_view(
    snapshot: &UsageSnapshot,
    style: &str,
    now: DateTime<Utc>,
    warning: Option<&str>,
) -> Result<CardView, UnknownStyleError> {
    builtin_styles().build(style, snapshot, now, warning)
}

fn bars_only(snapshot: &UsageSnapshot, now: DateTime<Utc>, warning: Option<&str>) -> CardView {
    let tiers = if snapshot.success {
        snapshot.tiers.as_slice()
    } else {
        &[]
    };
    card(
        snapshot,
        warning,
        ViewKind::Bar,
        Vec::new(),
        bars(tiers, now),
    )
}

fn rings_only(snapshot: &UsageSnapshot, _now: DateTime<Utc>, warning: Option<&str>) -> CardView {
    let tiers = if snapshot.success {
        snapshot.tiers.as_slice()
    } else {
        &[]
    };
    card(snapshot, warning, ViewKind::Ring, rings(tiers), Vec::new())
}

fn compact_statusline(
    snapshot: &UsageSnapshot,
    now: DateTime<Utc>,
    warning: Option<&str>,
) -> CardView {
    let tiers = if snapshot.success {
        snapshot.tiers.as_slice()
    } else {
        &[]
    };
    card(
        snapshot,
        warning,
        ViewKind::Statusline,
        Vec::new(),
        bars(tiers, now),
    )
}

fn card(
    snapshot: &UsageSnapshot,
    warning: Option<&str>,
    kind: ViewKind,
    rings: Vec<RingSpec>,
    bars: Vec<BarSpec>,
) -> CardView {
    let (badge, badge_kind) = badge(snapshot);
    CardView {
        account_email: snapshot.account_email.clone(),
        badge,
        badge_kind,
        kind,
        rings,
        bars,
        lines: lines(snapshot, warning),
    }
}

fn rings(tiers: &[QuotaTier]) -> Vec<RingSpec> {
    tiers
        .iter()
        .map(|tier| RingSpec {
            label: tier_label(&tier.name),
            percent: tier.utilization,
        })
        .collect()
}

fn bars(tiers: &[QuotaTier], now: DateTime<Utc>) -> Vec<BarSpec> {
    tiers
        .iter()
        .map(|tier| BarSpec {
            label: tier_label(&tier.name),
            percent: tier.utilization,
            detail: format_compact_reset(tier.resets_at, now),
        })
        .collect()
}

fn lines(snapshot: &UsageSnapshot, warning: Option<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(warning) = warning {
        lines.push(format!("网络波动，仍显示上次结果。{warning}"));
    }
    if snapshot.success {
        if let Some(available) = snapshot.reset_credits_available {
            lines.push(format!(
                "主动重置 {available} 次{}",
                if available > 0 { " · x" } else { "" }
            ));
        }
        if let Some(extra) = extra_line(snapshot.extra_usage.as_ref()) {
            lines.push(extra);
        } else if snapshot.tiers.is_empty() {
            lines.push("没有返回用量窗口".into());
        }
        return lines;
    }
    if let Some(message) = snapshot
        .credential_message
        .as_ref()
        .filter(|message| snapshot.error.as_ref() != Some(*message))
    {
        lines.push(message.clone());
    } else if let Some(error) = &snapshot.error {
        lines.push(error.clone());
    } else if let Some(message) = &snapshot.credential_message {
        lines.push(message.clone());
    } else {
        lines.push("未找到登录凭据".into());
    }
    lines
}

fn extra_line(extra: Option<&ExtraUsage>) -> Option<String> {
    let extra = extra.filter(|extra| extra.is_enabled)?;
    let symbol = if extra
        .currency
        .as_deref()
        .unwrap_or("")
        .eq_ignore_ascii_case("USD")
    {
        "$"
    } else {
        ""
    };
    let used = extra.used_credits.unwrap_or(0.0);
    let mut text = format!("超额  {symbol}{used:.2}");
    if let Some(limit) = extra.monthly_limit {
        text.push_str(&format!(" / {symbol}{limit:.2}"));
    }
    if let Some(utilization) = extra.utilization {
        text.push_str(&format!("  ({})", fmt_percent(utilization)));
    }
    Some(text)
}

fn badge(snapshot: &UsageSnapshot) -> (String, BadgeKind) {
    if snapshot.transient {
        return ("网络错误".into(), BadgeKind::Warn);
    }
    if snapshot.success
        && let Some(moment) = snapshot.queried_at
    {
        return (format_clock(moment), BadgeKind::Ok);
    }
    match snapshot.credential_status {
        CredentialStatus::NotFound => ("未登录".into(), BadgeKind::Muted),
        CredentialStatus::Expired => ("登录已过期".into(), BadgeKind::Error),
        CredentialStatus::ParseError => ("凭据无效".into(), BadgeKind::Error),
        CredentialStatus::Valid => ("查询失败".into(), BadgeKind::Error),
    }
}
