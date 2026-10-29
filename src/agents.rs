//! Agent adapters own launching and local session formats, independently of billing services.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;

use crate::registry::{FactoryRegistry, RegistryError};
use crate::sessions::{SessionRecord, SessionSource};
mod process;
pub use process::{ProcessHandle, RunningAgent};

/// Metadata needed to group, color, and resume one session provider.
#[derive(Debug, Clone, Copy)]
pub struct AgentKind {
    id: &'static str,
    name: &'static str,
    accent: Option<crate::theme::Rgb>,
    command: &'static str,
    home_env: &'static str,
    order: u16,
    valid_session_id: fn(&str) -> bool,
}

impl AgentKind {
    pub const CLAUDE: Self = Self::new(
        "claude",
        "Claude Code",
        None,
        "claude --resume",
        "CLAUDE_CONFIG_DIR",
        0,
    );
    pub const CODEX: Self = Self::new("codex", "Codex", None, "codex resume", "CODEX_HOME", 1);
    pub const GROK: Self = Self::new("grok", "Grok", None, "grok --resume", "GROK_HOME", 2);

    /// `accent = None` uses the shared theme; `order` controls group position.
    pub const fn new(
        id: &'static str,
        name: &'static str,
        accent: Option<crate::theme::Rgb>,
        command: &'static str,
        home_env: &'static str,
        order: u16,
    ) -> Self {
        Self {
            id,
            name,
            accent,
            command,
            home_env,
            order,
            valid_session_id: |id| uuid::Uuid::parse_str(id).is_ok(),
        }
    }

    /// Agents with non-UUID IDs supply their own validation without changing the catalog.
    pub const fn with_session_id_validator(mut self, validator: fn(&str) -> bool) -> Self {
        self.valid_session_id = validator;
        self
    }
    pub fn valid_session_id(self, id: &str) -> bool {
        (self.valid_session_id)(id)
    }

    pub fn id(self) -> &'static str {
        self.id
    }
    pub fn name(self) -> &'static str {
        self.name
    }
    pub fn accent(self) -> crate::theme::Rgb {
        self.accent.unwrap_or_else(|| crate::theme::accent(self.id))
    }
    pub fn order(self) -> u16 {
        self.order
    }
    pub(crate) fn command(self) -> &'static str {
        self.command
    }
    pub fn resume_command(self, id: &str) -> String {
        format!("{} {id}", self.command)
    }
    pub(crate) fn home_env(self) -> &'static str {
        self.home_env
    }
}

impl PartialEq for AgentKind {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for AgentKind {}

impl Hash for AgentKind {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// Where an Agent instance lives. A managed instance belongs to an account store, which is
/// reached only through `ManagedInstance`; this layer never names a concrete Agent or store.
#[derive(Debug, Clone)]
pub enum AgentContext {
    Local { home: PathBuf },
    Managed(Arc<dyn ManagedInstance>),
}

/// An instance an account store manages. The store resolves its account and revision when
/// used (after any source synchronization), not when the instance is described.
pub trait ManagedInstance: Send + Sync + std::fmt::Debug {
    /// The Agent type this store's instances run.
    fn agent(&self) -> &str;
    fn home(&self) -> Result<PathBuf, String>;
    fn performance_binding(
        &self,
        provider: &str,
        source: crate::performance::RequestSource,
    ) -> Result<crate::performance::Binding, String>;
    fn start(&self, request: &SpawnRequest) -> Result<Box<dyn RunningAgent>, String>;
    /// Read-only eligibility check, repeated before executing or exporting a resume.
    /// Custom stores retain their existing behavior unless they declare a restriction.
    fn validate_resume_source(&self, _source: &crate::sessions::Session) -> Result<(), String> {
        Ok(())
    }
    fn resume_shell_command(&self, session: &crate::sessions::Session) -> Result<String, String>;
    fn resume_shell_command_with_config(
        &self,
        session: &crate::sessions::Session,
        _usage_config: Option<&Path>,
    ) -> Result<String, String> {
        self.resume_shell_command(session)
    }
}

/// Agent-specific handling of a local Home, such as recognizing a Home an account store
/// manages. Without hooks a local Agent runs its command with the Home in its environment.
#[derive(Clone, Copy, Default)]
pub struct LocalHooks {
    pub start: Option<LocalStart>,
    pub resume: Option<LocalResume>,
}
pub type LocalStart = fn(&Path, &SpawnRequest) -> Result<Box<dyn RunningAgent>, String>;
pub type LocalResume = fn(&crate::sessions::Session, Option<&Path>) -> Result<String, String>;

#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub cwd: PathBuf,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub resume: Option<String>,
    pub usage_config: Option<PathBuf>,
}

#[derive(Clone)]
pub struct SpawnOutcome {
    pub status: ExitStatus,
    /// Local, unmanaged launches do not claim a persisted managed run binding.
    pub run: Option<crate::runs::RunRecord>,
}

pub trait Launcher: Send + Sync {
    fn spawn(&self, request: &SpawnRequest) -> Result<SpawnOutcome, String>;

    /// Whether this instance can access the selected source conversation. This must
    /// not launch, migrate, materialize a Home, or probe a native agent process.
    fn validate_resume_source(&self, _source: &crate::sessions::Session) -> Result<(), String> {
        Ok(())
    }

    /// Additive capability: legacy/custom launchers may remain foreground-only.
    fn start(&self, _request: &SpawnRequest) -> Result<Box<dyn RunningAgent>, String> {
        Err("此 Agent 不支持非阻塞启动".into())
    }
    fn supports_start(&self) -> bool {
        false
    }

    /// Exporting a launch into another terminal is a separate capability.
    /// Adapters must explicitly preserve their own credential/configuration binding.
    fn resume_shell_command(&self, _session: &crate::sessions::Session) -> Result<String, String> {
        Err("此 Agent 不支持导出恢复命令，请使用当前终端".into())
    }
    fn resume_shell_command_with_config(
        &self,
        session: &crate::sessions::Session,
        _usage_config: Option<&Path>,
    ) -> Result<String, String> {
        self.resume_shell_command(session)
    }
}

pub trait AgentAdapter: Send + Sync {
    fn kind(&self) -> AgentKind;
    fn launcher(&self) -> Option<&dyn Launcher> {
        None
    }
    fn session_source(&self) -> Option<SessionSource> {
        None
    }
    fn load_sessions(
        &self,
        home: &Path,
        cancel: &crate::background::Cancellation,
    ) -> Result<Vec<SessionRecord>, String> {
        if cancel.is_cancelled() {
            return Ok(Vec::new());
        }
        self.session_source()
            .map_or_else(|| Ok(Vec::new()), |source| (source.load)(home))
    }
    fn request_source(&self) -> Option<crate::performance::RequestSource> {
        None
    }
}

impl std::fmt::Debug for dyn AgentAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentAdapter")
            .field("kind", &self.kind().id())
            .finish()
    }
}

pub type AgentRegistry = FactoryRegistry<AgentContext, Arc<dyn AgentAdapter>>;

struct LocalAgent {
    kind: AgentKind,
    context: AgentContext,
    load: fn(&Path) -> Result<Vec<SessionRecord>, String>,
    hooks: LocalHooks,
    controlled: Option<ControlledSessionLoader>,
}

impl AgentAdapter for LocalAgent {
    fn kind(&self) -> AgentKind {
        self.kind
    }
    fn load_sessions(
        &self,
        home: &Path,
        cancel: &crate::background::Cancellation,
    ) -> Result<Vec<SessionRecord>, String> {
        if cancel.is_cancelled() {
            return Ok(Vec::new());
        }
        match self.controlled {
            Some(load) => load(home, cancel),
            None => (self.load)(home),
        }
    }
    fn request_source(&self) -> Option<crate::performance::RequestSource> {
        crate::performance::source(self.kind.id())
    }
    fn launcher(&self) -> Option<&dyn Launcher> {
        Some(self)
    }
    fn session_source(&self) -> Option<SessionSource> {
        Some(SessionSource {
            kind: self.kind,
            load: self.load,
        })
    }
}

impl Launcher for LocalAgent {
    fn validate_resume_source(&self, source: &crate::sessions::Session) -> Result<(), String> {
        match &self.context {
            AgentContext::Managed(instance) => instance.validate_resume_source(source),
            AgentContext::Local { home } => {
                // A local shell hook receives the source Home. Accepting another
                // Home here would make foreground and exported resumes diverge.
                if home
                    .canonicalize()
                    .ok()
                    .zip(source.home.canonicalize().ok())
                    .is_some_and(|(target, source)| target == source)
                {
                    Ok(())
                } else {
                    Err("目标实例与会话来源目录不同，不能使用此实例恢复".into())
                }
            }
        }
    }
    fn supports_start(&self) -> bool {
        true
    }
    fn resume_shell_command(&self, session: &crate::sessions::Session) -> Result<String, String> {
        self.resume_shell_command_with_config(session, None)
    }
    fn resume_shell_command_with_config(
        &self,
        session: &crate::sessions::Session,
        usage_config: Option<&Path>,
    ) -> Result<String, String> {
        match (&self.context, self.hooks.resume) {
            (AgentContext::Managed(instance), _) => {
                instance.resume_shell_command_with_config(session, usage_config)
            }
            (AgentContext::Local { .. }, Some(resume)) => resume(session, usage_config),
            (AgentContext::Local { .. }, None) => {
                crate::sessions::local_resume_shell_command(session)
            }
        }
    }
    fn spawn(&self, request: &SpawnRequest) -> Result<SpawnOutcome, String> {
        self.start(request)?.wait()
    }
    fn start(&self, request: &SpawnRequest) -> Result<Box<dyn RunningAgent>, String> {
        match &self.context {
            AgentContext::Managed(instance) => instance.start(request),
            AgentContext::Local { home } => {
                if let Some(start) = self.hooks.start {
                    return start(home, request);
                }
                let mut command = Command::new(&request.program);
                if let Some(id) = &request.resume {
                    if !self.kind.valid_session_id(id) {
                        return Err("无效的会话 ID".into());
                    }
                    command
                        .args(self.kind.command().split_whitespace().skip(1))
                        .arg(id);
                }
                let child = command
                    .args(&request.args)
                    .current_dir(&request.cwd)
                    .env(self.kind.home_env(), home)
                    .spawn()
                    .map_err(|error| format!("无法运行 {}：{error}", self.kind.name()))?;
                Ok(Box::new(ProcessHandle::new(child, None, None)))
            }
        }
    }
}

/// Factory for an Agent whose instances keep sessions in a local Home. A managed context is
/// accepted only from a store that manages this Agent type.
pub fn local(
    kind: AgentKind,
    load: fn(&Path) -> Result<Vec<SessionRecord>, String>,
    hooks: LocalHooks,
) -> impl Fn(AgentContext) -> Result<Arc<dyn AgentAdapter>, RegistryError> + Send + Sync + 'static {
    local_controlled(kind, load, hooks, None)
}
pub type ControlledSessionLoader =
    fn(&Path, &crate::background::Cancellation) -> Result<Vec<SessionRecord>, String>;
pub(crate) fn local_controlled(
    kind: AgentKind,
    load: fn(&Path) -> Result<Vec<SessionRecord>, String>,
    hooks: LocalHooks,
    controlled: Option<ControlledSessionLoader>,
) -> impl Fn(AgentContext) -> Result<Arc<dyn AgentAdapter>, RegistryError> + Send + Sync + 'static {
    move |context| {
        if let AgentContext::Managed(instance) = &context
            && instance.agent() != kind.id()
        {
            return Err(RegistryError::Factory(format!(
                "此账号库管理的是 {} 实例，不能用于 {}",
                instance.agent(),
                kind.name()
            )));
        }
        Ok(Arc::new(LocalAgent {
            kind,
            context,
            load,
            hooks,
            controlled,
        }) as Arc<dyn AgentAdapter>)
    }
}
