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

/// Managed account storage is resolved only during spawn, after source synchronization.
#[derive(Debug, Clone)]
pub enum AgentContext {
    Local {
        home: PathBuf,
    },
    ManagedCodex {
        root: PathBuf,
        account: String,
        revision: Option<i64>,
    },
}

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
    pub run: Option<crate::codex_manager::RunRecord>,
}

pub trait Launcher: Send + Sync {
    fn spawn(&self, request: &SpawnRequest) -> Result<SpawnOutcome, String>;

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
}

pub trait AgentAdapter: Send + Sync {
    fn kind(&self) -> AgentKind;
    fn launcher(&self) -> Option<&dyn Launcher> {
        None
    }
    fn session_source(&self) -> Option<SessionSource> {
        None
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
}

impl AgentAdapter for LocalAgent {
    fn kind(&self) -> AgentKind {
        self.kind
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
    fn supports_start(&self) -> bool {
        true
    }
    fn resume_shell_command(&self, session: &crate::sessions::Session) -> Result<String, String> {
        match &self.context {
            AgentContext::ManagedCodex {
                account, revision, ..
            } => crate::codex_manager::launch_shell_command(
                account,
                &session.cwd,
                *revision,
                Some(&session.id),
            )
            .map_err(|error| error.to_string()),
            AgentContext::Local { .. } => crate::sessions::local_resume_shell_command(session),
        }
    }
    fn spawn(&self, request: &SpawnRequest) -> Result<SpawnOutcome, String> {
        self.start(request)?.wait()
    }
    fn start(&self, request: &SpawnRequest) -> Result<Box<dyn RunningAgent>, String> {
        match &self.context {
            AgentContext::ManagedCodex {
                root,
                account,
                revision,
            } => crate::codex_manager::Manager::open(root.clone())
                .and_then(|mut manager| {
                    manager.start_recorded_with_config(
                        account,
                        &request.cwd,
                        *revision,
                        request.resume.as_deref(),
                        &request.program,
                        &request.args,
                        request.usage_config.as_deref(),
                    )
                })
                .map(|handle| Box::new(handle) as Box<dyn RunningAgent>)
                .map_err(|error| error.to_string()),
            AgentContext::Local { home } => {
                if self.kind == AgentKind::CODEX
                    && let Some((account, revision)) = crate::codex_manager::binding_for_home(home)
                        .map_err(|error| error.to_string())?
                {
                    return crate::hub::start_managed_codex(
                        &crate::codex_manager::default_root(),
                        &account,
                        Some(revision),
                        request,
                    );
                }
                if self.kind == AgentKind::CODEX {
                    return crate::codex_manager::start_local_codex(home, request)
                        .map(|handle| Box::new(handle) as Box<dyn RunningAgent>)
                        .map_err(|error| error.to_string());
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

pub fn builtin_registry() -> AgentRegistry {
    let mut registry = AgentRegistry::new();
    type Loader = fn(&Path) -> Result<Vec<SessionRecord>, String>;
    for (kind, load) in [
        (AgentKind::CLAUDE, crate::sessions::load_claude as Loader),
        (AgentKind::CODEX, crate::sessions::load_codex as Loader),
        (AgentKind::GROK, crate::sessions::load_grok as Loader),
    ] {
        registry
            .register(kind.id(), move |context| {
                if matches!(context, AgentContext::ManagedCodex { .. }) && kind != AgentKind::CODEX
                {
                    return Err(RegistryError::Factory(
                        "此 Agent 不支持 Codex 托管账号".into(),
                    ));
                }
                Ok(Arc::new(LocalAgent {
                    kind,
                    context,
                    load,
                }) as Arc<dyn AgentAdapter>)
            })
            .expect("内置 Agent ID 唯一");
    }
    registry
}
