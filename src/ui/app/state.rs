//! Feature-owned state for Dashboard's independent background workflows.
use super::*;

pub(super) struct FingerprintState {
    pub service: Arc<crate::codex_manager::fingerprint::FingerprintService>,
    pub task: Option<Task>,
    pub generation: u64,
    pub last_poll: Option<std::time::Instant>,
}

impl FingerprintState {
    pub fn new() -> Self {
        Self {
            service: Arc::new(
                crate::codex_manager::fingerprint::FingerprintService::from_environment(),
            ),
            task: None,
            generation: 0,
            last_poll: None,
        }
    }
}

pub(super) struct HomePerformanceState {
    pub cached: HashMap<String, crate::ui::performance::CachedSummary>,
    pub task: Option<Task>,
    pub pending: bool,
    pub generation: u64,
}

impl HomePerformanceState {
    pub fn new() -> Self {
        Self {
            cached: HashMap::new(),
            task: None,
            pending: false,
            generation: 0,
        }
    }
}

pub(super) struct PerformanceState {
    pub panel: Option<crate::ui::performance::Panel>,
    pub service: Arc<crate::performance::Service>,
    pub generation: u64,
}

impl PerformanceState {
    pub fn new() -> Self {
        Self {
            panel: None,
            service: Arc::new(crate::performance::Service::from_environment()),
            generation: 0,
        }
    }
}

pub(super) struct HomeActivityState {
    pub sessions: HashMap<sessions::SessionKey, crate::runs::SessionRuntime>,
    pub error: Option<String>,
    pub task: Option<Task>,
    pub pending: bool,
    pub dirty: bool,
    pub generation: u64,
    pub last_poll: std::time::Instant,
    pub root: PathBuf,
}

impl HomeActivityState {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            error: None,
            task: None,
            pending: false,
            dirty: true,
            generation: 0,
            last_poll: std::time::Instant::now(),
            root: crate::codex_manager::default_root(),
        }
    }

    pub fn invalidate(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.generation = self.generation.wrapping_add(1);
        self.pending = false;
        self.dirty = true;
    }
}

pub(super) struct HistoryState {
    pub panel: Option<HistoryPanel>,
    pub saved: Option<HistoryPanel>,
    pub loaded: Arc<crate::history::History>,
    pub task: Option<Task>,
    pub last_refresh: std::time::Instant,
    pub config: AppConfig,
    pub cache: Option<crate::history::HistoryCache>,
    pub loading: bool,
}

impl HistoryState {
    pub fn new(config: AppConfig) -> Self {
        Self {
            panel: None,
            saved: None,
            loaded: Arc::default(),
            task: None,
            last_refresh: std::time::Instant::now(),
            config,
            cache: Some(crate::history::HistoryCache::default()),
            loading: false,
        }
    }

    pub fn close_panel(&mut self) {
        if let Some(panel) = self.panel.take() {
            self.saved = Some(panel);
        }
    }

    pub fn begin_load(&mut self) -> Option<crate::history::HistoryCache> {
        if self.loading {
            return None;
        }
        self.loading = true;
        self.last_refresh = std::time::Instant::now();
        if let Some(panel) = &mut self.panel {
            panel.loading = true;
        }
        Some(self.cache.take().unwrap_or_default())
    }

    pub fn show_snapshot(&mut self, history: Arc<crate::history::History>) {
        self.loaded = history.clone();
        if let Some(panel) = self.panel.as_mut().or(self.saved.as_mut()) {
            panel.history = history;
        }
    }

    pub fn finish_load(&mut self) {
        self.loading = false;
        self.task = None;
        if let Some(panel) = self.panel.as_mut().or(self.saved.as_mut()) {
            panel.loading = false;
        }
    }
}

pub(super) struct BrowserState {
    pub panel: Option<SessionBrowser>,
    pub saved: Option<SessionBrowser>,
    pub task: Option<Task>,
    pub filter: Option<(Vec<String>, crate::history::HistoryQuery)>,
    pub return_to_history: bool,
    pub resume_request: Option<sessions::Session>,
    pub homes: sessions::SessionHomes,
    pub registry: sessions::SessionRegistry,
    pub generation: u64,
    pub loading: bool,
}

impl BrowserState {
    pub fn new(homes: sessions::SessionHomes, registry: sessions::SessionRegistry) -> Self {
        Self {
            panel: None,
            saved: None,
            task: None,
            filter: None,
            return_to_history: false,
            resume_request: None,
            homes,
            registry,
            generation: 0,
            loading: false,
        }
    }

    pub fn close(&mut self) -> bool {
        if let Some(browser) = self.panel.take() {
            self.saved = Some(browser);
        }
        self.generation = self.generation.wrapping_add(1);
        self.loading = false;
        self.task = None;
        std::mem::take(&mut self.return_to_history)
    }

    pub fn begin_load(&mut self) -> Option<u64> {
        if self.loading {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        self.loading = true;
        Some(self.generation)
    }
}

pub(super) struct HomeSessionsState {
    pub browser: SessionBrowser,
    pub loading: bool,
    pub generation: u64,
    pub warning: Option<String>,
    pub task: Option<Task>,
    pub project_store: Arc<crate::projects::Store>,
}

impl HomeSessionsState {
    pub fn new(resume: crate::config::ResumeMode) -> Self {
        let mut browser = SessionBrowser::new(Vec::new());
        browser.resume_mode = resume;
        Self {
            browser,
            loading: false,
            generation: 0,
            warning: None,
            task: None,
            project_store: Arc::new(crate::projects::Store::from_environment()),
        }
    }
}

pub(super) struct OverviewState {
    pub catalog: Option<crate::provider_catalog::Catalog>,
    pub error: Option<String>,
    pub loading: bool,
    pub pending: usize,
    pub generation: u64,
    pub service: Arc<crate::provider_catalog::CatalogService>,
    pub task: Option<Task>,
}

impl OverviewState {
    pub fn new() -> Self {
        Self {
            catalog: None,
            error: None,
            loading: false,
            pending: 0,
            generation: 0,
            service: Arc::new(crate::provider_catalog::CatalogService::from_environment()),
            task: None,
        }
    }

    pub fn complete_one(&mut self) {
        self.pending = self.pending.saturating_sub(1);
        self.loading = self.pending > 0;
    }
}

pub(super) struct ServiceState {
    pub statuses: HashMap<String, ServiceStatus>,
    pub generation: u64,
    pub pending: usize,
    pub http: Arc<dyn crate::http::HttpClient>,
    pub history: crate::ui::home::StatusHistory,
}

impl ServiceState {
    pub fn new() -> Self {
        Self {
            statuses: HashMap::new(),
            generation: 0,
            pending: 0,
            http: Arc::new(crate::http::ReqwestClient::new()),
            history: crate::ui::home::StatusHistory::default(),
        }
    }

    pub fn accept(&mut self, snapshot: ServiceStatus) {
        self.pending = self.pending.saturating_sub(1);
        let id = snapshot.provider.clone();
        self.history.record(&snapshot);
        let previous = self.statuses.remove(&id);
        self.statuses
            .insert(id, ServiceStatus::merge(previous, snapshot));
    }
}
