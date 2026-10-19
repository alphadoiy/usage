//! Shared text editing and layout for Provider forms.
use ratatui::{crossterm::event::KeyCode, layout::Rect};
use std::path::PathBuf;

pub(super) fn expand_path(text: &str) -> PathBuf {
    if text == "~" {
        crate::config::home_dir()
    } else if let Some(rest) = text.strip_prefix("~/") {
        crate::config::home_dir().join(rest)
    } else {
        text.into()
    }
}
pub(super) fn edit_input(text: &mut String, key: KeyCode) {
    match key {
        KeyCode::Char(c) => text.push(c),
        KeyCode::Backspace => {
            text.pop();
        }
        _ => {}
    }
}
pub(super) fn modal(area: Rect, height: u16) -> Rect {
    let w = area.width.min(86);
    let h = area.height.min(height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}
