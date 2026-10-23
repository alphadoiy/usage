//! Codex accounts as Agent instances. The generic Agent and Hub layers reach this store only
//! through `ManagedInstance` and the local hooks below.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agents::{
    AgentContext, LocalHooks, ManagedInstance, RunningAgent, SpawnOutcome, SpawnRequest,
};

use super::Manager;

/// One account, optionally pinned to a revision; resolved when the instance is used.
#[derive(Debug, Clone)]
pub struct ManagedAccount {
    pub root: PathBuf,
    pub account: String,
    pub revision: Option<i64>,
}

impl ManagedAccount {
    pub fn context(self) -> AgentContext {
        AgentContext::Managed(Arc::new(self))
    }
}

impl ManagedInstance for ManagedAccount {
    fn agent(&self) -> &str {
        "codex"
    }
    fn home(&self) -> Result<PathBuf, String> {
        let manager = Manager::open_read_only(self.root.clone())
            .map_err(|error| error.to_string())?
            .ok_or("账号库不存在")?;
        let profile = manager
            .resolve(&self.account)
            .map_err(|error| error.to_string())?;
        manager
            .home(&profile.id, self.revision.unwrap_or(profile.revision))
            .map_err(|error| error.to_string())
    }
    fn performance_binding(
        &self,
        provider: &str,
        source: crate::performance::RequestSource,
    ) -> Result<crate::performance::Binding, String> {
        let manager = Manager::open_read_only(self.root.clone())
            .map_err(|_| "无法读取性能账号绑定")?
            .ok_or("账号库不存在")?;
        let profile = manager.resolve(&self.account).map_err(|_| "账号不存在")?;
        let mut binding = manager
            .performance_binding(&profile, self.revision.unwrap_or(profile.revision))
            .map_err(|_| "无法读取性能账号绑定")?;
        binding.provider = provider.into();
        binding.source = source;
        Ok(binding)
    }
    fn start(&self, request: &SpawnRequest) -> Result<Box<dyn RunningAgent>, String> {
        Manager::open(self.root.clone())
            .and_then(|mut manager| {
                manager.start_recorded_with_config(
                    &self.account,
                    &request.cwd,
                    self.revision,
                    request.resume.as_deref(),
                    &request.program,
                    &request.args,
                    request.usage_config.as_deref(),
                )
            })
            .map(|handle| Box::new(handle) as Box<dyn RunningAgent>)
            .map_err(|error| error.to_string())
    }
    fn resume_shell_command(&self, session: &crate::sessions::Session) -> Result<String, String> {
        super::launch_shell_command_in(
            &self.root,
            &self.account,
            &session.cwd,
            self.revision,
            Some(&session.id),
        )
        .map_err(|error| error.to_string())
    }
}

/// A local Codex Home this store materialized resumes its own account and revision; any other
/// Home launches natively with notifications injected.
pub const LOCAL_HOOKS: LocalHooks = LocalHooks {
    start: Some(start_local),
    resume: Some(resume_local),
};

fn start_local(home: &Path, request: &SpawnRequest) -> Result<Box<dyn RunningAgent>, String> {
    if let Some((account, revision)) =
        super::binding_for_home(home).map_err(|error| error.to_string())?
    {
        return start(&super::default_root(), &account, Some(revision), request);
    }
    super::start_local_codex(home, request)
        .map(|handle| Box::new(handle) as Box<dyn RunningAgent>)
        .map_err(|error| error.to_string())
}

fn resume_local(session: &crate::sessions::Session) -> Result<String, String> {
    crate::sessions::validate_resume(session)?;
    if let Some((account, revision)) =
        super::binding_for_home(&session.home).map_err(|error| error.to_string())?
    {
        return super::launch_shell_command(
            &account,
            &session.cwd,
            Some(revision),
            Some(&session.id),
        )
        .map_err(|error| error.to_string());
    }
    super::local_resume_shell_command(&session.home, &session.cwd, &session.id)
        .map_err(|error| error.to_string())
}

/// Common entry for account launches and managed session resumes, through the Hub.
pub fn start(
    root: &Path,
    account: &str,
    revision: Option<i64>,
    request: &SpawnRequest,
) -> Result<Box<dyn RunningAgent>, String> {
    let provider = crate::hub::shared()
        .provider(crate::hub::ProviderDefinition {
            id: account.into(),
            agent: "codex".into(),
            context: ManagedAccount {
                root: root.to_path_buf(),
                account: account.into(),
                revision,
            }
            .context(),
            resources: Vec::new(),
            auth: None,
        })
        .map_err(|error| error.to_string())?;
    provider.start(request).map_err(|error| error.to_string())
}

pub fn spawn(
    root: &Path,
    account: &str,
    revision: Option<i64>,
    request: &SpawnRequest,
) -> Result<SpawnOutcome, String> {
    start(root, account, revision, request)?.wait()
}
