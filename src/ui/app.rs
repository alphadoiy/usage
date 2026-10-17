//! 事件循环。查询在 tokio 任务里进行，界面保持可操作。

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use tracing::Instrument;

use chrono::Utc;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph, Widget, Wrap};
use tokio::runtime::Handle;

use crate::config::{AppConfig, home_dir};
use crate::model::UsageSnapshot;
use crate::providers::{
    ProviderError, QUERY_TIMEOUT, ResetPreview, UsageProvider, query_with_timeout,
};
use crate::service_status::{self, ServiceStatus};
use crate::sessions;
use crate::ui::history::{self, HistoryPanel};
use crate::ui::menu::{self, Action as MenuAction, Command as MenuCommand};
use crate::ui::session_browser::{self, Focus, SessionBrowser};
use crate::ui::widgets::accent;

struct RefreshEvent {
    generation: u64,
    target: String,
    snapshot: UsageSnapshot,
}

enum AppEvent {
    HomeActivityLoaded {
        generation: u64,
        result: Result<HashMap<sessions::SessionKey, crate::runs::SessionRuntime>, String>,
    },
    OverviewLoaded {
        generation: u64,
        result: Result<(crate::provider_catalog::Catalog, usize), String>,
    },
    ProviderModels {
        generation: u64,
        target: String,
        models: crate::provider_catalog::Models,
    },
    ProviderResource {
        generation: u64,
        target: String,
        snapshot: crate::resources::ResourceSnapshot,
    },
    ServiceStatus {
        generation: u64,
        snapshot: ServiceStatus,
    },
    Notice(String),
    HistoryCached(crate::history::History),
    HistoryLoaded {
        history: crate::history::History,
        cache: crate::history::HistoryCache,
    },
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
    HomeSessionsLoaded {
        generation: u64,
        catalog: sessions::SessionCatalog,
    },
    HomeSessionLaunched {
        generation: u64,
        result: Result<(), String>,
    },
}

enum ResetStage {
    Loading,
    Confirm(ResetPreview),
    Consuming,
}

struct ResetFlow {
    provider_id: String,
    target: String,
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

pub fn run_history(
    handle: Handle,
    config: AppConfig,
    config_path: std::path::PathBuf,
    year: i32,
    monthly: bool,
) -> io::Result<()> {
    run_history_with_query(
        handle,
        config,
        config_path,
        year,
        monthly,
        crate::history::HistoryQuery::default(),
    )
}

pub fn run_history_with_query(
    handle: Handle,
    config: AppConfig,
    config_path: std::path::PathBuf,
    year: i32,
    monthly: bool,
    query: crate::history::HistoryQuery,
) -> io::Result<()> {
    let mut app = Dashboard::new(Vec::new(), config, config_path);
    app.history = Some(HistoryPanel::new(
        app.history_config.providers.clone(),
        year,
        monthly,
    ));
    if let Some(panel) = &mut app.history {
        panel.set_query(query);
    }
    app.start_history_load(&handle);
    run_dashboard(handle, app)
}

fn run_dashboard(handle: Handle, mut app: Dashboard) -> io::Result<()> {
    let mut terminal = ratatui::try_init().map_err(io::Error::other)?;
    let mut redraw = RedrawSchedule::new();
    app.refresh(&handle);
    let result = loop {
        let (received, reset_consumed) = app.drain_events();
        if !app.alerts_queue.is_empty() {
            let alerts = std::mem::take(&mut app.alerts_queue);
            if app.history_config.alerts.bell {
                use std::io::Write;
                let _ = write!(io::stdout(), "\x07");
                let _ = io::stdout().flush();
            }
            if app.history_config.alerts.desktop {
                let tx = app.tx.clone();
                handle.spawn_blocking(move || {
                    for alert in alerts {
                        if let Err(e) = crate::alerts::notify_desktop(&alert.message) {
                            let _ = tx.send(AppEvent::Notice(e));
                        }
                    }
                });
            }
        }
        if reset_consumed {
            app.quota_refresh_due = app.pending > 0;
            app.refresh_background(&handle);
            app.notice = Some("已消耗 1 次，正在刷新额度".into());
        }
        let deferred_refresh = app.quota_refresh_due && app.pending == 0;
        if deferred_refresh {
            app.quota_refresh_due = false;
            app.refresh_background(&handle);
        }
        app.maybe_refresh_home_activity(&handle);
        let refresh_started = app.maybe_auto_refresh(&handle) || deferred_refresh;
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
                    if let Some(command) = app.handle_navigation_key(key.code) {
                        match command {
                            MenuCommand::Activate(MenuAction::Quit) => break Ok(()),
                            MenuCommand::Activate(MenuAction::Home) => {
                                app.close_history();
                                app.close_browser(false);
                                app.refresh_background(&handle);
                            }
                            MenuCommand::Activate(MenuAction::History) => app.open_history(&handle),
                            MenuCommand::Activate(MenuAction::Sessions) => {
                                app.browser_filter = None;
                                app.open_browser(&handle);
                            }
                            MenuCommand::Activate(MenuAction::Codex) => {
                                ratatui::restore();
                                let result = crate::ui::run_codex_with_config(
                                    handle.clone(),
                                    Some(app.config_path.clone()),
                                );
                                terminal = match ratatui::try_init() {
                                    Ok(t) => t,
                                    Err(e) => break Err(io::Error::other(e)),
                                };
                                app.refresh_background(&handle);
                                app.notice = result.err().map(|e| e.to_string());
                                for instance in app.history_config.data_instances() {
                                    if ["codex", "claude", "grok"].contains(&instance.id.as_str()) {
                                        continue;
                                    }
                                    app.session_homes = app.session_homes.clone().with_instance(
                                        &instance.id,
                                        &instance.provider,
                                        &instance.path,
                                    );
                                }
                            }
                            MenuCommand::Stay | MenuCommand::Dismiss => {}
                        }
                        redraw.invalidate();
                        continue;
                    }
                    if app.history.is_some() {
                        if let Some(panel) = &mut app.history
                            && panel.is_editing()
                        {
                            panel.key(key.code);
                            redraw.invalidate();
                            continue;
                        }
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => {
                                if app.providers.is_empty() {
                                    break Ok(());
                                }
                                app.close_history();
                            }
                            KeyCode::Char('r') => app.start_history_load(&handle),
                            KeyCode::Enter => app.open_history_sessions(&handle),
                            code => {
                                if let Some(panel) = &mut app.history {
                                    panel.key(code);
                                }
                            }
                        }
                        redraw.invalidate();
                        continue;
                    }
                    if app.browser.is_some() {
                        app.handle_browser_key(key.code, &handle);
                        if let Err(error) = fulfill_resume_request(&mut app, &handle, &mut terminal)
                        {
                            break Err(error);
                        }
                        redraw.invalidate();
                        continue;
                    }
                    if app.handle_home_key(
                        key.code,
                        &handle,
                        terminal
                            .size()
                            .map(|s| (s.width, s.height))
                            .unwrap_or((80, 24)),
                    ) {
                        if let Err(error) = fulfill_resume_request(&mut app, &handle, &mut terminal)
                        {
                            break Err(error);
                        }
                        redraw.invalidate();
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                        KeyCode::Char('r') => app.refresh(&handle),
                        KeyCode::Char('x') => app.open_reset(&handle),
                        KeyCode::Char('p') => {
                            app.browser_filter = None;
                            app.open_browser(&handle);
                        }
                        KeyCode::Char('h') => app.open_history(&handle),
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

fn resume_in_terminal(
    handle: &Handle,
    session: &sessions::Session,
) -> Result<std::process::ExitStatus, String> {
    // The child keeps the default SIGINT behavior. Keep the TUI parent alive when
    // Ctrl-C reaches the foreground process group while raw mode is suspended.
    let _runtime = handle.enter();
    #[cfg(unix)]
    let _interrupts = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|e| format!("无法准备终端中断处理：{e}"))?;
    #[cfg(windows)]
    let _interrupts =
        tokio::signal::windows::ctrl_c().map_err(|e| format!("无法准备终端中断处理：{e}"))?;
    sessions::resume_in_current(session)
}

fn fulfill_resume_request(
    app: &mut Dashboard,
    handle: &Handle,
    terminal: &mut ratatui::DefaultTerminal,
) -> io::Result<()> {
    let Some(session) = app.resume_request.take() else {
        return Ok(());
    };
    ratatui::restore();
    let result = resume_in_terminal(handle, &session);
    *terminal = ratatui::try_init().map_err(io::Error::other)?;
    if let Some(browser) = &mut app.browser {
        browser.launching = false;
    }
    app.home_sessions.launching = false;
    if app.history.is_none() && app.browser.is_none() {
        app.refresh_background(handle);
    }
    app.notice = Some(match result {
        Ok(status) if status.success() => "会话已退出，返回 Usage".into(),
        Ok(status) => format!("会话退出：{status}"),
        Err(error) => error,
    });
    Ok(())
}

struct Dashboard {
    home_activity: HashMap<sessions::SessionKey, crate::runs::SessionRuntime>,
    home_activity_error: Option<String>,
    home_activity_task: Option<tokio::task::JoinHandle<()>>,
    home_activity_pending: bool,
    home_activity_dirty: bool,
    home_activity_generation: u64,
    home_activity_last_poll: std::time::Instant,
    home_activity_root: PathBuf,
    home_focus: super::home::Focus,
    home_connections: super::home::Connections,
    home_sessions: SessionBrowser,
    home_sessions_loading: bool,
    home_sessions_generation: u64,
    home_sessions_warning: Option<String>,
    home_sessions_task: Option<tokio::task::JoinHandle<()>>,
    status_history: super::home::StatusHistory,
    overview: Option<crate::provider_catalog::Catalog>,
    overview_error: Option<String>,
    overview_loading: bool,
    overview_pending: usize,
    overview_generation: u64,
    overview_service: Arc<crate::provider_catalog::CatalogService>,
    overview_task: Option<tokio::task::JoinHandle<()>>,
    home_scroll: u16,
    home_models_expanded: bool,
    home_provider: Option<String>,
    service_statuses: HashMap<String, ServiceStatus>,
    service_generation: u64,
    service_pending: usize,
    service_http: Arc<dyn crate::http::HttpClient>,
    menu: Option<menu::Menu>,
    history: Option<HistoryPanel>,
    saved_history: Option<HistoryPanel>,
    saved_browser: Option<SessionBrowser>,
    loaded_history: crate::history::History,
    browser_filter: Option<(Vec<String>, crate::history::HistoryQuery)>,
    history_return: bool,
    last_history_refresh: std::time::Instant,
    history_config: AppConfig,
    history_cache: Option<crate::history::HistoryCache>,
    history_loading: bool,
    quota_refresh_due: bool,
    monitor: crate::alerts::QuotaMonitor,
    alerts_queue: Vec<crate::alerts::Alert>,
    resume_request: Option<sessions::Session>,
    providers: Vec<Arc<dyn UsageProvider>>,
    cards: Vec<CardSlot>,
    refresh_seconds: u64,
    config_path: std::path::PathBuf,
    status: String,
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

impl Drop for Dashboard {
    fn drop(&mut self) {
        if let Some(task) = self.home_activity_task.take() {
            task.abort();
        }
        if let Some(task) = self.overview_task.take() {
            task.abort();
        }
        if let Some(task) = self.home_sessions_task.take() {
            task.abort();
        }
    }
}

struct CardSlot {
    /// Configured resource identity controls result routing and account actions.
    target: String,
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
        let mut session_homes = sessions::SessionHomes::new()
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
        for instance in config.data_instances() {
            if ["codex", "claude", "grok"].contains(&instance.id.as_str()) {
                continue;
            }
            session_homes = session_homes.with_instance(
                &instance.id,
                &instance.provider,
                instance.path.clone(),
            );
        }
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
                target: provider.cache_key(),
                loading: true,
                snapshot: None,
                warning: None,
            })
            .collect();
        Self {
            home_activity: HashMap::new(),
            home_activity_error: None,
            home_activity_task: None,
            home_activity_pending: false,
            home_activity_dirty: true,
            home_activity_generation: 0,
            home_activity_last_poll: std::time::Instant::now(),
            home_activity_root: crate::codex_manager::default_root(),
            home_focus: super::home::Focus::Providers,
            home_connections: super::home::Connections::default(),
            home_sessions: {
                let mut browser = SessionBrowser::new(Vec::new());
                browser.resume_mode = config.resume;
                browser
            },
            home_sessions_loading: false,
            home_sessions_generation: 0,
            home_sessions_warning: None,
            home_sessions_task: None,
            status_history: super::home::StatusHistory::default(),
            overview: None,
            overview_error: None,
            overview_loading: false,
            overview_pending: 0,
            overview_generation: 0,
            overview_service: Arc::new(crate::provider_catalog::CatalogService::from_environment()),
            overview_task: None,
            home_scroll: 0,
            home_models_expanded: false,
            home_provider: None,
            service_statuses: HashMap::new(),
            service_generation: 0,
            service_pending: 0,
            service_http: Arc::new(crate::http::ReqwestClient::new()),
            menu: None,
            history: None,
            saved_history: None,
            saved_browser: None,
            loaded_history: crate::history::History::default(),
            browser_filter: None,
            history_return: false,
            last_history_refresh: std::time::Instant::now(),
            history_config: config.clone(),
            history_cache: Some(crate::history::HistoryCache::default()),
            history_loading: false,
            quota_refresh_due: false,
            monitor: crate::alerts::QuotaMonitor::default(),
            alerts_queue: Vec::new(),
            resume_request: None,
            providers,
            cards,
            status: "Provider · 厂商状态 · 项目会话".into(),
            refresh_seconds: config.refresh_seconds,
            config_path,
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

    /// Menu navigation is handled before page keys, so Enter cannot launch a session beneath it.
    fn handle_navigation_key(&mut self, key: KeyCode) -> Option<MenuCommand> {
        if self.reset.is_some()
            || self.browser.as_ref().is_some_and(|b| b.launching)
            || self.home_sessions.launching
        {
            return None;
        }
        if let Some(menu) = &mut self.menu {
            let command = menu.key(key);
            if command != MenuCommand::Stay {
                self.menu = None;
            }
            return Some(command);
        }
        if key == KeyCode::F(10)
            || (key == KeyCode::Char('m')
                && self.history.is_none()
                && self.browser.is_none()
                && !self.home_sessions.searching)
        {
            let current = if self.history.is_some() {
                MenuAction::History
            } else if self.browser.is_some() {
                MenuAction::Sessions
            } else {
                MenuAction::Home
            };
            self.menu = Some(menu::Menu::new(current));
            return Some(MenuCommand::Stay);
        }
        match key {
            KeyCode::F(1) => Some(MenuCommand::Activate(MenuAction::Home)),
            KeyCode::F(2) => Some(MenuCommand::Activate(MenuAction::History)),
            KeyCode::F(3) => Some(MenuCommand::Activate(MenuAction::Sessions)),
            KeyCode::F(4) => Some(MenuCommand::Activate(MenuAction::Codex)),
            _ => None,
        }
    }

    fn close_history(&mut self) {
        if let Some(panel) = self.history.take() {
            self.saved_history = Some(panel);
        }
    }
    fn close_browser(&mut self, return_history: bool) {
        if let Some(browser) = self.browser.take() {
            self.saved_browser = Some(browser);
        }
        self.browser_generation = self.browser_generation.wrapping_add(1);
        self.browser_loading = false;
        self.notice = None;
        if return_history && self.history_return {
            self.history = self.saved_history.take();
        }
        self.history_return = false;
    }
    fn open_history(&mut self, handle: &Handle) {
        use chrono::Datelike;
        if self.history.is_some() {
            return;
        }
        self.close_browser(false);
        self.history = Some(self.saved_history.take().unwrap_or_else(|| {
            HistoryPanel::new(
                self.history_config.providers.clone(),
                chrono::Local::now().year(),
                false,
            )
        }));
        self.start_history_load(handle);
    }
    fn open_history_sessions(&mut self, handle: &Handle) {
        let Some(panel) = &self.history else {
            return;
        };
        if panel.loading {
            return;
        }
        let filter = (panel.sources(), panel.session_query());
        self.close_history();
        self.saved_browser = None;
        self.browser_filter = Some(filter);
        self.open_browser(handle);
        self.history_return = true;
    }
    fn apply_browser_usage(&mut self) {
        let (sources, query, label) = self.browser_filter.as_ref().map_or_else(
            || {
                (
                    self.history_config.providers.clone(),
                    crate::history::HistoryQuery::default(),
                    None,
                )
            },
            |(s, q)| {
                (
                    s.clone(),
                    q.clone(),
                    Some(format!(
                        "{} — {} · {}",
                        q.from.map_or(String::new(), |d| d.to_string()),
                        q.to.map_or(String::new(), |d| d.to_string()),
                        s.join("+")
                    )),
                )
            },
        );
        let usage = self.loaded_history.session_usage(&sources, &query);
        if let Some(browser) = &mut self.browser {
            browser.set_usage(usage, label);
        }
    }
    fn start_history_load(&mut self, handle: &Handle) {
        if self.history_loading {
            return;
        }
        self.history_loading = true;
        self.last_history_refresh = std::time::Instant::now();
        if let Some(panel) = &mut self.history {
            panel.loading = true;
        }
        let config = self.history_config.clone();
        let mut cache = self.history_cache.take().unwrap_or_default();
        let tx = self.tx.clone();
        handle.spawn_blocking(move || {
            let history = cache.load_with_cached(&config, |history| {
                let _ = tx.send(AppEvent::HistoryCached(history));
            });
            let _ = tx.send(AppEvent::HistoryLoaded { history, cache });
        });
    }

    fn open_browser(&mut self, handle: &Handle) {
        if self.browser.is_some() {
            self.apply_browser_usage();
            return;
        }
        self.close_history();
        if self.providers.is_empty() && self.saved_history.is_some() {
            self.history_return = true;
        }
        self.browser = Some(self.saved_browser.take().unwrap_or_else(|| {
            let mut browser = SessionBrowser::new(Vec::new());
            browser.resume_mode = self.history_config.resume;
            browser
        }));
        self.apply_browser_usage();
        self.start_browser_load(handle);
        if self.loaded_history.synced_at.is_none() {
            self.start_history_load(handle);
        }
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
                self.close_browser(true);
            }
            KeyCode::Char('r') => {
                self.start_browser_load(handle);
                self.start_history_load(handle);
            }
            KeyCode::Char('R') => {
                if let Some(browser) = &mut self.browser {
                    browser.resume_mode = browser.resume_mode.next();
                }
            }
            KeyCode::Char('y') => {
                if let Some(session) = self
                    .browser
                    .as_ref()
                    .and_then(|b| b.selected_session())
                    .cloned()
                {
                    let tx = self.tx.clone();
                    handle.spawn_blocking(move || {
                        let result = sessions::copy_resume_command(&session)
                            .map(|_| "已复制恢复命令（含项目及实例目录）".into())
                            .unwrap_or_else(|e| e);
                        let _ = tx.send(AppEvent::Notice(result));
                    });
                }
            }
            KeyCode::Char('o') => {
                if let Some(browser) = &mut self.browser {
                    browser.toggle_sort();
                }
            }
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
                let mode = browser.resume_mode;
                self.launch_session(session, mode, false, handle);
            }
            _ => {}
        }
    }

    fn maybe_refresh_home_activity(&mut self, handle: &Handle) {
        if self.history.is_some()
            || self.browser.is_some()
            || self.home_activity_pending
            || self.home_sessions_loading
            || self.home_sessions.sessions.is_empty()
            || (!self.home_activity_dirty
                && (self.refresh_seconds == 0
                    || self.home_activity_last_poll.elapsed() < Duration::from_secs(2)))
        {
            return;
        }
        self.home_activity_dirty = false;
        self.home_activity_pending = true;
        self.home_activity_generation = self.home_activity_generation.wrapping_add(1);
        let generation = self.home_activity_generation;
        self.home_activity_last_poll = std::time::Instant::now();
        let root = self.home_activity_root.clone();
        let sessions = self.home_sessions.sessions.clone();
        let tx = self.tx.clone();
        self.home_activity_task = Some(handle.spawn_blocking(move || {
            let result = (|| {
                let Some(manager) = crate::codex_manager::Manager::open_read_only(root)
                    .map_err(|_| "运行状态读取失败".to_string())?
                else {
                    return Ok(HashMap::new());
                };
                manager
                    .session_activity(&sessions)
                    .map_err(|_| "运行状态读取失败".to_string())
            })();
            let _ = tx.send(AppEvent::HomeActivityLoaded { generation, result });
        }));
    }

    fn start_home_sessions_load(&mut self, handle: &Handle) {
        if self.home_sessions_loading || self.home_sessions.launching {
            return;
        }
        self.home_sessions_loading = true;
        self.home_sessions_generation = self.home_sessions_generation.wrapping_add(1);
        let generation = self.home_sessions_generation;
        let mut homes = self.session_homes.clone();
        let registry = self.session_registry.clone();
        let tx = self.tx.clone();
        self.home_sessions_task = Some(handle.spawn_blocking(move || {
            // Discover newly materialized managed homes without blocking terminal input.
            if let Ok(instances) = crate::codex_manager::managed_instances() {
                for instance in instances {
                    homes = homes.with_instance(&instance.id, &instance.provider, &instance.path);
                }
            }
            let catalog = registry.load_all(&homes);
            let _ = tx.send(AppEvent::HomeSessionsLoaded {
                generation,
                catalog,
            });
        }));
    }

    fn launch_session(
        &mut self,
        session: sessions::Session,
        mode: crate::config::ResumeMode,
        home: bool,
        handle: &Handle,
    ) {
        if !sessions::uses_ghostty(mode) {
            self.resume_request = Some(session);
            return;
        }
        self.notice = Some("正在 Ghostty 右侧打开会话…".into());
        let tx = self.tx.clone();
        let generation = if home {
            self.home_sessions_generation
        } else {
            self.browser_generation
        };
        handle.spawn_blocking(move || {
            let result = sessions::resume_in_ghostty(&session);
            let event = if home {
                AppEvent::HomeSessionLaunched { generation, result }
            } else {
                AppEvent::SessionLaunched { generation, result }
            };
            let _ = tx.send(event);
        });
    }

    fn handle_home_key(
        &mut self,
        key: KeyCode,
        handle: &Handle,
        (width, height): (u16, u16),
    ) -> bool {
        use super::home::Focus as HomeFocus;
        if self.home_sessions.launching {
            return true;
        }
        if self.home_focus == HomeFocus::Projects && self.home_sessions.searching {
            match key {
                KeyCode::Esc | KeyCode::Enter => self.home_sessions.searching = false,
                KeyCode::Backspace => self.home_sessions.backspace(),
                KeyCode::Char(ch) => self.home_sessions.type_char(ch),
                _ => {}
            }
            return true;
        }
        match key {
            KeyCode::Tab | KeyCode::BackTab => {
                self.home_focus = self.home_focus.next(key == KeyCode::BackTab)
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k') => {
                let delta = if matches!(key, KeyCode::Up | KeyCode::Char('k')) {
                    -1
                } else {
                    1
                };
                match self.home_focus {
                    HomeFocus::Providers => {
                        let area =
                            super::home::areas(Rect::new(0, 1, width, height.saturating_sub(2)))[0];
                        self.scroll_home(delta, (area.width.saturating_sub(2), area.height));
                    }
                    HomeFocus::Connections => {
                        let area =
                            super::home::areas(Rect::new(0, 1, width, height.saturating_sub(2)))[1];
                        let inner = Rect::new(
                            0,
                            0,
                            area.width.saturating_sub(2),
                            area.height.saturating_sub(2),
                        );
                        self.home_connections.move_by(
                            delta,
                            inner,
                            &self.service_statuses,
                            &self.status_history,
                            self.service_pending > 0,
                        );
                    }
                    HomeFocus::Projects => self.home_sessions.move_by(delta as isize),
                }
            }
            KeyCode::Left | KeyCode::Right if self.home_focus == HomeFocus::Providers => {
                self.cycle_home_provider(key == KeyCode::Left);
            }
            KeyCode::Enter if self.home_focus == HomeFocus::Connections => {
                self.home_connections.open();
            }
            KeyCode::Esc
                if self.home_focus == HomeFocus::Connections
                    && self.home_connections.is_details() =>
            {
                self.home_connections.back();
            }
            KeyCode::Enter if self.home_focus == HomeFocus::Providers => {
                self.home_models_expanded = !self.home_models_expanded;
                self.home_scroll = 0;
            }
            KeyCode::Esc
                if self.home_focus == HomeFocus::Providers && self.home_models_expanded =>
            {
                self.home_models_expanded = false;
                self.home_scroll = 0;
            }
            KeyCode::Left if self.home_focus == HomeFocus::Projects => {
                self.home_sessions.focus_projects()
            }
            KeyCode::Right if self.home_focus == HomeFocus::Projects => {
                self.home_sessions.focus_sessions()
            }
            KeyCode::Esc
                if self.home_focus == HomeFocus::Projects
                    && self.home_sessions.focus() == Focus::Sessions =>
            {
                self.home_sessions.focus_projects()
            }
            KeyCode::Char('/') if self.home_focus == HomeFocus::Projects => {
                self.home_sessions.searching = true
            }
            KeyCode::Char('c') if self.home_focus == HomeFocus::Projects => {
                self.home_sessions.clear_search()
            }
            KeyCode::Char('R') if self.home_focus == HomeFocus::Projects => {
                self.home_sessions.resume_mode = self.home_sessions.resume_mode.next()
            }
            KeyCode::Enter => {
                if self.home_sessions_loading {
                    return true;
                }
                if self.home_sessions.focus() == Focus::Projects {
                    self.home_sessions.focus_sessions();
                    return true;
                }
                if let Some(session) = self.home_sessions.selected_session().cloned() {
                    self.home_sessions.launching = true;
                    self.launch_session(session, self.home_sessions.resume_mode, true, handle);
                }
            }
            _ => return false,
        }
        true
    }

    fn cycle_home_provider(&mut self, reverse: bool) {
        let Some(catalog) = self.overview.as_ref().filter(|c| c.providers.len() > 1) else {
            return;
        };
        let index = super::overview::selected(Some(catalog), self.home_provider.as_deref())
            .map_or(0, |(index, _)| index);
        let count = catalog.providers.len();
        let next = if reverse {
            (index + count - 1) % count
        } else {
            (index + 1) % count
        };
        self.home_provider = Some(catalog.providers[next].id.clone());
        self.home_scroll = 0;
        self.home_models_expanded = false;
    }

    fn scroll_home(&mut self, delta: i16, (width, height): (u16, u16)) {
        let max = home_lines(self, width)
            .len()
            .saturating_sub(height.saturating_sub(2) as usize)
            .min(u16::MAX as usize) as i32;
        self.home_scroll =
            (i32::from(self.home_scroll) + i32::from(delta) * 3).clamp(0, max) as u16;
    }

    fn refresh(&mut self, handle: &Handle) {
        self.refresh_with_models(handle, true);
    }

    fn refresh_background(&mut self, handle: &Handle) {
        self.refresh_with_models(handle, false);
    }

    fn refresh_with_models(&mut self, handle: &Handle, refresh_models: bool) {
        if self.history.is_none() && self.browser.is_none() {
            if refresh_models {
                self.refresh_overview(handle);
            } else {
                self.refresh_overview_with_models(handle, false);
            }
            self.start_home_sessions_load(handle);
        }
        self.refresh_service_statuses(handle);
        if self.pending > 0 {
            return;
        }
        self.notice = None;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.pending = self.providers.len();
        self.last_refresh = std::time::Instant::now();
        self.status = "正在刷新…".into();
        for provider in &self.providers {
            let provider = Arc::clone(provider);
            let tx = self.tx.clone();
            handle.spawn(
                async move {
                    let target = provider.cache_key();
                    let snapshot = query_with_timeout(provider.as_ref(), QUERY_TIMEOUT).await;
                    let _ = tx.send(AppEvent::Quota(Box::new(RefreshEvent {
                        generation,
                        target,
                        snapshot,
                    })));
                }
                .in_current_span(),
            );
        }
    }

    fn refresh_overview(&mut self, handle: &Handle) {
        self.refresh_overview_with_models(handle, true);
    }

    fn refresh_overview_with_models(&mut self, handle: &Handle, refresh_models: bool) {
        if !refresh_models && self.overview_loading {
            return;
        }
        if let Some(task) = self.overview_task.take() {
            task.abort();
        }
        self.overview_generation = self.overview_generation.wrapping_add(1);
        let generation = self.overview_generation;
        self.overview_loading = true;
        self.overview_error = None;
        let previous = self.overview.clone();
        let service = self.overview_service.clone();
        let http = service.http.clone();
        let path = self.config_path.clone();
        let tx = self.tx.clone();
        self.overview_task = Some(
            handle.spawn(
                async move {
                    let plan = tokio::task::spawn_blocking(move || {
                        let config = crate::config::load_config(&path, false)
                            .map_err(|_| "首页配置读取失败，请检查配置文件".to_string())?;
                        let mut plan = service.load(&config)?;
                        if let Some(previous) = previous {
                            plan.reuse(&previous, refresh_models);
                        }
                        Ok::<_, String>(plan)
                    })
                    .await;
                    match plan {
                        Ok(Ok(plan)) => {
                            let _ = tx.send(AppEvent::OverviewLoaded {
                                generation,
                                result: Ok((
                                    plan.catalog,
                                    plan.queries.len() + plan.resources.len(),
                                )),
                            });
                            tokio::join!(
                                crate::provider_catalog::collect_models(
                                    http,
                                    plan.queries,
                                    |target, models| {
                                        let _ = tx.send(AppEvent::ProviderModels {
                                            generation,
                                            target,
                                            models,
                                        });
                                    }
                                ),
                                crate::resources::collect_with_updates(
                                    &plan.resources,
                                    QUERY_TIMEOUT,
                                    |index, snapshot| {
                                        let reader = &plan.resources[index];
                                        let _ = tx.send(AppEvent::ProviderResource {
                                            generation,
                                            target: reader.binding().cache_key(reader.service()),
                                            snapshot: snapshot.clone(),
                                        });
                                    }
                                )
                            );
                        }
                        result => {
                            let message = match result {
                                Ok(Err(message)) => message,
                                _ => "首页读取任务中断".into(),
                            };
                            let _ = tx.send(AppEvent::OverviewLoaded {
                                generation,
                                result: Err(message),
                            });
                        }
                    }
                }
                .in_current_span(),
            ),
        );
    }

    fn refresh_service_statuses(&mut self, handle: &Handle) {
        if self.service_pending > 0 {
            return;
        }
        let sources = super::home::SOURCES;
        self.service_generation = self.service_generation.wrapping_add(1);
        let generation = self.service_generation;
        self.service_pending = sources.len();
        for source in sources {
            let http = Arc::clone(&self.service_http);
            let tx = self.tx.clone();
            handle.spawn(async move {
                let snapshot = service_status::query(http.as_ref(), source).await;
                let _ = tx.send(AppEvent::ServiceStatus {
                    generation,
                    snapshot,
                });
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
            target: provider.cache_key(),
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
            .find(|provider| provider.cache_key() == flow.target)
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
        if self.history.is_some() || self.browser.is_some() {
            if self.browser.as_ref().is_some_and(|b| b.launching) {
                return false;
            }
            if self.refresh_seconds > 0
                && !self.history_loading
                && self.last_history_refresh.elapsed() >= Duration::from_secs(self.refresh_seconds)
            {
                if self.browser.is_some() {
                    self.start_browser_load(handle);
                }
                self.start_history_load(handle);
                return true;
            }
            return false;
        }
        if self.refresh_seconds == 0
            || self.pending > 0
            || self.overview_loading
            || self.reset.is_some()
            || self.browser.is_some()
        {
            return false;
        }
        if self.last_refresh.elapsed() >= Duration::from_secs(self.refresh_seconds) {
            self.refresh_background(handle);
            return true;
        }
        false
    }

    fn drain_events(&mut self) -> (bool, bool) {
        let mut changed = false;
        let mut reset_consumed = false;
        while let Ok(event) = self.rx.try_recv() {
            let event = match event {
                AppEvent::HomeActivityLoaded { generation, result } => {
                    if generation == self.home_activity_generation {
                        self.home_activity_pending = false;
                        match result {
                            Ok(activity) => {
                                changed |= self.home_activity != activity
                                    || self.home_activity_error.is_some();
                                self.home_activity = activity;
                                self.home_activity_error = None;
                            }
                            Err(error) => {
                                changed |= !self.home_activity.is_empty()
                                    || self.home_activity_error.as_ref() != Some(&error);
                                self.home_activity.clear();
                                self.home_activity_error = Some(error);
                            }
                        }
                    }
                    continue;
                }
                AppEvent::HomeSessionsLoaded {
                    generation,
                    catalog,
                } => {
                    if generation == self.home_sessions_generation {
                        self.home_sessions_loading = false;
                        self.home_sessions_warning = if catalog.warnings.is_empty() {
                            None
                        } else {
                            Some(catalog.warnings.join(" · "))
                        };
                        self.home_sessions.refresh(catalog.sessions);
                        if let Some(task) = self.home_activity_task.take() {
                            task.abort();
                        }
                        self.home_activity_generation =
                            self.home_activity_generation.wrapping_add(1);
                        self.home_activity_pending = false;
                        self.home_activity_dirty = true;
                        let keys: HashSet<_> = self
                            .home_sessions
                            .sessions
                            .iter()
                            .map(sessions::Session::key)
                            .collect();
                        self.home_activity.retain(|key, _| keys.contains(key));
                        changed = true;
                    }
                    continue;
                }
                AppEvent::HomeSessionLaunched { generation, result } => {
                    if generation == self.home_sessions_generation {
                        self.home_sessions.launching = false;
                        self.notice = Some(match result {
                            Ok(()) => "已在 Ghostty 右侧恢复会话".into(),
                            Err(error) => error,
                        });
                        changed = true;
                    }
                    continue;
                }
                AppEvent::OverviewLoaded { generation, result } => {
                    if generation == self.overview_generation {
                        match result {
                            Ok((snapshot, pending)) => {
                                let selected = super::overview::selected(
                                    Some(&snapshot),
                                    self.home_provider.as_deref(),
                                )
                                .map(|(_, p)| p.id.clone());
                                if selected != self.home_provider {
                                    self.home_scroll = 0;
                                    self.home_models_expanded = false;
                                }
                                self.home_provider = selected;
                                self.overview = Some(snapshot);
                                self.overview_pending = pending;
                                self.overview_loading = pending > 0;
                                self.overview_error = None;
                            }
                            Err(message) => {
                                self.overview = None;
                                self.overview_error = Some(message);
                                self.overview_pending = 0;
                                self.overview_loading = false;
                            }
                        }
                        changed = true;
                    }
                    continue;
                }
                AppEvent::ProviderModels {
                    generation,
                    target,
                    models,
                } => {
                    if generation == self.overview_generation
                        && self
                            .overview
                            .as_mut()
                            .is_some_and(|catalog| catalog.apply_models(&target, models))
                    {
                        self.overview_pending = self.overview_pending.saturating_sub(1);
                        self.overview_loading = self.overview_pending > 0;
                        changed = true;
                    }
                    continue;
                }
                AppEvent::ProviderResource {
                    generation,
                    target,
                    snapshot,
                } => {
                    if generation == self.overview_generation
                        && self
                            .overview
                            .as_mut()
                            .is_some_and(|catalog| catalog.apply_resource(&target, snapshot))
                    {
                        self.overview_pending = self.overview_pending.saturating_sub(1);
                        self.overview_loading = self.overview_pending > 0;
                        changed = true;
                    }
                    continue;
                }
                AppEvent::ServiceStatus {
                    generation,
                    snapshot,
                } => {
                    if generation == self.service_generation {
                        self.service_pending = self.service_pending.saturating_sub(1);
                        let id = snapshot.provider.clone();
                        self.status_history.record(&snapshot);
                        let previous = self.service_statuses.remove(&id);
                        self.service_statuses
                            .insert(id, ServiceStatus::merge(previous, snapshot));
                        changed = true;
                    }
                    continue;
                }
                AppEvent::Notice(message) => {
                    self.notice = Some(message);
                    changed = true;
                    continue;
                }
                AppEvent::HistoryCached(history) => {
                    self.loaded_history = history.clone();
                    if let Some(panel) = self.history.as_mut().or(self.saved_history.as_mut()) {
                        panel.history = history;
                    }
                    self.apply_browser_usage();
                    changed = true;
                    continue;
                }
                AppEvent::HistoryLoaded { history, cache } => {
                    self.history_loading = false;
                    self.history_cache = Some(cache);
                    self.loaded_history = history.clone();
                    if let Some(panel) = self.history.as_mut().or(self.saved_history.as_mut()) {
                        panel.history = history;
                        panel.loading = false;
                    }
                    self.apply_browser_usage();
                    changed = true;
                    continue;
                }
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
                                if let Some(browser) = &mut self.browser {
                                    browser.launching = false;
                                }
                                self.close_browser(true);
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
                            if self.browser_filter.is_none() {
                                self.close_browser(true);
                            }
                            self.notice = Some(message);
                        } else {
                            if let Some(browser) = &mut self.browser {
                                browser.refresh(catalog.sessions);
                            }
                            self.apply_browser_usage();
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
            let Some(card) = self
                .cards
                .iter_mut()
                .find(|card| card.target == event.target)
            else {
                continue;
            };
            changed = true;
            self.pending = self.pending.saturating_sub(1);
            let alerts = self.monitor.observe(
                &event.target,
                &event.snapshot,
                &self.history_config.alerts,
                Utc::now(),
            );
            if !alerts.is_empty() {
                self.notice = Some(
                    alerts
                        .iter()
                        .map(|a| a.message.as_str())
                        .collect::<Vec<_>>()
                        .join(" · "),
                );
                self.alerts_queue.extend(alerts);
            }
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
                self.status = format!("已更新 {ok}/{} 个订阅", self.cards.len());
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
    let status = if app.history.is_some() {
        app.history
            .as_ref()
            .and_then(|p| p.history.synced_at)
            .map_or_else(
                || "本地 Token · 本地时区".into(),
                |t| {
                    format!(
                        "本地 Token · 更新 {}{}",
                        t.with_timezone(&chrono::Local).format("%H:%M:%S"),
                        if app.history_loading {
                            " · 同步中"
                        } else {
                            ""
                        }
                    )
                },
            )
    } else if app.browser_loading {
        "正在读取本机会话…".to_string()
    } else {
        app.notice.clone().unwrap_or_else(|| {
            app.browser.as_ref().map_or_else(
                || {
                    app.overview_error.clone().unwrap_or_else(|| {
                        if app.overview_loading {
                            "正在更新 Provider 状态…".into()
                        } else {
                            "Provider · 厂商状态 · 项目与会话".into()
                        }
                    })
                },
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
    let header_title = if app.history.is_some() {
        " F10 菜单  F1 首页  [F2 历史]  F3 会话  F4 账号 "
    } else if app.browser.is_some() {
        " F10 菜单  F1 首页  F2 历史  [F3 会话]  F4 账号 "
    } else {
        " F10 菜单  [F1 首页]  F2 历史  F3 会话  F4 账号 "
    };
    Paragraph::new(Line::from(vec![header_title.bold(), status.dim()]))
        .style(
            Style::new()
                .bg(Color::Rgb(22, 22, 24))
                .fg(Color::Rgb(242, 242, 247)),
        )
        .render(header, frame.buffer_mut());
    if let Some(panel) = &app.history {
        history::render(body, frame.buffer_mut(), panel);
    } else if let Some(browser) = &app.browser {
        if app.browser_loading && browser.sessions.is_empty() {
            Paragraph::new(" 正在扫描 Claude Code、Codex 和 Grok 的本机会话…")
                .style(Style::new().fg(Color::Rgb(142, 142, 147)))
                .render(body, frame.buffer_mut());
        } else {
            session_browser::render(body, frame.buffer_mut(), browser);
        }
    } else {
        render_home(body, frame.buffer_mut(), app);
    }
    let footer_text = if app.history.is_some() {
        if frame.area().width >= 105 {
            " ↑新 ↓旧  ←/→ 周  Enter 会话  v 范围  f 项目  M 模型  i 实例  m 日/月  Tab 来源  c 清筛选  r 更新  Esc 返回"
        } else {
            " ↑新 ↓旧  Enter 会话  v 范围  f 项目  M 模型  i 实例  m 日/月  r 更新  Esc 返回"
        }
    } else if let Some(browser) = &app.browser {
        if app.browser_loading {
            " 正在读取本机会话… Esc 返回"
        } else if browser.launching {
            " 正在 Ghostty 右侧打开会话… 请等待"
        } else {
            " ↑/↓ 选择  Tab 列表  Enter 打开  / 搜索  o 排序  R 恢复方式  y 复制  c 清除  r 更新  Esc 返回"
        }
    } else {
        match app.reset.as_ref().map(|flow| &flow.stage) {
            Some(ResetStage::Loading) => " 正在查询重置次数…   n/Esc 取消",
            Some(ResetStage::Confirm(_)) => " y 确认消耗 1 次   n/Esc 取消",
            Some(ResetStage::Consuming) => " 正在提交主动重置，请等待结果…",
            None if app.home_sessions.launching => " 正在恢复会话，请等待…",
            None => match app.home_focus {
                super::home::Focus::Providers => {
                    " Tab 看板  ←/→ 切换 Provider  ↑/↓ 滚动信息  Enter 展开/收起模型  r 刷新  F4 管理  q 退出"
                }
                super::home::Focus::Connections => {
                    if app.home_connections.is_details() {
                        " ↑/↓ 滚动详情  Esc 返回状态列表  Tab 看板  r 刷新  q 退出"
                    } else {
                        " Tab 看板  ↑/↓ 选择厂商  Enter 事件与历史  r 刷新  q 退出"
                    }
                }
                super::home::Focus::Projects => {
                    " Tab 看板  ↑/↓ 选择  ←项目 →会话  Enter 恢复  / 搜索  R 恢复方式  r 刷新  q 退出"
                }
            },
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
    if let Some(menu) = &app.menu {
        menu.render(frame);
    }
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

fn home_lines(app: &Dashboard, width: u16) -> Vec<Line<'static>> {
    super::overview::wrap(
        super::overview::lines(
            app.overview.as_ref(),
            app.overview_loading,
            app.overview_error.as_deref(),
            app.home_models_expanded,
            app.home_provider.as_deref(),
        ),
        width,
    )
}

fn render_home(area: Rect, buf: &mut Buffer, app: &Dashboard) {
    use super::home::{self, Focus as HomeFocus};
    let [providers, connections, projects] = home::areas(area);
    let provider_title =
        super::overview::selected(app.overview.as_ref(), app.home_provider.as_deref())
            .map(|(index, provider)| {
                format!(
                    "Provider · {} / {} · {}",
                    index + 1,
                    app.overview.as_ref().unwrap().providers.len(),
                    super::overview::clean(&provider.name)
                )
            })
            .unwrap_or_else(|| "Provider".into());
    let inner = home::board(
        providers,
        buf,
        &provider_title,
        app.home_focus == HomeFocus::Providers,
    );
    render_provider_contents(inner, buf, app);
    let inner = home::board(
        connections,
        buf,
        "官方 API · 厂商状态",
        app.home_focus == HomeFocus::Connections,
    );
    app.home_connections.render(
        inner,
        buf,
        &app.service_statuses,
        &app.status_history,
        app.service_pending > 0,
        app.home_focus == HomeFocus::Connections,
    );
    let mode = if sessions::uses_ghostty(app.home_sessions.resume_mode) {
        "分屏"
    } else {
        "当前终端"
    };
    let title = if app.home_sessions.query.is_empty() {
        format!(
            "项目与会话 · {} 项目 · {mode}{}",
            app.home_sessions.project_count(),
            if app.home_sessions_loading {
                " · 更新中"
            } else {
                ""
            }
        )
    } else {
        format!(
            "项目与会话 · 搜索 {}",
            super::overview::clean(&app.home_sessions.query)
        )
    };
    let mut inner = home::board(projects, buf, &title, app.home_focus == HomeFocus::Projects);
    if let Some(warning) = app
        .home_sessions_warning
        .as_ref()
        .or(app.home_activity_error.as_ref())
    {
        Paragraph::new(format!("读取警告 · {}", super::overview::clean(warning)))
            .fg(Color::Yellow)
            .render(
                Rect {
                    height: inner.height.min(1),
                    ..inner
                },
                buf,
            );
        inner.y = inner.y.saturating_add(1);
        inner.height = inner.height.saturating_sub(1);
    }
    if app.home_sessions.sessions.is_empty() {
        let message = if app.home_sessions_loading {
            "正在读取本地项目与会话…"
        } else {
            "尚无可恢复会话 · F4 启动 Agent · F3 完整会话页"
        };
        Paragraph::new(message)
            .wrap(Wrap { trim: false })
            .dim()
            .render(inner, buf);
    } else {
        session_browser::render_compact(inner, buf, &app.home_sessions, &|session| {
            let account = if let Some(binding) = session.instance.strip_prefix("managed-") {
                if let Some((id, revision)) = binding.rsplit_once("-r")
                    && let Some(account) = app.overview.as_ref().and_then(|snapshot| {
                        snapshot.providers.iter().find(|account| account.id == id)
                    })
                {
                    format!("{} · r{revision}", account.name)
                } else {
                    "托管账号（名称未加载）".into()
                }
            } else if session.instance == session.provider.id() {
                session.provider.name().into()
            } else {
                session.instance.clone()
            };
            match app.home_activity.get(&session.key()) {
                Some(activity) if !activity.runs.is_empty() => {
                    format!("{account} · {}", activity.label())
                }
                _ => account,
            }
        });
    }
}

fn render_provider_contents(area: Rect, buf: &mut Buffer, app: &Dashboard) {
    if area.is_empty() {
        return;
    }
    let lines = home_lines(app, area.width);
    let max = lines
        .len()
        .saturating_sub(usize::from(area.height))
        .min(u16::MAX as usize) as u16;
    Paragraph::new(lines)
        .scroll((app.home_scroll.min(max), 0))
        .render(area, buf);
}

#[cfg(test)]
fn isolated_config() -> AppConfig {
    AppConfig {
        claude_dir: Some("/nonexistent/usage-ui-fixture/claude".into()),
        codex_dir: Some("/nonexistent/usage-ui-fixture/codex".into()),
        grok_dir: Some("/nonexistent/usage-ui-fixture/grok".into()),
        ..AppConfig::default()
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
        target: &'static str,
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

        fn cache_key(&self) -> String {
            self.target.into()
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
        let mut app = Dashboard::new(
            vec![Arc::new(ResetTestProvider {
                target: "codex",
                available,
                consumed,
            })],
            isolated_config(),
            "/tmp/usage-reset-test.toml".into(),
        );
        app.service_http = Arc::new(crate::http::FakeHttp::new());
        app
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
        let mut app = Dashboard::new(Vec::new(), isolated_config(), "/tmp/usage-test.toml".into());
        app.service_http = Arc::new(crate::http::FakeHttp::new());
        app.cards = vec![
            CardSlot {
                target: "claude".into(),
                loading: false,
                snapshot: Some(snapshot_not_found("claude", "Claude Code", None)),
                warning: None,
            },
            CardSlot {
                target: "codex".into(),
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
                target: "grok".into(),
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
                agent: None,
                provider: AgentKind::CLAUDE,
                instance: "claude".into(),
                id: "123e4567-e89b-12d3-a456-426614174000".into(),
                cwd: "/tmp/project".into(),
                title: "Claude task".into(),
                recency_at_ms: 0,
                home: "/tmp/claude".into(),
            },
            Session {
                agent: None,
                provider: AgentKind::CODEX,
                instance: "codex".into(),
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
            isolated_config(),
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
                target: "codex".into(),
                snapshot: snapshot_not_found("codex", "Codex", None),
            })))
            .unwrap();
        assert_eq!(app.drain_events(), (false, false));
        assert_eq!(app.pending, 1);
        app.tx
            .send(AppEvent::Quota(Box::new(RefreshEvent {
                generation: 2,
                target: "codex".into(),
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
    async fn reset_consumes_the_confirmed_account_even_when_another_has_the_same_tool() {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let mut app = Dashboard::new(
            vec![
                Arc::new(ResetTestProvider {
                    target: "a",
                    available: 2,
                    consumed: first.clone(),
                }),
                Arc::new(ResetTestProvider {
                    target: "b",
                    available: 2,
                    consumed: second.clone(),
                }),
            ],
            isolated_config(),
            "/tmp/usage-reset-accounts.toml".into(),
        );
        app.reset = Some(ResetFlow {
            provider_id: "codex".into(),
            target: "b".into(),
            provider_name: "Codex".into(),
            generation: 0,
            stage: ResetStage::Confirm(ResetPreview {
                available: 2,
                next_expires_at: None,
                account_label: Some("b".into()),
            }),
        });
        app.handle_reset_key(KeyCode::Char('y'), &Handle::current());
        drain_until(&mut app, |app| app.reset.is_none()).await;
        assert_eq!(first.load(Ordering::SeqCst), 0);
        assert_eq!(second.load(Ordering::SeqCst), 1);
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
            target: "codex".into(),
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
    fn main_menu_routes_codex_and_dismisses_without_changing_the_current_page() {
        let mut app = reset_dashboard(2, Arc::new(AtomicUsize::new(0)));
        assert_eq!(
            app.handle_navigation_key(KeyCode::Char('m')),
            Some(MenuCommand::Stay)
        );
        assert!(app.menu.is_some());
        for _ in 0..3 {
            assert_eq!(
                app.handle_navigation_key(KeyCode::Down),
                Some(MenuCommand::Stay)
            );
        }
        assert_eq!(
            app.handle_navigation_key(KeyCode::Enter),
            Some(MenuCommand::Activate(MenuAction::Codex))
        );
        assert!(app.menu.is_none());
        assert_eq!(
            app.handle_navigation_key(KeyCode::F(4)),
            Some(MenuCommand::Activate(MenuAction::Codex))
        );
        app.history = Some(HistoryPanel::new(vec!["codex".into()], 2026, true));
        assert_eq!(app.handle_navigation_key(KeyCode::Char('m')), None);
        assert_eq!(
            app.handle_navigation_key(KeyCode::F(10)),
            Some(MenuCommand::Stay)
        );
        assert_eq!(
            app.handle_navigation_key(KeyCode::Esc),
            Some(MenuCommand::Dismiss)
        );
        assert!(app.menu.is_none());
        assert!(app.history.as_ref().unwrap().monthly);
    }

    #[test]
    fn menu_does_not_intercept_reset_confirmation() {
        let mut app = reset_dashboard(2, Arc::new(AtomicUsize::new(0)));
        app.reset = Some(ResetFlow {
            provider_id: "codex".into(),
            target: "codex".into(),
            provider_name: "Codex".into(),
            generation: 1,
            stage: ResetStage::Loading,
        });
        assert_eq!(app.handle_navigation_key(KeyCode::F(10)), None);
        assert!(app.menu.is_none());
    }

    #[tokio::test]
    async fn homepage_status_updates_without_quota_credentials_and_marks_failed_refresh() {
        let mut app = dashboard();
        let http = Arc::new(crate::http::FakeHttp::new());
        http.route(
            service_status::CLAUDE.endpoint,
            Ok(crate::http::HttpResponse {
                status: 200,
                headers: Default::default(),
                body: br#"{"status":{"indicator":"none","description":"OK"},"components":[]}"#
                    .to_vec(),
            }),
        );
        app.service_http = http.clone();
        app.refresh_service_statuses(&Handle::current());
        app.refresh_service_statuses(&Handle::current());
        drain_until(&mut app, |a| a.service_pending == 0).await;
        assert_eq!(http.calls.lock().unwrap().len(), 3);
        assert_eq!(app.pending, 0);
        assert!(!app.cards[0].snapshot.as_ref().unwrap().success);
        let time = app.service_statuses["claude"].fetched_at;
        for width in [28, 72, 110] {
            let area = Rect::new(0, 0, width, 32);
            let mut buffer = Buffer::empty(area);
            render_home(area, &mut buffer, &app);
            let text = buffer
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            let compact = text.split_whitespace().collect::<String>();
            assert!(compact.contains("Claude·正常"), "{width}: {text}");
        }
        app.service_http = Arc::new(crate::http::FakeHttp::new());
        app.refresh_service_statuses(&Handle::current());
        drain_until(&mut app, |a| a.service_pending == 0).await;
        assert_eq!(app.service_statuses["claude"].fetched_at, time);
        assert!(app.service_statuses["claude"].stale);
        let area = Rect::new(0, 0, 72, 32);
        let mut buffer = Buffer::empty(area);
        render_home(area, &mut buffer, &app);
        let text = buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            text.split_whitespace()
                .collect::<String>()
                .contains("更新失败"),
            "{text}"
        );
        let stale_cell = buffer.content.iter().find(|c| c.symbol() == "○").unwrap();
        assert_eq!(stale_cell.fg, Color::DarkGray);
    }

    #[tokio::test]
    async fn old_status_events_do_not_replace_the_current_generation() {
        let mut app = dashboard();
        app.service_generation = 2;
        app.service_pending = 1;
        let snapshot =
            service_status::query(&crate::http::FakeHttp::new(), service_status::CLAUDE).await;
        app.tx
            .send(AppEvent::ServiceStatus {
                generation: 1,
                snapshot,
            })
            .unwrap();
        assert_eq!(app.drain_events(), (false, false));
        assert!(app.service_statuses.is_empty());
        assert_eq!(app.service_pending, 1);
    }

    struct CountingQuota(Arc<AtomicUsize>);
    #[async_trait]
    impl UsageProvider for CountingQuota {
        fn id(&self) -> &'static str {
            "codex"
        }
        fn display_name(&self) -> &'static str {
            "Codex"
        }
        async fn query(&self) -> UsageSnapshot {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            snapshot_not_found("codex", "Codex", None)
        }
    }
    #[tokio::test]
    async fn repeated_quota_refresh_joins_pending_work() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut app = Dashboard::new(
            vec![Arc::new(CountingQuota(calls.clone()))],
            isolated_config(),
            "/tmp/test-config".into(),
        );
        app.service_http = Arc::new(crate::http::FakeHttp::new());
        let handle = Handle::current();
        app.refresh(&handle);
        app.refresh(&handle);
        app.refresh(&handle);
        drain_until(&mut app, |a| a.pending == 0).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn switching_history_preserves_date_source_and_monthly_view() {
        use chrono::Datelike;
        let root = std::env::temp_dir().join(format!("usage-ui-{}", uuid::Uuid::new_v4()));
        let mut app = dashboard();
        app.history_config.providers.clear();
        app.history_cache = Some(crate::history::HistoryCache::at(
            root.join("history.sqlite3"),
        ));
        let date = chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        app.history = Some(HistoryPanel::new(
            vec!["codex".into(), "claude".into()],
            date.year(),
            true,
        ));
        let panel = app.history.as_mut().unwrap();
        panel.selected = date;
        panel.key(KeyCode::Char('1'));
        app.close_history();
        app.open_history(&Handle::current());
        let panel = app.history.as_ref().unwrap();
        assert_eq!(panel.selected, date);
        assert!(panel.monthly);
        assert_eq!(panel.sources(), vec!["codex"]);
        drain_until(&mut app, |a| !a.history_loading).await;
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn drilldown_returns_to_saved_history_and_uses_selected_day_usage() {
        let mut app = dashboard();
        let selected = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let mut panel = HistoryPanel::new(vec!["codex".into()], 2026, false);
        panel.selected = selected;
        let record = crate::history::Record {
            source: "codex".into(),
            instance: "codex".into(),
            id: "event".into(),
            session: "session".into(),
            project: "/tmp/p".into(),
            model: "gpt-test".into(),
            time: selected
                .and_hms_opt(12, 0, 0)
                .unwrap()
                .and_local_timezone(chrono::Local)
                .single()
                .unwrap()
                .with_timezone(&Utc),
            tokens: crate::history::Tokens {
                input: 123,
                ..Default::default()
            },
        };
        app.loaded_history.records.push(record);
        app.browser_filter = Some((panel.sources(), panel.session_query()));
        app.saved_history = Some(panel);
        app.history_return = true;
        app.browser = Some(SessionBrowser::new(vec![Session {
            agent: None,
            provider: AgentKind::CODEX,
            instance: "codex".into(),
            id: "session".into(),
            cwd: "/tmp/p".into(),
            title: "task".into(),
            recency_at_ms: 0,
            home: "/tmp".into(),
        }]));
        app.apply_browser_usage();
        assert_eq!(app.browser.as_ref().unwrap().visible_count(), 1);
        app.close_browser(true);
        assert_eq!(app.history.as_ref().unwrap().selected, selected);
    }
    #[tokio::test]
    async fn history_auto_refresh_scans_locally_and_skips_quota_network() {
        let root = std::env::temp_dir().join(format!("usage-auto-{}", uuid::Uuid::new_v4()));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut app = Dashboard::new(
            vec![Arc::new(CountingQuota(calls.clone()))],
            isolated_config(),
            "/tmp/test-config".into(),
        );
        app.history_config.providers.clear();
        app.history_cache = Some(crate::history::HistoryCache::at(
            root.join("history.sqlite3"),
        ));
        app.history = Some(HistoryPanel::new(Vec::new(), 2026, false));
        app.refresh_seconds = 1;
        app.last_history_refresh = std::time::Instant::now() - Duration::from_secs(2);
        assert!(app.maybe_auto_refresh(&Handle::current()));
        assert!(app.history_loading);
        drain_until(&mut app, |a| !a.history_loading).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        app.refresh_seconds = 0;
        app.last_history_refresh = std::time::Instant::now() - Duration::from_secs(2);
        assert!(!app.maybe_auto_refresh(&Handle::current()));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn current_resume_is_requested_only_after_session_enter() {
        let mut app = dashboard();
        app.browser = Some(SessionBrowser::new(vec![sessions::Session {
            agent: None,
            provider: AgentKind::CODEX,
            instance: "work".into(),
            id: "123e4567-e89b-12d3-a456-426614174001".into(),
            cwd: "/tmp/project".into(),
            home: "/tmp/work".into(),
            title: "Work".into(),
            recency_at_ms: 0,
        }]));
        app.browser.as_mut().unwrap().resume_mode = crate::config::ResumeMode::Current;
        app.handle_browser_key(KeyCode::Enter, &Handle::current());
        assert!(app.resume_request.is_none());
        app.handle_browser_key(KeyCode::Enter, &Handle::current());
        assert_eq!(app.resume_request.as_ref().unwrap().instance, "work");
        assert!(app.browser.as_ref().unwrap().launching);
    }
    #[test]
    fn quota_events_queue_one_alert_and_failures_do_not_emit_recovery() {
        let mut app = dashboard();
        app.history_config.alerts.enabled = true;
        let now = Utc::now();
        let snapshot = snapshot_ok(
            "codex",
            "Codex",
            vec![crate::model::QuotaTier {
                name: "primary".into(),
                utilization: 90.0,
                resets_at: None,
            }],
            None,
            now,
        );
        app.tx
            .send(AppEvent::Quota(Box::new(RefreshEvent {
                generation: app.generation,
                target: "codex".into(),
                snapshot,
            })))
            .unwrap();
        app.drain_events();
        assert_eq!(app.alerts_queue.len(), 1);
        assert!(app.notice.as_deref().unwrap().contains("剩余额度"));
        app.alerts_queue.clear();
        app.tx
            .send(AppEvent::Quota(Box::new(RefreshEvent {
                generation: app.generation,
                target: "codex".into(),
                snapshot: crate::model::snapshot_transient("codex", "Codex", "network"),
            })))
            .unwrap();
        app.drain_events();
        assert!(app.alerts_queue.is_empty());
        let snapshot = snapshot_ok(
            "codex",
            "Codex",
            vec![crate::model::QuotaTier {
                name: "primary".into(),
                utilization: 10.0,
                resets_at: None,
            }],
            None,
            now + ChronoDuration::seconds(1),
        );
        app.tx
            .send(AppEvent::Quota(Box::new(RefreshEvent {
                generation: app.generation,
                target: "codex".into(),
                snapshot,
            })))
            .unwrap();
        app.drain_events();
        assert!(app.alerts_queue[0].recovered);
    }
}

#[cfg(test)]
mod account_regression_tests {
    use super::*;
    use async_trait::async_trait;

    struct Account(&'static str);
    #[async_trait]
    impl UsageProvider for Account {
        fn id(&self) -> &'static str {
            "codex"
        }
        fn display_name(&self) -> &'static str {
            "Codex"
        }
        fn cache_key(&self) -> String {
            self.0.into()
        }
        async fn query(&self) -> UsageSnapshot {
            crate::model::snapshot_ok(
                "codex",
                self.0,
                vec![crate::model::QuotaTier {
                    name: "five_hour".into(),
                    utilization: 95.0,
                    resets_at: None,
                }],
                None,
                Utc::now(),
            )
        }
    }

    #[tokio::test]
    async fn two_accounts_receive_their_own_card_results() {
        let mut app = Dashboard::new(
            vec![Arc::new(Account("a")), Arc::new(Account("b"))],
            isolated_config(),
            "/nonexistent/usage-ablation.toml".into(),
        );
        app.service_http = Arc::new(crate::http::FakeHttp::new());
        app.history_config.alerts.enabled = true;
        app.refresh(&Handle::current());
        tokio::time::timeout(Duration::from_secs(1), async {
            while app.pending > 0 {
                app.drain_events();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(app.alerts_queue.len(), 2);
        let names: Vec<_> = app
            .cards
            .iter()
            .map(|card| card.snapshot.as_ref().map(|s| s.display_name.as_str()))
            .collect();
        assert_eq!(
            names,
            [Some("a"), Some("b")],
            "refresh events still select the first card by tool type"
        );
    }
}

#[cfg(test)]
mod overview_regression_tests {
    use super::*;
    use crate::provider_catalog::{Catalog, Models, Provider, ProviderKind};

    fn overview() -> Catalog {
        Catalog {
            providers: vec![Provider {
                id: "hidden-profile-uuid".into(),
                name: "工作 Provider".into(),
                agent: Some("Codex".into()),
                kind: ProviderKind::CompatibleApi,
                configured_model: Some("default-model".into()),
                models: Models::Loading,
                usage: Default::default(),
            }],
            ..Default::default()
        }
    }
    fn dashboard() -> Dashboard {
        Dashboard::new(
            Vec::new(),
            isolated_config(),
            "/nonexistent/usage-overview.toml".into(),
        )
    }
    fn rendered(app: &Dashboard, width: u16, height: u16) -> String {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        render_provider_contents(buffer.area, &mut buffer, app);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn automatic_refresh_keeps_content_and_skips_models_while_manual_refresh_requeries() {
        use crate::http::{HttpClient, HttpResponse, TransportError};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct RefreshHttp {
            models: AtomicUsize,
            balances: AtomicUsize,
            release: tokio::sync::Semaphore,
        }
        #[async_trait::async_trait]
        impl HttpClient for RefreshHttp {
            async fn request(
                &self,
                _: &str,
                url: &str,
                _: &[(&str, String)],
                _: Option<Vec<u8>>,
            ) -> Result<HttpResponse, TransportError> {
                let (status, body) = if url.ends_with("/models") {
                    self.models.fetch_add(1, Ordering::SeqCst);
                    (200, r#"{"data":[{"id":"kept-model"}]}"#.to_string())
                } else {
                    let round = self.balances.fetch_add(1, Ordering::SeqCst);
                    if round > 0 {
                        self.release.acquire().await.unwrap().forget();
                    }
                    if round > 1 {
                        (503, "temporary failure".into())
                    } else {
                        (
                            200,
                            format!(
                                r#"{{"code":true,"data":{{"object":"token_usage","total_used":0,"total_available":{},"unlimited_quota":true}}}}"#,
                                (round + 1) * 2500000
                            ),
                        )
                    }
                };
                Ok(HttpResponse {
                    status,
                    headers: Default::default(),
                    body: body.into_bytes(),
                })
            }
        }
        async fn settle(app: &mut Dashboard) {
            tokio::time::timeout(Duration::from_secs(2), async {
                while app.overview_loading {
                    app.drain_events();
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        async fn waiting(app: &mut Dashboard, http: &RefreshHttp, count: usize) {
            tokio::time::timeout(Duration::from_secs(2), async {
                while http.balances.load(Ordering::SeqCst) < count {
                    app.drain_events();
                    tokio::task::yield_now().await;
                }
                app.drain_events();
            })
            .await
            .unwrap();
        }
        let root =
            std::env::temp_dir().join(format!("usage-home-refresh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("usage.toml");
        std::fs::write(
            &path,
            r#"[[resources]]
id="relay"
service="new_api_token"
base_url="https://relay.invalid"
credential_env="FAKE_KEY"
options={quota_per_unit="500000",currency="USD"}
inference={base_url="https://relay.invalid/v1",credential_env="FAKE_KEY"}
"#,
        )
        .unwrap();
        let http = Arc::new(RefreshHttp {
            models: AtomicUsize::new(0),
            balances: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
        });
        let mut app = dashboard();
        app.config_path = path;
        app.service_http = Arc::new(crate::http::FakeHttp::new());
        app.overview_service = Arc::new(crate::provider_catalog::CatalogService::new(
            root.join("absent-manager"),
            http.clone(),
            Arc::new(|_| Ok("synthetic-key".into())),
        ));
        let handle = Handle::current();
        app.refresh(&handle);
        settle(&mut app).await;
        assert_eq!(http.models.load(Ordering::SeqCst), 1);
        assert!(
            rendered(&app, 100, 30)
                .split_whitespace()
                .collect::<String>()
                .contains("余额5USD")
        );
        app.home_models_expanded = true;
        let selected = app.home_provider.clone();
        app.refresh_seconds = 1;
        app.last_refresh = std::time::Instant::now() - Duration::from_secs(2);
        assert!(app.maybe_auto_refresh(&handle));
        waiting(&mut app, &http, 2).await;
        let output = rendered(&app, 100, 30)
            .split_whitespace()
            .collect::<String>();
        assert!(output.contains("余额5USD"));
        assert!(output.contains("kept-model"));
        assert!(!output.contains("查询中"));
        assert_eq!(http.models.load(Ordering::SeqCst), 1);
        assert_eq!(app.home_provider, selected);
        assert!(app.home_models_expanded);
        let generation = app.overview_generation;
        app.refresh_background(&handle);
        assert_eq!(app.overview_generation, generation);
        http.release.add_permits(1);
        settle(&mut app).await;
        assert!(
            rendered(&app, 100, 30)
                .split_whitespace()
                .collect::<String>()
                .contains("余额10USD")
        );

        app.refresh(&handle);
        waiting(&mut app, &http, 3).await;
        assert!(
            rendered(&app, 100, 30)
                .split_whitespace()
                .collect::<String>()
                .contains("余额10USD")
        );
        http.release.add_permits(1);
        settle(&mut app).await;
        assert_eq!(http.models.load(Ordering::SeqCst), 2);
        let output = rendered(&app, 100, 30)
            .split_whitespace()
            .collect::<String>();
        assert!(!output.contains("余额10USD"));
        assert!(output.contains("kept-model"));
        assert!(
            app.overview.as_ref().unwrap().providers[0]
                .usage
                .sources
                .iter()
                .any(|s| {
                    matches!(&s.state, crate::provider_catalog::QueryState::Result(result)
                if result.state == crate::resources::ResourceState::TransientFailure)
                })
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn background_refresh_uses_selected_config_and_does_not_create_store() {
        let root = std::env::temp_dir().join(format!("usage-home-events-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("usage.toml");
        std::fs::write(
            &path,
            r#"[[resources]]
id="billing-only"
service="new_api_account"
base_url="https://unused.invalid"
credential_env="SYNTHETIC_ONLY"
options={user_id=1}
"#,
        )
        .unwrap();
        let http = Arc::new(crate::http::FakeHttp::new());
        let mut app = dashboard();
        app.config_path = path.clone();
        app.overview_service = Arc::new(crate::provider_catalog::CatalogService::new(
            root.join("absent-manager"),
            http.clone(),
            Arc::new(|_| panic!("billing-only resource resolved credentials")),
        ));
        app.refresh_overview(&Handle::current());
        tokio::time::timeout(Duration::from_secs(1), async {
            while app.overview_loading {
                app.drain_events();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(app.overview.as_ref().unwrap().providers.is_empty());
        assert!(http.calls.lock().unwrap().is_empty());
        assert!(!root.join("absent-manager").exists());
        std::fs::write(path, "invalid [ TOML").unwrap();
        app.refresh_overview(&Handle::current());
        tokio::time::timeout(Duration::from_secs(1), async {
            while app.overview_loading {
                app.drain_events();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(app.overview.is_none());
        assert!(app.overview_error.is_some());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn model_refresh_rejects_old_generation_duplicate_and_misrouted_results() {
        let mut app = dashboard();
        app.overview_generation = 2;
        app.overview_loading = true;
        app.tx
            .send(AppEvent::OverviewLoaded {
                generation: 1,
                result: Ok((overview(), 1)),
            })
            .unwrap();
        app.drain_events();
        assert!(app.overview.is_none());
        app.tx
            .send(AppEvent::OverviewLoaded {
                generation: 2,
                result: Ok((overview(), 1)),
            })
            .unwrap();
        app.drain_events();
        for (generation, target) in [(1, "hidden-profile-uuid"), (2, "unexpected")] {
            app.tx
                .send(AppEvent::ProviderModels {
                    generation,
                    target: target.into(),
                    models: Models::Ready(vec!["wrong-model".into()]),
                })
                .unwrap();
        }
        app.drain_events();
        assert_eq!(app.overview_pending, 1);
        for model in ["correct-model", "duplicate-model"] {
            app.tx
                .send(AppEvent::ProviderModels {
                    generation: 2,
                    target: "hidden-profile-uuid".into(),
                    models: Models::Ready(vec![model.into()]),
                })
                .unwrap();
        }
        app.drain_events();
        assert_eq!(app.overview_pending, 0);
        assert!(!app.overview_loading);
        assert_eq!(
            app.overview.as_ref().unwrap().providers[0].models,
            Models::Ready(vec!["correct-model".into()])
        );
        app.tx
            .send(AppEvent::OverviewLoaded {
                generation: 2,
                result: Err("配置读取失败".into()),
            })
            .unwrap();
        app.drain_events();
        assert!(app.overview.is_none());
        assert_eq!(app.overview_error.as_deref(), Some("配置读取失败"));
    }

    #[test]
    fn provider_budget_updates_are_scoped_by_generation_and_identity_and_render_above_models() {
        use crate::provider_catalog::{ProviderUsage, QueryState, UsageSource};
        use crate::resources::{
            Money, ResourceBinding, ResourceMetric, ResourceScope, ResourceSnapshot, ResourceState,
        };
        let binding = ResourceBinding {
            account: "shared-wallet".into(),
            revision: "r1".into(),
        };
        let target = binding.cache_key("new_api_token");
        let mut catalog = overview();
        catalog.providers[0].usage = ProviderUsage {
            sources: vec![UsageSource {
                target: target.clone(),
                label: "工作令牌".into(),
                service: "new_api_token".into(),
                binding: binding.clone(),
                state: QueryState::Loading,
            }],
            hint: None,
        };
        let mut app = dashboard();
        app.overview = Some(catalog);
        app.overview_generation = 2;
        app.overview_pending = 1;
        app.overview_loading = true;
        let result = ResourceSnapshot {
            binding,
            service: "new_api_token".into(),
            scope: ResourceScope::Token,
            state: ResourceState::Ready,
            metrics: vec![
                ResourceMetric::Balance {
                    remaining: Money {
                        amount: "12.345678".into(),
                        currency: "USD".into(),
                    },
                },
                ResourceMetric::Allowance {
                    remaining: Some("6172839".into()),
                    used: Some("1000000".into()),
                    unit: "new_api_quota".into(),
                    unlimited: true,
                },
            ],
            queried_at: Some(Utc::now()),
            message: None,
        };
        app.tx
            .send(AppEvent::ProviderResource {
                generation: 1,
                target: target.clone(),
                snapshot: result.clone(),
            })
            .unwrap();
        app.drain_events();
        assert_eq!(app.overview_pending, 1);
        app.tx
            .send(AppEvent::ProviderResource {
                generation: 2,
                target: target.clone(),
                snapshot: result.clone(),
            })
            .unwrap();
        app.drain_events();
        assert_eq!(app.overview_pending, 0);
        assert!(!app.overview_loading);
        let mut duplicate = result;
        duplicate.metrics.clear();
        app.tx
            .send(AppEvent::ProviderResource {
                generation: 2,
                target,
                snapshot: duplicate,
            })
            .unwrap();
        app.drain_events();
        let output = rendered(&app, 100, 30)
            .split_whitespace()
            .collect::<String>();
        assert!(output.contains("余额12.345678USD"));
        assert!(output.contains("USD（接口返回）"));
        assert!(output.contains("令牌不限额"));
        assert!(!output.contains("原始额度"));
        assert!(!output.contains("6172839"));
        assert!(!output.contains("1000000"));
        assert!(!output.contains("账号余额需配置"));
        assert!(output.contains("工作令牌·令牌"));
        assert!(output.find("余额12.345678USD").unwrap() < output.find("默认模型").unwrap());
        for hidden in ["启动", "会话", "shared-wallet", "hidden-profile-uuid"] {
            assert!(!output.contains(hidden));
        }
    }

    #[tokio::test]
    async fn provider_board_shows_only_current_provider_and_scrolls_its_details() {
        let mut app = dashboard();
        let mut catalog = overview();
        catalog.providers[0].models =
            Models::Ready((0..24).map(|n| format!("available-model-{n}")).collect());
        catalog.providers.push(Provider {
            id: "hidden-oauth-id".into(),
            name: "个人订阅".into(),
            agent: Some("Codex".into()),
            kind: ProviderKind::Subscription,
            configured_model: None,
            models: Models::Unavailable("OAuth 订阅未接入模型列表接口".into()),
            usage: Default::default(),
        });
        app.overview = Some(catalog);
        assert!(!rendered(&app, 100, 30).contains("available-model-23"));
        assert!(
            rendered(&app, 100, 30)
                .split_whitespace()
                .collect::<String>()
                .contains("另21个")
        );
        app.handle_home_key(KeyCode::Enter, &Handle::current(), (100, 32));
        assert!(app.home_models_expanded);
        assert!(app.resume_request.is_none());
        app.cards.push(CardSlot {
            target: "hidden-card".into(),
            loading: true,
            snapshot: None,
            warning: None,
        });
        for width in [28, 52, 100] {
            let mut pages = String::new();
            app.home_scroll = 0;
            for _ in 0..100 {
                pages.push_str(&rendered(&app, width, 12));
                let old = app.home_scroll;
                app.scroll_home(1, (width, 14));
                if old == app.home_scroll {
                    break;
                }
            }
            let compact = pages.split_whitespace().collect::<String>();
            for expected in [
                "工作Provider",
                "兼容API·BYOK",
                "default-model",
                "available-model-23",
            ] {
                assert!(
                    compact.contains(expected),
                    "missing {expected} at {width}: {pages}"
                );
            }
            for removed in [
                "启动",
                "运行中",
                "会话",
                "Legacy",
                "hidden-profile",
                "hidden-oauth",
            ] {
                assert!(!compact.contains(removed), "unexpected {removed}: {pages}");
            }
            app.scroll_home(i16::MIN, (width, 14));
            assert_eq!(app.home_scroll, 0);
        }
        app.handle_home_key(KeyCode::Right, &Handle::current(), (100, 32));
        assert_eq!(app.home_scroll, 0);
        assert!(!app.home_models_expanded);
        let output = rendered(&app, 100, 30)
            .split_whitespace()
            .collect::<String>();
        assert!(output.contains("个人订阅"));
        assert!(!output.contains("工作Provider"));
        assert!(!output.contains("available-model"));
        app.handle_home_key(KeyCode::Left, &Handle::current(), (100, 32));
        assert!(
            rendered(&app, 100, 30)
                .split_whitespace()
                .collect::<String>()
                .contains("工作Provider")
        );
        app.handle_home_key(KeyCode::Enter, &Handle::current(), (100, 32));
        assert!(app.handle_home_key(KeyCode::Esc, &Handle::current(), (100, 32)));
        assert!(!app.home_models_expanded);
        assert!(!rendered(&app, 100, 30).contains("available-model-23"));
        for (width, height) in [(0, 0), (1, 1), (2, 1)] {
            rendered(&app, width, height);
        }
    }

    #[tokio::test]
    async fn provider_switch_keeps_identity_on_refresh_and_recovers_when_removed() {
        let mut app = dashboard();
        app.overview_generation = 9;
        let a = overview().providers.remove(0);
        let mut b = a.clone();
        b.id = "provider-b".into();
        b.name = "其他 Provider".into();
        b.models = Models::Ready((0..30).map(|n| format!("b-model-{n}")).collect());
        app.tx
            .send(AppEvent::OverviewLoaded {
                generation: 9,
                result: Ok((
                    Catalog {
                        providers: vec![a.clone(), b.clone()],
                        ..Default::default()
                    },
                    0,
                )),
            })
            .unwrap();
        app.drain_events();
        assert_eq!(app.home_provider.as_deref(), Some(a.id.as_str()));
        app.handle_home_key(KeyCode::Right, &Handle::current(), (100, 30));
        assert_eq!(app.home_provider.as_deref(), Some("provider-b"));
        app.handle_home_key(KeyCode::Enter, &Handle::current(), (100, 30));
        app.handle_home_key(KeyCode::Down, &Handle::current(), (100, 30));
        let scroll = app.home_scroll;
        assert!(scroll > 0);
        app.tx
            .send(AppEvent::OverviewLoaded {
                generation: 9,
                result: Ok((
                    Catalog {
                        providers: vec![b, a.clone()],
                        ..Default::default()
                    },
                    0,
                )),
            })
            .unwrap();
        app.drain_events();
        assert_eq!(app.home_provider.as_deref(), Some("provider-b"));
        assert_eq!(app.home_scroll, scroll);
        assert!(app.home_models_expanded);
        app.handle_home_key(KeyCode::Left, &Handle::current(), (100, 30));
        assert_eq!(app.home_provider.as_deref(), Some(a.id.as_str()));
        assert_eq!(app.home_scroll, 0);
        assert!(!app.home_models_expanded);
        app.handle_home_key(KeyCode::Right, &Handle::current(), (100, 30));
        app.tx
            .send(AppEvent::OverviewLoaded {
                generation: 9,
                result: Ok((
                    Catalog {
                        providers: vec![a.clone()],
                        ..Default::default()
                    },
                    0,
                )),
            })
            .unwrap();
        app.drain_events();
        assert_eq!(app.home_provider.as_deref(), Some(a.id.as_str()));
        app.tx
            .send(AppEvent::OverviewLoaded {
                generation: 9,
                result: Ok((Catalog::default(), 0)),
            })
            .unwrap();
        app.drain_events();
        assert!(app.home_provider.is_none());
        for key in [KeyCode::Left, KeyCode::Right] {
            assert!(app.handle_home_key(key, &Handle::current(), (100, 30)));
        }
        assert!(app.resume_request.is_none());
    }

    #[tokio::test]
    async fn overview_refresh_cancels_previous_task_and_drop_cancels_current_task() {
        let mut app = dashboard();
        let previous = Handle::current().spawn(std::future::pending::<()>());
        let previous_abort = previous.abort_handle();
        app.overview_task = Some(previous);
        app.refresh_overview(&Handle::current());
        tokio::task::yield_now().await;
        assert!(previous_abort.is_finished());
        app.overview_task.take().unwrap().abort();
        let current = Handle::current().spawn(std::future::pending::<()>());
        let current_abort = current.abort_handle();
        app.overview_task = Some(current);
        drop(app);
        tokio::task::yield_now().await;
        assert!(current_abort.is_finished());
    }
}

#[cfg(test)]
mod home_board_tests {
    use super::super::home::Focus as HomeFocus;
    use super::*;
    use crate::config::ResumeMode;
    use crate::sessions::{AgentKind, Session};

    fn app() -> Dashboard {
        let mut app = Dashboard::new(
            Vec::new(),
            isolated_config(),
            "/nonexistent/home-boards.toml".into(),
        );
        app.service_http = Arc::new(crate::http::FakeHttp::new());
        app
    }
    fn session(instance: &str, project: &str, recency: i64) -> Session {
        Session {
            agent: None,
            provider: AgentKind::CODEX,
            instance: instance.into(),
            id: "123e4567-e89b-12d3-a456-426614174001".into(),
            cwd: format!("/synthetic/{project}").into(),
            title: format!("{project} 会话"),
            recency_at_ms: recency,
            home: format!("/synthetic/{instance}").into(),
        }
    }
    fn text(buffer: &Buffer) -> String {
        buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .split_whitespace()
            .collect()
    }

    #[test]
    fn default_home_renders_three_boards_with_projects_and_sessions() {
        let mut app = app();
        app.home_sessions
            .refresh(vec![session("account-a", "ProjectA", 1)]);
        for width in [28, 52, 80, 120] {
            let area = Rect::new(0, 0, width, 22);
            let mut buffer = Buffer::empty(area);
            render_home(area, &mut buffer, &app);
            let output = text(&buffer);
            for expected in [
                "Provider",
                "官方API",
                "OpenAI",
                "Claude",
                "Grok",
                "项目与会话",
                "ProjectA",
            ] {
                assert!(
                    output.contains(expected),
                    "missing {expected} at {width}: {output}"
                );
            }
            assert!(!output.contains("123e4567"));
            if width >= 80 {
                assert!(output.contains("ProjectA会话"), "{output}");
            }
        }
        for (width, height) in [(0, 0), (1, 1), (12, 4)] {
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
            render_home(buffer.area, &mut buffer, &app);
        }
    }

    #[tokio::test]
    async fn status_details_select_vendor_scroll_and_return_without_launching_sessions() {
        let mut app = app();
        let handle = Handle::current();
        for key in [KeyCode::Tab, KeyCode::Down, KeyCode::Enter] {
            assert!(app.handle_home_key(key, &handle, (80, 24)));
        }
        assert_eq!(app.home_focus, HomeFocus::Connections);
        assert!(app.home_connections.is_details());
        let area = Rect::new(0, 0, 80, 22);
        let mut buffer = Buffer::empty(area);
        render_home(area, &mut buffer, &app);
        let status_area = super::super::home::areas(area)[1];
        let output = (status_area.y..status_area.bottom())
            .flat_map(|y| (status_area.x..status_area.right()).map(move |x| (x, y)))
            .map(|position| buffer[position].symbol())
            .collect::<String>();
        assert!(output.contains("Claude"));
        assert!(!output.contains("OpenAI"));
        assert!(app.resume_request.is_none());
        for _ in 0..100 {
            app.handle_home_key(KeyCode::Down, &handle, (80, 24));
        }
        assert!(app.handle_home_key(KeyCode::Esc, &handle, (80, 24)));
        assert!(!app.home_connections.is_details());
        render_home(area, &mut buffer, &app);
        assert!(text(&buffer).contains(">○Claude"));
        assert!(text(&buffer).contains("OpenAI"));
        assert!(!app.handle_home_key(KeyCode::Esc, &handle, (80, 24)));
    }

    #[tokio::test]
    async fn quick_resume_preserves_instance_home_and_session_identity() {
        let mut app = app();
        app.home_sessions.resume_mode = ResumeMode::Current;
        app.home_sessions.refresh(vec![
            session("account-b", "ProjectB", 2),
            session("account-a", "ProjectA", 1),
        ]);
        let handle = Handle::current();
        app.handle_home_key(KeyCode::Tab, &handle, (100, 30));
        assert_eq!(app.home_focus, HomeFocus::Connections);
        app.handle_home_key(KeyCode::Tab, &handle, (100, 30));
        assert_eq!(app.home_focus, HomeFocus::Projects);
        app.handle_home_key(KeyCode::Down, &handle, (100, 30));
        app.handle_home_key(KeyCode::Enter, &handle, (100, 30));
        assert!(app.resume_request.is_none());
        app.handle_home_key(KeyCode::Enter, &handle, (100, 30));
        let requested = app.resume_request.as_ref().unwrap();
        assert_eq!(requested.instance, "account-a");
        assert_eq!(requested.home, std::path::Path::new("/synthetic/account-a"));
        assert_eq!(requested.cwd, std::path::Path::new("/synthetic/ProjectA"));
        assert_eq!(requested.id, "123e4567-e89b-12d3-a456-426614174001");
        app.handle_home_key(KeyCode::Down, &handle, (100, 30));
        app.handle_home_key(KeyCode::Enter, &handle, (100, 30));
        assert_eq!(app.resume_request.as_ref().unwrap().instance, "account-a");
        assert!(app.home_sessions.launching);
    }

    #[test]
    fn compact_sessions_show_account_names_and_hide_identifier_fallback_titles() {
        let mut app = app();
        let id = "123e4567-e89b-12d3-a456-426614174002";
        let mut session = session(&format!("managed-{id}-r2"), "ProjectA", 1);
        session.title = session.id.clone();
        app.home_sessions.refresh(vec![session]);
        app.home_activity.insert(
            app.home_sessions.sessions[0].key(),
            crate::runs::SessionRuntime {
                runs: vec![crate::runs::SessionRun {
                    run_id: "private-run-uuid".into(),
                    process: crate::runs::RunState::Running,
                    activity: crate::runs::SessionActivity::Working,
                    model: Some("reported-model".into()),
                }],
            },
        );
        app.overview = Some(crate::provider_catalog::Catalog {
            providers: vec![crate::provider_catalog::Provider {
                id: id.into(),
                name: "工作账号".into(),
                agent: Some("Codex".into()),
                kind: crate::provider_catalog::ProviderKind::CompatibleApi,
                configured_model: None,
                models: crate::provider_catalog::Models::Ready(Vec::new()),
                usage: Default::default(),
            }],
            ..Default::default()
        });
        let area = Rect::new(0, 0, 120, 30);
        let mut buffer = Buffer::empty(area);
        render_home(area, &mut buffer, &app);
        let project = super::super::home::areas(area)[2];
        let output = (project.y..project.bottom())
            .flat_map(|y| (project.x..project.right()).map(move |x| (x, y)))
            .map(|position| buffer[position].symbol())
            .collect::<String>()
            .split_whitespace()
            .collect::<String>();
        assert!(output.contains("未命名会话"));
        assert!(output.contains("工作账号"));
        assert!(output.contains("r2"));
        assert!(output.contains("执行中"));
        assert!(output.contains("ProjectA(1)"));
        assert!(!output.contains("(1)·0"));
        assert!(!output.contains("private-run-uuid"));
        assert!(!output.contains("123e4567"));
    }

    #[tokio::test]
    async fn search_does_not_open_menu_and_stale_events_do_not_replace_selection_or_unlock_launch()
    {
        let mut app = app();
        app.home_focus = HomeFocus::Projects;
        app.home_sessions
            .refresh(vec![session("account-a", "ProjectA", 1)]);
        app.handle_home_key(KeyCode::Char('/'), &Handle::current(), (80, 24));
        assert_eq!(app.handle_navigation_key(KeyCode::Char('m')), None);
        app.handle_home_key(KeyCode::Char('m'), &Handle::current(), (80, 24));
        assert_eq!(app.home_sessions.query, "m");
        assert!(app.menu.is_none());
        app.home_sessions.clear_search();
        app.home_sessions_generation = 2;
        app.home_sessions.launching = true;
        app.tx
            .send(AppEvent::HomeSessionsLoaded {
                generation: 1,
                catalog: sessions::SessionCatalog::default(),
            })
            .unwrap();
        app.tx
            .send(AppEvent::HomeSessionLaunched {
                generation: 1,
                result: Ok(()),
            })
            .unwrap();
        app.drain_events();
        assert!(app.home_sessions.launching);
        assert_eq!(app.home_sessions.sessions.len(), 1);
    }

    #[tokio::test]
    async fn runtime_poll_is_read_only_rejects_stale_events_and_does_not_refresh_provider() {
        let root = std::env::temp_dir().join(format!("usage-runtime-ui-{}", uuid::Uuid::new_v4()));
        let mut app = app();
        app.home_activity_root = root.clone();
        app.home_sessions
            .refresh(vec![session("account-a", "ProjectA", 1)]);
        let selected = app.home_sessions.selected_session().unwrap().key();
        let handle = Handle::current();
        app.refresh_seconds = 0;
        app.maybe_refresh_home_activity(&handle);
        tokio::time::timeout(Duration::from_secs(2), async {
            while app.home_activity_pending {
                app.drain_events();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!root.exists());
        assert_eq!(app.overview_generation, 0);
        assert!(app.home_activity.is_empty());
        let generation = app.home_activity_generation;
        app.maybe_refresh_home_activity(&handle);
        assert_eq!(app.home_activity_generation, generation);
        let activity = crate::runs::SessionRuntime {
            runs: vec![crate::runs::SessionRun {
                run_id: "internal-id".into(),
                process: crate::runs::RunState::Running,
                activity: crate::runs::SessionActivity::Working,
                model: None,
            }],
        };
        app.tx
            .send(AppEvent::HomeActivityLoaded {
                generation,
                result: Ok(HashMap::from([(selected.clone(), activity.clone())])),
            })
            .unwrap();
        assert!(app.drain_events().0);
        app.tx
            .send(AppEvent::HomeActivityLoaded {
                generation,
                result: Ok(HashMap::from([(selected.clone(), activity)])),
            })
            .unwrap();
        assert!(
            !app.drain_events().0,
            "identical local status invalidated the UI"
        );
        app.home_activity_generation += 1;
        app.tx
            .send(AppEvent::HomeActivityLoaded {
                generation,
                result: Err("old error".into()),
            })
            .unwrap();
        app.drain_events();
        assert!(!app.home_activity.is_empty());
        app.tx
            .send(AppEvent::HomeActivityLoaded {
                generation: app.home_activity_generation,
                result: Err("运行状态读取失败".into()),
            })
            .unwrap();
        app.drain_events();
        assert!(app.home_activity.is_empty());
        assert!(app.home_activity_error.is_some());
        assert_eq!(
            app.home_sessions.selected_session().unwrap().key(),
            selected
        );
    }

    #[tokio::test]
    async fn home_discovers_sessions_in_background_and_keeps_selection_on_refresh() {
        fn load(home: &std::path::Path) -> Result<Vec<sessions::SessionRecord>, String> {
            Ok(vec![sessions::SessionRecord {
                id: "123e4567-e89b-12d3-a456-426614174001".into(),
                cwd: home.join("project"),
                title: std::fs::read_to_string(home.join("title")).unwrap(),
                recency_at_ms: 1,
            }])
        }
        let root =
            std::env::temp_dir().join(format!("usage-home-sessions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("project")).unwrap();
        std::fs::write(root.join("title"), "before").unwrap();
        let mut app = app();
        app.session_homes = sessions::SessionHomes::new().with("codex", root.clone());
        app.session_registry = sessions::SessionRegistry::new();
        app.session_registry
            .register(sessions::SessionSource {
                kind: AgentKind::CODEX,
                load,
            })
            .unwrap();
        for title in ["before", "after"] {
            std::fs::write(root.join("title"), title).unwrap();
            app.start_home_sessions_load(&Handle::current());
            let generation = app.home_sessions_generation;
            app.start_home_sessions_load(&Handle::current());
            assert_eq!(app.home_sessions_generation, generation);
            tokio::time::timeout(Duration::from_secs(1), async {
                while app.home_sessions_loading {
                    app.drain_events();
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(app.home_sessions.selected_session().unwrap().title, title);
            app.home_sessions.focus_sessions();
        }
        assert_eq!(app.home_sessions.focus(), Focus::Sessions);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
