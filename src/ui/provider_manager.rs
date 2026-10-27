//! Provider management. Accounts and Agent configuration retain their existing stores.
use super::forms::{edit_input, expand_path, modal};
use crate::codex_manager::{self, AuthMode, Manager, Profile};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Widget, Wrap};
use std::{
    io,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};
use tokio::runtime::Handle;

enum Prompt {
    Project(Box<ProjectPicker>),
    Remove(String),
}
/// `launch` holds the account to start once a project is chosen; None only changes the default.
struct ProjectPicker {
    query: String,
    selected: usize,
    launch: Option<Profile>,
    /// Result of the last add / pin / remove, shown inside the picker.
    status: String,
}
impl ProjectPicker {
    fn prompt(launch: Option<Profile>) -> Prompt {
        Prompt::Project(Box::new(Self {
            query: String::new(),
            selected: 0,
            launch,
            status: String::new(),
        }))
    }
}

/// The project store merged with session directories, which load once in the background.
struct Projects {
    store: crate::projects::Store,
    sessions: Option<Vec<(PathBuf, i64)>>,
    records: Vec<crate::projects::Record>,
    list: Vec<crate::projects::Project>,
}
impl Projects {
    fn loaded(
        &mut self,
        sessions: Vec<(PathBuf, i64)>,
        records: Result<Vec<crate::projects::Record>, String>,
    ) -> Option<String> {
        self.sessions = Some(sessions);
        let error = records.map(|records| self.records = records).err();
        self.recompute();
        error
    }
    /// Store changes apply immediately; session directories are reused.
    fn reload(&mut self) -> Result<(), String> {
        self.records = self.store.list()?;
        self.recompute();
        Ok(())
    }
    fn recompute(&mut self) {
        self.list = crate::projects::catalog(
            &self.records,
            self.sessions
                .iter()
                .flatten()
                .map(|(path, at)| (path.as_path(), *at)),
        );
    }
    fn get(&self, path: &Path) -> Option<&crate::projects::Project> {
        self.list.iter().find(|p| p.path == path)
    }
    /// A launch in a directory not yet listed adds it, so new projects persist after first use.
    fn launched(&mut self, path: &Path) -> Result<(), String> {
        if self.get(path).is_none() {
            self.store.add(path)?;
        }
        self.store.record_launch(path)?;
        self.reload()
    }
}

/// Session directories for the project list: sessions the caller already loaded, or a scan.
pub enum ProjectSessions {
    Loaded(Vec<(PathBuf, i64)>),
    Scan(
        crate::sessions::SessionRegistry,
        crate::sessions::SessionHomes,
    ),
}
impl ProjectSessions {
    pub fn from_sessions(sessions: &[crate::sessions::Session]) -> Self {
        Self::Loaded(
            sessions
                .iter()
                .map(|s| (s.cwd.clone(), s.recency_at_ms))
                .collect(),
        )
    }
    fn directories(self) -> Vec<(PathBuf, i64)> {
        match self {
            Self::Loaded(directories) => directories,
            Self::Scan(registry, homes) => {
                Self::from_sessions(&registry.load_all(&homes).sessions).directories()
            }
        }
    }
}
/// A typed absolute or `~` path that exists leads so it is not shadowed by a longer project
/// sharing its prefix; then the default project, then the project list.
fn typed_path(query: &str) -> Option<PathBuf> {
    let query = query.trim();
    (query.starts_with('/') || query.starts_with('~'))
        .then(|| expand_path(query).canonicalize().ok())
        .flatten()
        .filter(|path| path.is_dir())
}
fn project_choices(cwd: &Path, projects: &[crate::projects::Project], query: &str) -> Vec<PathBuf> {
    let needle = query.trim().to_lowercase();
    let mut choices: Vec<PathBuf> = typed_path(query).into_iter().collect();
    for path in std::iter::once(cwd).chain(projects.iter().map(|p| p.path.as_path())) {
        if !choices.iter().any(|c| c == path)
            && (needle.is_empty() || path.to_string_lossy().to_lowercase().contains(&needle))
        {
            choices.push(path.to_path_buf());
        }
    }
    choices
}
fn display_path(path: &Path) -> String {
    match path.strip_prefix(crate::config::home_dir()) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}
fn subscription_account(profile: &Profile) -> bool {
    crate::plugins::endpoint_account(&profile.base_url).is_some()
}
fn identity(profile: &Profile) -> String {
    if subscription_account(profile) {
        "订阅服务 · 共享插件账号".into()
    } else if let Some(oauth) = &profile.oauth {
        format!("ChatGPT OAuth · {}", oauth.label())
    } else {
        format!(
            "{} · BYOK · {}",
            profile.base_url,
            if profile.follow {
                "跟随 CC Switch"
            } else {
                "独立配置"
            }
        )
    }
}
fn actions(profile: Option<&Profile>) -> String {
    let mut text = "? 操作 · ↑/↓ 选择 · a 添加 · r 刷新 · Esc 返回".to_string();
    if let Some(profile) = profile {
        text.push_str(" · Enter 启动 · e 配置");
        if profile.auth_mode == AuthMode::Byok && !subscription_account(profile) {
            text.push_str(" · b 额度设置");
        }
        if profile.auth_mode == AuthMode::Oauth {
            text.push_str(" · l 重新登录");
        }
        if profile.source.is_some() {
            text.push_str(" · f 跟随 · s 同步");
        }
    }
    text
}

fn shortcuts(profile: Option<&Profile>) -> super::shortcuts::Menu {
    use super::shortcuts::{Action, Menu};
    let action = |key, hint, label| Action::new(KeyCode::Char(key), hint, label);
    let mut actions = vec![
        action('a', "a / F5", "添加 Provider"),
        action('i', "i", "从 CC Switch 导入"),
        action('o', "o", "选择默认项目"),
        action('r', "r", "刷新列表"),
    ];
    if let Some(profile) = profile {
        actions.extend([
            Action::new(KeyCode::Enter, "Enter", "选择项目并启动 Agent"),
            action('e', "e", "编辑 Agent 配置"),
            action('E', "Shift+E", "用外部编辑器编辑完整 TOML"),
            action('c', "c", "复制 Provider"),
            action('d', "d / Delete", "移除 Provider（需确认）"),
        ]);
        if profile.auth_mode == AuthMode::Oauth {
            actions.push(action('l', "l", "重新登录 OAuth 账号"));
        }
        if profile.auth_mode == AuthMode::Byok && !subscription_account(profile) {
            actions.push(action('b', "b", "设置 New API 额度换算"));
        }
        if profile.source.is_some() {
            actions.extend([
                action('f', "f", "切换跟随 CC Switch"),
                action('s', "s", "同步 CC Switch"),
            ]);
        }
    }
    Menu::new(
        "Provider 管理",
        "↑/↓ 选择 Provider · Esc 返回上一页",
        actions,
    )
}
fn draw_accounts(
    frame: &mut ratatui::Frame,
    profiles: &[Profile],
    selected: usize,
    cwd: &std::path::Path,
    notice: &str,
) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(3),
    ])
    .areas(frame.area());
    Paragraph::new(Line::from(vec![
        " Provider 管理 ".bold(),
        format!("默认项目：{}", display_path(cwd)).dim(),
    ]))
    .render(header, frame.buffer_mut());
    let items: Vec<ListItem> = profiles
        .iter()
        .map(|p| {
            ListItem::new(vec![
                Line::from(format!("{}  ·  {}", p.name, p.model)),
                Line::from(identity(p)).dim(),
                Line::from(format!("{} {}", p.id, p.source_status)).dim(),
            ])
        })
        .collect();
    if items.is_empty() {
        Paragraph::new("还没有配置 Provider。\n\na / F5 添加 Provider（BYOK / OAuth / WorkBuddy）\ni 从 CC Switch 导入（先预览再保存）\no 选择默认项目目录")
                        .block(Block::bordered()).render(body,frame.buffer_mut());
    } else {
        let mut state = ListState::default().with_selected(Some(selected));
        frame.render_stateful_widget(
            List::new(items)
                .block(Block::bordered())
                .highlight_style(Style::new().bg(Color::Rgb(35, 45, 65)))
                .highlight_symbol("› "),
            body,
            &mut state,
        );
    }
    Paragraph::new(format!(
        "{}\n{notice}",
        super::widgets::fit_hints(&actions(profiles.get(selected)), footer.width)
    ))
    .style(Style::new().fg(Color::Rgb(142, 142, 147)))
    .wrap(Wrap { trim: false })
    .render(footer, frame.buffer_mut());
}

pub fn run_with_config(
    handle: Handle,
    usage_config: Option<PathBuf>,
    sessions: ProjectSessions,
    default_project: Option<PathBuf>,
) -> io::Result<()> {
    let mut manager = Manager::open_default().map_err(io::Error::other)?;
    let mut profiles = manager.list().map_err(io::Error::other)?;
    let mut selected = 0usize;
    let mut cwd = match default_project.filter(|path| path.is_dir()) {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let mut prompt: Option<Prompt> = None;
    let mut shortcuts_menu: Option<super::shortcuts::Menu> = None;
    let mut notice = "选择 Provider · e 配置 Agent · Enter 选择项目并启动 Codex".to_string();
    let mut projects = Projects {
        store: crate::projects::Store::from_environment(),
        sessions: None,
        records: Vec::new(),
        list: Vec::new(),
    };
    let (projects_tx, projects_rx) = mpsc::channel();
    let store = projects.store.clone();
    std::thread::spawn(move || {
        let _ = projects_tx.send((sessions.directories(), store.list()));
    });
    let mut refreshed = Instant::now();
    let mut terminal = crate::ui::terminal::enter().map_err(io::Error::other)?;
    let result = (|| -> io::Result<()> {
        loop {
            if projects.sessions.is_none()
                && let Ok((sessions, records)) = projects_rx.try_recv()
                && let Some(error) = projects.loaded(sessions, records)
            {
                notice = error;
            }
            terminal.draw(|frame| {
                draw_accounts(frame, &profiles, selected, &cwd, &notice);
                if let Some(prompt) = &prompt {
                    draw_prompt(frame, prompt, &cwd, &projects);
                }
                if let Some(menu) = &shortcuts_menu {
                    menu.render(frame);
                }
            })?;
            if !event::poll(Duration::from_millis(100))? {
                if refreshed.elapsed() >= Duration::from_secs(15) && prompt.is_none() {
                    match manager.sync(false) {
                        Ok(report) if !report.updated.is_empty() || report.needs_attention() => {
                            notice = report.to_string()
                        }
                        Err(error) => notice = error.to_string(),
                        _ => {}
                    }
                    reload(&manager, &mut profiles, &mut selected, &mut notice);
                    refreshed = Instant::now();
                }
                continue;
            }
            let Event::Key(mut key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                return Ok(());
            }
            if let Some(menu) = &mut shortcuts_menu {
                match menu.key(key.code) {
                    super::shortcuts::Decision::Run(code) => {
                        shortcuts_menu = None;
                        key.code = code;
                    }
                    super::shortcuts::Decision::Close => {
                        shortcuts_menu = None;
                        continue;
                    }
                    super::shortcuts::Decision::Stay => continue,
                }
            } else if key.code == KeyCode::Char('?') && prompt.is_none() {
                shortcuts_menu = Some(shortcuts(profiles.get(selected)));
                continue;
            }
            if let Some(current) = prompt.take() {
                match current {
                    Prompt::Project(mut picker) => {
                        let choices = project_choices(&cwd, &projects.list, &picker.query);
                        let control = key.modifiers.contains(KeyModifiers::CONTROL);
                        let target = choices.get(picker.selected).cloned();
                        match key.code {
                            KeyCode::Esc => {}
                            KeyCode::Enter => match target {
                                Some(path) => {
                                    cwd = path;
                                    notice = match &picker.launch {
                                        Some(p) => {
                                            let mut notice = launch(
                                                &handle,
                                                &mut terminal,
                                                &manager,
                                                p,
                                                &cwd,
                                                usage_config.as_deref(),
                                            )?;
                                            if let Err(error) = projects.launched(&cwd) {
                                                notice.push_str(&format!(" · 项目未记录：{error}"));
                                            }
                                            notice
                                        }
                                        None => format!("默认项目：{}", display_path(&cwd)),
                                    };
                                    reload(&manager, &mut profiles, &mut selected, &mut notice);
                                }
                                None => {
                                    picker.status = "项目目录不存在".into();
                                    prompt = Some(Prompt::Project(picker));
                                }
                            },
                            KeyCode::Up => {
                                picker.selected = picker.selected.saturating_sub(1);
                                prompt = Some(Prompt::Project(picker));
                            }
                            KeyCode::Down => {
                                picker.selected =
                                    (picker.selected + 1).min(choices.len().saturating_sub(1));
                                prompt = Some(Prompt::Project(picker));
                            }
                            KeyCode::Char('a') if control => {
                                // A typed path is added even when a listed project is selected.
                                picker.status = match typed_path(&picker.query).or(target) {
                                    Some(path) => projects
                                        .store
                                        .add(&path)
                                        .and_then(|path| {
                                            projects.reload()?;
                                            Ok(format!("已添加项目：{}", display_path(&path)))
                                        })
                                        .unwrap_or_else(|e| e),
                                    None => "输入以 / 或 ~ 开头的目录后添加".into(),
                                };
                                picker.query.clear();
                                picker.selected = 0;
                                prompt = Some(Prompt::Project(picker));
                            }
                            KeyCode::Char('t') if control => {
                                if let Some(path) = target {
                                    let pinned = projects.get(&path).is_some_and(|p| p.pinned);
                                    picker.status = projects
                                        .store
                                        .set_pinned(&path, !pinned)
                                        .and_then(|_| projects.reload())
                                        .map(|_| {
                                            format!(
                                                "{}：{}",
                                                if pinned {
                                                    "已取消置顶"
                                                } else {
                                                    "已置顶"
                                                },
                                                display_path(&path)
                                            )
                                        })
                                        .unwrap_or_else(|e| e);
                                }
                                prompt = Some(Prompt::Project(picker));
                            }
                            KeyCode::Char('d') if control => {
                                if let Some(path) = target {
                                    picker.status = projects
                                        .store
                                        .remove(&path)
                                        .and_then(|_| projects.reload())
                                        .map(|_| {
                                            format!(
                                                "已移出项目列表（会话保留）：{}",
                                                display_path(&path)
                                            )
                                        })
                                        .unwrap_or_else(|e| e);
                                    picker.selected = picker.selected.saturating_sub(1);
                                }
                                prompt = Some(Prompt::Project(picker));
                            }
                            code => {
                                if code == KeyCode::Char('u') && control {
                                    picker.query.clear();
                                } else if !control {
                                    edit_input(&mut picker.query, code);
                                }
                                picker.selected = 0;
                                prompt = Some(Prompt::Project(picker));
                            }
                        }
                    }
                    Prompt::Remove(id) => {
                        if key.code == KeyCode::Char('y') {
                            notice = manager
                                .remove(&id)
                                .map(|_| "已移出列表，历史和版本已保留".into())
                                .unwrap_or_else(|e| e.to_string());
                            reload(&manager, &mut profiles, &mut selected, &mut notice);
                        } else if key.code != KeyCode::Esc && key.code != KeyCode::Char('n') {
                            prompt = Some(Prompt::Remove(id));
                        }
                    }
                }
                continue;
            }
            let profile = profiles.get(selected).cloned();
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(profiles.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Char('a') | KeyCode::F(5) => {
                    let result =
                        super::provider_setup::run(handle.clone(), &mut terminal, &|frame| {
                            draw_accounts(frame, &profiles, selected, &cwd, &notice)
                        });
                    apply_setup(result, &manager, &mut profiles, &mut selected, &mut notice);
                }
                KeyCode::Char('l') => {
                    if let Some(profile) = profile {
                        if profile.auth_mode == AuthMode::Oauth {
                            let result = super::provider_setup::relogin(
                                handle.clone(),
                                &mut terminal,
                                &|frame| draw_accounts(frame, &profiles, selected, &cwd, &notice),
                                &profile,
                            );
                            apply_setup(
                                result,
                                &manager,
                                &mut profiles,
                                &mut selected,
                                &mut notice,
                            );
                        } else {
                            notice = if subscription_account(&profile) {
                                "该插件暂不支持原地替换登录；按 a 登录并绑定新账号".into()
                            } else {
                                "BYOK 使用 API Key；按 e 修改配置".into()
                            };
                        }
                    }
                }
                KeyCode::Char('b') => {
                    if let Some(profile) = profile {
                        let path = usage_config
                            .clone()
                            .unwrap_or_else(crate::config::default_config_path);
                        let result = super::provider_setup::edit_quota(
                            &mut terminal,
                            &|frame| draw_accounts(frame, &profiles, selected, &cwd, &notice),
                            &profile,
                            &path,
                        );
                        notice = match result {
                            Ok(true) => "额度换算已保存，返回首页后刷新生效".into(),
                            Ok(false) => "已取消额度设置".into(),
                            Err(error) => error.to_string(),
                        };
                    }
                }
                KeyCode::Char('e') => {
                    if let Some(profile) = profile {
                        let result = super::provider_setup::edit(
                            handle.clone(),
                            &mut terminal,
                            &|frame| draw_accounts(frame, &profiles, selected, &cwd, &notice),
                            &profile,
                        );
                        apply_setup(result, &manager, &mut profiles, &mut selected, &mut notice);
                    }
                }
                KeyCode::Char('E') => {
                    if let Some(p) = profile {
                        match manager.revision(&p.id, p.revision) {
                            Ok(rev) => {
                                crate::ui::terminal::leave();
                                let edited = codex_manager::cli::edit_text(&rev.config);
                                terminal =
                                    crate::ui::terminal::enter().map_err(io::Error::other)?;
                                notice = edited
                                    .and_then(|text| {
                                        manager.update(&p.id, p.revision, &text, None, None)
                                    })
                                    .map(|p| format!("已保存 {}", p.name))
                                    .unwrap_or_else(|e| e.to_string());
                                reload(&manager, &mut profiles, &mut selected, &mut notice);
                            }
                            Err(e) => notice = e.to_string(),
                        }
                    }
                }
                KeyCode::Char('c') => {
                    if let Some(p) = profile {
                        notice = manager
                            .copy(&p.id, &format!("{} 副本", p.name))
                            .map(|p| format!("已复制：{}", p.name))
                            .unwrap_or_else(|e| e.to_string());
                        reload(&manager, &mut profiles, &mut selected, &mut notice);
                    }
                }
                KeyCode::Char('d') | KeyCode::Delete => {
                    if let Some(p) = profile {
                        prompt = Some(Prompt::Remove(p.id));
                    }
                }
                KeyCode::Char('i') => {
                    let result = super::provider_setup::run_native(
                        handle.clone(),
                        &mut terminal,
                        &|frame| draw_accounts(frame, &profiles, selected, &cwd, &notice),
                        super::provider_setup::AddMethod::CcSwitch,
                    );
                    apply_setup(result, &manager, &mut profiles, &mut selected, &mut notice);
                }
                KeyCode::Char('o') => prompt = Some(ProjectPicker::prompt(None)),
                KeyCode::Char('f') => {
                    if let Some(p) = profile {
                        notice = manager
                            .follow(&p.id, !p.follow)
                            .map(|_| {
                                if p.follow {
                                    "已暂停跟随".into()
                                } else {
                                    "已开启跟随".into()
                                }
                            })
                            .unwrap_or_else(|e| e.to_string());
                        reload(&manager, &mut profiles, &mut selected, &mut notice);
                    }
                }
                KeyCode::Char('s') => {
                    notice = manager
                        .sync(false)
                        .map(|r| r.to_string())
                        .unwrap_or_else(|e| e.to_string());
                    reload(&manager, &mut profiles, &mut selected, &mut notice);
                    refreshed = Instant::now();
                }
                KeyCode::Char('r') => reload(&manager, &mut profiles, &mut selected, &mut notice),
                KeyCode::Enter => {
                    if let Some(p) = profile {
                        prompt = Some(ProjectPicker::prompt(Some(p)));
                    }
                }
                _ => {}
            }
        }
    })();
    crate::ui::terminal::leave();
    result
}
fn reload(
    manager: &Manager,
    profiles: &mut Vec<Profile>,
    selected: &mut usize,
    notice: &mut String,
) {
    let id = profiles.get(*selected).map(|p| p.id.clone());
    match manager.list() {
        Ok(new) => {
            *profiles = new;
            *selected = id
                .and_then(|id| profiles.iter().position(|p| p.id == id))
                .unwrap_or((*selected).min(profiles.len().saturating_sub(1)));
        }
        Err(e) => *notice = e.to_string(),
    }
}
fn apply_setup(
    result: io::Result<Option<super::provider_setup::Added>>,
    manager: &Manager,
    profiles: &mut Vec<Profile>,
    selected: &mut usize,
    notice: &mut String,
) {
    match result {
        Ok(Some(added)) => {
            *notice = added.notice;
            reload(manager, profiles, selected, notice);
            if let Some(profile) = added.profile {
                *selected = profiles
                    .iter()
                    .position(|p| p.id == profile.id)
                    .unwrap_or(*selected);
            }
        }
        Ok(None) => {}
        Err(error) => *notice = error.to_string(),
    }
}

/// Start Codex for the account in `cwd`: a Ghostty split when available, otherwise this terminal.
fn launch(
    handle: &Handle,
    terminal: &mut DefaultTerminal,
    manager: &Manager,
    profile: &Profile,
    cwd: &Path,
    usage_config: Option<&Path>,
) -> io::Result<String> {
    if crate::sessions::uses_ghostty(crate::config::ResumeMode::Auto) {
        return Ok(
            codex_manager::launch_shell_command(&profile.id, cwd, None, None)
                .and_then(|command| codex_manager::with_usage_config(command, usage_config))
                .and_then(|command| {
                    crate::sessions::command_in_ghostty(cwd, &command)
                        .map_err(codex_manager::Error::message)
                })
                .map(|_| {
                    format!(
                        "已在右侧打开 Codex（{}）；可继续选择其他账号",
                        display_path(cwd)
                    )
                })
                .unwrap_or_else(|e| e.to_string()),
        );
    }
    crate::ui::terminal::leave();
    let status = {
        let _runtime = handle.enter();
        #[cfg(unix)]
        let _interrupts =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        crate::codex_manager::managed::spawn(
            manager.root(),
            &profile.id,
            None,
            &crate::agents::SpawnRequest {
                cwd: cwd.to_path_buf(),
                program: "codex".into(),
                args: Vec::new(),
                resume: None,
                usage_config: usage_config.map(Path::to_path_buf),
            },
        )
        .map(|outcome| outcome.status)
    };
    *terminal = crate::ui::terminal::enter().map_err(io::Error::other)?;
    Ok(status
        .map(|s| format!("Codex 已退出：{s}"))
        .unwrap_or_else(|e| e.to_string()))
}

const PICKER_ROWS: usize = 10;
fn project_label(path: &Path, cwd: &Path, project: Option<&crate::projects::Project>) -> String {
    let mut label = format!(
        "{}{}",
        if project.is_some_and(|p| p.pinned) {
            "★ "
        } else {
            ""
        },
        display_path(path)
    );
    if path == cwd {
        label.push_str("  （默认）");
    }
    if project.is_some_and(|p| p.manual) {
        label.push_str(" · 手动添加");
    }
    if project.is_some_and(|p| p.missing) {
        label.push_str(" · 目录不存在");
    }
    label
}
fn draw_prompt(frame: &mut ratatui::Frame, prompt: &Prompt, cwd: &Path, projects: &Projects) {
    let (title, text, height) = match prompt {
        Prompt::Project(picker) => {
            let choices = project_choices(cwd, &projects.list, &picker.query);
            let first = picker.selected.saturating_sub(PICKER_ROWS - 1);
            let mut lines = vec![
                Line::from(format!("筛选或路径：{}▏", picker.query)),
                Line::from(""),
            ];
            for (index, path) in choices.iter().enumerate().skip(first).take(PICKER_ROWS) {
                let label = project_label(path, cwd, projects.get(path));
                lines.push(if index == picker.selected {
                    Line::from(format!("› {label}")).bold()
                } else {
                    Line::from(format!("  {label}")).dim()
                });
            }
            if choices.is_empty() {
                lines.push(Line::from("没有匹配的项目；输入以 / 或 ~ 开头的完整路径").dim());
            }
            if projects.sessions.is_none() {
                lines.push(Line::from("正在读取项目…").dim());
            }
            if !picker.status.is_empty() {
                lines.push(Line::from(picker.status.clone()).fg(Color::LightBlue));
            }
            lines.push(Line::from(""));
            let confirm = if picker.launch.is_some() {
                "启动"
            } else {
                "设为默认"
            };
            lines.push(
                Line::from(format!(
                    "↑↓ 选择 · 输入筛选或路径 · Enter {confirm} · Esc 取消"
                ))
                .dim(),
            );
            lines.push(Line::from("Ctrl+A 添加 · Ctrl+T 置顶 · Ctrl+D 移出列表").dim());
            let title = match &picker.launch {
                Some(p) => format!("在哪个项目启动 {}", p.name),
                None => "默认项目目录".into(),
            };
            (title, lines, PICKER_ROWS as u16 + 10)
        }
        Prompt::Remove(_) => (
            "移出列表".into(),
            vec![
                Line::from("配置版本及历史仍会保留。"),
                Line::from(""),
                Line::from("y 确认 · n / Esc 取消"),
            ],
            9,
        ),
    };
    let area = modal(frame.area(), height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> Profile {
        Profile {
            id: "fixture".into(),
            name: "WorkBuddy".into(),
            revision: 1,
            base_url: crate::plugins::account_endpoint(&uuid::Uuid::new_v4().to_string()),
            model: "model-a".into(),
            source: None,
            follow: false,
            source_status: String::new(),
            auth_mode: AuthMode::Byok,
            oauth: None,
        }
    }
    #[test]
    fn project_choices_lead_with_default_filter_and_accept_typed_paths() {
        let root = std::env::temp_dir()
            .join(format!("usage-projects-{}", uuid::Uuid::new_v4()))
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let typed = root.join("app");
        let longer = root.join("app-server");
        std::fs::create_dir_all(&typed).unwrap();
        std::fs::create_dir_all(&longer).unwrap();
        let typed = typed.canonicalize().unwrap();
        let longer = longer.canonicalize().unwrap();
        let cwd = PathBuf::from("/synthetic/default");
        let other = PathBuf::from("/synthetic/Other");
        let recent = crate::projects::catalog(
            &[],
            [
                (longer.as_path(), 3),
                (cwd.as_path(), 2),
                (other.as_path(), 1),
            ],
        );
        assert_eq!(
            project_choices(&cwd, &recent, ""),
            vec![cwd.clone(), longer.clone(), "/synthetic/Other".into()]
        );
        assert_eq!(
            project_choices(&cwd, &recent, "other"),
            vec![PathBuf::from("/synthetic/Other")]
        );
        // An existing typed path is not shadowed by a recent project sharing its prefix.
        let choices = project_choices(&cwd, &recent, &typed.to_string_lossy());
        assert_eq!(choices.first(), Some(&typed));
        assert!(project_choices(&cwd, &recent, "/synthetic/missing").is_empty());
        std::fs::remove_dir_all(root.join("app")).unwrap();
        std::fs::remove_dir_all(root.join("app-server")).unwrap();
        let _ = std::fs::remove_dir(root);
    }
    #[test]
    fn launching_in_a_new_directory_keeps_it_as_a_project() {
        let root =
            std::env::temp_dir().join(format!("usage-launch-projects-{}", uuid::Uuid::new_v4()));
        let fresh = root.join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        let fresh = fresh.canonicalize().unwrap();
        let derived = PathBuf::from("/synthetic/derived");
        let mut projects = Projects {
            store: crate::projects::Store::at(root.join("projects.sqlite3")),
            sessions: None,
            records: Vec::new(),
            list: Vec::new(),
        };
        assert!(
            projects
                .loaded(vec![(derived.clone(), 1)], Ok(Vec::new()))
                .is_none()
        );
        projects.launched(&fresh).unwrap();
        let project = projects.get(&fresh).unwrap();
        assert!(project.manual);
        assert_eq!(projects.list[0].path, fresh, "the latest launch leads");
        // Launching in a session-derived project records the launch without making it manual.
        projects.launched(&derived).unwrap();
        let launched = projects.get(&derived).unwrap();
        assert!(!launched.manual);
        assert!(
            launched.recency_ms > 1,
            "the launch time replaces older session activity"
        );
        assert_eq!(
            project_label(&fresh, &derived, projects.get(&fresh)),
            format!("{} · 手动添加", display_path(&fresh))
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn management_labels_and_actions_follow_the_selected_provider() {
        let mut p = profile();
        assert!(identity(&p).contains("订阅服务"));
        assert!(!identity(&p).contains("BYOK"));
        assert!(!identity(&p).contains("usage-plugin.invalid"));
        assert!(!actions(Some(&p)).contains("重新登录"));
        assert!(!actions(Some(&p)).contains("跟随"));
        p.base_url = "https://api.example.test/v1".into();
        p.source = Some("cc-switch".into());
        p.follow = true;
        assert!(identity(&p).contains("BYOK"));
        assert!(actions(Some(&p)).contains("跟随"));
        p.auth_mode = AuthMode::Oauth;
        assert!(actions(Some(&p)).contains("重新登录"));
        assert!(!actions(None).contains("移除"));
    }
}
