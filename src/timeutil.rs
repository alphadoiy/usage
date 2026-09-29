//! 凭据过期和重置倒计时共用的时间解析。

use chrono::{DateTime, NaiveDateTime, Utc};

pub fn utcnow() -> DateTime<Utc> {
    Utc::now()
}

pub fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
        return Some(parsed.with_timezone(&Utc));
    }
    let bare = text.trim_end_matches('Z');
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(bare, format) {
            return Some(naive.and_utc());
        }
    }
    None
}

/// 兼容秒、毫秒时间戳和 ISO 字符串。解析不了时不当作过期。
pub fn is_expired_value(expires_at: &serde_json::Value, now: DateTime<Utc>) -> bool {
    let now_secs = now.timestamp();
    match expires_at {
        serde_json::Value::Number(number) => {
            let Some(ts) = number.as_i64() else {
                return false;
            };
            let secs = if ts > 1_000_000_000_000 {
                ts / 1000
            } else {
                ts
            };
            secs < now_secs
        }
        serde_json::Value::String(text) => parse_time(text)
            .map(|parsed| parsed.timestamp() < now_secs)
            .unwrap_or(false),
        _ => false,
    }
}

pub fn format_reset(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(resets_at) = resets_at else {
        return String::new();
    };
    let seconds = (resets_at - now).num_seconds();
    if seconds <= 0 {
        return "即将重置".to_string();
    }
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}天{hours}小时后重置")
    } else if hours > 0 {
        format!("{hours}小时{minutes}分后重置")
    } else if minutes > 0 {
        format!("{minutes}分后重置")
    } else {
        "不到1分后重置".to_string()
    }
}

pub fn format_compact_reset(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(resets_at) = resets_at else {
        return String::new();
    };
    let seconds = (resets_at - now).num_seconds();
    if seconds <= 0 {
        "即将".into()
    } else {
        compact_duration(seconds)
    }
}

pub fn compact_duration(seconds: i64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{}s", seconds.max(0))
    }
}

pub fn format_clock(moment: DateTime<Utc>) -> String {
    moment
        .with_timezone(&chrono::Local)
        .format("%H:%M:%S")
        .to_string()
}
