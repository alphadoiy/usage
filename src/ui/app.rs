//! 事件循环。查询在 tokio 任务里进行，界面保持可操作。

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use crate::background::{Task, Tasks};
use futures_util::FutureExt;

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
use crate::ui::history::{HistoryPanel, PickerRequest, ReportRequest};
use crate::ui::menu::{self, Action as MenuAction, Command as MenuCommand};
use crate::ui::session_browser::{Focus, SessionBrowser};
mod render;
mod state;
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
#[derive(Clone)]
struct BrowserUsageRequest {
    history: Arc<crate::history::History>,
    sources: Vec<String>,
    query: crate::history::HistoryQuery,
    label: Option<String>,
}
impl BrowserUsageRequest {
    fn same_key(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.history, &other.history)
            && self.sources == other.sources
            && self.query == other.query
    }
    fn run(
        &self,
        cancel: &crate::background::Cancellation,
    ) -> Result<HashMap<sessions::SessionKey, crate::history::SessionUsage>, String> {
        self.history
            .session_usage_controlled(&self.sources, &self.query, cancel)
    }
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
    HistoryFailed(String),
    HistoryCached(crate::history::History),
    HistoryLoaded {
        history: crate::history::History,
        cache: crate::history::HistoryCache,
    },
    HistoryReport {
        generation: u64,
        request: ReportRequest,
        result: Result<crate::history::HistoryReport, String>,
    },
    HistoryPicker {
        generation: u64,
        request: PickerRequest,
        result: Result<Vec<String>, String>,
    },
    BrowserUsage {
        generation: u64,
        request: BrowserUsageRequest,
        result: Result<HashMap<sessions::SessionKey, crate::history::SessionUsage>, String>,
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
    app.history_state.panel = Some(HistoryPanel::new(
        app.history_state.config.providers.clone(),
        year,
        monthly,
    ));
    if let Some(panel) = &mut app.history_state.panel {
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
            if app.history_state.config.alerts.bell {
                use std::io::Write;
                let _ = write!(io::stdout(), "\x07");
                let _ = io::stdout().flush();
            }
            if app.history_state.config.alerts.desktop {
                let tx = app.tx.clone();
                app.background_tasks.push(Task::blocking(
                    &handle,
                    move |cancel| {
                        for alert in alerts {
                            if cancel.is_cancelled() {
                                break;
                            }
                            crate::alerts::notify_desktop_controlled(&alert.message, &cancel)?;
                        }
                        Ok(())
                    },
                    move |result| {
                        if let Err(error) = result {
                            let _ = tx.send(AppEvent::Notice(error));
                        }
                    },
                ));
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
        app.ensure_history_picker(&handle);
        app.ensure_history_report(&handle);
        app.ensure_browser_usage(&handle);
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
                                app.browser_state.filter = None;
                                app.open_browser(&handle);
                            }
                            MenuCommand::Activate(MenuAction::Providers) => {
                                crate::ui::terminal::leave();
                                let result = crate::ui::run_provider_manager_with_config(
                                    handle.clone(),
                                    Some(app.config_path.clone()),
                                    // Reuse the home board's sessions once loaded rather than scanning again.
                                    if app.home_sessions_state.generation > 0
                                        && !app.home_sessions_state.loading
                                    {
                                        crate::ui::ProjectSessions::from_sessions(
                                            &app.home_sessions_state.browser.sessions,
                                        )
                                    } else {
                                        crate::ui::ProjectSessions::Scan(
                                            app.browser_state.registry.clone(),
                                            app.browser_state.homes.clone(),
                                        )
                                    },
                                    // The project picked on the home board becomes the launch default.
                                    (app.home_focus == super::home::Focus::Projects)
                                        .then(|| {
                                            app.home_sessions_state
                                                .browser
                                                .selected_project()
                                                .cloned()
                                        })
                                        .flatten(),
                                );
                                terminal = match crate::ui::terminal::enter() {
                                    Ok(t) => t,
                                    Err(e) => break Err(io::Error::other(e)),
                                };
                                app.refresh_background(&handle);
                                app.notice = result.err().map(|e| e.to_string());
                                for instance in app.history_state.config.data_instances() {
                                    if ["codex", "claude", "grok"].contains(&instance.id.as_str()) {
                                        continue;
                                    }
                                    app.browser_state.homes =
                                        app.browser_state.homes.clone().with_instance(
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
                    if app.history_state.panel.is_some() {
                        if let Some(panel) = &mut app.history_state.panel
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
                                if let Some(panel) = &mut app.history_state.panel {
                                    panel.key(code);
                                }
                            }
                        }
                        redraw.invalidate();
                        continue;
                    }
                    if app.browser_state.panel.is_some() {
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
                            app.browser_state.filter = None;
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
    let Some(session) = app.browser_state.resume_request.take() else {
        return Ok(());
    };
    crate::ui::terminal::leave();
    let result = resume_in_terminal(handle, &session);
    *terminal = crate::ui::terminal::enter().map_err(io::Error::other)?;
    if let Some(browser) = &mut app.browser_state.panel {
        browser.launching = false;
    }
    app.home_sessions_state.browser.launching = false;
    if app.history_state.panel.is_none() && app.browser_state.panel.is_none() {
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
    fingerprint: state::FingerprintState,
    home_performance: state::HomePerformanceState,
    performance: state::PerformanceState,
    home_activity: state::HomeActivityState,
    home_focus: super::home::Focus,
    home_connections: super::home::Connections,
    home_sessions_state: state::HomeSessionsState,
    overview_state: state::OverviewState,
    service_state: state::ServiceState,
    home_scroll: u16,
    home_models_expanded: bool,
    home_provider: Option<String>,
    menu: Option<menu::Menu>,
    history_state: state::HistoryState,
    browser_state: state::BrowserState,
    background_tasks: Tasks,
    quota_refresh_due: bool,
    monitor: crate::alerts::QuotaMonitor,
    alerts_queue: Vec<crate::alerts::Alert>,
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
    reset_preview_task: Option<Task>,
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
            fingerprint: state::FingerprintState::new(),
            home_performance: state::HomePerformanceState::new(),
            performance: state::PerformanceState::new(),
            home_activity: state::HomeActivityState::new(),
            home_focus: super::home::Focus::Providers,
            home_connections: super::home::Connections::default(),
            home_sessions_state: state::HomeSessionsState::new(config.resume),
            overview_state: state::OverviewState::new(),
            service_state: state::ServiceState::new(),
            home_scroll: 0,
            home_models_expanded: false,
            home_provider: None,
            menu: None,
            history_state: state::HistoryState::new(config.clone()),
            browser_state: state::BrowserState::new(session_homes, session_registry),
            background_tasks: Tasks::default(),
            quota_refresh_due: false,
            monitor: crate::alerts::QuotaMonitor::default(),
            alerts_queue: Vec::new(),
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
            reset_preview_task: None,
        }
    }

    /// Menu navigation is handled before page keys, so Enter cannot launch a session beneath it.
    fn handle_navigation_key(&mut self, key: KeyCode) -> Option<MenuCommand> {
        if self.reset.is_some()
            || self
                .browser_state
                .panel
                .as_ref()
                .is_some_and(|b| b.launching)
            || self.home_sessions_state.browser.launching
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
                && self.history_state.panel.is_none()
                && self.browser_state.panel.is_none()
                && !self.home_sessions_state.browser.searching)
        {
            let current = if self.history_state.panel.is_some() {
                MenuAction::History
            } else if self.browser_state.panel.is_some() {
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
                if self.history_state.panel.is_none()
                    && self.browser_state.panel.is_none()
                    && self.home_focus == super::home::Focus::Providers =>
            {
                Some(MenuCommand::Activate(MenuAction::AddProvider))
            }
            _ => None,
        }
    }

    fn close_history(&mut self) {
        self.history_state.close_panel();
    }
    fn close_browser(&mut self, return_history: bool) {
        let was_returning = self.browser_state.close();
        self.notice = None;
        if return_history && was_returning {
            self.history_state.panel = self.history_state.saved.take();
        }
    }
    fn open_history(&mut self, handle: &Handle) {
        use chrono::Datelike;
        if self.history_state.panel.is_some() {
            return;
        }
        self.close_browser(false);
        self.history_state.panel = Some(self.history_state.saved.take().unwrap_or_else(|| {
            HistoryPanel::new(
                self.history_state.config.providers.clone(),
                chrono::Local::now().year(),
                false,
            )
        }));
        self.start_history_load(handle);
    }
    fn open_history_sessions(&mut self, handle: &Handle) {
        let Some(panel) = &self.history_state.panel else {
            return;
        };
        if panel.loading {
            return;
        }
        let filter = (panel.sources(), panel.session_query());
        self.close_history();
        self.browser_state.saved = None;
        self.browser_state.filter = Some(filter);
        self.open_browser(handle);
        self.browser_state.return_to_history = true;
    }
    fn apply_browser_usage(&mut self) {
        let Some(request) = self.browser_usage_request() else {
            return;
        };
        if request.history.is_database_backed() {
            if let Some((cached, usage)) = &self.browser_state.usage_cache
                && cached.same_key(&request)
                && let Some(browser) = &mut self.browser_state.panel
            {
                browser.set_usage(usage.clone(), request.label);
            }
            return;
        }
        let usage = request
            .history
            .session_usage(&request.sources, &request.query);
        if let Some(browser) = &mut self.browser_state.panel {
            browser.set_usage(usage, request.label);
        }
    }
    fn browser_usage_request(&self) -> Option<BrowserUsageRequest> {
        self.browser_state.panel.as_ref()?;
        let (sources, query, label) = self.browser_state.filter.as_ref().map_or_else(
            || {
                (
                    self.history_state.config.providers.clone(),
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
        Some(BrowserUsageRequest {
            history: self.history_state.loaded.clone(),
            sources,
            query,
            label,
        })
    }
    fn ensure_browser_usage(&mut self, handle: &Handle) {
        let desired = self
            .browser_usage_request()
            .filter(|r| r.history.is_database_backed());
        if desired.is_none() && self.browser_state.usage_pending.is_none() {
            return;
        }
        if let Some(request) = &desired {
            if self
                .browser_state
                .usage_cache
                .as_ref()
                .is_some_and(|(cached, _)| cached.same_key(request))
            {
                return;
            }
            if self
                .browser_state
                .usage_pending
                .as_ref()
                .is_some_and(|pending| pending.same_key(request))
            {
                return;
            }
        }
        self.browser_state.usage_task = None;
        self.browser_state.usage_pending = None;
        self.browser_state.usage_generation = self.browser_state.usage_generation.wrapping_add(1);
        let Some(request) = desired else { return };
        if let Some(browser) = &mut self.browser_state.panel {
            let label = request.label.as_ref().map_or_else(
                || "正在汇总会话用量…".to_string(),
                |label| format!("{label} · 正在汇总"),
            );
            browser.set_usage(HashMap::new(), Some(label));
        }
        let generation = self.browser_state.usage_generation;
        let work_request = request.clone();
        let event_request = request.clone();
        let tx = self.tx.clone();
        self.browser_state.usage_pending = Some(request);
        self.browser_state.usage_task = Some(Task::blocking(
            handle,
            move |cancel| work_request.run(&cancel),
            move |result| {
                let _ = tx.send(AppEvent::BrowserUsage {
                    generation,
                    request: event_request,
                    result,
                });
            },
        ));
    }
    fn start_history_load(&mut self, handle: &Handle) {
        let Some(mut cache) = self.history_state.begin_load() else {
            return;
        };
        let config = self.history_state.config.clone();
        let tx = self.tx.clone();
        let cached_tx = tx.clone();
        self.history_state.task = Some(Task::blocking(
            handle,
            move |cancel| {
                let history =
                    cache.load_compact_with_cached_controlled(&config, &cancel, |history| {
                        if !cancel.is_cancelled() {
                            let _ = cached_tx.send(AppEvent::HistoryCached(history));
                        }
                    });
                Ok((history, cache))
            },
            move |result| {
                let event = match result {
                    Ok((history, cache)) => AppEvent::HistoryLoaded { history, cache },
                    Err(error) => AppEvent::HistoryFailed(error),
                };
                let _ = tx.send(event);
            },
        ));
    }

    fn ensure_history_report(&mut self, handle: &Handle) {
        let desired = self
            .history_state
            .panel
            .as_ref()
            .and_then(HistoryPanel::report_request);
        if desired.is_none() && self.history_state.report_pending.is_none() {
            return;
        }
        if let (Some(pending), Some(request)) = (&self.history_state.report_pending, &desired)
            && pending.same_key(request)
        {
            return;
        }
        self.history_state.report_task = None;
        self.history_state.report_pending = None;
        self.history_state.report_generation = self.history_state.report_generation.wrapping_add(1);
        let Some(request) = desired else { return };
        let generation = self.history_state.report_generation;
        let work_request = request.clone();
        let event_request = request.clone();
        let tx = self.tx.clone();
        self.history_state.report_pending = Some(request);
        self.history_state.report_task = Some(Task::blocking(
            handle,
            move |cancel| Ok(work_request.run(&cancel)),
            move |result| {
                let _ = tx.send(AppEvent::HistoryReport {
                    generation,
                    request: event_request,
                    result,
                });
            },
        ));
    }

    fn ensure_history_picker(&mut self, handle: &Handle) {
        let desired = self
            .history_state
            .panel
            .as_ref()
            .and_then(HistoryPanel::picker_request);
        if desired.is_none() && self.history_state.picker_pending.is_none() {
            return;
        }
        if let (Some(pending), Some(request)) = (&self.history_state.picker_pending, &desired)
            && pending.same_key(request)
        {
            return;
        }
        self.history_state.picker_task = None;
        self.history_state.picker_pending = None;
        self.history_state.picker_generation = self.history_state.picker_generation.wrapping_add(1);
        let Some(request) = desired else { return };
        let generation = self.history_state.picker_generation;
        let work_request = request.clone();
        let event_request = request.clone();
        let tx = self.tx.clone();
        self.history_state.picker_pending = Some(request);
        self.history_state.picker_task = Some(Task::blocking(
            handle,
            move |cancel| work_request.run(&cancel),
            move |result| {
                let _ = tx.send(AppEvent::HistoryPicker {
                    generation,
                    request: event_request,
                    result,
                });
            },
        ));
    }

    fn open_browser(&mut self, handle: &Handle) {
        if self.browser_state.panel.is_some() {
            self.apply_browser_usage();
            return;
        }
        self.close_history();
        if self.providers.is_empty() && self.history_state.saved.is_some() {
            self.browser_state.return_to_history = true;
        }
        self.browser_state.panel = Some(self.browser_state.saved.take().unwrap_or_else(|| {
            let mut browser = SessionBrowser::new(Vec::new());
            browser.resume_mode = self.history_state.config.resume;
            browser
        }));
        self.apply_browser_usage();
        self.start_browser_load(handle);
        if self.history_state.loaded.synced_at.is_none() {
            self.start_history_load(handle);
        }
    }

    fn start_browser_load(&mut self, handle: &Handle) {
        let Some(generation) = self.browser_state.begin_load() else {
            return;
        };
        let homes = self.browser_state.homes.clone();
        let registry = self.browser_state.registry.clone();
        let tx = self.tx.clone();
        self.notice = None;
        self.browser_state.task = Some(Task::blocking(
            handle,
            move |cancel| Ok(registry.load_all_controlled(&homes, &cancel)),
            move |result| {
                let catalog = result.unwrap_or_else(|error| sessions::SessionCatalog {
                    warnings: vec![error],
                    ..Default::default()
                });
                let _ = tx.send(AppEvent::SessionsLoaded {
                    generation,
                    catalog,
                });
            },
        ));
    }

    fn handle_browser_key(&mut self, key: KeyCode, handle: &Handle) {
        if self
            .browser_state
            .panel
            .as_ref()
            .is_some_and(|browser| browser.launching)
        {
            return;
        }
        if self
            .browser_state
            .panel
            .as_ref()
            .is_some_and(|browser| browser.searching)
        {
            if let Some(browser) = &mut self.browser_state.panel {
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
                    .browser_state
                    .panel
                    .as_ref()
                    .is_some_and(|b| b.focus() == Focus::Sessions) =>
            {
                if let Some(browser) = &mut self.browser_state.panel {
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
                if let Some(browser) = &mut self.browser_state.panel {
                    browser.resume_mode = browser.resume_mode.next();
                }
            }
            KeyCode::Char('y') => {
                if let Some(session) = self
                    .browser_state
                    .panel
                    .as_ref()
                    .and_then(|b| b.selected_session())
                    .cloned()
                {
                    let tx = self.tx.clone();
                    // An accepted clipboard write completes independently of the view.
                    handle.spawn_blocking(move || {
                        let result = sessions::copy_resume_command(&session)
                            .map(|_| "已复制恢复命令（含项目及实例目录）".into())
                            .unwrap_or_else(|e| e);
                        let _ = tx.send(AppEvent::Notice(result));
                    });
                }
            }
            KeyCode::Char('o') => {
                if let Some(browser) = &mut self.browser_state.panel {
                    browser.toggle_sort();
                }
            }
            KeyCode::Tab => {
                if let Some(browser) = &mut self.browser_state.panel {
                    browser.toggle_focus();
                }
            }
            KeyCode::Left | KeyCode::BackTab => {
                if let Some(browser) = &mut self.browser_state.panel {
                    browser.focus_projects();
                }
            }
            KeyCode::Right => {
                if let Some(browser) = &mut self.browser_state.panel {
                    browser.focus_sessions();
                }
            }
            KeyCode::Char('/') => {
                if let Some(browser) = &mut self.browser_state.panel {
                    browser.searching = true;
                }
            }
            KeyCode::Char('c') => {
                if let Some(browser) = &mut self.browser_state.panel {
                    browser.clear_search();
                }
            }
            KeyCode::Backspace => {
                if let Some(browser) = &mut self.browser_state.panel {
                    browser.backspace();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(browser) = &mut self.browser_state.panel {
                    browser.move_by(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(browser) = &mut self.browser_state.panel {
                    browser.move_by(1);
                }
            }
            KeyCode::Enter => {
                if self.browser_state.loading {
                    return;
                }
                let Some(browser) = &mut self.browser_state.panel else {
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
        if self.history_state.panel.is_some()
            || self.browser_state.panel.is_some()
            || self.fingerprint.task.is_some()
            || self
                .fingerprint
                .last_poll
                .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.fingerprint.last_poll = Some(std::time::Instant::now());
        self.fingerprint.generation = self.fingerprint.generation.wrapping_add(1);
        let generation = self.fingerprint.generation;
        let service = self.fingerprint.service.clone();
        let tx = self.tx.clone();
        self.fingerprint.task = Some(Task::blocking(
            handle,
            move |_| {
                service
                    .snapshots()
                    .map_err(|_| "本机测试记录读取失败".to_owned())
            },
            move |result| {
                let _ = tx.send(AppEvent::FingerprintLoaded { generation, result });
            },
        ));
    }

    fn maybe_refresh_home_activity(&mut self, handle: &Handle) {
        if self.history_state.panel.is_some()
            || self.browser_state.panel.is_some()
            || self.home_activity.pending
            || self.home_sessions_state.loading
            || self.home_sessions_state.browser.sessions.is_empty()
            || (!self.home_activity.dirty
                && (self.refresh_seconds == 0
                    || self.home_activity.last_poll.elapsed() < Duration::from_secs(2)))
        {
            return;
        }
        self.home_activity.dirty = false;
        self.home_activity.pending = true;
        self.home_activity.generation = self.home_activity.generation.wrapping_add(1);
        let generation = self.home_activity.generation;
        self.home_activity.last_poll = std::time::Instant::now();
        let root = self.home_activity.root.clone();
        let sessions = self.home_sessions_state.browser.sessions.clone();
        let tx = self.tx.clone();
        self.home_activity.task = Some(Task::blocking(
            handle,
            move |_| {
                let Some(manager) = crate::codex_manager::Manager::open_read_only(root)
                    .map_err(|_| "运行状态读取失败".to_string())?
                else {
                    return Ok(HashMap::new());
                };
                manager
                    .session_activity(&sessions)
                    .map_err(|_| "运行状态读取失败".to_string())
            },
            move |result| {
                let _ = tx.send(AppEvent::HomeActivityLoaded { generation, result });
            },
        ));
    }

    fn start_home_sessions_load(&mut self, handle: &Handle) {
        if self.home_sessions_state.loading || self.home_sessions_state.browser.launching {
            return;
        }
        self.home_sessions_state.loading = true;
        self.home_sessions_state.generation = self.home_sessions_state.generation.wrapping_add(1);
        let generation = self.home_sessions_state.generation;
        let mut homes = self.browser_state.homes.clone();
        let registry = self.browser_state.registry.clone();
        let store = self.home_sessions_state.project_store.clone();
        let tx = self.tx.clone();
        self.home_sessions_state.task = Some(Task::blocking(
            handle,
            move |cancel| {
                if let Ok(instances) = crate::codex_manager::managed_instances() {
                    for instance in instances {
                        if cancel.is_cancelled() {
                            return Err("已取消".into());
                        }
                        homes =
                            homes.with_instance(&instance.id, &instance.provider, &instance.path);
                    }
                }
                let catalog = registry.load_all_controlled(&homes, &cancel);
                Ok((catalog, store.list()))
            },
            move |result| {
                let (catalog, projects) = result.unwrap_or_else(|error| {
                    (
                        sessions::SessionCatalog {
                            warnings: vec![error.clone()],
                            ..Default::default()
                        },
                        Err(error),
                    )
                });
                let _ = tx.send(AppEvent::HomeSessionsLoaded {
                    generation,
                    catalog,
                    projects,
                });
            },
        ));
    }

    fn launch_session(
        &mut self,
        session: sessions::Session,
        mode: crate::config::ResumeMode,
        home: bool,
        handle: &Handle,
    ) {
        if !sessions::uses_ghostty(mode) {
            self.browser_state.resume_request = Some(session);
            return;
        }
        self.notice = Some("正在 Ghostty 右侧打开会话…".into());
        let tx = self.tx.clone();
        let generation = if home {
            self.home_sessions_state.generation
        } else {
            self.browser_state.generation
        };
        // Accepted launches must not be cancelled when their source view closes.
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
        if self.home_sessions_state.browser.launching {
            return true;
        }
        if self.home_focus == HomeFocus::Projects && self.home_sessions_state.browser.searching {
            match key {
                KeyCode::Esc | KeyCode::Enter => self.home_sessions_state.browser.searching = false,
                KeyCode::Backspace => self.home_sessions_state.browser.backspace(),
                KeyCode::Char(ch) => self.home_sessions_state.browser.type_char(ch),
                _ => {}
            }
            return true;
        }
        match key {
            KeyCode::Char('v') if self.home_focus == HomeFocus::Providers => {
                self.open_performance(handle)
            }
            KeyCode::Char('c') if self.home_focus == HomeFocus::Providers => {
                if let Some((_, provider)) = super::overview::selected(
                    self.overview_state.catalog.as_ref(),
                    self.home_provider.as_deref(),
                ) {
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
                if super::overview::selected(
                    self.overview_state.catalog.as_ref(),
                    self.home_provider.as_deref(),
                )
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
                            &self.service_state.statuses,
                            &self.service_state.history,
                            self.service_state.pending > 0,
                        );
                    }
                    HomeFocus::Projects => self.home_sessions_state.browser.move_by(delta as isize),
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
                self.home_sessions_state.browser.focus_projects()
            }
            KeyCode::Right if self.home_focus == HomeFocus::Projects => {
                self.home_sessions_state.browser.focus_sessions()
            }
            KeyCode::Esc
                if self.home_focus == HomeFocus::Projects
                    && self.home_sessions_state.browser.focus() == Focus::Sessions =>
            {
                self.home_sessions_state.browser.focus_projects()
            }
            KeyCode::Char('/') if self.home_focus == HomeFocus::Projects => {
                self.home_sessions_state.browser.searching = true
            }
            KeyCode::Char('c') if self.home_focus == HomeFocus::Projects => {
                self.home_sessions_state.browser.clear_search()
            }
            KeyCode::Char('R') if self.home_focus == HomeFocus::Projects => {
                self.home_sessions_state.browser.resume_mode =
                    self.home_sessions_state.browser.resume_mode.next()
            }
            KeyCode::Enter => {
                if self.home_sessions_state.loading {
                    return true;
                }
                if self.home_sessions_state.browser.focus() == Focus::Projects {
                    self.home_sessions_state.browser.focus_sessions();
                    return true;
                }
                if let Some(session) = self.home_sessions_state.browser.selected_session().cloned()
                {
                    self.home_sessions_state.browser.launching = true;
                    self.launch_session(
                        session,
                        self.home_sessions_state.browser.resume_mode,
                        true,
                        handle,
                    );
                }
            }
            _ => return false,
        }
        true
    }

    fn cycle_home_provider(&mut self, reverse: bool) {
        let Some(catalog) = self
            .overview_state
            .catalog
            .as_ref()
            .filter(|c| c.providers.len() > 1)
        else {
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
        if self.history_state.panel.is_some()
            || self.browser_state.panel.is_some()
            || self.provider_action.is_some()
            || self.reset.is_some()
            || self.home_subscription_details
        {
            return false;
        }
        let Some((_, provider)) = super::overview::selected(
            self.overview_state.catalog.as_ref(),
            self.home_provider.as_deref(),
        ) else {
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
        if let Some((_, provider)) = super::overview::selected(
            self.overview_state.catalog.as_ref(),
            self.home_provider.as_deref(),
        ) {
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
        if self.history_state.panel.is_none() && self.browser_state.panel.is_none() {
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
            self.background_tasks
                .push(Task::spawn(handle, move |_| async move {
                    let target = provider.cache_key();
                    let snapshot = std::panic::AssertUnwindSafe(query_with_timeout(
                        provider.as_ref(),
                        QUERY_TIMEOUT,
                    ))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| {
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
                }));
        }
    }

    fn refresh_overview(&mut self, handle: &Handle) {
        self.refresh_overview_with_models(handle, true);
    }

    fn refresh_overview_with_models(&mut self, handle: &Handle, refresh_models: bool) {
        if !refresh_models && self.overview_state.loading {
            return;
        }
        if let Some(task) = self.overview_state.task.take() {
            task.abort();
        }
        self.overview_state.generation = self.overview_state.generation.wrapping_add(1);
        let generation = self.overview_state.generation;
        self.overview_state.loading = true;
        self.overview_state.error = None;
        let previous = self.overview_state.catalog.clone();
        let service = self.overview_state.service.clone();
        let request = crate::provider_catalog::RefreshRequest {
            config_path: self.config_path.clone(),
            previous,
            refresh_models,
        };
        let tx = self.tx.clone();
        self.overview_state.task = Some(Task::spawn(handle, move |_| async move {
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
                .await;
        }));
    }

    fn refresh_service_statuses(&mut self, handle: &Handle) {
        if self.service_state.pending > 0 {
            return;
        }
        let sources = super::home::SOURCES;
        self.service_state.generation = self.service_state.generation.wrapping_add(1);
        let generation = self.service_state.generation;
        self.service_state.pending = sources.len();
        for source in sources {
            let http = Arc::clone(&self.service_state.http);
            let tx = self.tx.clone();
            self.background_tasks
                .push(Task::spawn(handle, move |_| async move {
                    let snapshot =
                        std::panic::AssertUnwindSafe(service_status::query(http.as_ref(), source))
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|_| ServiceStatus::failed(source, "状态查询任务中断"));
                    let _ = tx.send(AppEvent::ServiceStatus {
                        generation,
                        snapshot,
                    });
                }));
        }
    }

    fn refresh_home_performance(&mut self, handle: &Handle) -> bool {
        if self.home_performance.pending {
            return false;
        }
        let Some(binding) = super::overview::selected(
            self.overview_state.catalog.as_ref(),
            self.home_provider.as_deref(),
        )
        .and_then(|(_, p)| p.performance.clone()) else {
            return false;
        };
        let key = binding.cache_key();
        if self
            .home_performance
            .cached
            .get(&key)
            .is_some_and(|s| s.updated.elapsed() < super::performance::HOME_TTL)
        {
            return false;
        }
        self.home_performance.generation = self.home_performance.generation.wrapping_add(1);
        let generation = self.home_performance.generation;
        self.home_performance.pending = true;
        let service = self.performance.service.clone();
        let tx = self.tx.clone();
        self.home_performance.task = Some(Task::blocking(
            handle,
            move |_| {
                service.summary(
                    &binding,
                    crate::performance::Period::Day,
                    Utc::now().timestamp_millis(),
                )
            },
            move |result| {
                let _ = tx.send(AppEvent::HomePerformance {
                    generation,
                    key,
                    result,
                });
            },
        ));
        true
    }

    fn open_performance(&mut self, handle: &Handle) {
        let Some((_, provider)) = super::overview::selected(
            self.overview_state.catalog.as_ref(),
            self.home_provider.as_deref(),
        ) else {
            self.notice = Some("请先选择首页 Provider".into());
            return;
        };
        let Some(binding) = provider.performance.clone() else {
            self.notice = Some("此 Provider 尚未绑定本地请求日志".into());
            return;
        };
        self.performance.panel = Some(super::performance::Panel::new(
            binding,
            provider.name.clone(),
        ));
        self.load_performance(handle, false);
    }

    fn handle_performance_key(&mut self, key: KeyCode, handle: &Handle) -> bool {
        let Some(panel) = &mut self.performance.panel else {
            return false;
        };
        match panel.key(key) {
            super::performance::Action::Close => self.performance.panel = None,
            super::performance::Action::Reload => self.load_performance(handle, false),
            super::performance::Action::Rescan => self.load_performance(handle, true),
            super::performance::Action::Stay => {}
        }
        true
    }

    fn load_performance(&mut self, handle: &Handle, rescan: bool) {
        let Some(panel) = &mut self.performance.panel else {
            return;
        };
        panel.stop();
        self.performance.generation = self.performance.generation.wrapping_add(1);
        panel.generation = self.performance.generation;
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
            self.performance
                .service
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
        let Some((_, selected)) = super::overview::selected(
            self.overview_state.catalog.as_ref(),
            self.home_provider.as_deref(),
        ) else {
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
        self.reset_preview_task = Some(Task::spawn(handle, move |_| async move {
            let result = std::panic::AssertUnwindSafe(async {
                match provider.reset() {
                    Some(reset) => tokio::time::timeout(QUERY_TIMEOUT, reset.preview_reset())
                        .await
                        .unwrap_or_else(|_| Err(ProviderError("查询主动重置次数超时".into()))),
                    None => Err(ProviderError("工具不支持主动重置".into())),
                }
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(ProviderError("主动重置预览任务中断".into())));
            let _ = tx.send(AppEvent::ResetPreview { generation, result });
        }));
    }

    fn handle_reset_key(&mut self, key: KeyCode, handle: &Handle) -> bool {
        let Some(flow) = self.reset.as_mut() else {
            return false;
        };
        match flow.stage.key(key) {
            super::confirmation::Decision::Cancel => {
                self.reset = None;
                self.reset_preview_task = None;
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
        // This mutation has already been confirmed; closing the view does not undo it.
        handle.spawn(async move {
            let result = match provider.reset() {
                Some(reset) => reset.consume_reset().await,
                None => Err(ProviderError("工具不支持主动重置".into())),
            };
            let _ = tx.send(AppEvent::ResetConsumed { generation, result });
        });
    }

    fn maybe_auto_refresh(&mut self, handle: &Handle) -> bool {
        if let Some(panel) = &self.performance.panel {
            if !panel.loading && panel.refreshed.elapsed() >= Duration::from_secs(5) {
                self.load_performance(handle, false);
                return true;
            }
            return false;
        }
        if self.history_state.panel.is_some() || self.browser_state.panel.is_some() {
            if self
                .browser_state
                .panel
                .as_ref()
                .is_some_and(|b| b.launching)
            {
                return false;
            }
            if self.refresh_seconds > 0
                && !self.history_state.loading
                && self.history_state.last_refresh.elapsed()
                    >= Duration::from_secs(self.refresh_seconds)
            {
                if self.browser_state.panel.is_some() {
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
            || self.overview_state.loading
            || self.reset.is_some()
            || self.browser_state.panel.is_some()
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
                    if generation == self.fingerprint.generation {
                        self.fingerprint.task = None;
                        changed |= self.subscription_tests.set(result);
                    }
                    continue;
                }
                AppEvent::HomePerformance {
                    generation,
                    key,
                    result,
                } => {
                    if generation == self.home_performance.generation {
                        self.home_performance.pending = false;
                        self.home_performance.task = None;
                        if self.overview_state.catalog.as_ref().is_some_and(|c| {
                            c.providers.iter().any(|p| {
                                p.performance.as_ref().is_some_and(|b| b.cache_key() == key)
                            })
                        }) {
                            self.home_performance.cached.insert(
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
                    if let Some(panel) = &mut self.performance.panel
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
                        self.home_performance.generation =
                            self.home_performance.generation.wrapping_add(1);
                        if let Some(task) = self.home_performance.task.take() {
                            task.abort();
                        }
                        self.home_performance.pending = false;
                        if let Some(summary) = summary {
                            self.home_performance.cached.insert(
                                key,
                                super::performance::CachedSummary {
                                    result: Ok(Some(summary)),
                                    updated: std::time::Instant::now(),
                                },
                            );
                        } else {
                            self.home_performance.cached.remove(&key);
                        }
                    }
                    continue;
                }
                AppEvent::HomeActivityLoaded { generation, result } => {
                    if generation == self.home_activity.generation {
                        self.home_activity.pending = false;
                        match result {
                            Ok(activity) => {
                                changed |= self.home_activity.sessions != activity
                                    || self.home_activity.error.is_some();
                                self.home_activity.sessions = activity;
                                self.home_activity.error = None;
                            }
                            Err(error) => {
                                changed |= !self.home_activity.sessions.is_empty()
                                    || self.home_activity.error.as_ref() != Some(&error);
                                self.home_activity.sessions.clear();
                                self.home_activity.error = Some(error);
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
                    if generation == self.home_sessions_state.generation {
                        self.home_sessions_state.loading = false;
                        let mut warnings = catalog.warnings;
                        // A failed read keeps the last project records rather than dropping manual projects.
                        let records = projects.map_err(|error| warnings.push(error)).ok();
                        self.home_sessions_state.warning =
                            (!warnings.is_empty()).then(|| warnings.join(" · "));
                        self.home_sessions_state
                            .browser
                            .refresh_with_projects(catalog.sessions, records);
                        self.home_activity.invalidate();
                        let keys: HashSet<_> = self
                            .home_sessions_state
                            .browser
                            .sessions
                            .iter()
                            .map(sessions::Session::key)
                            .collect();
                        self.home_activity
                            .sessions
                            .retain(|key, _| keys.contains(key));
                        changed = true;
                    }
                    continue;
                }
                AppEvent::HomeSessionLaunched { generation, result } => {
                    if generation == self.home_sessions_state.generation {
                        self.home_sessions_state.browser.launching = false;
                        self.notice = Some(match result {
                            Ok(()) => "已在 Ghostty 右侧恢复会话".into(),
                            Err(error) => error,
                        });
                        changed = true;
                    }
                    continue;
                }
                AppEvent::OverviewLoaded { generation, result } => {
                    if generation == self.overview_state.generation {
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
                                self.overview_state.catalog = Some(snapshot);
                                self.overview_state.pending = pending;
                                self.overview_state.loading = pending > 0;
                                self.overview_state.error = None;
                            }
                            Err(message) => {
                                self.overview_state.catalog = None;
                                self.overview_state.error = Some(message);
                                self.overview_state.pending = 0;
                                self.overview_state.loading = false;
                            }
                        }
                        changed = true;
                    }
                    continue;
                }
                AppEvent::OverviewFinished { generation } => {
                    if generation == self.overview_state.generation {
                        changed |= self.overview_state.loading;
                        self.overview_state.loading = false;
                        self.overview_state.pending = 0;
                        self.overview_state.task = None;
                    }
                    continue;
                }
                AppEvent::ProviderModels {
                    generation,
                    target,
                    models,
                } => {
                    if generation == self.overview_state.generation
                        && self
                            .overview_state
                            .catalog
                            .as_mut()
                            .is_some_and(|catalog| catalog.apply_models(&target, models))
                    {
                        self.overview_state.complete_one();
                        changed = true;
                    }
                    continue;
                }
                AppEvent::ProviderResource {
                    generation,
                    target,
                    snapshot,
                } => {
                    if generation == self.overview_state.generation
                        && self
                            .overview_state
                            .catalog
                            .as_mut()
                            .is_some_and(|catalog| catalog.apply_resource(&target, snapshot))
                    {
                        self.overview_state.complete_one();
                        changed = true;
                    }
                    continue;
                }
                AppEvent::ServiceStatus {
                    generation,
                    snapshot,
                } => {
                    if generation == self.service_state.generation {
                        self.service_state.accept(snapshot);
                        changed = true;
                    }
                    continue;
                }
                AppEvent::Notice(message) => {
                    self.notice = Some(message);
                    changed = true;
                    continue;
                }
                AppEvent::HistoryFailed(error) => {
                    self.history_state.finish_load();
                    self.notice = Some(error);
                    changed = true;
                    continue;
                }
                AppEvent::HistoryCached(history) => {
                    self.history_state.show_snapshot(Arc::new(history));
                    self.apply_browser_usage();
                    changed = true;
                    continue;
                }
                AppEvent::HistoryLoaded { history, cache } => {
                    self.history_state.cache = Some(cache);
                    let history = if history.synced_at.is_none()
                        && self.history_state.loaded.synced_at.is_some()
                    {
                        self.notice = history.warnings.first().cloned();
                        self.history_state.loaded.clone()
                    } else {
                        Arc::new(history)
                    };
                    self.history_state.show_snapshot(history);
                    self.history_state.finish_load();
                    self.apply_browser_usage();
                    changed = true;
                    continue;
                }
                AppEvent::HistoryReport {
                    generation,
                    request,
                    result,
                } => {
                    if generation == self.history_state.report_generation {
                        self.history_state.report_task = None;
                        self.history_state.report_pending = None;
                        if let Some(panel) = &mut self.history_state.panel {
                            changed |= match result {
                                Ok(report) => panel.accept_report(&request, report),
                                Err(error) => {
                                    panel.report_failed(request, error);
                                    true
                                }
                            };
                        }
                    }
                    continue;
                }
                AppEvent::HistoryPicker {
                    generation,
                    request,
                    result,
                } => {
                    if generation == self.history_state.picker_generation {
                        self.history_state.picker_task = None;
                        self.history_state.picker_pending = None;
                        if let Some(panel) = &mut self.history_state.panel {
                            changed |= panel.accept_picker(&request, result);
                        }
                    }
                    continue;
                }
                AppEvent::BrowserUsage {
                    generation,
                    request,
                    result,
                } => {
                    if generation == self.browser_state.usage_generation {
                        self.browser_state.usage_task = None;
                        self.browser_state.usage_pending = None;
                        if self
                            .browser_usage_request()
                            .as_ref()
                            .is_some_and(|current| current.same_key(&request))
                        {
                            match result {
                                Ok(usage) => {
                                    if let Some(browser) = &mut self.browser_state.panel {
                                        browser.set_usage(usage.clone(), request.label.clone());
                                    }
                                    self.browser_state.usage_cache = Some((request, usage));
                                }
                                Err(error) => {
                                    self.notice = Some(format!("会话用量查询失败：{error}"));
                                    self.browser_state.usage_cache =
                                        Some((request, HashMap::new()));
                                }
                            }
                            changed = true;
                        }
                    }
                    continue;
                }
                AppEvent::Quota(event) => event,
                AppEvent::ResetPreview { generation, result } => {
                    if self
                        .reset
                        .as_ref()
                        .is_some_and(|flow| flow.generation == generation)
                    {
                        self.reset_preview_task = None;
                        changed = true;
                        if let Ok(preview) = &result
                            && let Some(flow) = &self.reset
                            && let Some(catalog) = &mut self.overview_state.catalog
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
                    if generation == self.browser_state.generation
                        && self.browser_state.panel.is_some()
                    {
                        changed = true;
                        match result {
                            Ok(()) => {
                                if let Some(browser) = &mut self.browser_state.panel {
                                    browser.launching = false;
                                }
                                self.close_browser(true);
                                self.notice = Some("已在 Ghostty 右侧恢复会话".into());
                            }
                            Err(error) => {
                                if let Some(browser) = &mut self.browser_state.panel {
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
                    if generation == self.browser_state.generation
                        && self.browser_state.panel.is_some()
                    {
                        changed = true;
                        self.browser_state.loading = false;
                        let message = if catalog.warnings.is_empty() {
                            "未找到可恢复的本机会话".to_string()
                        } else {
                            catalog.warnings.join(" · ")
                        };
                        if catalog.sessions.is_empty()
                            && self
                                .browser_state
                                .panel
                                .as_ref()
                                .is_some_and(|b| b.sessions.is_empty())
                        {
                            if self.browser_state.filter.is_none() {
                                self.close_browser(true);
                            }
                            self.notice = Some(message);
                        } else {
                            if let Some(browser) = &mut self.browser_state.panel {
                                browser.refresh(catalog.sessions);
                            }
                            self.apply_browser_usage();
                            self.notice = if self
                                .browser_state
                                .panel
                                .as_ref()
                                .is_some_and(|b| b.sessions.is_empty())
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
                &self.history_state.config.alerts,
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
