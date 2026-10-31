//! Terminal transport, independent of Agent identity, credentials and session storage.
mod ghostty;
mod rex;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use crate::config::ResumeMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Current,
    Tab,
    RightSplit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTarget {
    pub id: &'static str,
    pub label: &'static str,
    pub placement: Placement,
}

impl TerminalTarget {
    pub fn label(self) -> &'static str {
        self.label
    }
    pub fn is_current(self) -> bool {
        self.placement == Placement::Current
    }
}

const CURRENT: TerminalTarget = TerminalTarget {
    id: "current",
    label: "当前终端",
    placement: Placement::Current,
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
}

impl TerminalContext {
    pub fn new(platform: Platform, vars: impl IntoIterator<Item = (String, OsString)>) -> Self {
        Self {
            platform,
            vars: vars.into_iter().collect(),
        }
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
    ) -> Result<Command, String>;
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
                return Err("外部终端适配器必须声明新标签页或分屏".into());
            }
        }
        Ok(Self { backends })
    }

    pub fn capture(&self) -> TerminalContext {
        TerminalContext::new(
            Platform::current(),
            self.backends
                .iter()
                .flat_map(|b| b.environment_keys().iter())
                .filter_map(|key| std::env::var_os(key).map(|v| ((*key).to_string(), v))),
        )
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
        let mut command = backend.command(
            context,
            &TerminalRequest {
                cwd: &cwd,
                shell_command: request.shell_command,
            },
        )?;
        // Bind the adapter's routing context to the prepared plan, rather than
        // re-reading a potentially changed parent environment when it executes.
        for key in backend.environment_keys() {
            match context.var(key) {
                Some(value) => {
                    command.env(key, value);
                }
                None => {
                    command.env_remove(key);
                }
            }
        }
        Ok(TerminalLaunch { target, command })
    }
}

/// Command object: validation/preparation and execution are separate boundaries.
/// No Debug implementation, so command arguments cannot enter ordinary diagnostics.
pub struct TerminalLaunch {
    pub target: TerminalTarget,
    command: Command,
}

impl TerminalLaunch {
    pub fn execute(mut self) -> Result<(), String> {
        tracing::debug!(event = "terminal_launch", backend = self.target.id, placement = ?self.target.placement);
        let output = self
            .command
            .output()
            .map_err(|e| format!("无法启动{}：{e}", self.target.label))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{}启动失败：{}",
                self.target.label,
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

pub fn builtin_registry() -> &'static TerminalRegistry {
    static REGISTRY: OnceLock<TerminalRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        TerminalRegistry::new(vec![Box::new(rex::Rex), Box::new(ghostty::Ghostty)])
            .expect("unique terminal adapters")
    })
}

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

pub fn command_in_terminal(cwd: &Path, command: &str, mode: ResumeMode) -> Result<(), String> {
    let registry = builtin_registry();
    registry
        .prepare(
            selection(mode),
            &registry.capture(),
            &TerminalRequest {
                cwd,
                shell_command: command,
            },
        )?
        .execute()
}

#[cfg(test)]
mod tests;
