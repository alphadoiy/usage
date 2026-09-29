//! 事件循环。查询在 tokio 任务里进行，界面保持可操作。

use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use chrono::Utc;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph, Widget, Wrap};
use tokio::runtime::Handle;

use crate::config::{AppConfig, ConfigError, home_dir, write_style};
use crate::model::UsageSnapshot;
use crate::providers::{
    ProviderError, QUERY_TIMEOUT, ResetPreview, UsageProvider, query_with_timeout,
};
use crate::sessions;
use crate::ui::session_browser::{self, Focus, SessionBrowser};
use crate::ui::widgets::{accent, preferred_height, render_card, render_loading};
use crate::view::{CardView, builtin_styles};

const MIN_CARD_WIDTH: u16 = 27;
const CARD_GAP: u16 = 2;

struct RefreshEvent {
    generation: u64,
    tool: String,
    snapshot: UsageSnapshot,
}

enum AppEvent {
    Quota(Box<RefreshEvent>),
    ResetPreview {
        generation: u64,
        result: Result<ResetPreview, ProviderError>,
    },
    ResetConsumed {
        generation: u64,
        result: Result<(), ProviderError>,
    },
    SessionLaunched {
        generation: u64,
        result: Result<(), String>,
    },
    SessionsLoaded {
        generation: u64,
        catalog: sessions::SessionCatalog,
    },
}

enum ResetStage {
    Loading,
    Confirm(ResetPreview),
    Consuming,
}

struct ResetFlow {
    provider_id: String,
    provider_name: String,
    generation: u64,
    stage: ResetStage,
}

struct RedrawSchedule {
    dirty: bool,
    last_draw_second: Option<i64>,
}

impl RedrawSchedule {
    fn new() -> Self {
        Self {
            dirty: true,
            last_draw_second: None,
        }
    }

    fn invalidate(&mut self) {
        self.dirty = true;
    }

    fn take(&mut self, now_second: i64) -> bool {
        if !self.dirty && self.last_draw_second == Some(now_second) {
            return false;
        }
        self.dirty = false;
        self.last_draw_second = Some(now_second);
        true
    }
}

pub fn run(
    handle: Handle,
    providers: Vec<Arc<dyn UsageProvider>>,
    config: AppConfig,
    config_path: std::path::PathBuf,
) -> io::Result<()> {
    let app = Dashboard::new(providers, config, config_path);
    run_dashboard(handle, app)
}

/// Runs the TUI with additional or replacement local session sources.
pub fn run_with_sessions(
    handle: Handle,
    providers: Vec<Arc<dyn UsageProvider>>,
    config: AppConfig,
    config_path: std::path::PathBuf,
    registry: sessions::SessionRegistry,
    homes: sessions::SessionHomes,
) -> io::Result<()> {
    let app = Dashboard::new_with_sessions(providers, config, config_path, registry, homes);
    run_dashboard(handle, app)
}

fn run_dashboard(handle: Handle, mut app: Dashboard) -> io::Result<()> {
    let mut terminal = ratatui::try_init().map_err(io::Error::other)?;
    let mut redraw = RedrawSchedule::new();
    app.refresh(&handle);
    let result = loop {
        let (received, reset_consumed) = app.drain_events();
        if reset_consumed {
            app.refresh(&handle);
            app.notice = Some("已消耗 1 次，正在刷新额度".into());
        }
        let refresh_started = app.maybe_auto_refresh(&handle);
        if received || reset_consumed || refresh_started {
            redraw.invalidate();
        }
        if redraw.take(Utc::now().timestamp())
            && let Err(error) = terminal.draw(|frame| draw(frame, &app))
        {
            break Err(error);
        }
        match event::poll(Duration::from_millis(80)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        break Ok(());
                    }
                    if app.handle_reset_key(key.code, &handle) {
                        redraw.invalidate();
                        continue;
                    }
                    if app.browser.is_some() {
                        app.handle_browser_key(key.code, &handle);
                        redraw.invalidate();
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                        KeyCode::Char('r') => app.refresh(&handle),
                        KeyCode::Char('1') => app.set_style("bar"),
                        KeyCode::Char('2') => app.set_style("ring"),
                        KeyCode::Char('3') => app.set_style("statusline"),
                        KeyCode::Char('s') => app.save_style(),
                        KeyCode::Char('x') => app.open_reset(&handle),
                        KeyCode::Char('p') => app.open_browser(&handle),
                        KeyCode::Char('j') | KeyCode::Down => {
                            app.scroll_by(1, terminal.size().map(|size| size.width).unwrap_or(80))
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            app.scroll_by(-1, terminal.size().map(|size| size.width).unwrap_or(80))
                        }
                        _ => {}
                    }
                    redraw.invalidate();
                }
                Ok(Event::Resize(_, _)) => redraw.invalidate(),
                Err(error) => break Err(error),
                _ => {}
            },
            Ok(false) => {}
            Err(error) => break Err(error),
        }
    };
    ratatui::restore();
    result
}

struct Dashboard {
    providers: Vec<Arc<dyn UsageProvider>>,
    cards: Vec<CardSlot>,
    style_name: String,
    refresh_seconds: u64,
    config_path: std::path::PathBuf,
    status: String,
    scroll: u16,
    generation: u64,
    pending: usize,
    last_refresh: std::time::Instant,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    notice: Option<String>,
    reset: Option<ResetFlow>,
    reset_generation: u64,
    session_homes: sessions::SessionHomes,
    session_registry: sessions::SessionRegistry,
    browser: Option<SessionBrowser>,
    browser_generation: u64,
    browser_loading: bool,
}

struct CardSlot {
    id: String,
    title: String,
    loading: bool,
    snapshot: Option<UsageSnapshot>,
    warning: Option<String>,
}

impl Dashboard {
    fn new(
        providers: Vec<Arc<dyn UsageProvider>>,
        config: AppConfig,
        config_path: std::path::PathBuf,
    ) -> Self {
        let home = home_dir();
        let session_homes = sessions::SessionHomes::new()
            .with(
                "claude",
                config
                    .claude_dir
                    .clone()
                    .unwrap_or_else(|| home.join(".claude")),
            )
            .with(
                "codex",
                config
                    .codex_dir
                    .clone()
                    .unwrap_or_else(|| home.join(".codex")),
            )
            .with(
                "grok",
                config
                    .grok_dir
                    .clone()
                    .unwrap_or_else(|| home.join(".grok")),
            );
        Self::new_with_sessions(
            providers,
            config,
            config_path,
            sessions::builtin_registry(),
            session_homes,
        )
    }

    fn new_with_sessions(
        providers: Vec<Arc<dyn UsageProvider>>,
        config: AppConfig,
        config_path: std::path::PathBuf,
        session_registry: sessions::SessionRegistry,
        session_homes: sessions::SessionHomes,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let cards = providers
            .iter()
            .map(|provider| CardSlot {
                id: provider.id().to_string(),
                title: provider.display_name().to_string(),
                loading: true,
                snapshot: None,
                warning: None,
            })
            .collect();
        let styles = builtin_styles();
        Self {
            providers,
            cards,
            status: format!("百分比是已用额度 · {}", styles.label(&config.style)),
            style_name: config.style,
            refresh_seconds: config.refresh_seconds,
            config_path,
            scroll: 0,
            generation: 0,
            pending: 0,
            last_refresh: std::time::Instant::now(),
            tx,
            rx,
            notice: None,
            reset: None,
            reset_generation: 0,
            session_homes,
            session_registry,
            browser: None,
            browser_generation: 0,
            browser_loading: false,
        }
    }

    fn open_browser(&mut self, handle: &Handle) {
        self.browser = Some(SessionBrowser::new(Vec::new()));
        self.start_browser_load(handle);
    }

    fn start_browser_load(&mut self, handle: &Handle) {
        if self.browser_loading {
            return;
        }
        self.browser_generation = self.browser_generation.wrapping_add(1);
        let generation = self.browser_generation;
        let homes = self.session_homes.clone();
        let registry = self.session_registry.clone();
        let tx = self.tx.clone();
        self.browser_loading = true;
        self.notice = None;
        handle.spawn_blocking(move || {
            let catalog = registry.load_all(&homes);
            let _ = tx.send(AppEvent::SessionsLoaded {
                generation,
                catalog,
            });
        });
    }

    fn handle_browser_key(&mut self, key: KeyCode, handle: &Handle) {
        if self
            .browser
            .as_ref()
            .is_some_and(|browser| browser.launching)
        {
            return;
        }
        if self
            .browser
            .as_ref()
            .is_some_and(|browser| browser.searching)
        {
            if let Some(browser) = &mut self.browser {
                match key {
                    KeyCode::Esc | KeyCode::Enter => browser.searching = false,
                    KeyCode::Backspace => browser.backspace(),
                    KeyCode::Char(ch) => browser.type_char(ch),
                    _ => {}
                }
            }
            return;
        }
        match key {
            KeyCode::Esc
                if self
                    .browser
                    .as_ref()
                    .is_some_and(|b| b.focus() == Focus::Sessions) =>
            {
                if let Some(browser) = &mut self.browser {
                    browser.focus_projects();
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.browser = None;
                self.browser_generation = self.browser_generation.wrapping_add(1);
                self.browser_loading = false;
                self.notice = None;
            }
            KeyCode::Char('r') => self.start_browser_load(handle),
            KeyCode::Tab => {
                if let Some(browser) = &mut self.browser {
                    browser.toggle_focus();
                }
            }
            KeyCode::Left | KeyCode::BackTab => {
                if let Some(browser) = &mut self.browser {
                    browser.focus_projects();
                }
            }
            KeyCode::Right => {
                if let Some(browser) = &mut self.browser {
                    browser.focus_sessions();
                }
            }
            KeyCode::Char('/') => {
                if let Some(browser) = &mut self.browser {
                    browser.searching = true;
                }
            }
            KeyCode::Char('c') => {
                if let Some(browser) = &mut self.browser {
                    browser.clear_search();
                }
            }
            KeyCode::Backspace => {
                if let Some(browser) = &mut self.browser {
                    browser.backspace();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(browser) = &mut self.browser {
                    browser.move_by(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(browser) = &mut self.browser {
                    browser.move_by(1);
                }
            }
            KeyCode::Enter => {
                if self.browser_loading {
                    return;
                }
                let Some(browser) = &mut self.browser else {
                    return;
                };
                if browser.focus() == Focus::Projects {
                    browser.focus_sessions();
                    return;
                }
                let Some(session) = browser.selected_session().cloned() else {
                    return;
                };
                browser.launching = true;
                self.notice = Some("正在 Ghostty 右侧打开会话…".into());
                let tx = self.tx.clone();
                let generation = self.browser_generation;
                handle.spawn_blocking(move || {
                    let result = sessions::resume_in_ghostty(&session);
                    let _ = tx.send(AppEvent::SessionLaunched { generation, result });
                });
            }
            _ => {}
        }
    }

    fn set_style(&mut self, style_name: &str) {
        self.style_name = style_name.to_string();
        let label = builtin_styles().label(style_name);
        self.status = format!("当前样式：{label}。按 s 写入配置");
    }

    fn save_style(&mut self) {
        match write_style(&self.config_path, &self.style_name) {
            Ok(()) => self.notice = Some(format!("已写入 {}", self.config_path.display())),
            Err(ConfigError(message)) => self.notice = Some(message),
        }
    }

    fn scroll_by(&mut self, delta: i16, width: u16) {
        let columns = card_columns(width, self.cards.len());
        let max_scroll = self.max_scroll(columns);
        let next = i32::from(self.scroll) + i32::from(delta) * columns as i32;
        self.scroll = next.clamp(0, i32::from(max_scroll)) as u16;
    }

    fn max_scroll(&self, columns: usize) -> u16 {
        (self.cards.len().saturating_sub(1) / columns.max(1) * columns.max(1)) as u16
    }

    fn refresh(&mut self, handle: &Handle) {
        self.notice = None;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.pending = self.providers.len();
        self.last_refresh = std::time::Instant::now();
        self.status = "正在刷新…".into();
        for provider in &self.providers {
            let provider = Arc::clone(provider);
            let tx = self.tx.clone();
            handle.spawn(async move {
                let snapshot = query_with_timeout(provider.as_ref(), QUERY_TIMEOUT).await;
                let _ = tx.send(AppEvent::Quota(Box::new(RefreshEvent {
                    generation,
                    tool: provider.id().to_string(),
                    snapshot,
                })));
            });
        }
    }

    fn open_reset(&mut self, handle: &Handle) {
        let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.reset().is_some())
        else {
            self.notice = Some("当前没有支持主动重置的工具".into());
            return;
        };
        let provider = Arc::clone(provider);
        self.notice = None;
        self.reset_generation = self.reset_generation.wrapping_add(1);
        let generation = self.reset_generation;
        self.reset = Some(ResetFlow {
            provider_id: provider.id().into(),
            provider_name: provider.display_name().into(),
            generation,
            stage: ResetStage::Loading,
        });
        let tx = self.tx.clone();
        handle.spawn(async move {
            let result = match provider.reset() {
                Some(reset) => tokio::time::timeout(QUERY_TIMEOUT, reset.preview_reset())
                    .await
                    .unwrap_or_else(|_| Err(ProviderError("查询主动重置次数超时".into()))),
                None => Err(ProviderError("工具不支持主动重置".into())),
            };
            let _ = tx.send(AppEvent::ResetPreview { generation, result });
        });
    }

    fn handle_reset_key(&mut self, key: KeyCode, handle: &Handle) -> bool {
        let Some(flow) = self.reset.as_ref() else {
            return false;
        };
        match (&flow.stage, key) {
            (ResetStage::Loading | ResetStage::Confirm(_), KeyCode::Esc)
            | (ResetStage::Loading | ResetStage::Confirm(_), KeyCode::Char('n' | 'q')) => {
                self.reset = None;
                self.reset_generation = self.reset_generation.wrapping_add(1);
            }
            (ResetStage::Confirm(_), KeyCode::Char('y')) => self.consume_reset(handle),
            _ => {}
        }
        true
    }

    fn consume_reset(&mut self, handle: &Handle) {
        let Some(flow) = self.reset.as_mut() else {
            return;
        };
        let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.id() == flow.provider_id)
            .cloned()
        else {
            self.reset = None;
            self.notice = Some("找不到主动重置工具".into());
            return;
        };
        let generation = flow.generation;
        flow.stage = ResetStage::Consuming;
        let tx = self.tx.clone();
        handle.spawn(async move {
            let result = match provider.reset() {
                Some(reset) => reset.consume_reset().await,
                None => Err(ProviderError("工具不支持主动重置".into())),
            };
            let _ = tx.send(AppEvent::ResetConsumed { generation, result });
        });
    }

    fn maybe_auto_refresh(&mut self, handle: &Handle) -> bool {
        if self.refresh_seconds == 0
            || self.pending > 0
            || self.reset.is_some()
            || self.browser.is_some()
        {
            return false;
        }
        if self.last_refresh.elapsed() >= Duration::from_secs(self.refresh_seconds) {
            self.refresh(handle);
            return true;
        }
        false
    }

    fn drain_events(&mut self) -> (bool, bool) {
        let mut changed = false;
        let mut reset_consumed = false;
        while let Ok(event) = self.rx.try_recv() {
            let event = match event {
                AppEvent::Quota(event) => event,
                AppEvent::ResetPreview { generation, result } => {
                    if self
                        .reset
                        .as_ref()
                        .is_some_and(|flow| flow.generation == generation)
                    {
                        changed = true;
                        match result {
                            Ok(preview) if preview.available > 0 => {
                                if let Some(flow) = &mut self.reset {
                                    flow.stage = ResetStage::Confirm(preview);
                                }
                            }
                            Ok(_) => {
                                self.reset = None;
                                self.notice = Some("没有可用的主动重置次数".into());
                            }
                            Err(error) => {
                                self.reset = None;
                                self.notice = Some(format!("查询重置次数失败: {error}"));
                            }
                        }
                    }
                    continue;
                }
                AppEvent::ResetConsumed { generation, result } => {
                    if self
                        .reset
                        .as_ref()
                        .is_some_and(|flow| flow.generation == generation)
                    {
                        changed = true;
                        self.reset = None;
                        match result {
                            Ok(()) => {
                                reset_consumed = true;
                            }
                            Err(error) => self.notice = Some(format!("主动重置失败: {error}")),
                        }
                    }
                    continue;
                }
                AppEvent::SessionLaunched { generation, result } => {
                    if generation == self.browser_generation && self.browser.is_some() {
                        changed = true;
                        match result {
                            Ok(()) => {
                                self.browser = None;
                                self.notice = Some("已在 Ghostty 右侧恢复会话".into());
                            }
                            Err(error) => {
                                if let Some(browser) = &mut self.browser {
                                    browser.launching = false;
                                }
                                self.notice = Some(error);
                            }
                        }
                    }
                    continue;
                }
                AppEvent::SessionsLoaded {
                    generation,
                    catalog,
                } => {
                    if generation == self.browser_generation && self.browser.is_some() {
                        changed = true;
                        self.browser_loading = false;
                        let message = if catalog.warnings.is_empty() {
                            "未找到可恢复的本机会话".to_string()
                        } else {
                            catalog.warnings.join(" · ")
                        };
                        if catalog.sessions.is_empty()
                            && self.browser.as_ref().is_some_and(|b| b.sessions.is_empty())
                        {
                            self.browser = None;
                            self.notice = Some(message);
                        } else {
                            if let Some(browser) = &mut self.browser {
                                browser.refresh(catalog.sessions);
                            }
                            self.notice =
                                if self.browser.as_ref().is_some_and(|b| b.sessions.is_empty())
                                    || !catalog.warnings.is_empty()
                                {
                                    Some(message)
                                } else {
                                    None
                                };
                        }
                    }
                    continue;
                }
            };
            if event.generation != self.generation {
                continue;
            }
            changed = true;
            self.pending = self.pending.saturating_sub(1);
            if let Some(card) = self.cards.iter_mut().find(|card| card.id == event.tool) {
                if event.snapshot.transient {
                    if card
                        .snapshot
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.success)
                    {
                        card.warning = event.snapshot.error;
                    } else {
                        card.loading = false;
                        card.snapshot = Some(event.snapshot);
                        card.warning = None;
                    }
                } else {
                    card.loading = false;
                    card.snapshot = Some(event.snapshot);
                    card.warning = None;
                }
            }
            if self.pending == 0 {
                let ok = self
                    .cards
                    .iter()
                    .filter(|card| {
                        card.snapshot
                            .as_ref()
                            .is_some_and(|snapshot| snapshot.success)
                    })
                    .count();
                let warned = self
                    .cards
                    .iter()
                    .filter(|card| card.warning.is_some())
                    .count();
                let label = builtin_styles().label(&self.style_name);
                self.status = format!("已更新 {ok}/{} 个工具 · {label}", self.cards.len());
                if warned > 0 {
                    self.status.push_str(&format!(" · {warned} 个沿用上次结果"));
                }
            }
        }
        (changed, reset_consumed)
    }
}

fn draw(frame: &mut Frame, app: &Dashboard) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let status = if app.browser_loading {
        "正在读取本机会话…".to_string()
    } else {
        app.notice.clone().unwrap_or_else(|| {
            app.browser.as_ref().map_or_else(
                || app.status.clone(),
                |browser| {
                    format!(
                        "{} 个项目 · 显示 {}/{} 个会话",
                        browser.project_count(),
                        browser.visible_count(),
                        browser.sessions.len()
                    )
                },
            )
        })
    };
    let header_title = if app.browser.is_some() {
        " 会话 "
    } else {
        " 用量 "
    };
    Paragraph::new(Line::from(vec![header_title.bold(), status.dim()]))
        .style(
            Style::new()
                .bg(Color::Rgb(22, 22, 24))
                .fg(Color::Rgb(242, 242, 247)),
        )
        .render(header, frame.buffer_mut());
    if let Some(browser) = &app.browser {
        if app.browser_loading && browser.sessions.is_empty() {
            Paragraph::new(" 正在扫描 Claude Code、Codex 和 Grok 的本机会话…")
                .style(Style::new().fg(Color::Rgb(142, 142, 147)))
                .render(body, frame.buffer_mut());
        } else {
            session_browser::render(body, frame.buffer_mut(), browser);
        }
    } else {
        draw_cards(frame, body, app);
    }
    let footer_text = if let Some(browser) = &app.browser {
        if app.browser_loading {
            " 正在读取本机会话… Esc 返回"
        } else if browser.launching {
            " 正在 Ghostty 右侧打开会话… 请等待"
        } else {
            " ↑/↓ 选择  Tab/←/→ 切换列表  Enter 进入/打开  / 搜索  c 清除  r 更新  Esc 返回"
        }
    } else {
        match app.reset.as_ref().map(|flow| &flow.stage) {
            Some(ResetStage::Loading) => " 正在查询重置次数…   n/Esc 取消",
            Some(ResetStage::Confirm(_)) => " y 确认消耗 1 次   n/Esc 取消",
            Some(ResetStage::Consuming) => " 正在提交主动重置，请等待结果…",
            None if app
                .providers
                .iter()
                .any(|provider| provider.reset().is_some()) =>
            {
                " q 退出  r 刷新  1 点阵  2 圆环  3 圆点  p 会话  x 重置  s 保存  j/k 滚动"
            }
            None => " q 退出   r 刷新   1 点阵   2 圆环   3 圆点   p 会话   s 保存   j/k 滚动",
        }
    };
    Paragraph::new(footer_text)
        .style(
            Style::new()
                .fg(Color::Rgb(142, 142, 147))
                .bg(Color::Rgb(22, 22, 24)),
        )
        .render(footer, frame.buffer_mut());
    draw_reset_modal(frame, app);
}

fn draw_reset_modal(frame: &mut Frame, app: &Dashboard) {
    let Some(flow) = &app.reset else {
        return;
    };
    let area = frame.area();
    let width = area.width.min(62);
    let height = area
        .height
        .min(if matches!(flow.stage, ResetStage::Confirm(_)) {
            10
        } else {
            5
        });
    let modal = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let mut lines = Vec::new();
    match &flow.stage {
        ResetStage::Loading => lines.push("正在查询可用的主动重置次数…".to_string()),
        ResetStage::Confirm(preview) => {
            lines.push(format!("可用次数：{}", preview.available));
            if let Some(account) = &preview.account_label {
                lines.push(format!("账号：{account}"));
            }
            if let Some(expiry) = preview.next_expires_at {
                lines.push(format!(
                    "最近到期：{}",
                    expiry.with_timezone(&chrono::Local).format("%m-%d %H:%M")
                ));
            }
            lines.push("将消耗 1 次，重置当前 5 小时额度。".into());
            lines.push("按 y 确认，按 n 或 Esc 取消".into());
        }
        ResetStage::Consuming => lines.push("正在提交，请勿重复操作…".into()),
    }
    Clear.render(modal, frame.buffer_mut());
    let block = Block::bordered()
        .title(format!(" {} 主动重置 ", flow.provider_name))
        .style(
            Style::new()
                .bg(Color::Rgb(22, 22, 24))
                .fg(Color::Rgb(242, 242, 247)),
        )
        .border_style(accent(&flow.provider_id));
    let inner = block.inner(modal);
    block.render(modal, frame.buffer_mut());
    Paragraph::new(lines.join("\n"))
        .wrap(Wrap { trim: true })
        .render(inner, frame.buffer_mut());
}

fn draw_cards(frame: &mut Frame, area: Rect, app: &Dashboard) {
    render_cards(area, frame.buffer_mut(), app);
}

fn card_columns(width: u16, card_count: usize) -> usize {
    let fitting = usize::from(width.saturating_add(CARD_GAP) / (MIN_CARD_WIDTH + CARD_GAP));
    fitting.clamp(1, 3).min(card_count.max(1))
}

fn column_rects(area: Rect, columns: usize) -> Vec<Rect> {
    let gaps = CARD_GAP.saturating_mul(columns.saturating_sub(1) as u16);
    let available = area.width.saturating_sub(gaps);
    let base = available / columns as u16;
    let remainder = available % columns as u16;
    let mut x = area.x;
    (0..columns)
        .map(|index| {
            let width = base + u16::from(index < usize::from(remainder));
            let rect = Rect { x, width, ..area };
            x = x.saturating_add(width).saturating_add(CARD_GAP);
            rect
        })
        .collect()
}

fn render_cards(area: Rect, buf: &mut Buffer, app: &Dashboard) {
    let now = Utc::now();
    let styles = builtin_styles();
    let columns = card_columns(area.width, app.cards.len());
    let column_areas = column_rects(area, columns);
    let start = usize::from(app.scroll.min(app.max_scroll(columns))) / columns * columns;
    let visible_cards = app.cards.get(start..).unwrap_or(&[]);
    let mut y = area.y;
    for row in visible_cards.chunks(columns) {
        if y >= area.bottom() {
            break;
        }
        let views = row
            .iter()
            .map(|card| {
                card.snapshot.as_ref().map(|snapshot| {
                    styles
                        .build(&app.style_name, snapshot, now, card.warning.as_deref())
                        .unwrap_or_else(|_| CardView::fallback(snapshot))
                })
            })
            .collect::<Vec<_>>();
        let height = row
            .iter()
            .zip(views.iter())
            .zip(column_areas.iter())
            .map(|((card, view), column)| {
                if card.loading || view.is_none() {
                    3
                } else {
                    preferred_height(view.as_ref().expect("view"), column.width)
                }
            })
            .max()
            .unwrap_or(0)
            .min(area.bottom().saturating_sub(y));
        for ((card, view), column) in row.iter().zip(views).zip(column_areas.iter()) {
            let rect = Rect {
                y,
                height,
                ..*column
            };
            if let Some(view) = view {
                render_card(rect, buf, &card.id, &card.title, &view);
            } else {
                render_loading(rect, buf, &card.id, &card.title);
            }
        }
        y = y.saturating_add(height).saturating_add(1);
    }
}

impl CardView {
    fn fallback(snapshot: &UsageSnapshot) -> Self {
        builtin_styles()
            .build("bar", snapshot, Utc::now(), None)
            .expect("bar style")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use chrono::Duration as ChronoDuration;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;
    use crate::model::{QuotaTier, snapshot_not_found, snapshot_ok};
    use crate::providers::UsageReset;
    use crate::sessions::{AgentKind, Session};

    struct ResetTestProvider {
        available: u32,
        consumed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl UsageProvider for ResetTestProvider {
        fn id(&self) -> &'static str {
            "codex"
        }

        fn display_name(&self) -> &'static str {
            "Codex"
        }

        async fn query(&self) -> UsageSnapshot {
            snapshot_not_found("codex", "Codex", None)
        }

        fn reset(&self) -> Option<&dyn UsageReset> {
            Some(self)
        }
    }

    #[async_trait]
    impl UsageReset for ResetTestProvider {
        async fn preview_reset(&self) -> Result<ResetPreview, ProviderError> {
            Ok(ResetPreview {
                available: self.available,
                next_expires_at: None,
                account_label: Some("account-123".into()),
            })
        }

        async fn consume_reset(&self) -> Result<(), ProviderError> {
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.consumed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn reset_dashboard(available: u32, consumed: Arc<AtomicUsize>) -> Dashboard {
        Dashboard::new(
            vec![Arc::new(ResetTestProvider {
                available,
                consumed,
            })],
            AppConfig::default(),
            "/tmp/usage-reset-test.toml".into(),
        )
    }

    async fn drain_until(
        app: &mut Dashboard,
        condition: impl Fn(&Dashboard) -> bool,
    ) -> (bool, bool) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let outcome = app.drain_events();
                if condition(app) {
                    return outcome;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reset event")
    }

    fn dashboard() -> Dashboard {
        let now = Utc::now();
        let mut app = Dashboard::new(
            Vec::new(),
            AppConfig::default(),
            "/tmp/usage-test.toml".into(),
        );
        app.cards = vec![
            CardSlot {
                id: "claude".into(),
                title: "Claude Code".into(),
                loading: false,
                snapshot: Some(snapshot_not_found("claude", "Claude Code", None)),
                warning: None,
            },
            CardSlot {
                id: "codex".into(),
                title: "Codex".into(),
                loading: false,
                snapshot: Some(snapshot_ok(
                    "codex",
                    "Codex",
                    vec![
                        QuotaTier {
                            name: "five_hour".into(),
                            utilization: 89.0,
                            resets_at: Some(now + ChronoDuration::hours(1)),
                        },
                        QuotaTier {
                            name: "seven_day".into(),
                            utilization: 29.0,
                            resets_at: Some(now + ChronoDuration::days(5)),
                        },
                    ],
                    None,
                    now,
                )),
                warning: None,
            },
            CardSlot {
                id: "grok".into(),
                title: "Grok".into(),
                loading: false,
                snapshot: Some(snapshot_not_found("grok", "Grok", None)),
                warning: None,
            },
        ];
        app
    }

    #[tokio::test]
    async fn browser_navigates_projects_and_searches_without_launching() {
        let mut app = dashboard();
        app.browser = Some(SessionBrowser::new(vec![
            Session {
                provider: AgentKind::CLAUDE,
                id: "123e4567-e89b-12d3-a456-426614174000".into(),
                cwd: "/tmp/project".into(),
                title: "Claude task".into(),
                recency_at_ms: 0,
                home: "/tmp/claude".into(),
            },
            Session {
                provider: AgentKind::CODEX,
                id: "123e4567-e89b-12d3-a456-426614174001".into(),
                cwd: "/tmp/other".into(),
                title: "Codex task".into(),
                recency_at_ms: 0,
                home: "/tmp/codex".into(),
            },
        ]));
        let handle = Handle::current();
        assert_eq!(app.browser.as_ref().unwrap().focus(), Focus::Projects);
        app.handle_browser_key(KeyCode::Enter, &handle);
        assert_eq!(app.browser.as_ref().unwrap().focus(), Focus::Sessions);
        app.handle_browser_key(KeyCode::Char('/'), &handle);
        app.handle_browser_key(KeyCode::Char('t'), &handle);
        assert_eq!(app.browser.as_ref().unwrap().visible_count(), 2);
        app.handle_browser_key(KeyCode::Esc, &handle);
        assert!(!app.browser.as_ref().unwrap().searching);
        app.handle_browser_key(KeyCode::Esc, &handle);
        assert_eq!(app.browser.as_ref().unwrap().focus(), Focus::Projects);
        app.handle_browser_key(KeyCode::Esc, &handle);
        assert!(app.browser.is_none());
    }

    #[tokio::test]
    async fn browser_load_runs_in_background_and_reports_empty_result() {
        let mut app = dashboard();
        let missing = std::env::temp_dir().join(format!("usage-missing-{}", uuid::Uuid::new_v4()));
        app.session_homes = sessions::SessionHomes::new()
            .with("claude", missing.join("claude"))
            .with("codex", missing.join("codex"))
            .with("grok", missing.join("grok"));
        let handle = Handle::current();
        app.open_browser(&handle);
        assert!(app.browser_loading);
        assert!(app.browser.is_some());
        tokio::time::timeout(Duration::from_secs(2), async {
            while app.browser_loading {
                app.drain_events();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(app.browser.is_none());
        assert_eq!(app.notice.as_deref(), Some("未找到可恢复的本机会话"));
    }

    #[tokio::test]
    async fn closing_browser_discards_pending_catalog() {
        let mut app = dashboard();
        let missing = std::env::temp_dir().join(format!("usage-missing-{}", uuid::Uuid::new_v4()));
        app.session_homes = sessions::SessionHomes::new()
            .with("claude", missing.join("claude"))
            .with("codex", missing.join("codex"))
            .with("grok", missing.join("grok"));
        let handle = Handle::current();
        app.open_browser(&handle);
        app.handle_browser_key(KeyCode::Esc, &handle);
        assert!(!app.browser_loading);
        assert!(app.browser.is_none());
        tokio::time::sleep(Duration::from_millis(10)).await;
        app.drain_events();
        assert!(app.browser.is_none());
        assert!(app.notice.is_none());
    }

    fn custom_sessions(home: &std::path::Path) -> Result<Vec<sessions::SessionRecord>, String> {
        Ok(vec![sessions::SessionRecord {
            id: "123e4567-e89b-12d3-a456-426614174004".into(),
            cwd: home.join("project"),
            title: "Custom task".into(),
            recency_at_ms: 42,
        }])
    }

    #[tokio::test]
    async fn dashboard_uses_injected_session_registry() {
        let root = std::env::temp_dir().join(format!("usage-dashboard-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("project")).unwrap();
        let kind = AgentKind::new(
            "custom",
            "Custom Agent",
            Some((180, 120, 210)),
            "custom resume",
            "CUSTOM_HOME",
            3,
        );
        let mut registry = sessions::SessionRegistry::new();
        registry
            .register(sessions::SessionSource {
                kind,
                load: custom_sessions,
            })
            .unwrap();
        let mut app = Dashboard::new_with_sessions(
            Vec::new(),
            AppConfig::default(),
            root.join("config.toml"),
            registry,
            sessions::SessionHomes::new().with("custom", &root),
        );
        let handle = Handle::current();
        app.open_browser(&handle);
        tokio::time::timeout(Duration::from_secs(2), async {
            while app.browser_loading {
                app.drain_events();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            app.browser
                .as_ref()
                .unwrap()
                .selected_session()
                .unwrap()
                .provider,
            kind
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area().width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn idle_redraws_only_on_clock_change_or_invalidation() {
        let mut redraw = RedrawSchedule::new();
        assert!(redraw.take(100));
        for _ in 0..750 {
            assert!(!redraw.take(100));
        }
        assert!(redraw.take(101));
        assert!(!redraw.take(101));
        redraw.invalidate();
        assert!(redraw.take(101));
    }

    #[test]
    fn only_current_query_results_trigger_redraw() {
        let mut app = dashboard();
        app.generation = 2;
        app.pending = 1;
        app.tx
            .send(AppEvent::Quota(Box::new(RefreshEvent {
                generation: 1,
                tool: "codex".into(),
                snapshot: snapshot_not_found("codex", "Codex", None),
            })))
            .unwrap();
        assert_eq!(app.drain_events(), (false, false));
        assert_eq!(app.pending, 1);
        app.tx
            .send(AppEvent::Quota(Box::new(RefreshEvent {
                generation: 2,
                tool: "codex".into(),
                snapshot: snapshot_not_found("codex", "Codex", None),
            })))
            .unwrap();
        assert_eq!(app.drain_events(), (true, false));
        assert_eq!(app.pending, 0);
    }

    #[tokio::test]
    async fn reset_requires_confirmation_and_consumes_once() {
        let consumed = Arc::new(AtomicUsize::new(0));
        let mut app = reset_dashboard(2, Arc::clone(&consumed));
        let handle = Handle::current();
        app.open_reset(&handle);
        drain_until(&mut app, |app| {
            app.reset
                .as_ref()
                .is_some_and(|flow| matches!(flow.stage, ResetStage::Confirm(_)))
        })
        .await;
        assert_eq!(consumed.load(Ordering::SeqCst), 0);
        assert!(app.handle_reset_key(KeyCode::Esc, &handle));
        assert!(app.reset.is_none());
        assert_eq!(consumed.load(Ordering::SeqCst), 0);

        app.open_reset(&handle);
        drain_until(&mut app, |app| {
            app.reset
                .as_ref()
                .is_some_and(|flow| matches!(flow.stage, ResetStage::Confirm(_)))
        })
        .await;
        assert!(app.handle_reset_key(KeyCode::Char('y'), &handle));
        assert!(app.handle_reset_key(KeyCode::Char('y'), &handle));
        let outcome = drain_until(&mut app, |app| app.reset.is_none()).await;
        assert_eq!(outcome, (true, true));
        assert_eq!(consumed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn zero_reset_credits_never_open_confirmation() {
        let consumed = Arc::new(AtomicUsize::new(0));
        let mut app = reset_dashboard(0, Arc::clone(&consumed));
        app.open_reset(&Handle::current());
        drain_until(&mut app, |app| app.reset.is_none()).await;
        assert!(app.notice.as_deref().unwrap().contains("没有可用"));
        assert_eq!(consumed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn reset_confirmation_is_visible_over_dashboard() {
        let mut app = reset_dashboard(2, Arc::new(AtomicUsize::new(0)));
        app.reset = Some(ResetFlow {
            provider_id: "codex".into(),
            provider_name: "Codex".into(),
            generation: 1,
            stage: ResetStage::Confirm(ResetPreview {
                available: 2,
                next_expires_at: Some(Utc::now() + ChronoDuration::days(1)),
                account_label: Some("123e4567-e89b-12d3-a456-426614174000".into()),
            }),
        });
        for width in [30, 80] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            let rendered = (0..24)
                .map(|y| row_text(terminal.backend().buffer(), y))
                .collect::<Vec<_>>()
                .join("\n");
            let compact = rendered.split_whitespace().collect::<String>();
            assert!(compact.contains("Codex主动重置"), "{rendered}");
            assert!(compact.contains("可用次数：2"), "{rendered}");
            assert!(compact.contains("账号：123e4567"), "{rendered}");
            assert!(compact.contains("426614174000"), "{rendered}");
            assert!(compact.contains("将消耗1次"), "{rendered}");
            assert!(compact.contains("按y确认"), "{rendered}");
        }
    }

    #[test]
    fn three_cards_share_one_row_and_one_height() {
        let app = dashboard();
        assert_eq!(card_columns(88, app.cards.len()), 3);
        let area = Rect::new(0, 0, 88, 12);
        let mut buffer = Buffer::empty(area);
        render_cards(area, &mut buffer, &app);

        for x in [0, 30, 60] {
            assert_eq!(buffer[(x, 0)].symbol(), "╭");
            assert_eq!(buffer[(x, 6)].symbol(), "╰");
        }
        assert!(row_text(&buffer, 0).contains("Claude Code"));
        assert!(row_text(&buffer, 0).contains("Codex"));
        assert!(row_text(&buffer, 0).contains("Grok"));
        assert!(row_text(&buffer, 1).contains("89%"));
        assert!(row_text(&buffer, 4).contains("29%"));
        assert_eq!(buffer[(32, 2)].symbol(), "⣿");
        assert_eq!(buffer[(32, 5)].symbol(), "⣿");
    }

    #[test]
    fn cards_wrap_at_smaller_widths_and_scroll_by_row() {
        let mut app = dashboard();
        assert_eq!(card_columns(72, app.cards.len()), 2);
        assert_eq!(card_columns(52, app.cards.len()), 1);
        let area = Rect::new(0, 0, 72, 15);
        let mut buffer = Buffer::empty(area);
        render_cards(area, &mut buffer, &app);
        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(37, 0)].symbol(), "╭");
        assert_eq!(buffer[(0, 8)].symbol(), "╭");
        assert!(row_text(&buffer, 8).contains("Grok"));

        app.scroll_by(1, 88);
        assert_eq!(app.scroll, 0);
        app.scroll_by(1, 72);
        assert_eq!(app.scroll, 2);
        app.scroll_by(-1, 72);
        assert_eq!(app.scroll, 0);
    }
}
