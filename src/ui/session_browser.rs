use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, StatefulWidget, Widget};
use unicode_width::UnicodeWidthChar;

use crate::sessions::{AgentKind, Session};

const BLUE: Color = Color::Rgb(110, 168, 254);
const DIM: Color = Color::Rgb(142, 142, 147);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Projects,
    Sessions,
}

struct Project {
    cwd: PathBuf,
    sessions: Vec<usize>,
    latest_ms: i64,
}

pub struct SessionBrowser {
    pub sessions: Vec<Session>,
    kinds: Vec<AgentKind>,
    projects: Vec<Project>,
    project_selected: usize,
    session_selected: usize,
    focus: Focus,
    pub query: String,
    pub searching: bool,
    pub launching: bool,
}

impl SessionBrowser {
    pub fn new(sessions: Vec<Session>) -> Self {
        let mut browser = Self {
            sessions,
            kinds: Vec::new(),
            projects: Vec::new(),
            project_selected: 0,
            session_selected: 0,
            focus: Focus::Projects,
            query: String::new(),
            searching: false,
            launching: false,
        };
        browser.rebuild();
        browser
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn focus_sessions(&mut self) {
        if !self.projects.is_empty() {
            self.focus = Focus::Sessions;
        }
    }

    pub fn focus_projects(&mut self) {
        self.focus = Focus::Projects;
    }

    pub fn toggle_focus(&mut self) {
        match self.focus {
            Focus::Projects => self.focus_sessions(),
            Focus::Sessions => self.focus_projects(),
        }
    }

    pub fn selected_session(&self) -> Option<&Session> {
        let project = self.projects.get(self.project_selected)?;
        self.sessions
            .get(*project.sessions.get(self.session_selected)?)
    }

    pub fn visible_count(&self) -> usize {
        self.projects
            .iter()
            .map(|project| project.sessions.len())
            .sum()
    }

    pub fn project_count(&self) -> usize {
        self.projects.len()
    }

    pub fn selected_project(&self) -> Option<&PathBuf> {
        self.projects.get(self.project_selected).map(|p| &p.cwd)
    }

    pub fn move_by(&mut self, delta: isize) {
        match self.focus {
            Focus::Projects if !self.projects.is_empty() => {
                let next = (self.project_selected as isize + delta)
                    .clamp(0, self.projects.len() as isize - 1) as usize;
                if next != self.project_selected {
                    self.project_selected = next;
                    self.session_selected = 0;
                }
            }
            Focus::Sessions => {
                let count = self
                    .projects
                    .get(self.project_selected)
                    .map_or(0, |project| project.sessions.len());
                if count > 0 {
                    self.session_selected = (self.session_selected as isize + delta)
                        .clamp(0, count as isize - 1)
                        as usize;
                }
            }
            Focus::Projects => {}
        }
    }

    pub fn type_char(&mut self, ch: char) {
        self.query.push(ch);
        self.rebuild();
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.rebuild();
    }

    pub fn clear_search(&mut self) {
        self.query.clear();
        self.searching = false;
        self.rebuild();
    }

    pub fn refresh(&mut self, sessions: Vec<Session>) {
        let previous_project = self.selected_project().cloned();
        let previous_session = self
            .selected_session()
            .map(|session| (session.provider, session.id.clone()));
        self.sessions = sessions;
        self.launching = false;
        self.rebuild_with_selection(previous_project, previous_session);
    }

    fn rebuild(&mut self) {
        let previous_project = self.selected_project().cloned();
        let previous_session = self
            .selected_session()
            .map(|session| (session.provider, session.id.clone()));
        self.rebuild_with_selection(previous_project, previous_session);
    }

    fn rebuild_with_selection(
        &mut self,
        previous_project: Option<PathBuf>,
        previous_session: Option<(AgentKind, String)>,
    ) {
        self.kinds = self
            .sessions
            .iter()
            .map(|session| session.provider)
            .collect();
        self.kinds
            .sort_by(|a, b| a.order().cmp(&b.order()).then_with(|| a.id().cmp(b.id())));
        self.kinds.dedup_by(|a, b| a.id() == b.id());
        let needle = self.query.to_lowercase();
        let search_id = needle.chars().count() >= 6;
        let mut groups = HashMap::<PathBuf, Vec<usize>>::new();
        for (index, session) in self.sessions.iter().enumerate() {
            let path_matches = session
                .cwd
                .to_string_lossy()
                .to_lowercase()
                .contains(&needle);
            let matches = needle.is_empty()
                || path_matches
                || session.title.to_lowercase().contains(&needle)
                || session.provider.name().to_lowercase().contains(&needle)
                || (search_id && session.id.to_lowercase().contains(&needle));
            if matches {
                groups.entry(session.cwd.clone()).or_default().push(index);
            }
        }
        self.projects = groups
            .into_iter()
            .map(|(cwd, mut sessions)| {
                let latest_ms = sessions
                    .iter()
                    .map(|&index| self.sessions[index].recency_at_ms)
                    .max()
                    .unwrap_or(0);
                sessions.sort_by(|&a, &b| {
                    self.sessions[a]
                        .provider
                        .order()
                        .cmp(&self.sessions[b].provider.order())
                        .then_with(|| {
                            self.sessions[a]
                                .provider
                                .id()
                                .cmp(self.sessions[b].provider.id())
                        })
                        .then_with(|| {
                            self.sessions[b]
                                .recency_at_ms
                                .cmp(&self.sessions[a].recency_at_ms)
                        })
                });
                Project {
                    cwd,
                    sessions,
                    latest_ms,
                }
            })
            .collect();
        self.projects.sort_by(|a, b| {
            b.latest_ms
                .cmp(&a.latest_ms)
                .then_with(|| a.cwd.cmp(&b.cwd))
        });
        self.project_selected = previous_project
            .and_then(|cwd| self.projects.iter().position(|project| project.cwd == cwd))
            .unwrap_or(0);
        self.session_selected = previous_session
            .and_then(|(provider, id)| {
                self.projects
                    .get(self.project_selected)?
                    .sessions
                    .iter()
                    .position(|&index| {
                        self.sessions[index].provider == provider && self.sessions[index].id == id
                    })
            })
            .unwrap_or(0);
    }
}

fn agent_color(kind: AgentKind) -> Color {
    let (red, green, blue) = kind.accent();
    Color::Rgb(red, green, blue)
}

pub fn render(area: Rect, buf: &mut Buffer, browser: &SessionBrowser) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let preview_height = if area.height >= 12 { 4 } else { 0 };
    let [search_area, lists_area, preview_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(preview_height),
    ])
    .areas(area);
    let cursor = if browser.searching { "▏" } else { "" };
    let search = if browser.query.is_empty() && !browser.searching {
        " / 搜索项目、标题、工具或会话 ID".to_string()
    } else {
        format!(" 搜索：{}{cursor}", browser.query)
    };
    Paragraph::new(search)
        .style(Style::new().fg(DIM))
        .render(search_area, buf);

    let [project_area, session_area] = if lists_area.width >= 72 {
        Layout::horizontal([Constraint::Percentage(34), Constraint::Fill(1)]).areas(lists_area)
    } else {
        Layout::vertical([
            Constraint::Length(lists_area.height.min(8)),
            Constraint::Fill(1),
        ])
        .areas(lists_area)
    };
    render_projects(project_area, buf, browser);
    render_sessions(session_area, buf, browser);
    if preview_height > 0 {
        render_preview(preview_area, buf, browser);
    }
}

fn render_preview(area: Rect, buf: &mut Buffer, browser: &SessionBrowser) {
    let Some(session) = browser.selected_session() else {
        return;
    };
    let block = Block::bordered()
        .title(format!(
            " 恢复预览 · {} · Ghostty 右侧分屏 ",
            session.provider.name()
        ))
        .border_style(DIM);
    let inner = block.inner(area);
    block.render(area, buf);
    let width = usize::from(inner.width);
    let lines = vec![
        Line::from(preview_text(
            &format!("目录  {}", session.cwd.display()),
            width,
        )),
        Line::from(preview_text(
            &format!("运行  {}", session.provider.resume_command(&session.id)),
            width,
        )),
    ];
    Paragraph::new(lines)
        .style(Style::new().fg(DIM))
        .render(inner, buf);
}

fn preview_text(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let value = clean(value);
    if value
        .chars()
        .map(|ch| ch.width().unwrap_or(0))
        .sum::<usize>()
        <= width
    {
        return value;
    }
    let mut short = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let char_width = ch.width().unwrap_or(0);
        if used + char_width + 1 > width {
            break;
        }
        short.push(ch);
        used += char_width;
    }
    short.push('…');
    short
}

fn render_projects(area: Rect, buf: &mut Buffer, browser: &SessionBrowser) {
    let active = browser.focus == Focus::Projects;
    let block = Block::bordered()
        .title(format!(" 项目 · {} ", browser.project_count()))
        .border_style(if active { BLUE } else { DIM });
    let inner = block.inner(area);
    block.render(area, buf);
    if browser.projects.is_empty() {
        Paragraph::new("没有匹配的项目")
            .style(Style::new().fg(DIM))
            .render(inner, buf);
        return;
    }
    let items = browser
        .projects
        .iter()
        .map(|project| {
            let name = project.cwd.file_name().map_or_else(
                || project.cwd.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
            ListItem::new(vec![
                Line::from(format!("{}  ({})", clean(&name), project.sessions.len())),
                Line::from(clean(&project.cwd.to_string_lossy())).dim(),
            ])
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(browser.project_selected));
    StatefulWidget::render(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::new().fg(BLUE).bold()),
        inner,
        buf,
        &mut state,
    );
}

fn render_sessions(area: Rect, buf: &mut Buffer, browser: &SessionBrowser) {
    let active = browser.focus == Focus::Sessions;
    let title = browser.selected_project().map_or_else(
        || "会话".to_string(),
        |path| {
            let name = path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
            format!(
                "{} · {} 个会话",
                clean(&name),
                browser.projects[browser.project_selected].sessions.len()
            )
        },
    );
    let block = Block::bordered()
        .title(format!(" {title} "))
        .border_style(if active { BLUE } else { DIM });
    let inner = block.inner(area);
    block.render(area, buf);
    let Some(project) = browser.projects.get(browser.project_selected) else {
        return;
    };
    let mut items = Vec::new();
    let mut selected_display = None;
    let mut session_index = 0;
    let mut offset = 0;
    for &kind in &browser.kinds {
        let count = project.sessions[offset..]
            .iter()
            .take_while(|&&index| browser.sessions[index].provider == kind)
            .count();
        if count == 0 {
            continue;
        }
        items.push(
            ListItem::new(format!("{} · {count}", kind.name()))
                .style(Style::new().fg(agent_color(kind)).bold()),
        );
        for &index in &project.sessions[offset..offset + count] {
            let session = &browser.sessions[index];
            if session_index == browser.session_selected {
                selected_display = Some(items.len());
            }
            let time = chrono::DateTime::from_timestamp_millis(session.recency_at_ms)
                .map(|time| {
                    time.with_timezone(&chrono::Local)
                        .format("%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!("{time}  "), Style::new().fg(DIM)),
                Span::raw(clean(&session.title)),
            ])));
            session_index += 1;
        }
        offset += count;
    }
    let mut state = ListState::default().with_selected(selected_display);
    StatefulWidget::render(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::new().fg(BLUE).bold()),
        inner,
        buf,
        &mut state,
    );
}

fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(provider: AgentKind, title: &str, cwd: &str, recency: i64) -> Session {
        Session {
            provider,
            id: format!("123e4567-e89b-12d3-a456-{recency:012}"),
            cwd: cwd.into(),
            title: title.into(),
            recency_at_ms: recency,
            home: "/tmp".into(),
        }
    }

    #[test]
    fn projects_contain_provider_groups_and_navigation() {
        let mut browser = SessionBrowser::new(vec![
            session(AgentKind::GROK, "Analyze logs", "/tmp/beta", 20),
            session(AgentKind::CODEX, "Review UI", "/tmp/beta", 30),
            session(AgentKind::CLAUDE, "Fix parser", "/tmp/beta", 10),
            session(AgentKind::CODEX, "Other", "/tmp/alpha", 40),
        ]);
        assert_eq!(browser.project_count(), 2);
        assert_eq!(
            browser.selected_project().unwrap(),
            &PathBuf::from("/tmp/alpha")
        );
        browser.move_by(1);
        assert_eq!(
            browser.selected_project().unwrap(),
            &PathBuf::from("/tmp/beta")
        );
        browser.focus_sessions();
        assert_eq!(
            browser.selected_session().unwrap().provider,
            AgentKind::CLAUDE
        );
        browser.move_by(1);
        assert_eq!(
            browser.selected_session().unwrap().provider,
            AgentKind::CODEX
        );
        browser.move_by(1);
        assert_eq!(
            browser.selected_session().unwrap().provider,
            AgentKind::GROK
        );
    }

    #[test]
    fn search_filters_projects_and_sessions() {
        let mut browser = SessionBrowser::new(vec![
            session(AgentKind::CLAUDE, "Fix parser", "/tmp/alpha", 10),
            session(AgentKind::CODEX, "Review UI", "/tmp/beta", 20),
            session(AgentKind::GROK, "Analyze logs", "/tmp/beta", 30),
        ]);
        browser.type_char('R');
        browser.type_char('e');
        assert_eq!(browser.project_count(), 1);
        assert_eq!(browser.visible_count(), 1);
        assert_eq!(browser.selected_session().unwrap().title, "Review UI");
        browser.clear_search();
        assert_eq!(browser.project_count(), 2);
        browser.type_char('b');
        assert_eq!(browser.visible_count(), 2);
    }

    #[test]
    fn refresh_keeps_selected_session_after_source_reorders() {
        let mut sessions = vec![
            session(AgentKind::CLAUDE, "Claude task", "/tmp/beta", 10),
            session(AgentKind::CODEX, "Codex task", "/tmp/beta", 20),
            session(AgentKind::GROK, "Grok task", "/tmp/beta", 30),
        ];
        let mut browser = SessionBrowser::new(sessions.clone());
        browser.focus_sessions();
        browser.move_by(1);
        let selected = browser.selected_session().unwrap().id.clone();
        sessions.rotate_left(1);
        browser.refresh(sessions);
        assert_eq!(browser.selected_session().unwrap().id, selected);
        assert_eq!(browser.focus(), Focus::Sessions);
    }

    #[test]
    fn renders_two_levels_at_narrow_and_wide_widths() {
        let browser = SessionBrowser::new(vec![
            session(AgentKind::GROK, "Analyze logs", "/tmp/beta", 20),
            session(AgentKind::CODEX, "Review UI", "/tmp/beta", 30),
            session(AgentKind::CLAUDE, "Fix parser", "/tmp/beta", 10),
        ]);
        for width in [44, 100] {
            let area = Rect::new(0, 0, width, 22);
            let mut buffer = Buffer::empty(area);
            render(area, &mut buffer, &browser);
            let text = (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("beta  (3)"), "{text}");
            assert!(text.contains("Claude Code"), "{text}");
            assert!(text.contains("Codex"), "{text}");
            assert!(text.contains("Grok"), "{text}");
            let compact = text.replace(' ', "");
            assert!(compact.contains("恢复预览"), "{text}");
            assert!(compact.contains("Ghostty右侧分屏"), "{text}");
            assert!(compact.contains("目录"), "{text}");
            assert!(compact.contains("运行"), "{text}");
        }
    }

    #[test]
    fn preview_tracks_selected_session_and_truncates_to_width() {
        let mut browser = SessionBrowser::new(vec![
            session(AgentKind::CLAUDE, "First", "/tmp/a project", 10),
            session(AgentKind::CODEX, "Second", "/tmp/a project", 20),
        ]);
        browser.focus_sessions();
        browser.move_by(1);
        let area = Rect::new(0, 0, 44, 20);
        let mut buffer = Buffer::empty(area);
        render(area, &mut buffer, &browser);
        let rows = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(
            rows.iter()
                .any(|row| row.replace(' ', "").contains("Codex·Ghostty右侧分屏"))
        );
        assert!(
            rows.iter()
                .any(|row| row.replace(' ', "").contains("codexresume"))
        );
        assert!(
            unicode_width::UnicodeWidthStr::width(preview_text("目录  /tmp/中文项目", 12).as_str())
                <= 12
        );
    }

    #[test]
    fn registered_provider_metadata_drives_grouping() {
        let custom = AgentKind::new(
            "custom",
            "Custom Agent",
            Some((180, 120, 210)),
            "custom resume",
            "CUSTOM_HOME",
            3,
        );
        assert_eq!(agent_color(custom), Color::Rgb(180, 120, 210));
        let mut browser = SessionBrowser::new(vec![
            session(custom, "Custom task", "/tmp/beta", 30),
            session(AgentKind::CODEX, "Codex task", "/tmp/beta", 20),
        ]);
        browser.focus_sessions();
        assert_eq!(
            browser.selected_session().unwrap().provider,
            AgentKind::CODEX
        );
        browser.move_by(1);
        assert_eq!(browser.selected_session().unwrap().provider, custom);
        let area = Rect::new(0, 0, 90, 15);
        let mut buffer = Buffer::empty(area);
        render(area, &mut buffer, &browser);
        let text = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Custom Agent"), "{text}");
    }
}
