//! 提供方的统一配色，供 TUI 和状态栏共同使用。

pub type Rgb = (u8, u8, u8);

pub const TEXT: Rgb = (242, 242, 247);
pub const WARNING: Rgb = (255, 159, 10);
pub const ERROR: Rgb = (255, 69, 58);

pub fn accent(provider_id: &str) -> Rgb {
    match provider_id {
        "claude" => (217, 119, 87),
        "codex" => (96, 165, 250),
        "grok" => TEXT,
        _ => (142, 142, 147),
    }
}

pub fn meter(provider_id: &str, percent: f64) -> Rgb {
    if provider_id == "grok" {
        TEXT
    } else if percent >= 90.0 {
        ERROR
    } else if percent >= 70.0 {
        WARNING
    } else {
        accent(provider_id)
    }
}
