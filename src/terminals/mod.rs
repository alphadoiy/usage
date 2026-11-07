//! Terminal transport, independent of Agent identity, credentials and session storage.
mod adaptive;
mod ghostty;
mod herdr;
mod layout;
mod rex;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use crate::config::{FocusMode, LayoutMode, ResumeMode};

/// Capability-level preference for UI descriptions, before live layout planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Current,
    Tab,
    Split,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub split: bool,
    pub tab: bool,
    /// Reads the existing panes and tiles by their size instead of using a fixed direction.
    pub adaptive: bool,
}

/// User intent for one launch, independent of the terminal that carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LaunchOptions {
    pub layout: LayoutMode,
    pub focus: FocusMode,
}

impl LaunchOptions {
    pub fn from_config(config: &crate::config::AppConfig) -> Self {
        Self {
            layout: config.layout,
            focus: config.focus,
        }
    }
}

/// `Auto` and `Split` prefer a split and `Tab` prefers a tab; whatever the terminal
/// cannot do degrades to the other, never to a different terminal.
pub fn resolve(capabilities: Capabilities, layout: LayoutMode) -> Placement {
    match (capabilities.split, capabilities.tab) {
        (false, false) => Placement::Current,
        (true, false) => Placement::Split,
        (false, true) => Placement::Tab,
        (true, true) if layout == LayoutMode::Tab => Placement::Tab,
        (true, true) => Placement::Split,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTarget {
    pub id: &'static str,
    pub label: &'static str,
    pub capabilities: Capabilities,
}

impl TerminalTarget {
    pub fn label(self) -> &'static str {
        self.label
    }
    pub fn is_current(self) -> bool {
        !self.capabilities.split && !self.capabilities.tab
    }
    pub fn placement(self, options: LaunchOptions) -> Placement {
        resolve(self.capabilities, options.layout)
    }
    /// Truthful description of what this launch will do, including degradation.
    pub fn describe(self, options: LaunchOptions) -> String {
        let placement = self.placement(options);
        let wanted_split = options.layout == LayoutMode::Split;
        let wanted_tab = options.layout == LayoutMode::Tab;
        match placement {
            Placement::Current => self.label.to_owned(),
            Placement::Split if wanted_tab => format!("{} 右侧分屏（不支持标签页）", self.label),
            Placement::Split if self.capabilities.adaptive && !wanted_split => {
                format!("{} 自动排布", self.label)
            }
            Placement::Split if self.capabilities.adaptive => {
                format!("{} 分屏", self.label)
            }
            Placement::Split => format!("{} 右侧分屏", self.label),
            Placement::Tab if wanted_split => format!("{} 新标签页（不支持分屏）", self.label),
            Placement::Tab => format!("{} 新标签页", self.label),
        }
    }
}

const CURRENT: TerminalTarget = TerminalTarget {
    id: "current",
    label: "当前终端",
    capabilities: Capabilities {
        split: false,
        tab: false,
        adaptive: false,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOS,
    Windows,
    Linux,
    Other,
}

impl Platform {
    fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Self::MacOS,
            "windows" => Self::Windows,
            "linux" => Self::Linux,
            _ => Self::Other,
        }
    }
}

/// One immutable snapshot, containing only variables declared by registered adapters.
/// Never log or Debug this value: terminal context can contain local/server paths.
pub struct TerminalContext {
    platform: Platform,
    vars: BTreeMap<String, OsString>,
    /// Columns and rows of the pane that runs Usage, when it has a terminal.
    size: Option<(u32, u32)>,
}

impl TerminalContext {
    pub fn new(platform: Platform, vars: impl IntoIterator<Item = (String, OsString)>) -> Self {
        Self {
            platform,
            vars: vars.into_iter().collect(),
            size: None,
        }
    }
    pub fn with_size(mut self, cols: u32, rows: u32) -> Self {
        self.size = Some((cols, rows));
        self
    }
    /// Unknown dimensions cannot authorize an automatic split.
    pub fn size(&self) -> (u32, u32) {
        self.size.unwrap_or((0, 0))
    }
    pub fn platform(&self) -> Platform {
        self.platform
    }
    pub fn var(&self, name: &str) -> Option<&OsStr> {
        self.vars
            .get(name)
            .filter(|v| !v.is_empty())
            .map(OsString::as_os_str)
    }
}

/// Already-exported launcher command. It must contain identifiers, never credentials.
/// Treat it as opaque: adapters must not parse it or reconstruct Agent arguments.
pub struct TerminalRequest<'a> {
    pub cwd: &'a Path,
    pub shell_command: &'a str,
    pub options: LaunchOptions,
    /// Only resume requests may reuse a live pane; new-instance launches always create one.
    pub reuse_key: Option<&'a str>,
}

/// Adapter/strategy: detection is read-only; command construction must not spawn.
/// A successful launch must give the Agent its own PTY and preserve native OSC output.
pub trait TerminalBackend: Send + Sync {
    fn target(&self) -> TerminalTarget;
    fn environment_keys(&self) -> &'static [&'static str];
    fn available(&self, context: &TerminalContext) -> bool;
    fn command(
        &self,
        context: &TerminalContext,
        request: &TerminalRequest<'_>,
    ) -> Result<TerminalCommand, String>;
}

type CommandContinuation = Box<dyn FnOnce(&[u8]) -> Result<TerminalCommand, String> + Send>;
type CommandObserver = Box<dyn FnOnce(&[u8]) + Send>;

/// Some transports create a terminal first, then address it using the response ID.
/// Both stages are prepared without side effects and execute at most once.
pub struct TerminalCommand {
    command: Command,
    next: Option<CommandContinuation>,
    after: Option<CommandObserver>,
    created: bool,
}

impl From<Command> for TerminalCommand {
    fn from(command: Command) -> Self {
        Self {
            command,
            next: None,
            after: None,
            created: false,
        }
    }
}

impl TerminalCommand {
    /// Following stages only arrange focus; a failure must not imply creation failed.
    pub fn marks_created(mut self) -> Self {
        self.created = true;
        self
    }

    /// Runs once this stage has succeeded, with its stdout. Bookkeeping only: it cannot fail
    /// the launch, because the terminal has already done what was asked.
    pub fn after(mut self, hook: impl FnOnce(&[u8]) + Send + 'static) -> Self {
        self.after = Some(Box::new(hook));
        self
    }

    /// The next stage receives the previous stage's stdout and may chain further stages.
    pub fn then<C: Into<TerminalCommand>>(
        mut self,
        next: impl FnOnce(&[u8]) -> Result<C, String> + Send + 'static,
    ) -> Self {
        self.next = Some(Box::new(move |stdout| next(stdout).map(Into::into)));
        self
    }
}

#[derive(Clone, Copy)]
pub enum Selection<'a> {
    Auto,
    Current,
    Backend(&'a str),
}

/// Registration order is automatic-detection priority. Explicit selection never falls back.
pub struct TerminalRegistry {
    backends: Vec<Box<dyn TerminalBackend>>,
}

impl TerminalRegistry {
    pub fn new(backends: Vec<Box<dyn TerminalBackend>>) -> Result<Self, String> {
        let mut ids = std::collections::HashSet::new();
        for backend in &backends {
            let target = backend.target();
            if target.id.trim().is_empty() || target.id == CURRENT.id || !ids.insert(target.id) {
                return Err("终端适配器 ID 为空、保留或重复".into());
            }
            if target.is_current() {
                return Err("外部终端适配器必须声明支持新标签页或分屏".into());
            }
        }
        Ok(Self { backends })
    }

    pub fn capture(&self) -> TerminalContext {
        let context = TerminalContext::new(
            Platform::current(),
            self.backends
                .iter()
                .flat_map(|b| b.environment_keys().iter())
                .filter_map(|key| std::env::var_os(key).map(|v| ((*key).to_string(), v))),
        );
        match ratatui::crossterm::terminal::size() {
            Ok((cols, rows)) if cols > 0 && rows > 0 => {
                context.with_size(u32::from(cols), u32::from(rows))
            }
            _ => context,
        }
    }

    fn select(
        &self,
        selection: Selection<'_>,
        context: &TerminalContext,
    ) -> Result<Option<&dyn TerminalBackend>, String> {
        match selection {
            Selection::Current => Ok(None),
            Selection::Auto => Ok(self
                .backends
                .iter()
                .find(|b| b.available(context))
                .map(Box::as_ref)),
            Selection::Backend(id) => self
                .backends
                .iter()
                .find(|b| b.target().id == id)
                .map(|b| Some(b.as_ref()))
                .ok_or_else(|| format!("未注册终端适配器：{id}")),
        }
    }

    pub fn target(
        &self,
        selection: Selection<'_>,
        context: &TerminalContext,
    ) -> Result<TerminalTarget, String> {
        Ok(self
            .select(selection, context)?
            .map_or(CURRENT, |b| b.target()))
    }

    pub fn prepare(
        &self,
        selection: Selection<'_>,
        context: &TerminalContext,
        request: &TerminalRequest<'_>,
    ) -> Result<TerminalLaunch, String> {
        let backend = self
            .select(selection, context)?
            .ok_or("当前终端不支持创建独立标签页或分屏")?;
        let target = backend.target();
        if !backend.available(context) {
            return Err(format!("当前环境无法使用{}", target.label));
        }
        let cwd = request.cwd.canonicalize().map_err(|_| "项目目录不存在")?;
        if !cwd.is_dir() {
            return Err("项目路径不是目录".into());
        }
        if request.shell_command.trim().is_empty() {
            return Err("终端启动命令为空".into());
        }
        let command = backend.command(
            context,
            &TerminalRequest {
                cwd: &cwd,
                shell_command: request.shell_command,
                reuse_key: request.reuse_key,
                options: LaunchOptions {
                    layout: match (target.capabilities.split, target.capabilities.tab) {
                        (true, false) => LayoutMode::Split,
                        (false, true) => LayoutMode::Tab,
                        _ => request.options.layout,
                    },
                    ..request.options
                },
            },
        )?;
        // Bind the adapter's routing context to the prepared plan, rather than
        // re-reading a potentially changed parent environment when it executes.
        let environment = backend
            .environment_keys()
            .iter()
            .map(|key| ((*key).to_owned(), context.var(key).map(OsStr::to_os_string)))
            .collect();
        Ok(TerminalLaunch {
            target,
            command,
            environment,
        })
    }
}

/// Command object: validation/preparation and execution are separate boundaries.
/// No Debug implementation, so command arguments cannot enter ordinary diagnostics.
pub struct TerminalLaunch {
    pub target: TerminalTarget,
    command: TerminalCommand,
    environment: Vec<(String, Option<OsString>)>,
}

impl TerminalLaunch {
    pub fn execute(self) -> Result<(), String> {
        // Serialize observation + creation + persistence across Usage processes, so
        // simultaneous resumes cannot both observe an absent pane and create duplicates.
        use fs2::FileExt;
        use sha2::{Digest, Sha256};
        let dir = crate::paths::data_dir().join("terminal-locks");
        crate::private_fs::create_dir(&dir).map_err(|e| format!("无法锁定终端启动：{e}"))?;
        let path = dir.join(format!(
            "{:x}.lock",
            Sha256::digest(self.target.id.as_bytes())
        ));
        crate::private_fs::create(&path, b"").map_err(|e| format!("无法锁定终端启动：{e}"))?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("无法锁定终端启动：{e}"))?;
        lock.lock_exclusive()
            .map_err(|e| format!("无法锁定终端启动：{e}"))?;
        tracing::debug!(event = "terminal_launch", backend = self.target.id);
        let mut stage = self.command;
        let mut created = false;
        loop {
            let output = Self::execute_command(self.target, &self.environment, &mut stage.command)
                .map_err(|error| {
                    if created {
                        format!("实例已创建，但后续焦点操作失败；请勿重复启动：{error}")
                    } else {
                        error
                    }
                })?;
            created |= stage.created;
            if let Some(hook) = stage.after.take() {
                hook(&output.stdout);
            }
            match stage.next {
                Some(next) => stage = next(&output.stdout)?,
                None => return Ok(()),
            }
        }
    }

    fn execute_command(
        target: TerminalTarget,
        environment: &[(String, Option<OsString>)],
        command: &mut Command,
    ) -> Result<std::process::Output, String> {
        for (key, value) in environment {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        let output = command
            .output()
            .map_err(|e| format!("无法启动{}：{e}", target.label))?;
        if output.status.success() {
            Ok(output)
        } else {
            // Some control CLIs report errors on stdout (Herdr prints `{ "error": … }`).
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let detail = if detail.is_empty() {
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            } else {
                detail
            };
            Err(format!("{}启动失败：{detail}", target.label))
        }
    }
}

pub fn builtin_registry() -> &'static TerminalRegistry {
    static REGISTRY: OnceLock<TerminalRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        TerminalRegistry::new(vec![
            Box::new(herdr::Herdr),
            Box::new(rex::Rex),
            Box::new(ghostty::Ghostty),
        ])
        .expect("unique terminal adapters")
    })
}

/// Opaque launcher command for adapters that cannot set the directory or keep a failed
/// pane open themselves: `sh -c WRAPPER usage-agent CWD COMMAND`. Only a launcher failure
/// pauses the pane, so its error is not lost; normal exit and Ctrl+C close it.
pub const AGENT_WRAPPER: &str = r#"cd -- "$1" || exit 1; sh -lc "$2"; status=$?; case $status in 0|130|143) ;; *) printf '\n[usage] 启动失败（退出码 %s），按回车关闭\n' "$status"; read -r _ ;; esac; exit "$status""#;

fn selection(mode: ResumeMode) -> Selection<'static> {
    match mode.backend_id() {
        Some(id) => Selection::Backend(id),
        None if mode == ResumeMode::Current => Selection::Current,
        None => Selection::Auto,
    }
}

pub fn terminal_target(mode: ResumeMode) -> TerminalTarget {
    let registry = builtin_registry();
    registry
        .target(selection(mode), &registry.capture())
        .expect("configured terminal is registered")
}

pub fn command_in_terminal(
    cwd: &Path,
    command: &str,
    mode: ResumeMode,
    options: LaunchOptions,
) -> Result<(), String> {
    let registry = builtin_registry();
    registry
        .prepare(
            selection(mode),
            &registry.capture(),
            &TerminalRequest {
                cwd,
                shell_command: command,
                reuse_key: None,
                options,
            },
        )?
        .execute()
}

#[cfg(test)]
mod tests;

/// Resume requests carry an opaque identity including the chosen launcher/target.
/// Selecting another account therefore never focuses a pane using different credentials.
pub fn resume_in_terminal(
    cwd: &Path,
    command: &str,
    mode: ResumeMode,
    options: LaunchOptions,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let identity = format!(
        "{}\0{}",
        cwd.canonicalize().map_err(|e| e.to_string())?.display(),
        command
    );
    let key = format!("{:x}", Sha256::digest(identity.as_bytes()));
    let registry = builtin_registry();
    registry
        .prepare(
            selection(mode),
            &registry.capture(),
            &TerminalRequest {
                cwd,
                shell_command: command,
                options,
                reuse_key: Some(&key),
            },
        )?
        .execute()
}
