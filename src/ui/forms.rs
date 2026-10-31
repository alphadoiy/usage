//! Shared text editing and layout for Provider forms.
use ratatui::{
    Frame,
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
    layout::Rect,
    style::Modifier,
    widgets::{Paragraph, Widget},
};
use std::path::PathBuf;
use tui_input::{Input, InputRequest};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) fn expand_path(text: &str) -> PathBuf {
    if text == "~" {
        crate::config::home_dir()
    } else if let Some(rest) = text.strip_prefix("~/") {
        crate::config::home_dir().join(rest)
    } else {
        text.into()
    }
}
/// Single-line editing state. Forms own focus and submission; this owns text and cursor.
#[derive(Clone, Default)]
pub(super) struct TextInput(Input);

impl TextInput {
    pub fn as_str(&self) -> &str {
        self.0.value()
    }
    pub fn clear(&mut self) {
        self.0.reset();
    }
    /// Returns whether the value changed, so callers can invalidate dependent data.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        use InputRequest::*;
        let request = match (key.code, key.modifiers) {
            (KeyCode::Left | KeyCode::Char('b'), KeyModifiers::ALT) => Some(GoToPrevWord),
            (KeyCode::Right | KeyCode::Char('f'), KeyModifiers::ALT) => Some(GoToNextWord),
            (KeyCode::Char('d'), KeyModifiers::ALT) => Some(DeleteNextWord),
            (KeyCode::Char(c), _) if c.is_control() => None,
            _ => tui_input::backend::crossterm::to_input_request(&Event::Key(key)),
        };
        request
            .and_then(|r| self.0.handle(r))
            .is_some_and(|change| change.value)
    }
    pub fn paste(&mut self, text: &str) -> bool {
        let mut changed = false;
        // A pasted newline never submits a form or creates an invisible extra line.
        for c in text.chars().filter(|c| !c.is_control()) {
            changed |= self.0.handle(InputRequest::InsertChar(c)).is_some();
        }
        changed
    }
    pub fn render(&self, frame: &mut Frame, area: Rect, secret: bool, focused: bool) {
        if area.is_empty() {
            return;
        }
        let (visible, x) = self.viewport(area.width, secret, focused);
        frame.render_widget(Paragraph::new(visible), area);
        if focused {
            frame.set_cursor_position((area.x + x, area.y));
        }
    }
    /// Buffer-based screens use the same viewport with a visible block caret.
    pub fn render_buffer(&self, area: Rect, buffer: &mut Buffer, focused: bool) {
        if area.is_empty() {
            return;
        }
        let (visible, x) = self.viewport(area.width, false, focused);
        Paragraph::new(visible).render(area, buffer);
        if focused && let Some(cell) = buffer.cell_mut((area.x + x, area.y)) {
            cell.set_style(ratatui::style::Style::new().add_modifier(Modifier::REVERSED));
        }
    }
    fn viewport(&self, width: u16, secret: bool, focused: bool) -> (String, u16) {
        let (text, cursor) = if secret {
            let before: String = self.chars().take(self.0.cursor()).collect();
            (
                "•".repeat(self.graphemes(true).count()),
                before.graphemes(true).count(),
            )
        } else {
            (self.to_string(), self.0.visual_cursor())
        };
        let desired = if focused {
            cursor.saturating_sub(usize::from(width.saturating_sub(1)))
        } else {
            0
        };
        let mut skipped = 0;
        let visible: String = text
            .graphemes(true)
            .skip_while(|g| {
                if skipped < desired {
                    skipped += g.width();
                    true
                } else {
                    false
                }
            })
            .collect();
        let x = cursor
            .saturating_sub(skipped)
            .min(usize::from(width.saturating_sub(1)));
        (visible, x as u16)
    }
}
impl std::ops::Deref for TextInput {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl std::fmt::Display for TextInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.as_str(), f)
    }
}
impl std::fmt::Debug for TextInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TextInput(..)")
    }
}
impl From<String> for TextInput {
    fn from(value: String) -> Self {
        Self(Input::new(
            value.chars().filter(|c| !c.is_control()).collect(),
        ))
    }
}
impl From<&str> for TextInput {
    fn from(value: &str) -> Self {
        value.to_owned().into()
    }
}
impl From<std::borrow::Cow<'_, str>> for TextInput {
    fn from(value: std::borrow::Cow<'_, str>) -> Self {
        value.into_owned().into()
    }
}
impl PartialEq<&str> for TextInput {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
pub(super) fn input_row(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    input: &TextInput,
    secret: bool,
    focused: bool,
) {
    let label = format!("{} {label}：", if focused { "›" } else { " " });
    let width = (label.width() as u16).min(area.width / 2);
    frame.render_widget(
        Paragraph::new(label),
        Rect::new(area.x, area.y, width, area.height),
    );
    input.render(
        frame,
        Rect::new(area.x + width, area.y, area.width - width, area.height),
        secret,
        focused,
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn editing_in_the_middle_preserves_unicode_graphemes() {
        let mut input: TextInput = "a中👩‍💻z".into();
        input.key(KeyCode::Left.into());
        input.key(KeyCode::Backspace.into());
        assert_eq!(input.as_str(), "a中z");
        input.key(KeyCode::Char('文').into());
        input.key(KeyCode::Delete.into());
        assert_eq!(input.as_str(), "a中文");
        input.key(KeyCode::Home.into());
        input.key(KeyCode::Char('前').into());
        input.key(KeyCode::End.into());
        input.key(KeyCode::Char('后').into());
        assert_eq!(input.as_str(), "前a中文后");
    }

    #[test]
    fn paste_inserts_at_cursor_and_filters_control_characters() {
        let mut input: TextInput = "ab".into();
        input.key(KeyCode::Left.into());
        input.paste("中\r\n文\t\u{1b}");
        assert_eq!(input.as_str(), "a中文b");
        assert!(!input.key(KeyCode::Enter.into()));
        input.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(input.is_empty());
    }

    #[test]
    fn secret_rendering_and_debug_never_expose_the_value() {
        let input: TextInput = "synthetic-secret-👩‍💻".into();
        let mut terminal = Terminal::new(TestBackend::new(12, 3)).unwrap();
        terminal
            .draw(|frame| input.render(frame, Rect::new(2, 1, 6, 1), true, true))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(!text.contains("synthetic"));
        assert!(text.contains('•'));
        assert!(!format!("{input:?}").contains("synthetic"));
        let cursor = terminal.get_cursor_position().unwrap();
        assert_eq!((cursor.x, cursor.y), (7, 1));
    }

    #[test]
    fn scrolling_tracks_cursor_in_a_narrow_field() {
        let mut input: TextInput = "abcdef中文".into();
        let mut terminal = Terminal::new(TestBackend::new(12, 3)).unwrap();
        terminal
            .draw(|frame| input.render(frame, Rect::new(2, 1, 6, 1), false, true))
            .unwrap();
        // Wide characters occupy a following spacer cell; inspect their screen
        // positions instead of concatenating every cell's symbol.
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 1)].symbol(), "f");
        assert_eq!(buffer[(3, 1)].symbol(), "中");
        assert_eq!(buffer[(5, 1)].symbol(), "文");
        assert_eq!(terminal.get_cursor_position().unwrap().x, 7);
        input.key(KeyCode::Home.into());
        terminal
            .draw(|frame| input.render(frame, Rect::new(2, 1, 6, 1), false, true))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("abcdef"));
        assert_eq!(terminal.get_cursor_position().unwrap().x, 2);
    }
}
