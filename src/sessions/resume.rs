//! One resume decision shared by terminal execution and command export.
use super::{Session, copy_command, local_resume_shell_command, validate_resume};
use crate::agents::{AgentAdapter, AgentContext, SpawnRequest};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;

/// A destination never replaces the source conversation's identity or adapter.
#[derive(Debug, Clone, Default)]
pub enum ResumeTarget {
    #[default]
    Original,
    Instance(Arc<dyn AgentAdapter>),
}

#[derive(Debug, Clone)]
pub struct ResumeIntent {
    pub session: Session,
    pub target: ResumeTarget,
    pub usage_config: Option<PathBuf>,
}

/// Compatibility request for callers which resume through the original instance.
/// Keep its public fields and delegate every operation to the new use case.
#[derive(Debug, Clone)]
pub struct ResumeRequest {
    pub session: Session,
    pub usage_config: Option<PathBuf>,
}

impl ResumeRequest {
    pub fn new(session: Session, usage_config: Option<PathBuf>) -> Self {
        Self {
            session,
            usage_config,
        }
    }
    pub fn prepare(&self) -> Result<ResumePlan, String> {
        ResumeIntent::new(self.session.clone(), self.usage_config.clone()).prepare()
    }
    pub fn run_in_current(&self) -> Result<ExitStatus, String> {
        self.prepare()?.run_in_current()
    }
    pub fn shell_command(&self) -> Result<String, String> {
        self.prepare()?.shell_command()
    }
    pub fn copy_command(&self) -> Result<(), String> {
        self.prepare()?.copy_command()
    }
    pub fn run_in_ghostty(&self) -> Result<(), String> {
        self.prepare()?.run_in_ghostty()
    }
}

impl ResumeIntent {
    pub fn new(session: Session, usage_config: Option<PathBuf>) -> Self {
        Self {
            session,
            target: ResumeTarget::Original,
            usage_config,
        }
    }

    pub fn with_target(mut self, target: ResumeTarget) -> Self {
        self.target = target;
        self
    }

    /// Read-only preparation. Resolves the adapter once; native configuration
    /// probing and managed-Home creation remain at the actual launch boundary.
    pub fn prepare(&self) -> Result<ResumePlan, String> {
        let agent = match &self.target {
            ResumeTarget::Instance(agent) => Some(agent.clone()),
            ResumeTarget::Original => match &self.session.agent {
                Some(agent) => Some(agent.clone()),
                None => legacy_agent(&self.session)?,
            },
        };
        let plan = ResumePlan {
            session: self.session.clone(),
            agent,
            usage_config: self.usage_config.clone(),
        };
        plan.validate()?;
        Ok(plan)
    }

    pub fn run_in_current(&self) -> Result<ExitStatus, String> {
        self.prepare()?.run_in_current()
    }
    pub fn shell_command(&self) -> Result<String, String> {
        self.prepare()?.shell_command()
    }
    pub fn copy_command(&self) -> Result<(), String> {
        self.prepare()?.copy_command()
    }
    pub fn run_in_ghostty(&self) -> Result<(), String> {
        self.prepare()?.run_in_ghostty()
    }
}

/// Immutable source and destination binding. Availability is rechecked whenever
/// the plan executes or exports, because the underlying store can change.
#[derive(Debug, Clone)]
pub struct ResumePlan {
    session: Session,
    agent: Option<Arc<dyn AgentAdapter>>,
    usage_config: Option<PathBuf>,
}

impl ResumePlan {
    pub fn session(&self) -> &Session {
        &self.session
    }
    pub fn cwd(&self) -> &Path {
        &self.session.cwd
    }
    pub fn usage_config(&self) -> Option<&Path> {
        self.usage_config.as_deref()
    }

    fn validate(&self) -> Result<(), String> {
        validate_resume(&self.session)?;
        if let Some(agent) = &self.agent {
            if agent.kind() != self.session.provider {
                return Err("恢复目标与会话的 Agent 类型不匹配".into());
            }
            agent
                .launcher()
                .ok_or("此 Agent 不支持恢复会话")?
                .validate_resume_source(&self.session)?;
        }
        Ok(())
    }

    pub fn run_in_current(&self) -> Result<ExitStatus, String> {
        self.validate()?;
        let mut parts = self.session.provider.command().split_whitespace();
        let program = parts.next().ok_or("恢复命令为空")?;
        if let Some(agent) = &self.agent {
            return agent
                .launcher()
                .ok_or("此 Agent 不支持恢复会话")?
                .spawn(&SpawnRequest {
                    cwd: self.session.cwd.clone(),
                    program: program.into(),
                    args: Vec::new(),
                    resume: Some(self.session.id.clone()),
                    usage_config: self.usage_config.clone(),
                })
                .map(|outcome| outcome.status);
        }
        Command::new(program)
            .args(parts)
            .arg(&self.session.id)
            .current_dir(&self.session.cwd)
            .env(self.session.provider.home_env(), &self.session.home)
            .status()
            .map_err(|e| format!("无法运行 {}：{e}", self.session.provider.name()))
    }

    pub fn shell_command(&self) -> Result<String, String> {
        self.validate()?;
        match &self.agent {
            Some(agent) => agent
                .launcher()
                .ok_or("此 Agent 不支持恢复会话")?
                .resume_shell_command_with_config(&self.session, self.usage_config.as_deref()),
            None => local_resume_shell_command(&self.session),
        }
    }
    pub fn copy_command(&self) -> Result<(), String> {
        copy_command(&self.shell_command()?)
    }
    pub fn run_in_ghostty(&self) -> Result<(), String> {
        super::command_in_ghostty(self.cwd(), &self.shell_command()?)
    }
}

/// UI-facing choice. Its availability is informative; prepare/execution enforce it again.
#[derive(Debug, Clone)]
pub struct ResumeChoice {
    pub id: String,
    pub label: String,
    pub detail: String,
    pub target: ResumeTarget,
    pub unavailable: Option<String>,
}

impl ResumeChoice {
    pub fn new(
        id: String,
        label: String,
        detail: String,
        target: ResumeTarget,
        source: &Session,
    ) -> Self {
        let unavailable = ResumeIntent::new(source.clone(), None)
            .with_target(target.clone())
            .prepare()
            .err();
        Self {
            id,
            label,
            detail,
            target,
            unavailable,
        }
    }
}

fn legacy_agent(session: &Session) -> Result<Option<Arc<dyn AgentAdapter>>, String> {
    let registry = &crate::hub::shared().agents;
    if !registry.contains(session.provider.id()) {
        return Ok(None);
    }
    registry
        .create(
            session.provider.id(),
            AgentContext::Local {
                home: session.home.clone(),
            },
        )
        .map(Some)
        .map_err(|error| error.to_string())
}

#[cfg(all(test, unix))]
mod tests;
