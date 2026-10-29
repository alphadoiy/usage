use super::*;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

pub(super) struct Picker {
    pub request: String,
    pub session: sessions::Session,
    pub mode: crate::config::ResumeMode,
    pub home: bool,
    pub choices: Vec<sessions::ResumeChoice>,
    pub loading: bool,
    pub selected: usize,
}

impl Dashboard {
    pub(super) fn open_resume_provider(&mut self, home: bool, handle: &Handle) {
        let browser = if home {
            Some(&self.home_sessions_state.browser)
        } else {
            self.browser_state.panel.as_ref()
        };
        let Some(browser) = browser.filter(|b| b.focus() == Focus::Sessions && !b.launching) else {
            return;
        };
        let Some(session) = browser
            .selected_session()
            .filter(|s| s.provider == sessions::AgentKind::CODEX)
            .cloned()
        else {
            self.notice = Some("请选择一个 Codex 会话".into());
            return;
        };
        let request = uuid::Uuid::new_v4().to_string();
        let root = self.home_activity.root.clone();
        self.resume_provider = Some(Picker {
            request: request.clone(),
            session: session.clone(),
            mode: browser.resume_mode,
            home,
            choices: Vec::new(),
            loading: true,
            selected: 0,
        });
        let tx = self.tx.clone();
        handle.spawn_blocking(move || {
            let result = crate::codex_manager::managed::resume_choices(&root, &session);
            let _ = tx.send(AppEvent::ResumeProviders { request, result });
        });
    }

    pub(super) fn handle_resume_provider_key(&mut self, key: KeyCode, handle: &Handle) -> bool {
        let Some(picker) = &mut self.resume_provider else {
            return false;
        };
        match key {
            KeyCode::Esc => {
                self.resume_provider = None;
            }
            KeyCode::Up | KeyCode::Char('k') => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                picker.selected = (picker.selected + 1).min(picker.choices.len())
            }
            KeyCode::Enter if !picker.loading => {
                let target = picker
                    .selected
                    .checked_sub(1)
                    .and_then(|i| picker.choices.get(i))
                    .map_or(sessions::ResumeTarget::Original, |choice| {
                        choice.target.clone()
                    });
                let intent = sessions::ResumeIntent::new(
                    picker.session.clone(),
                    Some(self.config_path.clone()),
                )
                .with_target(target);
                let plan = match intent.prepare() {
                    Ok(plan) => plan,
                    Err(error) => {
                        if let Some(choice) = picker
                            .selected
                            .checked_sub(1)
                            .and_then(|i| picker.choices.get_mut(i))
                        {
                            choice.unavailable = Some(error);
                        } else {
                            self.notice = Some(error);
                            self.resume_provider = None;
                        }
                        return true;
                    }
                };
                let picker = self.resume_provider.take().unwrap();
                self.launch_resume_plan(plan, picker.mode, picker.home, handle);
            }
            _ => {}
        }
        true
    }
}

pub(super) fn draw(frame: &mut Frame, picker: &Picker) {
    let width = frame.area().width.min(76);
    let height = frame
        .area()
        .height
        .min((picker.choices.len() + 8).min(21) as u16);
    let area = Rect::new(
        frame.area().x + (frame.area().width - width) / 2,
        frame.area().y + (frame.area().height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" 使用 Provider 继续会话 ")
        .title_bottom(" ↑/↓ 选择 · Enter 继续 · Esc 返回 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if picker.loading {
        frame.render_widget(Paragraph::new("正在读取 Provider…"), inner);
        return;
    }
    let reason = if let Some(index) = picker.selected.checked_sub(1) {
        picker
            .choices
            .get(index)
            .and_then(|choice| choice.unavailable.as_deref())
    } else {
        // The default row only shows a common restriction when every alternative
        // has that restriction. Another row's failure must not describe this one.
        picker
            .choices
            .first()
            .and_then(|choice| choice.unavailable.as_deref())
            .filter(|reason| {
                picker
                    .choices
                    .iter()
                    .all(|choice| choice.unavailable.as_deref() == Some(*reason))
            })
    };
    let inner = if let Some(reason) = reason {
        let [reason_area, list_area] =
            Layout::vertical([Constraint::Length(4), Constraint::Fill(1)]).areas(inner);
        frame.render_widget(
            Paragraph::new(reason)
                .style(Style::new().fg(Color::Yellow))
                .wrap(Wrap { trim: false }),
            reason_area,
        );
        list_area
    } else {
        inner
    };
    let mut items = vec![ListItem::new(Line::from("默认恢复方式"))];
    items.extend(picker.choices.iter().map(|choice| {
        ListItem::new(Line::from(format!(
            "{} · {}{}",
            choice.label,
            choice.detail,
            if choice.unavailable.is_some() {
                " · 不可用"
            } else {
                ""
            },
        )))
        .style(if choice.unavailable.is_some() {
            Style::new().fg(Color::DarkGray)
        } else {
            Style::default()
        })
    }));
    let list = List::new(items)
        .highlight_symbol("› ")
        .highlight_style(Style::new().fg(Color::LightBlue).bold());
    frame.render_stateful_widget(
        list,
        inner,
        &mut ListState::default().with_selected(Some(picker.selected)),
    );
}
