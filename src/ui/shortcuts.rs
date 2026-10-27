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
    pub disabled: Option<&'static str>,
}
impl Action {
    pub const fn new(key: KeyCode, hint: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            hint,
            disabled: None,
        }
    }
    pub fn disabled(mut self, reason: &'static str) -> Self {
        self.disabled = Some(reason);
        self
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
    query: String,
}
impl Menu {
    pub fn new(title: &'static str, navigation: &'static str, actions: Vec<Action>) -> Self {
        Self {
            title,
            navigation,
            actions,
            selected: 0,
            query: String::new(),
        }
    }
    fn visible(&self) -> Vec<&Action> {
        let query = self.query.to_lowercase();
        self.actions
            .iter()
            .filter(|action| {
                let text = format!("{} {}", action.label, action.hint).to_lowercase();
                query.split_whitespace().all(|part| text.contains(part))
            })
            .collect()
    }
    pub fn key(&mut self, key: KeyCode) -> Decision {
        let count = self.visible().len();
        match key {
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.selected = 0;
            }
            KeyCode::Esc => return Decision::Close,
            KeyCode::Up | KeyCode::BackTab => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = count.saturating_sub(1),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(6),
            KeyCode::PageDown => self.selected = (self.selected + 6).min(count.saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(action) = self.visible().get(self.selected)
                    && action.disabled.is_none()
                {
                    return Decision::Run(action.key);
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
            }
            KeyCode::Char(ch) => {
                self.query.push(ch);
                self.selected = 0;
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
            .min((self.actions.len() as u16).saturating_add(8));
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        super::widgets::clear_popup_edges(popup, frame.buffer_mut());
        frame.render_widget(Clear, popup);
        let block = Block::bordered()
            .title(format!(" {} · 操作 ", self.title))
            .style(Style::new().bg(Color::Rgb(22, 22, 24)).fg(Color::White))
            .border_style(Color::LightBlue);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let [search, navigation, list, reason, hint] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(u16::from(area.height >= 14)),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        frame.render_widget(Paragraph::new(format!("搜索操作：{}▏", self.query)), search);
        frame.render_widget(
            Paragraph::new(self.navigation)
                .style(Style::new().fg(Color::DarkGray))
                .wrap(Wrap { trim: false }),
            navigation,
        );
        let visible = self.visible();
        let items: Vec<_> = visible
            .iter()
            .map(|a| {
                ListItem::new(format!(
                    "{:<12} {}{}",
                    a.hint,
                    a.label,
                    if a.disabled.is_some() {
                        " · 不可用"
                    } else {
                        ""
                    }
                ))
                .style(Style::new().fg(if a.disabled.is_some() {
                    Color::DarkGray
                } else {
                    Color::White
                }))
            })
            .collect();
        if visible.is_empty() {
            frame.render_widget(Paragraph::new("没有匹配的操作 · Esc 清除搜索"), list);
        } else {
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_symbol("› ")
                    .highlight_style(Style::new().bg(Color::Rgb(35, 45, 65))),
                list,
                &mut ListState::default().with_selected(Some(self.selected)),
            );
        }
        let description = visible
            .get(self.selected)
            .and_then(|action| action.disabled)
            .unwrap_or("");
        frame.render_widget(
            Paragraph::new(description).style(Style::new().fg(Color::Yellow)),
            reason,
        );
        frame.render_widget(
            Paragraph::new("↑/↓ 选择 · Enter 执行 · Esc 清除 / 关闭"),
            hint,
        );
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
        assert!(matches!(menu.key(KeyCode::Char('r')), Decision::Stay));
        assert!(matches!(
            menu.key(KeyCode::Enter),
            Decision::Run(KeyCode::Char('r'))
        ));
    }

    #[test]
    fn search_matches_labels_and_keys_and_disabled_actions_do_not_execute() {
        let mut menu = Menu::new(
            "Provider",
            "",
            vec![
                Action::new(KeyCode::Char('x'), "x", "重置额度").disabled("不支持重置"),
                Action::new(KeyCode::Char('r'), "r", "刷新"),
            ],
        );
        for ch in "重置".chars() {
            menu.key(KeyCode::Char(ch));
        }
        assert_eq!(menu.visible().len(), 1);
        assert!(matches!(menu.key(KeyCode::Enter), Decision::Stay));
        assert!(matches!(menu.key(KeyCode::Esc), Decision::Stay));
        assert!(menu.query.is_empty());
        for ch in "刷新".chars() {
            menu.key(KeyCode::Char(ch));
        }
        assert!(matches!(
            menu.key(KeyCode::Enter),
            Decision::Run(KeyCode::Char('r'))
        ));
        menu.key(KeyCode::Backspace);
        menu.key(KeyCode::Char('无'));
        assert!(menu.visible().is_empty());
        assert!(matches!(menu.key(KeyCode::Enter), Decision::Stay));
    }
}
