use super::*;
use crate::ui::shortcuts::{Action as Shortcut, Decision, Menu};

impl Dashboard {
    /// None passes the key through; Stay/Close consume it; Run reuses normal dispatch.
    pub(super) fn handle_shortcuts_key(&mut self, key: KeyCode) -> Option<Decision> {
        if let Some(menu) = &mut self.shortcuts {
            let decision = menu.key(key);
            if !matches!(decision, Decision::Stay) {
                self.shortcuts = None;
            }
            return Some(decision);
        }
        if key != KeyCode::Char('?')
            || self.reset.is_some()
            || self.provider_action.is_some()
            || self.menu.is_some()
            || self
                .browser_state
                .panel
                .as_ref()
                .is_some_and(|b| b.searching || b.launching)
            || self
                .history_state
                .panel
                .as_ref()
                .is_some_and(|p| p.is_editing())
            || (self.browser_state.panel.is_none()
                && self.history_state.panel.is_none()
                && self.home_sessions_state.browser.searching)
            || self.home_sessions_state.browser.launching
        {
            return None;
        }
        if let Some(panel) = &self.cleanup {
            self.shortcuts = panel.shortcuts();
            return self.shortcuts.as_ref().map(|_| Decision::Stay);
        }
        let action = |key, hint, label| Shortcut::new(KeyCode::Char(key), hint, label);
        let enter = |label| Shortcut::new(KeyCode::Enter, "Enter", label);
        let refresh = action('r', "r", "刷新");
        let (title, navigation, mut actions) = if self.performance.panel.is_some() {
            (
                "性能",
                "↑/↓ 选择或滚动 · ←/→ 切换时间范围 · PgUp/PgDn 翻页",
                vec![enter("查看请求详情"), refresh],
            )
        } else if self.history_state.panel.is_some() {
            (
                "Token 历史",
                "↑ 新日期 / ↓ 旧日期 · ←/→ 按周 · PgUp/PgDn 跳到有记录的日期",
                vec![
                    enter("查看选中日期的会话"),
                    action('v', "v", "切换时间范围"),
                    action('f', "f", "筛选项目"),
                    action('M', "Shift+M", "筛选模型"),
                    action('i', "i", "筛选实例"),
                    action('m', "m", "切换每日 / 每月"),
                    Shortcut::new(KeyCode::Tab, "Tab", "下一个来源筛选"),
                    Shortcut::new(KeyCode::BackTab, "Shift+Tab", "上一个来源筛选"),
                    action('c', "c", "清除所有筛选"),
                    action('t', "t", "回到今天"),
                    action('[', "[", "上一年"),
                    action(']', "]", "下一年"),
                    refresh,
                ],
            )
        } else if self.browser_state.panel.is_some() {
            (
                "会话",
                "↑/↓ 选择 · Tab / Shift+Tab 切换列表 · ← 项目 / → 会话",
                vec![
                    enter("进入列表 / 恢复会话"),
                    action('/', "/", "搜索项目和会话"),
                    action(' ', "Space", "勾选 / 取消当前会话或项目"),
                    action('a', "a", "全选 / 取消当前项目列表"),
                    action('d', "d / Delete", "将选中会话移入回收站"),
                    action('D', "Shift+D", "清理当前项目列表（需确认）"),
                    action('b', "b", "打开回收站"),
                    action('o', "o", "切换最近活动 / Token 排序"),
                    action('R', "Shift+R", "切换当前终端 / Ghostty 分屏恢复"),
                    action('y', "y", "复制会话恢复命令"),
                    action('c', "c", "清除搜索"),
                    refresh,
                ],
            )
        } else {
            match self.home_focus {
                super::super::home::Focus::Providers => {
                    let mut actions = vec![
                        enter("展开 / 收起模型列表"),
                        action('a', "a", "添加 Provider"),
                        action('v', "v", "查看性能"),
                        action('x', "x", "查询 / 主动重置额度"),
                        action('c', "c", "签到"),
                    ];
                    if crate::ui::overview::selected(
                        self.overview_state.catalog.as_ref(),
                        self.home_provider.as_deref(),
                    )
                    .is_some_and(|(_, p)| crate::ui::subscription_tests::supports(p))
                    {
                        actions.push(action('t', "t", "订阅测试"));
                    }
                    actions.push(Shortcut::new(KeyCode::F(4), "F4", "管理 Provider"));
                    actions.push(refresh);
                    (
                        "Provider",
                        "Tab / Shift+Tab 切换看板 · ←/→ 切换 Provider · ↑/↓ 滚动",
                        actions,
                    )
                }
                super::super::home::Focus::Connections => (
                    "厂商状态",
                    "Tab / Shift+Tab 切换看板 · ↑/↓ 选择厂商或滚动详情",
                    vec![enter("查看事件与历史"), refresh],
                ),
                super::super::home::Focus::Projects => (
                    "项目与会话",
                    "Tab / Shift+Tab 切换看板 · ↑/↓ 选择 · ← 项目 / → 会话",
                    vec![
                        enter("进入列表 / 恢复会话"),
                        action('/', "/", "搜索项目和会话"),
                        action('c', "c", "清除搜索"),
                        action('R', "Shift+R", "切换会话恢复方式"),
                        Shortcut::new(KeyCode::F(3), "F3", "打开完整会话页，选择或清理会话"),
                        refresh,
                    ],
                ),
            }
        };
        if self.browser_state.panel.is_none()
            && self.history_state.panel.is_none()
            && self.performance.panel.is_none()
            && self.home_focus == super::super::home::Focus::Providers
        {
            let provider = crate::ui::overview::selected(
                self.overview_state.catalog.as_ref(),
                self.home_provider.as_deref(),
            )
            .map(|(_, p)| p);
            for action in &mut actions {
                let reason = match action.key {
                    KeyCode::Char('v') if provider.is_none_or(|p| p.performance.is_none()) => {
                        Some("此 Provider 未配置性能采集")
                    }
                    KeyCode::Char('x') if provider.is_none_or(|p| p.usage.reset.is_none()) => {
                        Some("此 Provider 不支持主动重置")
                    }
                    KeyCode::Char('c')
                        if provider.is_none_or(|p| {
                            !p.operations.iter().any(|op| op.0.id() == "checkin")
                        }) =>
                    {
                        Some("此 Provider 不支持签到")
                    }
                    _ => None,
                };
                if let Some(reason) = reason {
                    *action = action.disabled(reason);
                }
            }
        }
        if self.performance.panel.is_none() {
            actions.push(Shortcut::new(
                KeyCode::F(10),
                "F10",
                "页面菜单（F1–F4 可直接切换）",
            ));
        }
        actions.push(Shortcut::new(KeyCode::Esc, "Esc", "返回 / 收起当前层级"));
        self.shortcuts = Some(Menu::new(title, navigation, actions));
        Some(Decision::Stay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn app() -> Dashboard {
        let mut app = Dashboard::new(
            Vec::new(),
            isolated_config(),
            "/nonexistent/shortcuts.toml".into(),
        );
        app.browser_state.panel = Some(SessionBrowser::new(vec![sessions::Session {
            agent: None,
            provider: sessions::AgentKind::CODEX,
            instance: "codex".into(),
            id: "123e4567-e89b-12d3-a456-426614174000".into(),
            cwd: "/synthetic/project".into(),
            title: "Test session".into(),
            recency_at_ms: 1,
            home: "/synthetic/codex".into(),
        }]));
        app
    }

    #[tokio::test]
    async fn action_menu_uses_normal_dispatch_and_preserves_confirmation() {
        let mut app = app();
        let handle = Handle::current();
        assert!(matches!(
            app.handle_shortcuts_key(KeyCode::Char('?')),
            Some(Decision::Stay)
        ));
        for ch in "排序".chars() {
            app.handle_shortcuts_key(KeyCode::Char(ch));
        }
        let Some(Decision::Run(key)) = app.handle_shortcuts_key(KeyCode::Enter) else {
            panic!("sort action must dispatch")
        };
        app.handle_browser_key(key, &handle);
        assert!(app.browser_state.panel.as_ref().unwrap().sort_tokens);
        assert!(app.shortcuts.is_none());
        app.handle_browser_key(KeyCode::Right, &handle);
        app.handle_shortcuts_key(KeyCode::Char('?'));
        for ch in "移入回收站".chars() {
            app.handle_shortcuts_key(KeyCode::Char(ch));
        }
        let Some(Decision::Run(key)) = app.handle_shortcuts_key(KeyCode::Enter) else {
            panic!("delete action must dispatch")
        };
        app.handle_browser_key(key, &handle);
        assert!(
            app.cleanup.is_some(),
            "menu must open confirmation rather than deleting files"
        );
        app.handle_cleanup_key(KeyCode::Esc, &handle);
        assert!(app.cleanup.is_none());
        app.handle_shortcuts_key(KeyCode::Char('?'));
        app.handle_shortcuts_key(KeyCode::End);
        assert!(
            matches!(
                app.handle_shortcuts_key(KeyCode::Enter),
                Some(Decision::Run(KeyCode::Esc))
            ),
            "the return action must dispatch Escape, not merely close the menu"
        );
    }

    #[tokio::test]
    async fn search_input_owns_text_and_escape_unwinds_search_selection_and_lists() {
        let mut app = app();
        let handle = Handle::current();
        app.handle_browser_key(KeyCode::BackTab, &handle);
        assert_eq!(
            app.browser_state.panel.as_ref().unwrap().focus(),
            Focus::Sessions
        );
        app.handle_browser_key(KeyCode::Char('a'), &handle);
        assert_eq!(app.browser_state.panel.as_ref().unwrap().marked.len(), 1);
        app.handle_browser_key(KeyCode::Char('/'), &handle);
        assert!(app.handle_shortcuts_key(KeyCode::Char('?')).is_none());
        app.handle_browser_key(KeyCode::Char('?'), &handle);
        assert_eq!(app.browser_state.panel.as_ref().unwrap().query, "?");
        app.handle_browser_key(KeyCode::Esc, &handle);
        assert!(app.browser_state.panel.as_ref().unwrap().query.is_empty());
        assert!(!app.browser_state.panel.as_ref().unwrap().searching);
        app.handle_browser_key(KeyCode::Esc, &handle);
        assert!(app.browser_state.panel.as_ref().unwrap().marked.is_empty());
        assert_eq!(
            app.browser_state.panel.as_ref().unwrap().focus(),
            Focus::Sessions
        );
        app.handle_browser_key(KeyCode::Esc, &handle);
        assert_eq!(
            app.browser_state.panel.as_ref().unwrap().focus(),
            Focus::Projects
        );
        app.handle_browser_key(KeyCode::Esc, &handle);
        assert!(app.browser_state.panel.is_none());
    }

    #[test]
    fn action_menus_render_at_narrow_sizes_and_function_navigation_is_preserved() {
        let mut app = app();
        app.handle_shortcuts_key(KeyCode::Char('?'));
        for (width, height) in [(120, 30), (60, 18), (24, 7), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            if width == 120 {
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                let text = text.split_whitespace().collect::<String>();
                assert!(text.contains("操作") && text.contains("回收站") && text.contains("排序"));
            }
        }
        assert!(matches!(
            app.handle_shortcuts_key(KeyCode::Esc),
            Some(Decision::Close)
        ));
        for (key, action) in [
            (1, MenuAction::Home),
            (2, MenuAction::History),
            (3, MenuAction::Sessions),
            (4, MenuAction::Providers),
        ] {
            assert_eq!(
                app.handle_navigation_key(KeyCode::F(key)),
                Some(MenuCommand::Activate(action))
            );
        }
    }
}
