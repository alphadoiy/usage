//! 事件循环。查询在 tokio 任务里进行，界面保持可操作。

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use tracing::Instrument;

use chrono::Utc;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use tokio::runtime::Handle;

use crate::config::AppConfig;
use crate::model::UsageSnapshot;
use crate::providers::{
    ProviderError, QUERY_TIMEOUT, ResetPreview, UsageProvider, query_with_timeout,
};
use crate::service_status::{self, ServiceStatus};
use crate::sessions;
use crate::ui::history::HistoryPanel;
use crate::ui::menu::{self, Action as MenuAction, Command as MenuCommand};
use crate::ui::session_browser::{Focus, SessionBrowser};
mod render;
#[cfg(test)]
use ratatui::{buffer::Buffer, style::Color};
use render::{draw, home_lines};
#[cfg(test)]
use render::{render_home, render_provider_contents};

struct RefreshEvent {
    generation: u64,
    target: String,
    snapshot: UsageSnapshot,
}

enum AppEvent {
    FingerprintLoaded {
        generation: u64,
        result: Result<Vec<crate::codex_manager::fingerprint::Snapshot>, String>,
    },
    HomePerformance {
        generation: u64,
        key: String,
        result: Result<Option<crate::performance::Summary>, String>,
    },
    Performance {
        generation: u64,
        result: Result<crate::performance::Snapshot, String>,
        loading: bool,
    },
    HomeActivityLoaded {
        generation: u64,
        result: Result<HashMap<sessions::SessionKey, crate::runs::SessionRuntime>, String>,
    },
    OverviewLoaded {
        generation: u64,
        result: Result<(crate::provider_catalog::Catalog, usize), String>,
    },
    OverviewFinished {
        generation: u64,
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
        projects: Result<Vec<crate::projects::Record>, String>,
    },
    HomeSessionLaunched {
        generation: u64,
        result: Result<(), String>,
    },
}

type ResetStage = super::confirmation::Confirmation<ResetPreview>;

struct ResetFlow {
    provider_id: String,
    provider: Arc<dyn UsageProvider>,
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
    let mut terminal = crate::ui::terminal::enter().map_err(io::Error::other)?;
    let mut redraw = RedrawSchedule::new();
    app.refresh(&handle);
    let result = loop {
        app.maybe_refresh_fingerprint(&handle);
        let status_started = app.maybe_refresh_operation_status(&handle);
        let (received, reset_consumed) = app.drain_events();
        if app
            .provider_action
            .as_mut()
            .is_some_and(|panel| panel.take_refresh())
        {
            app.refresh_background(&handle);
        }
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
        if received || reset_consumed || refresh_started || status_started {
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
                    if app.handle_performance_key(key.code, &handle) {
                        redraw.invalidate();
                        continue;
                    }
                    if let Some(panel) = &mut app.provider_action {
                        if panel.key(key.code, &handle) {
                            app.provider_action = None;
                        }
                        redraw.invalidate();
                        continue;
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
                            MenuCommand::Activate(MenuAction::Providers) => {
                                crate::ui::terminal::leave();
                                let result = crate::ui::run_provider_manager_with_config(
                                    handle.clone(),
                                    Some(app.config_path.clone()),
                                    // Reuse the home board's sessions once loaded rather than scanning again.
                                    if app.home_sessions_generation > 0
                                        && !app.home_sessions_loading
                                    {
                                        crate::ui::ProjectSessions::from_sessions(
                                            &app.home_sessions.sessions,
                                        )
                                    } else {
                                        crate::ui::ProjectSessions::Scan(
                                            app.session_registry.clone(),
                                            app.session_homes.clone(),
                                        )
                                    },
                                    // The project picked on the home board becomes the launch default.
                                    (app.home_focus == super::home::Focus::Projects)
                                        .then(|| app.home_sessions.selected_project().cloned())
                                        .flatten(),
                                );
                                terminal = match crate::ui::terminal::enter() {
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
                            MenuCommand::Activate(MenuAction::AddProvider) => {
                                let result = super::provider_setup::run(
                                    handle.clone(),
                                    &mut terminal,
                                    &|frame| draw(frame, &app),
                                );
                                app.notice = match result {
                                    Ok(Some(added)) => {
                                        if let Some(profile) = added.profile {
                                            app.home_provider = Some(profile.id);
                                        }
                                        Some(added.notice)
                                    }
                                    Ok(None) => None,
                                    Err(error) => Some(error.to_string()),
                                };
                                app.refresh_background(&handle);
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
    crate::ui::terminal::leave();
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
    crate::ui::terminal::leave();
    let result = resume_in_terminal(handle, &session);
    *terminal = crate::ui::terminal::enter().map_err(io::Error::other)?;
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
    subscription_tests: super::subscription_tests::Tests,
    home_subscription_details: bool,
    fingerprint_service: Arc<crate::codex_manager::fingerprint::FingerprintService>,
    fingerprint_task: Option<tokio::task::JoinHandle<()>>,
    fingerprint_generation: u64,
    fingerprint_last_poll: Option<std::time::Instant>,
    home_performance: HashMap<String, super::performance::CachedSummary>,
    home_performance_task: Option<tokio::task::JoinHandle<()>>,
    home_performance_pending: bool,
    home_performance_generation: u64,
    performance: Option<super::performance::Panel>,
    performance_service: Arc<crate::performance::Service>,
    performance_generation: u64,
    home_activity: HashMap<sessions::SessionKey, crate::runs::SessionRuntime>,
    home_activity_error: Option<String>,
    home_activity_task: Option<tokio::task::JoinHandle<()>>,
    home_activity_pending: bool,
    home_activity_dirty: bool,
    home_activity_generation: u64,
    home_activity_last_poll: std::time::Instant,
    home_activity_root: PathBuf,
    project_store: Arc<crate::projects::Store>,
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
    provider_action: Option<super::provider_actions::Panel>,
    operation_statuses: super::provider_actions::Statuses,
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
        if let Some(task) = self.fingerprint_task.take() {
            task.abort();
        }
        if let Some(task) = self.home_performance_task.take() {
            task.abort();
        }
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
        let session_homes = sessions::SessionHomes::from_config(&config);
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
            subscription_tests: super::subscription_tests::Tests::default(),
            home_subscription_details: false,
            fingerprint_service: Arc::new(
                crate::codex_manager::fingerprint::FingerprintService::from_environment(),
            ),
            fingerprint_task: None,
            fingerprint_generation: 0,
            fingerprint_last_poll: None,
            home_activity: HashMap::new(),
            home_performance: HashMap::new(),
            home_performance_task: None,
            home_performance_pending: false,
            home_performance_generation: 0,
            performance: None,
            performance_service: Arc::new(crate::performance::Service::from_environment()),
            performance_generation: 0,
            home_activity_error: None,
            home_activity_task: None,
            home_activity_pending: false,
            home_activity_dirty: true,
            home_activity_generation: 0,
            home_activity_last_poll: std::time::Instant::now(),
            home_activity_root: crate::codex_manager::default_root(),
            project_store: Arc::new(crate::projects::Store::from_environment()),
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
            provider_action: None,
            operation_statuses: super::provider_actions::Statuses::default(),
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
            KeyCode::F(4) => Some(MenuCommand::Activate(MenuAction::Providers)),
            KeyCode::F(5) => Some(MenuCommand::Activate(MenuAction::AddProvider)),
            KeyCode::Char('a')
                if self.history.is_none()
                    && self.browser.is_none()
                    && self.home_focus == super::home::Focus::Providers =>
            {
                Some(MenuCommand::Activate(MenuAction::AddProvider))
            }
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

    fn maybe_refresh_fingerprint(&mut self, handle: &Handle) {
        if self.history.is_some()
            || self.browser.is_some()
            || self.fingerprint_task.is_some()
            || self
                .fingerprint_last_poll
                .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.fingerprint_last_poll = Some(std::time::Instant::now());
        self.fingerprint_generation = self.fingerprint_generation.wrapping_add(1);
        let generation = self.fingerprint_generation;
        let service = self.fingerprint_service.clone();
        let tx = self.tx.clone();
        self.fingerprint_task = Some(handle.spawn_blocking(move || {
            let result = service
                .snapshots()
                .map_err(|_| "本机测试记录读取失败".to_owned());
            let _ = tx.send(AppEvent::FingerprintLoaded { generation, result });
        }));
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
        let store = self.project_store.clone();
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
                projects: store.list(),
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
            KeyCode::Char('v') if self.home_focus == HomeFocus::Providers => {
                self.open_performance(handle)
            }
            KeyCode::Char('c') if self.home_focus == HomeFocus::Providers => {
                if let Some((_, provider)) =
                    super::overview::selected(self.overview.as_ref(), self.home_provider.as_deref())
                {
                    if let Some(operation) =
                        provider.operations.iter().find(|op| op.0.id() == "checkin")
                    {
                        self.provider_action = Some(super::provider_actions::Panel::open(
                            provider.name.clone(),
                            operation.clone(),
                            handle,
                        ));
                    } else {
                        self.notice = Some("此 Provider 暂不支持签到".into());
                    }
                }
            }
            KeyCode::Char('t') if self.home_focus == HomeFocus::Providers => {
                if super::overview::selected(self.overview.as_ref(), self.home_provider.as_deref())
                    .is_some_and(|(_, p)| super::subscription_tests::supports(p))
                {
                    self.home_subscription_details = !self.home_subscription_details;
                    self.home_scroll = 0;
                } else {
                    self.notice = Some("订阅测试目前仅支持 GPT 官方订阅 Provider".into());
                }
            }
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
                if self.home_focus == HomeFocus::Providers && self.home_subscription_details =>
            {
                self.home_subscription_details = false;
                self.home_scroll = 0;
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
        self.home_subscription_details = false;
    }

    fn scroll_home(&mut self, delta: i16, (width, height): (u16, u16)) {
        let max = home_lines(self, width)
            .len()
            .saturating_sub(height.saturating_sub(2) as usize)
            .min(u16::MAX as usize) as i32;
        self.home_scroll =
            (i32::from(self.home_scroll) + i32::from(delta) * 3).clamp(0, max) as u16;
    }

    fn maybe_refresh_operation_status(&mut self, handle: &Handle) -> bool {
        if self.history.is_some()
            || self.browser.is_some()
            || self.provider_action.is_some()
            || self.reset.is_some()
            || self.home_subscription_details
        {
            return false;
        }
        let Some((_, provider)) =
            super::overview::selected(self.overview.as_ref(), self.home_provider.as_deref())
        else {
            return false;
        };
        let mut started = false;
        for operation in &provider.operations {
            started |= self
                .operation_statuses
                .ensure(operation, Utc::now(), handle);
        }
        started
    }

    fn refresh(&mut self, handle: &Handle) {
        if let Some((_, provider)) =
            super::overview::selected(self.overview.as_ref(), self.home_provider.as_deref())
        {
            for operation in &provider.operations {
                self.operation_statuses.invalidate(operation);
            }
        }
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
                    // A panicking provider still answers, as in the CLI: otherwise `pending`
                    // never reaches zero and every later refresh is skipped.
                    let query = {
                        let provider = Arc::clone(&provider);
                        tokio::spawn(
                            async move { query_with_timeout(provider.as_ref(), QUERY_TIMEOUT).await }
                                .in_current_span(),
                        )
                    };
                    let snapshot = query.await.unwrap_or_else(|_| {
                        crate::model::snapshot_transient(
                            provider.id(),
                            provider.display_name(),
                            "查询任务中断",
                        )
                    });
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
        let request = crate::provider_catalog::RefreshRequest {
            config_path: self.config_path.clone(),
            previous,
            refresh_models,
        };
        let tx = self.tx.clone();
        self.overview_task = Some(
            handle.spawn(
                service
                    .refresh(request, move |update| {
                        use crate::provider_catalog::RefreshUpdate;
                        let event = match update {
                            RefreshUpdate::Loaded(result) => {
                                AppEvent::OverviewLoaded { generation, result }
                            }
                            RefreshUpdate::Models { target, models } => AppEvent::ProviderModels {
                                generation,
                                target,
                                models,
                            },
                            RefreshUpdate::Resource { target, snapshot } => {
                                AppEvent::ProviderResource {
                                    generation,
                                    target,
                                    snapshot,
                                }
                            }
                            RefreshUpdate::Finished => AppEvent::OverviewFinished { generation },
                        };
                        tx.send(event).is_ok()
                    })
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

    fn refresh_home_performance(&mut self, handle: &Handle) -> bool {
        if self.home_performance_pending {
            return false;
        }
        let Some(binding) =
            super::overview::selected(self.overview.as_ref(), self.home_provider.as_deref())
                .and_then(|(_, p)| p.performance.clone())
        else {
            return false;
        };
        let key = binding.cache_key();
        if self
            .home_performance
            .get(&key)
            .is_some_and(|s| s.updated.elapsed() < super::performance::HOME_TTL)
        {
            return false;
        }
        self.home_performance_generation = self.home_performance_generation.wrapping_add(1);
        let generation = self.home_performance_generation;
        self.home_performance_pending = true;
        let service = self.performance_service.clone();
        let tx = self.tx.clone();
        self.home_performance_task = Some(handle.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                service.summary(
                    &binding,
                    crate::performance::Period::Day,
                    Utc::now().timestamp_millis(),
                )
            })
            .await
            .unwrap_or_else(|_| Err("性能摘要读取中断".into()));
            let _ = tx.send(AppEvent::HomePerformance {
                generation,
                key,
                result,
            });
        }));
        true
    }

    fn open_performance(&mut self, handle: &Handle) {
        let Some((_, provider)) =
            super::overview::selected(self.overview.as_ref(), self.home_provider.as_deref())
        else {
            self.notice = Some("请先选择首页 Provider".into());
            return;
        };
        let Some(binding) = provider.performance.clone() else {
            self.notice = Some("此 Provider 尚未绑定本地请求日志".into());
            return;
        };
        self.performance = Some(super::performance::Panel::new(
            binding,
            provider.name.clone(),
        ));
        self.load_performance(handle, false);
    }

    fn handle_performance_key(&mut self, key: KeyCode, handle: &Handle) -> bool {
        let Some(panel) = &mut self.performance else {
            return false;
        };
        match panel.key(key) {
            super::performance::Action::Close => self.performance = None,
            super::performance::Action::Reload => self.load_performance(handle, false),
            super::performance::Action::Rescan => self.load_performance(handle, true),
            super::performance::Action::Stay => {}
        }
        true
    }

    fn load_performance(&mut self, handle: &Handle, rescan: bool) {
        let Some(panel) = &mut self.performance else {
            return;
        };
        panel.stop();
        self.performance_generation = self.performance_generation.wrapping_add(1);
        panel.generation = self.performance_generation;
        panel.loading = true;
        let generation = panel.generation;
        let request = crate::performance::LoadRequest {
            binding: panel.binding.clone(),
            period: panel.period,
            model: panel.detail.clone(),
            page: panel.page,
            now: Utc::now().timestamp_millis(),
            rediscover: rescan,
        };
        let tx = self.tx.clone();
        panel.task = Some(
            self.performance_service
                .start_load(handle, request, move |update| {
                    tx.send(AppEvent::Performance {
                        generation,
                        result: update.result,
                        loading: update.loading,
                    })
                    .is_ok()
                }),
        );
    }

    fn open_reset(&mut self, handle: &Handle) {
        let Some((_, selected)) =
            super::overview::selected(self.overview.as_ref(), self.home_provider.as_deref())
        else {
            self.notice = Some("请先选择首页 Provider".into());
            return;
        };
        let Some(reset) = &selected.usage.reset else {
            self.notice = Some(format!("{} 不支持主动重置", selected.name));
            return;
        };
        let provider = reset.provider();
        let provider_name = selected.name.clone();
        self.notice = None;
        self.reset_generation = self.reset_generation.wrapping_add(1);
        let generation = self.reset_generation;
        self.reset = Some(ResetFlow {
            provider_id: provider.id().into(),
            target: provider.cache_key(),
            provider_name,
            provider: provider.clone(),
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
        let Some(flow) = self.reset.as_mut() else {
            return false;
        };
        match flow.stage.key(key) {
            super::confirmation::Decision::Cancel => {
                self.reset = None;
                self.reset_generation = self.reset_generation.wrapping_add(1);
            }
            super::confirmation::Decision::Execute => self.consume_reset(handle),
            super::confirmation::Decision::Ignore => {}
        }
        true
    }

    fn consume_reset(&mut self, handle: &Handle) {
        let Some(flow) = self.reset.as_mut() else {
            return;
        };
        let provider = flow.provider.clone();
        let generation = flow.generation;
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
        if let Some(panel) = &self.performance {
            if !panel.loading && panel.refreshed.elapsed() >= Duration::from_secs(5) {
                self.load_performance(handle, false);
                return true;
            }
            return false;
        }
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
        let summary_started = self.refresh_home_performance(handle);
        if self.refresh_seconds == 0
            || self.pending > 0
            || self.overview_loading
            || self.reset.is_some()
            || self.browser.is_some()
        {
            return summary_started;
        }
        if self.last_refresh.elapsed() >= Duration::from_secs(self.refresh_seconds) {
            self.refresh_background(handle);
            return true;
        }
        summary_started
    }

    fn drain_events(&mut self) -> (bool, bool) {
        let mut changed = self
            .provider_action
            .as_mut()
            .is_some_and(|panel| panel.poll());
        changed |= self.operation_statuses.poll();
        if let Some(panel) = &mut self.provider_action {
            panel.update_status(&mut self.operation_statuses);
        }
        let mut reset_consumed = false;
        while let Ok(event) = self.rx.try_recv() {
            let event = match event {
                AppEvent::FingerprintLoaded { generation, result } => {
                    if generation == self.fingerprint_generation {
                        self.fingerprint_task = None;
                        changed |= self.subscription_tests.set(result);
                    }
                    continue;
                }
                AppEvent::HomePerformance {
                    generation,
                    key,
                    result,
                } => {
                    if generation == self.home_performance_generation {
                        self.home_performance_pending = false;
                        self.home_performance_task = None;
                        if self.overview.as_ref().is_some_and(|c| {
                            c.providers.iter().any(|p| {
                                p.performance.as_ref().is_some_and(|b| b.cache_key() == key)
                            })
                        }) {
                            self.home_performance.insert(
                                key,
                                super::performance::CachedSummary {
                                    result,
                                    updated: std::time::Instant::now(),
                                },
                            );
                            changed = true;
                        }
                    }
                    continue;
                }
                AppEvent::Performance {
                    generation,
                    result,
                    loading,
                } => {
                    let mut home_update = None;
                    if let Some(panel) = &mut self.performance
                        && panel.generation == generation
                    {
                        panel.loading = loading;
                        panel.refreshed = std::time::Instant::now();
                        match result {
                            Ok(snapshot) => {
                                if !loading {
                                    home_update = Some((
                                        panel.binding.cache_key(),
                                        (panel.period == crate::performance::Period::Day)
                                            .then(|| snapshot.summary.clone()),
                                    ));
                                }
                                panel.selected =
                                    panel.selected.min(snapshot.models.len().saturating_sub(1));
                                panel.snapshot = Some(snapshot);
                                panel.error = None;
                            }
                            Err(error) => panel.error = Some(error),
                        }
                        changed = true;
                    }
                    if let Some((key, summary)) = home_update {
                        self.home_performance_generation =
                            self.home_performance_generation.wrapping_add(1);
                        if let Some(task) = self.home_performance_task.take() {
                            task.abort();
                        }
                        self.home_performance_pending = false;
                        if let Some(summary) = summary {
                            self.home_performance.insert(
                                key,
                                super::performance::CachedSummary {
                                    result: Ok(Some(summary)),
                                    updated: std::time::Instant::now(),
                                },
                            );
                        } else {
                            self.home_performance.remove(&key);
                        }
                    }
                    continue;
                }
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
                    projects,
                } => {
                    if generation == self.home_sessions_generation {
                        self.home_sessions_loading = false;
                        let mut warnings = catalog.warnings;
                        // A failed read keeps the last project records rather than dropping manual projects.
                        let records = projects.map_err(|error| warnings.push(error)).ok();
                        self.home_sessions_warning =
                            (!warnings.is_empty()).then(|| warnings.join(" · "));
                        self.home_sessions
                            .refresh_with_projects(catalog.sessions, records);
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
                                    self.home_subscription_details = false;
                                    self.home_scroll = 0;
                                    self.home_models_expanded = false;
                                }
                                self.home_provider = selected;
                                self.operation_statuses.retain(
                                    snapshot
                                        .providers
                                        .iter()
                                        .flat_map(|p| p.operations.iter().cloned()),
                                );
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
                AppEvent::OverviewFinished { generation } => {
                    if generation == self.overview_generation {
                        changed |= self.overview_loading;
                        self.overview_loading = false;
                        self.overview_pending = 0;
                        self.overview_task = None;
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
                        if let Ok(preview) = &result
                            && let Some(flow) = &self.reset
                            && let Some(catalog) = &mut self.overview
                        {
                            catalog.apply_reset_credits(&flow.target, preview.available);
                        }
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
mod account_regression_tests;
#[cfg(test)]
mod fingerprint_event_tests;
#[cfg(test)]
mod home_board_tests;
#[cfg(test)]
mod overview_regression_tests;
#[cfg(test)]
mod performance_regression_tests;
#[cfg(test)]
mod tests;
