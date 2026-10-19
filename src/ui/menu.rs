//! Shared navigation menu for the home, history, session, and account pages.
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Home,
    History,
    Sessions,
    Providers,
    AddProvider,
    Quit,
}

impl Action {
    fn title(self) -> &'static str {
        match self {
            Self::Home => "F1  运行与资源首页",
            Self::History => "F2  Token 历史",
            Self::Sessions => "F3  会话浏览",
            Self::Providers => "F4  Provider 管理",
            Self::AddProvider => "添加 Provider",
            Self::Quit => "    退出 usage",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Home => "查看托管 Agent 运行、共享余额与费用，以及官方订阅额度",
            Self::History => "查看本地 Token 贡献图、项目和模型统计",
            Self::Sessions => "浏览项目会话，并在终端或分屏中恢复",
            Self::Providers => "管理 BYOK、OAuth 与 WorkBuddy，配置 Agent、导入或同步 CC Switch",
            Self::AddProvider => "添加 BYOK、ChatGPT OAuth、WorkBuddy，或导入已有账号",
            Self::Quit => "关闭面板，返回终端",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    Stay,
    Dismiss,
    Activate(Action),
}

pub(super) struct Menu {
    actions: Vec<Action>,
    selected: usize,
}

impl Menu {
    pub(super) fn new(current: Action) -> Self {
        let actions = vec![
            Action::Home,
            Action::History,
            Action::Sessions,
            Action::Providers,
            Action::Quit,
        ];
        let selected = actions.iter().position(|a| *a == current).unwrap_or(0);
        Self { actions, selected }
    }

    pub(super) fn key(&mut self, key: KeyCode) -> Command {
        match key {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(10) => Command::Dismiss,
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.selected = (self.selected + self.actions.len() - 1) % self.actions.len();
                Command::Stay
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.selected = (self.selected + 1) % self.actions.len();
                Command::Stay
            }
            KeyCode::Home => {
                self.selected = 0;
                Command::Stay
            }
            KeyCode::End => {
                self.selected = self.actions.len() - 1;
                Command::Stay
            }
            KeyCode::Enter => Command::Activate(self.actions[self.selected]),
            KeyCode::F(1) | KeyCode::Char('1') => self.activate(Action::Home),
            KeyCode::F(2) | KeyCode::Char('2') => self.activate(Action::History),
            KeyCode::F(3) | KeyCode::Char('3') => self.activate(Action::Sessions),
            KeyCode::F(4) | KeyCode::Char('4') => self.activate(Action::Providers),
            KeyCode::F(5) | KeyCode::Char('5') => self.activate(Action::AddProvider),
            _ => Command::Stay,
        }
    }

    fn activate(&self, action: Action) -> Command {
        if action == Action::AddProvider || self.actions.contains(&action) {
            Command::Activate(action)
        } else {
            Command::Stay
        }
    }

    pub(super) fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let width = area.width.min(66);
        let height = area.height.min(13);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let block = Block::bordered().title(" 功能菜单 ").style(
            Style::new()
                .bg(Color::Rgb(22, 22, 24))
                .fg(Color::Rgb(242, 242, 247)),
        );
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let [list, description, hint] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .areas(inner);
        let items: Vec<_> = self
            .actions
            .iter()
            .map(|a| ListItem::new(a.title()))
            .collect();
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("› ")
                .highlight_style(Style::new().bg(Color::Rgb(35, 45, 65)).fg(Color::LightBlue)),
            list,
            &mut ListState::default().with_selected(Some(self.selected)),
        );
        frame.render_widget(
            Paragraph::new(self.actions[self.selected].description())
                .style(Style::new().fg(Color::Rgb(142, 142, 147)))
                .wrap(Wrap { trim: true }),
            description,
        );
        frame.render_widget(
            Paragraph::new("↑/↓ 选择 · Enter 打开 · Esc 返回").wrap(Wrap { trim: true }),
            hint,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn keyboard_selection_and_shortcut_open_the_same_account_page() {
        let mut menu = Menu::new(Action::Home);
        for _ in 0..3 {
            assert_eq!(menu.key(KeyCode::Down), Command::Stay);
        }
        assert_eq!(
            menu.key(KeyCode::Enter),
            Command::Activate(Action::Providers)
        );
        assert_eq!(
            menu.key(KeyCode::F(4)),
            Command::Activate(Action::Providers)
        );
        assert_eq!(
            menu.key(KeyCode::Char('4')),
            Command::Activate(Action::Providers)
        );
        assert_eq!(menu.key(KeyCode::Esc), Command::Dismiss);
    }

    #[test]
    fn standalone_history_can_open_home_without_subscription_providers() {
        let mut menu = Menu::new(Action::History);
        assert_eq!(menu.key(KeyCode::Enter), Command::Activate(Action::History));
        assert_eq!(menu.key(KeyCode::F(1)), Command::Activate(Action::Home));
        menu.key(KeyCode::Up);
        assert_eq!(menu.key(KeyCode::Enter), Command::Activate(Action::Home));
        menu.key(KeyCode::Up);
        assert_eq!(menu.key(KeyCode::Enter), Command::Activate(Action::Quit));
        menu.key(KeyCode::Down);
        assert_eq!(menu.key(KeyCode::Enter), Command::Activate(Action::Home));
        menu.key(KeyCode::Down);
        assert_eq!(menu.key(KeyCode::Enter), Command::Activate(Action::History));
    }

    #[test]
    fn add_is_a_shortcut_under_provider_management_not_a_duplicate_menu_item() {
        let mut menu = Menu::new(Action::Home);
        assert!(!menu.actions.contains(&Action::AddProvider));
        assert_eq!(
            menu.actions
                .iter()
                .filter(|&&a| a == Action::Providers)
                .count(),
            1
        );
        assert_eq!(
            menu.key(KeyCode::F(5)),
            Command::Activate(Action::AddProvider)
        );
    }

    #[test]
    fn account_management_is_visible_on_wide_and_narrow_menus() {
        let menu = Menu::new(Action::Providers);
        for width in [32, 80] {
            let mut terminal = Terminal::new(TestBackend::new(width, 18)).unwrap();
            terminal.draw(|frame| menu.render(frame)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            let compact = text.split_whitespace().collect::<String>();
            assert!(compact.contains("功能菜单"), "{text}");
            assert!(compact.contains("Provider管理"), "{text}");
            assert!(compact.contains("BYOK"), "{text}");
            assert!(compact.contains("Switch"), "{text}");
        }
    }
}
