//! Contextual action menu: secondary commands stay discoverable without a crowded footer.
use ratatui::{
    Frame,
    crossterm::event::KeyCode,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

#[derive(Clone, Copy)]
pub(super) struct Action {
    pub key: KeyCode,
    pub label: &'static str,
    pub hint: &'static str,
}
impl Action {
    pub const fn new(key: KeyCode, hint: &'static str, label: &'static str) -> Self {
        Self { key, label, hint }
    }
}

pub(super) enum Decision {
    Stay,
    Close,
    Run(KeyCode),
}

pub(super) struct Menu {
    title: &'static str,
    navigation: &'static str,
    actions: Vec<Action>,
    selected: usize,
}
impl Menu {
    pub fn new(title: &'static str, navigation: &'static str, actions: Vec<Action>) -> Self {
        Self {
            title,
            navigation,
            actions,
            selected: 0,
        }
    }
    pub fn key(&mut self, key: KeyCode) -> Decision {
        match key {
            KeyCode::Esc | KeyCode::Char('?' | 'q') => return Decision::Close,
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.selected = self.selected.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.selected = (self.selected + 1).min(self.actions.len().saturating_sub(1))
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.actions.len().saturating_sub(1),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(6),
            KeyCode::PageDown => {
                self.selected = (self.selected + 6).min(self.actions.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                if let Some(action) = self.actions.get(self.selected) {
                    return Decision::Run(action.key);
                }
            }
            key if self.actions.iter().any(|action| action.key == key) => {
                return Decision::Run(key);
            }
            _ => {}
        }
        Decision::Stay
    }
    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let width = area.width.min(76);
        let height = area
            .height
            .min((self.actions.len() as u16).saturating_add(6));
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let block = Block::bordered()
            .title(format!(" {} · 操作 ", self.title))
            .style(Style::new().bg(Color::Rgb(22, 22, 24)).fg(Color::White))
            .border_style(Color::LightBlue);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let [navigation, list, hint] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        frame.render_widget(
            Paragraph::new(self.navigation)
                .style(Style::new().fg(Color::DarkGray))
                .wrap(Wrap { trim: false }),
            navigation,
        );
        let items: Vec<_> = self
            .actions
            .iter()
            .map(|a| ListItem::new(format!("{:<12} {}", a.hint, a.label)))
            .collect();
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("› ")
                .highlight_style(Style::new().bg(Color::Rgb(35, 45, 65))),
            list,
            &mut ListState::default().with_selected(Some(self.selected)),
        );
        frame.render_widget(Paragraph::new("↑/↓ 选择 · Enter 执行 · Esc 关闭"), hint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menu_dispatches_only_chosen_action_and_escape_dismisses() {
        let mut menu = Menu::new(
            "会话",
            "",
            vec![
                Action::new(KeyCode::Char('r'), "r", "刷新"),
                Action::new(KeyCode::Delete, "Delete", "删除"),
            ],
        );
        assert!(matches!(menu.key(KeyCode::Down), Decision::Stay));
        assert!(matches!(
            menu.key(KeyCode::Enter),
            Decision::Run(KeyCode::Delete)
        ));
        assert!(matches!(menu.key(KeyCode::Esc), Decision::Close));
        assert!(matches!(
            menu.key(KeyCode::Char('r')),
            Decision::Run(KeyCode::Char('r'))
        ));
    }
}
