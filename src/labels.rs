//! 窗口名称的中文显示，与 cc-switch 的 tier 标识一致。

pub const TIER_LABELS: &[(&str, &str)] = &[
    ("five_hour", "5小时"),
    ("seven_day", "7天"),
    ("seven_day_fable", "Fable"),
    ("seven_day_opus", "7天(Opus)"),
    ("seven_day_sonnet", "7天(Sonnet)"),
    ("30_day", "30天"),
    ("weekly_limit", "7天"),
    ("monthly", "每月"),
    ("credits", "额度"),
];

pub const KNOWN_CLAUDE_TIERS: &[&str] = &[
    "five_hour",
    "seven_day",
    "seven_day_fable",
    "seven_day_opus",
    "seven_day_sonnet",
];

pub fn tier_label(name: &str) -> String {
    if let Some((_, label)) = TIER_LABELS.iter().find(|(key, _)| *key == name) {
        return (*label).to_string();
    }
    if let Some(days) = name.strip_suffix("_day")
        && days.chars().all(|ch| ch.is_ascii_digit())
    {
        return format!("{days}天");
    }
    if let Some(hours) = name.strip_suffix("_hour")
        && hours.chars().all(|ch| ch.is_ascii_digit())
    {
        return format!("{hours}小时");
    }
    name.to_string()
}

pub fn fmt_percent(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{:.0}%", value.round())
    } else {
        format!("{value:.1}%")
    }
}
