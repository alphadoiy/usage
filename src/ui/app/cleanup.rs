//! Session cleanup dialog and background mutations, independent of provider actions.
use super::*;

#[cfg(test)]
mod tests;
use crate::session_cleanup::{Entry, policy};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    text::Line,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, StatefulWidget, Widget, Wrap},
};

#[derive(Clone, Debug)]
pub(super) enum Action {
    Delete(Vec<sessions::Session>),
    Restore(String),
    Purge(Vec<String>),
}

#[derive(Clone, Debug)]
pub(super) enum SmartPlan {
    Ready(policy::Plan),
    Failed(String),
}

#[derive(Debug)]
pub(super) enum Mode {
    Loading,
    List,
    Confirm(Action),
    Working,
    Result(String),
    Smart(SmartPlan),
}

pub(super) struct Panel {
    entries: Vec<Entry>,
    selected: usize,
    mode: Mode,
    trash: bool,
    task: Option<Task>,
}

impl Panel {
    pub(super) fn shortcuts(&self) -> Option<crate::ui::shortcuts::Menu> {
        use crate::ui::shortcuts::{Action, Menu};
        if !matches!(self.mode, Mode::List) {
            return None;
        }
        Some(Menu::new(
            "回收站",
            "↑/↓ 选择会话 · Esc 返回会话页",
            vec![
                Action::new(KeyCode::Enter, "Enter / u", "恢复选中会话"),
                Action::new(KeyCode::Delete, "Delete / d", "永久删除选中会话（需确认）"),
                Action::new(KeyCode::Char('e'), "e", "清空回收站（需确认）"),
            ],
        ))
    }
    fn delete(sessions: Vec<sessions::Session>) -> Self {
        Self {
            entries: Vec::new(),
            selected: 0,
            mode: Mode::Confirm(Action::Delete(sessions)),
            trash: false,
            task: None,
        }
    }

    pub fn render(&self, frame: &mut Frame, trash_path: &std::path::Path) {
        let area = frame.area();
        let width = area.width.min(94);
        let height = area.height.min(22);
        let modal = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        crate::ui::widgets::clear_popup_edges(modal, frame.buffer_mut());
        Clear.render(modal, frame.buffer_mut());
        let block = Block::bordered()
            .title(" 会话清理 · 回收站 ")
            .style(Style::new().bg(Color::Rgb(22, 22, 24)).fg(Color::White))
            .border_style(Color::Yellow);
        let inner = block.inner(modal);
        block.render(modal, frame.buffer_mut());
        if matches!(self.mode, Mode::List) {
            let [list, hint] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(3)]).areas(inner);
            let items: Vec<_> = self
                .entries
                .iter()
                .map(|entry| {
                    ListItem::new(vec![
                        Line::from(format!(
                            "{} · {}",
                            crate::ui::overview::clean(&entry.title),
                            crate::ui::overview::clean(&entry.instance)
                        )),
                        Line::from(format!(
                            "{} · {} KiB · {}",
                            entry.provider,
                            entry.bytes.div_ceil(1024),
                            crate::ui::overview::clean(&entry.cwd.to_string_lossy())
                        ))
                        .dim(),
                    ])
                })
                .collect();
            if items.is_empty() {
                Paragraph::new("回收站为空").render(list, frame.buffer_mut());
            } else {
                let mut state = ListState::default().with_selected(Some(self.selected));
                StatefulWidget::render(
                    List::new(items)
                        .highlight_symbol("› ")
                        .highlight_style(Style::new().fg(Color::LightBlue)),
                    list,
                    frame.buffer_mut(),
                    &mut state,
                );
            }
            Paragraph::new(format!(
                "↑/↓ 选择 · Enter 恢复 · d 删除 · ? 更多 · Esc 返回\n不会自动清空回收站\n{}",
                trash_path.display()
            ))
            .dim()
            .render(hint, frame.buffer_mut());
            return;
        }
        let lines = match &self.mode {
            Mode::Loading => vec!["正在分析会话…".into()],
            Mode::Working => vec!["正在处理会话，请等待…".into()],
            Mode::Result(message) => vec![message.clone()],
            Mode::Confirm(action) => match action {
                Action::Delete(sessions) => {
                    let mut lines = vec![
                        format!("将 {} 个会话移入回收站，可恢复。", sessions.len()),
                        "".into(),
                    ];
                    lines.extend(sessions.iter().take(5).map(|session| {
                        format!(
                            "• {} · {} · {}",
                            crate::ui::overview::clean(&session.title),
                            crate::ui::overview::clean(&session.instance),
                            crate::ui::overview::clean(&session.cwd.to_string_lossy())
                        )
                    }));
                    if sessions.len() > 5 {
                        lines.push(format!("另 {} 个会话", sessions.len() - 5));
                    }
                    lines.push("".into());
                    lines.push(
                        "最近一分钟有写入或仍在托管运行的会话会跳过；失败项会单独说明。".into(),
                    );
                    lines
                }
                Action::Restore(key) => {
                    let mut lines = vec!["恢复此会话及附属文件？原位置已有文件时会停止。".into()];
                    if let Some(entry) = self.entries.iter().find(|e| e.key == *key) {
                        lines.push(format!(
                            "{} · {}",
                            crate::ui::overview::clean(&entry.title),
                            crate::ui::overview::clean(&entry.instance)
                        ));
                    }
                    lines
                }
                Action::Purge(keys) => {
                    let mut lines = vec![format!(
                        "永久删除 {} 个回收站会话？此操作无法恢复。",
                        keys.len()
                    )];
                    lines.extend(
                        self.entries
                            .iter()
                            .filter(|entry| keys.contains(&entry.key))
                            .take(5)
                            .map(|entry| {
                                format!(
                                    "• {} · {}",
                                    crate::ui::overview::clean(&entry.title),
                                    crate::ui::overview::clean(&entry.instance)
                                )
                            }),
                    );
                    lines
                }
            },
            Mode::Smart(SmartPlan::Ready(plan)) => {
                let empty = plan.count(policy::Reason::Empty);
                let stale = plan.count(policy::Reason::Stale);
                let mut lines = vec![
                    "智能清理预览 · 当前搜索与时间筛选范围".to_string(),
                    format!(
                        "空会话：{empty} 个 · {}：{stale} 个",
                        policy::Reason::Stale.label(plan.stale_days)
                    ),
                    format!(
                        "跳过置顶：{} · 运行中：{} · 无法确认：{}",
                        plan.skipped_pinned,
                        plan.skipped_active,
                        plan.errors.len()
                    ),
                    format!("将 {} 个会话移入回收站，可恢复。", plan.candidates.len()),
                    "执行时仍会复核运行状态与最近写入。".into(),
                    "".into(),
                ];
                lines.extend(plan.candidates.iter().map(|candidate| {
                    format!(
                        "• [{}] {} · {} · {}",
                        candidate.reason.label(plan.stale_days),
                        crate::ui::overview::clean(&candidate.session.title),
                        crate::ui::overview::clean(&candidate.session.instance),
                        crate::ui::overview::clean(&candidate.session.cwd.to_string_lossy())
                    )
                }));
                lines.extend(
                    plan.errors
                        .iter()
                        .map(|error| format!("跳过：{}", crate::ui::overview::clean(error))),
                );
                lines
            }

            Mode::Smart(SmartPlan::Failed(error)) => vec![format!("智能清理分析失败：{error}")],
            Mode::List => unreachable!(),
        };
        let [content, hint] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
        if matches!(self.mode, Mode::Smart(SmartPlan::Ready(_))) {
            let selected = self.selected.min(lines.len().saturating_sub(1));
            let mut state = ListState::default().with_selected(Some(selected));
            StatefulWidget::render(
                List::new(lines)
                    .highlight_symbol("› ")
                    .highlight_style(Style::new().fg(Color::LightBlue)),
                content,
                frame.buffer_mut(),
                &mut state,
            );
        } else {
            Paragraph::new(lines.join("\n"))
                .wrap(Wrap { trim: false })
                .render(content, frame.buffer_mut());
        }
        Paragraph::new(match self.mode {
            Mode::Confirm(_) => "y 确认 · n / Esc 取消",
            Mode::Smart(SmartPlan::Ready(ref plan)) if plan.candidates.is_empty() => {
                "↑/↓ 查看详情 · Esc 返回"
            }
            Mode::Smart(SmartPlan::Ready(_)) => "↑/↓ 查看明细 · y 确认 · n / Esc 取消",
            Mode::Loading if !self.trash => "Esc 取消分析",
            Mode::Result(_) | Mode::Smart(SmartPlan::Failed(_)) => "Enter / Esc 返回",
            _ => "请等待处理完成…",
        })
        .dim()
        .render(hint, frame.buffer_mut());
    }
}

impl Dashboard {
    pub(super) fn open_cleanup_delete(&mut self, project: bool) {
        if self.browser_state.loading {
            return;
        }
        let Some(browser) = &self.browser_state.panel else {
            return;
        };
        if !project && browser.marked.is_empty() && browser.focus() == Focus::Projects {
            self.notice = Some("请先进入会话列表选择会话；D 可清理当前项目列表".into());
            return;
        }
        let sessions = if project {
            browser.project_sessions()
        } else {
            browser.cleanup_selection()
        };
        if sessions.is_empty() {
            self.notice = Some("没有可清理的会话".into());
            return;
        }
        self.cleanup = Some(Panel::delete(sessions));
    }

    pub(super) fn open_smart_cleanup(&mut self, handle: &Handle) {
        if self.browser_state.loading {
            return;
        }
        let Some(browser) = &self.browser_state.panel else {
            return;
        };
        let sessions = browser.visible_sessions();
        if sessions.is_empty() {
            self.notice = Some("没有可清理的会话".into());
            return;
        }
        self.cleanup = Some(Panel {
            entries: Vec::new(),
            selected: 0,
            mode: Mode::Loading,
            trash: false,
            task: None,
        });
        let opens_store = self.cleanup_opens.clone();
        let project_store = self.home_sessions_state.project_store.clone();
        let run_root = self.home_activity.root.clone();
        let tx = self.tx.clone();
        self.cleanup_generation = self.cleanup_generation.wrapping_add(1);
        let generation = self.cleanup_generation;
        let task = Task::blocking(
            handle,
            move |cancel| {
                crate::session_cleanup::service::plan(
                    &sessions,
                    &opens_store,
                    &project_store,
                    &run_root,
                    &cancel,
                )
            },
            move |result| {
                let plan = match result {
                    Ok(plan) => SmartPlan::Ready(plan),
                    Err(error) => SmartPlan::Failed(error),
                };
                let _ = tx.send(AppEvent::SmartCleanupPlanned { generation, plan });
            },
        );
        self.cleanup.as_mut().unwrap().task = Some(task);
    }

    pub(super) fn accept_smart_cleanup(&mut self, generation: u64, plan: SmartPlan) -> bool {
        if generation != self.cleanup_generation {
            return false;
        }
        let Some(panel) = &mut self.cleanup else {
            return false;
        };
        if panel.trash || !matches!(panel.mode, Mode::Loading) || panel.task.is_none() {
            return false;
        }
        panel.task = None;
        panel.selected = 0;
        panel.mode = Mode::Smart(plan);
        true
    }

    pub(super) fn open_trash(&mut self, handle: &Handle) {
        self.cleanup = Some(Panel {
            entries: Vec::new(),
            selected: 0,
            mode: Mode::Loading,
            trash: true,
            task: None,
        });
        let store = self.cleanup_store.clone();
        let tx = self.tx.clone();
        handle.spawn(async move {
            let result = tokio::task::spawn_blocking(move || store.list())
                .await
                .unwrap_or_else(|_| Err("回收站读取任务异常结束".into()));
            let _ = tx.send(AppEvent::CleanupFinished {
                message: None,
                entries: result,
                mutated: false,
            });
        });
    }

    pub(super) fn handle_cleanup_key(&mut self, key: KeyCode, handle: &Handle) -> bool {
        let Some(panel) = &mut self.cleanup else {
            return false;
        };
        match &panel.mode {
            Mode::Working => {}
            Mode::Loading => {
                if !panel.trash && matches!(key, KeyCode::Esc | KeyCode::Char('n' | 'q')) {
                    self.cleanup = None;
                }
            }
            Mode::Result(_) => {
                if matches!(key, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                    if panel.trash {
                        panel.mode = Mode::List;
                    } else {
                        self.cleanup = None;
                    }
                }
            }
            Mode::Smart(SmartPlan::Ready(plan)) => {
                if matches!(key, KeyCode::Esc | KeyCode::Char('n' | 'q')) {
                    self.cleanup = None;
                } else if key == KeyCode::Up {
                    panel.selected = panel.selected.saturating_sub(1);
                } else if key == KeyCode::Down {
                    panel.selected =
                        (panel.selected + 1).min(plan.candidates.len() + plan.errors.len() + 6);
                } else if key == KeyCode::Char('y') && !plan.candidates.is_empty() {
                    let sessions = plan
                        .candidates
                        .iter()
                        .map(|candidate| candidate.session.clone())
                        .collect::<Vec<_>>();
                    panel.mode = Mode::Working;
                    self.run_cleanup(Action::Delete(sessions), handle);
                }
            }
            Mode::Smart(SmartPlan::Failed(_)) => {
                if matches!(key, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                    self.cleanup = None;
                }
            }
            Mode::Confirm(action) => {
                if matches!(key, KeyCode::Esc | KeyCode::Char('n' | 'q')) {
                    if panel.trash {
                        panel.mode = Mode::List;
                    } else {
                        self.cleanup = None;
                    }
                } else if key == KeyCode::Char('y') {
                    let action = action.clone();
                    panel.mode = Mode::Working;
                    self.run_cleanup(action, handle);
                }
            }
            Mode::List => match key {
                KeyCode::Esc | KeyCode::Char('q') => self.cleanup = None,
                KeyCode::Up | KeyCode::Char('k') => {
                    panel.selected = panel.selected.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    panel.selected = (panel.selected + 1).min(panel.entries.len().saturating_sub(1))
                }
                KeyCode::Enter | KeyCode::Char('u') => {
                    if let Some(entry) = panel.entries.get(panel.selected) {
                        panel.mode = Mode::Confirm(Action::Restore(entry.key.clone()));
                    }
                }
                KeyCode::Char('d') | KeyCode::Delete => {
                    if let Some(entry) = panel.entries.get(panel.selected) {
                        panel.mode = Mode::Confirm(Action::Purge(vec![entry.key.clone()]));
                    }
                }
                KeyCode::Char('e' | 'E') if !panel.entries.is_empty() => {
                    panel.mode = Mode::Confirm(Action::Purge(
                        panel.entries.iter().map(|e| e.key.clone()).collect(),
                    ))
                }
                _ => {}
            },
        }
        true
    }

    fn run_cleanup(&mut self, action: Action, handle: &Handle) {
        let store = self.cleanup_store.clone();
        let run_root = self.home_activity.root.clone();
        let tx = self.tx.clone();
        // Once confirmed, a mutation runs to completion even if the dashboard exits.
        handle.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let mut succeeded = 0;
                let mut errors = Vec::new();
                match action {
                    Action::Delete(sessions) => {
                        let active =
                            crate::session_cleanup::service::active_sessions(&run_root, &sessions);
                        match active {
                            Err(error) => errors.push(format!("运行状态读取失败，未清理：{error}")),
                            Ok(active) => {
                                for session in sessions {
                                    let result = if active.contains_key(&session.key()) {
                                        Err("会话仍在托管运行，已跳过".into())
                                    } else {
                                        store.delete(&session).map(|_| ())
                                    };
                                    match result {
                                        Ok(()) => succeeded += 1,
                                        Err(error) => errors.push(format!(
                                            "{}：{error}",
                                            crate::ui::overview::clean(&session.title)
                                        )),
                                    }
                                }
                            }
                        }
                    }
                    Action::Restore(key) => match store.restore(&key) {
                        Ok(()) => succeeded += 1,
                        Err(error) => errors.push(error),
                    },
                    Action::Purge(keys) => {
                        for key in keys {
                            match store.purge(&key) {
                                Ok(()) => succeeded += 1,
                                Err(error) => errors.push(format!("{key}：{error}")),
                            }
                        }
                    }
                }
                let mut message = format!(
                    "成功处理 {succeeded} 个会话 · {} 项失败／跳过",
                    errors.len()
                );
                for error in errors.iter().take(8) {
                    message.push_str(&format!("\n{error}"));
                }
                if errors.len() > 8 {
                    message.push_str(&format!("\n另 {} 项失败／跳过", errors.len() - 8));
                }
                (message, store.list())
            })
            .await
            .unwrap_or_else(|_| {
                (
                    "会话处理任务异常结束，请检查回收站".into(),
                    Err("请重新打开回收站".into()),
                )
            });
            let _ = tx.send(AppEvent::CleanupFinished {
                message: Some(result.0),
                entries: result.1,
                mutated: true,
            });
        });
    }

    pub(super) fn accept_cleanup(
        &mut self,
        message: Option<String>,
        entries: Result<Vec<Entry>, String>,
        mutated: bool,
    ) {
        if let Some(panel) = &mut self.cleanup {
            match entries {
                Ok(entries) => {
                    panel.entries = entries;
                    panel.selected = panel.selected.min(panel.entries.len().saturating_sub(1));
                    panel.mode = message.map_or(Mode::List, Mode::Result);
                }
                Err(error) => {
                    panel.mode = Mode::Result(format!(
                        "{}\n回收站读取失败：{error}",
                        message.unwrap_or_default()
                    ))
                }
            }
        }
        self.cleanup_refresh_due |= mutated;
    }

    pub(super) fn refresh_after_cleanup(&mut self, handle: &Handle) -> bool {
        if !std::mem::take(&mut self.cleanup_refresh_due) {
            return false;
        }
        self.browser_state.task = None;
        self.browser_state.generation = self.browser_state.generation.wrapping_add(1);
        self.browser_state.loading = false;
        if let Some(browser) = &mut self.browser_state.panel {
            browser.marked.clear();
        }
        self.start_browser_load(handle);
        self.home_sessions_state.task = None;
        self.home_sessions_state.generation = self.home_sessions_state.generation.wrapping_add(1);
        self.home_sessions_state.loading = false;
        self.start_home_sessions_load(handle);
        true
    }
}
