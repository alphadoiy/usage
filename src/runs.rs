//! Launch-time identity and pinned resource associations. No raw arguments or keys are stored.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::resources::ResourceBinding;
use crate::resources::connections::ConnectionConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivity {
    Idle,
    Working,
    Interrupted,
    Ended,
}
impl SessionActivity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "等待输入",
            Self::Working => "执行中",
            Self::Interrupted => "已中断",
            Self::Ended => "会话已关闭",
        }
    }
}

/// Confirmed by a session event, never inferred from a shared Home or project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSession {
    pub id: String,
    pub cwd: std::path::PathBuf,
    pub activity: SessionActivity,
    pub turn_id: Option<String>,
    pub model: Option<String>,
    pub first_seen: String,
    pub last_seen: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunEvent {
    pub sequence: i64,
    pub kind: String,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub model: Option<String>,
    pub observed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRun {
    pub run_id: String,
    pub process: RunState,
    pub activity: SessionActivity,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionRuntime {
    pub runs: Vec<SessionRun>,
}
impl SessionRuntime {
    pub fn label(&self) -> String {
        let active: Vec<_> = self
            .runs
            .iter()
            .filter(|run| run.process == RunState::Running)
            .collect();
        match active.as_slice() {
            [run] => run.activity.label().into(),
            [] => self.runs.first().map_or(String::new(), |run| {
                match run.process {
                    RunState::Completed => "进程已退出",
                    RunState::Failed => "异常结束",
                    RunState::Stopped => "已停止",
                    _ => "运行状态待确认",
                }
                .into()
            }),
            runs => {
                let activity = runs
                    .iter()
                    .max_by_key(|run| match run.activity {
                        SessionActivity::Working => 4,
                        SessionActivity::Idle => 3,
                        SessionActivity::Interrupted => 2,
                        SessionActivity::Ended => 1,
                    })
                    .unwrap()
                    .activity;
                format!("{} 个运行实例 · {}", runs.len(), activity.label())
            }
        }
    }
}

/// OS process identity, kept private to the run store rather than displayed in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pid: u32,
    started: String,
}

#[cfg(test)]
mod process_tests {
    use super::*;
    use crate::codex_manager::RunRecord;

    fn run(pid: u32) -> RunRecord {
        RunRecord {
            id: "synthetic-run".into(),
            profile_id: "synthetic-profile".into(),
            revision: 1,
            cwd: "/synthetic-project".into(),
            started_at: "2026-10-16T00:00:00Z".into(),
            finished_at: None,
            pid: Some(pid),
            exit_code: None,
            context: None,
            sessions: Vec::new(),
            process_identity: ProcessIdentity::capture(pid),
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn process_liveness_requires_matching_start_fingerprint() {
        let mut record = run(std::process::id());
        assert!(record.process_identity.is_some());
        assert_eq!(RunState::observe(&record), RunState::Running);
        record
            .process_identity
            .as_mut()
            .unwrap()
            .started
            .push_str("-different");
        assert_eq!(RunState::observe(&record), RunState::Stopped, "reused PID");
        record.process_identity = None;
        assert_eq!(RunState::observe(&record), RunState::Unknown, "old record");
        record.pid = None;
        assert_eq!(RunState::observe(&record), RunState::Unknown);
        record.finished_at = Some("2026-10-16T01:00:00Z".into());
        record.exit_code = Some(0);
        assert_eq!(RunState::observe(&record), RunState::Completed);
        record.exit_code = Some(1);
        assert_eq!(RunState::observe(&record), RunState::Failed);
    }

    #[cfg(unix)]
    #[test]
    fn exited_child_without_finish_record_is_stopped() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let record = run(child.id());
        child.wait().unwrap();
        assert_eq!(RunState::observe(&record), RunState::Stopped);
    }
}

impl ProcessIdentity {
    pub(crate) fn capture(pid: u32) -> Option<Self> {
        if pid == 0 || pid > i32::MAX as u32 {
            return None;
        }
        #[cfg(target_os = "macos")]
        {
            let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
            // SAFETY: proc_pidinfo receives a valid, correctly sized writable buffer.
            let read = unsafe {
                libc::proc_pidinfo(
                    pid as i32,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            if read == size {
                // SAFETY: the OS initialized the complete buffer above.
                let info = unsafe { info.assume_init() };
                return Some(Self {
                    pid,
                    started: format!("{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec),
                });
            }
        }
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let (_, fields) = stat.rsplit_once(')')?;
            let start = fields.split_whitespace().nth(19)?;
            let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
            return Some(Self {
                pid,
                started: format!("{}:{start}", boot.trim()),
            });
        }
        #[allow(unreachable_code)]
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Completed,
    Failed,
    Stopped,
    Unknown,
}

impl RunState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "运行中",
            Self::Completed => "已完成",
            Self::Failed => "异常结束",
            Self::Stopped => "已停止 · 缺少结束记录",
            Self::Unknown => "状态待确认",
        }
    }

    pub fn observe(run: &crate::codex_manager::RunRecord) -> Self {
        if run.finished_at.is_some() {
            return if run.exit_code == Some(0) {
                Self::Completed
            } else {
                Self::Failed
            };
        }
        let Some(pid) = run.pid.filter(|pid| *pid > 0 && *pid <= i32::MAX as u32) else {
            return Self::Unknown;
        };
        if let Some(current) = ProcessIdentity::capture(pid) {
            return match &run.process_identity {
                Some(expected) if *expected == current => Self::Running,
                Some(_) => Self::Stopped,
                None => Self::Unknown,
            };
        }
        #[cfg(unix)]
        {
            // SAFETY: signal 0 only checks existence/permissions; it sends no signal.
            if unsafe { libc::kill(pid as i32, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return Self::Stopped;
            }
        }
        Self::Unknown
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityState {
    Resolved,
    Unresolved,
}

/// Resolved by the launcher, not an observation of later in-session changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchIdentity {
    pub state: IdentityState,
    pub agent: String,
    pub auth_mode: String,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    /// A reference such as `managed_revision` or `environment`, never a key value.
    pub credential_source: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    Matched,
    UnresolvedIdentity,
    MissingInferenceBinding,
    EndpointMismatch,
    CredentialMismatch,
    MissingCredential,
    InvalidResourceConfiguration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLink {
    pub service: String,
    pub binding: ResourceBinding,
    pub state: LinkState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunContext {
    pub identity: LaunchIdentity,
    pub resources: Vec<ResourceLink>,
    /// A configuration failure does not prevent the original managed launch.
    pub resources_error: Option<String>,
}

/// Private database representation. Proofs never appear in CLI output or logs.
#[derive(Serialize, Deserialize)]
pub(crate) struct CapturedContext {
    pub context: RunContext,
    proofs: Vec<Option<CredentialProof>>,
    #[serde(default)]
    pub(crate) process: Option<ProcessIdentity>,
}

#[derive(Serialize, Deserialize)]
struct CredentialProof {
    inference: String,
    query: String,
}

/// Query credentials are frozen after validation so a later environment read cannot switch accounts.
pub struct PinnedResources {
    pub configs: Vec<ConnectionConfig>,
    credentials: std::collections::BTreeMap<String, String>,
}
impl PinnedResources {
    pub fn credential_resolver(&self) -> std::sync::Arc<crate::resources::CredentialResolver> {
        let credentials = self.credentials.clone();
        std::sync::Arc::new(move |reference| {
            credentials
                .get(reference)
                .cloned()
                .ok_or_else(|| "运行未绑定此凭据".into())
        })
    }
}

fn proof(run_id: &str, role: &str, key: &str) -> String {
    let mut hash = Sha256::new();
    for part in ["usage-run-credential-v1", run_id, role, key] {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn endpoint(value: &str) -> Option<String> {
    let url = crate::resources::api_endpoint(value)?;
    Some(url.trim_end_matches('/').to_string())
}

impl CapturedContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn capture(
        run_id: &str,
        profile_id: &str,
        identity: LaunchIdentity,
        key: Option<&str>,
        configs: Result<&[ConnectionConfig], &str>,
        credentials: &dyn Fn(&str) -> Option<String>,
        scope: &dyn Fn(&ConnectionConfig) -> Option<crate::resources::ResourceScope>,
    ) -> Self {
        let mut captured = Self {
            context: RunContext {
                identity,
                resources: Vec::new(),
                resources_error: None,
            },
            proofs: Vec::new(),
            process: None,
        };
        let configs = match configs {
            Ok(configs) => configs,
            Err(reason) => {
                captured.context.resources_error = Some(reason.into());
                return captured;
            }
        };
        for config in configs
            .iter()
            .filter(|c| c.profile_ids.iter().any(|p| p == profile_id))
        {
            let mut credential_proof = None;
            let scope = scope(config);
            let state = if captured.context.identity.state != IdentityState::Resolved {
                LinkState::UnresolvedIdentity
            } else if scope.is_none() {
                LinkState::InvalidResourceConfiguration
            } else if let Some(inference) = &config.inference {
                if endpoint(&inference.base_url).is_none()
                    || captured
                        .context
                        .identity
                        .endpoint
                        .as_deref()
                        .and_then(endpoint)
                        != endpoint(&inference.base_url)
                {
                    LinkState::EndpointMismatch
                } else {
                    match (
                        key,
                        credentials(&inference.credential_env),
                        credentials(&config.credential_env),
                    ) {
                        (Some(key), Some(expected), Some(query))
                            if !key.is_empty() && !query.is_empty() =>
                        {
                            if key == expected
                                && (scope != Some(crate::resources::ResourceScope::Token)
                                    || query == key)
                            {
                                credential_proof = Some(CredentialProof {
                                    inference: proof(run_id, "inference", key),
                                    query: proof(run_id, "query", &query),
                                });
                                LinkState::Matched
                            } else {
                                LinkState::CredentialMismatch
                            }
                        }
                        _ => LinkState::MissingCredential,
                    }
                }
            } else {
                LinkState::MissingInferenceBinding
            };
            captured.context.resources.push(ResourceLink {
                service: config.service.clone(),
                binding: config.binding(),
                state,
            });
            captured.proofs.push(credential_proof);
        }
        captured
    }

    /// Validate the launch-time association before any query or network request.
    pub(crate) fn select(
        &self,
        run_id: &str,
        profile_id: &str,
        configs: &[ConnectionConfig],
        ids: &[String],
        credentials: &dyn Fn(&str) -> Option<String>,
    ) -> Result<PinnedResources, String> {
        if self.context.identity.state != IdentityState::Resolved {
            return Err("此运行的实际配置未解析，不能绑定资源查询".into());
        }
        if self.context.resources_error.is_some() {
            return Err("启动时未能捕获资源配置，不能回退到当前账号关联".into());
        }
        for id in ids {
            if !self
                .context
                .resources
                .iter()
                .any(|l| l.binding.account == *id && l.state == LinkState::Matched)
            {
                return Err("所选资源未在此运行启动时匹配".into());
            }
        }
        let mut selected = Vec::new();
        let mut pinned_credentials = std::collections::BTreeMap::new();
        for (index, link) in self.context.resources.iter().enumerate().filter(|(_, l)| {
            l.state == LinkState::Matched && (ids.is_empty() || ids.contains(&l.binding.account))
        }) {
            let config = configs
                .iter()
                .find(|c| c.id == link.binding.account)
                .ok_or("运行关联的资源连接已删除")?;
            if config.service != link.service
                || config.binding() != link.binding
                || !config.profile_ids.iter().any(|id| id == profile_id)
            {
                return Err("运行关联的资源配置已变更；请重新启动以建立新关联".into());
            }
            let inference = config.inference.as_ref().ok_or("资源缺少推理身份配置")?;
            let expected = self
                .proofs
                .get(index)
                .and_then(Option::as_ref)
                .ok_or("运行缺少凭据校验记录")?;
            let inference_key =
                credentials(&inference.credential_env).ok_or("推理凭据环境变量未设置")?;
            let query_key = credentials(&config.credential_env).ok_or("查询凭据环境变量未设置")?;
            if proof(run_id, "inference", &inference_key) != expected.inference
                || proof(run_id, "query", &query_key) != expected.query
            {
                return Err("资源凭据已轮换，与此运行启动时不一致".into());
            }
            selected.push(config.clone());
            pinned_credentials.insert(format!("env:{}", config.credential_env), query_key);
        }
        if selected.is_empty() {
            return Err("此运行没有已匹配的资源；查看 usage codex runs --json 中的关联状态".into());
        }
        Ok(PinnedResources {
            configs: selected,
            credentials: pinned_credentials,
        })
    }
}

#[cfg(test)]
mod binding_tests {
    use super::*;
    fn config() -> ConnectionConfig {
        serde_json::from_value(serde_json::json!({
            "id":"a", "service":"new_api_account", "base_url":"https://a.invalid", "credential_env":"QUERY_TEST_ONLY",
            "profile_ids":["123e4567-e89b-12d3-a456-426614174001"], "options":{"user_id":1},
            "inference":{"base_url":"https://a.invalid/v1","credential_env":"INFERENCE_TEST_ONLY"}
        })).unwrap()
    }
    const PROFILE: &str = "123e4567-e89b-12d3-a456-426614174001";
    fn credentials(name: &str) -> Option<String> {
        match name {
            "QUERY_TEST_ONLY" => Some("synthetic-query".into()),
            "INFERENCE_TEST_ONLY" => Some("synthetic-inference".into()),
            _ => None,
        }
    }
    fn capture(run: &str) -> CapturedContext {
        CapturedContext::capture(
            run,
            PROFILE,
            LaunchIdentity {
                state: IdentityState::Resolved,
                agent: "custom".into(),
                auth_mode: "byok".into(),
                endpoint: Some("https://a.invalid/v1".into()),
                model: Some("m".into()),
                model_provider: Some("provider".into()),
                credential_source: Some("environment".into()),
                reason: None,
            },
            Some("synthetic-inference"),
            Ok(&[config()]),
            &credentials,
            &|_| Some(crate::resources::ResourceScope::Account),
        )
    }
    #[test]
    fn run_binding_query_resolver_keeps_validated_values_without_reopening_environment() {
        let pinned = capture("run-a")
            .select("run-a", PROFILE, &[config()], &[], &credentials)
            .unwrap();
        assert_eq!(
            pinned.credential_resolver()("env:QUERY_TEST_ONLY").unwrap(),
            "synthetic-query"
        );
        assert!(pinned.credential_resolver()("env:UNRELATED_TEST_ONLY").is_err());
    }
    #[test]
    fn run_binding_credential_proofs_cannot_be_reused_across_runs() {
        let captured = capture("run-a");
        assert!(
            captured
                .select("run-b", PROFILE, &[config()], &[], &credentials)
                .is_err()
        );
    }
    #[test]
    fn run_binding_inconsistent_unresolved_context_is_rejected() {
        let mut captured = capture("run-a");
        captured.context.identity.state = IdentityState::Unresolved;
        assert!(
            captured
                .select("run-a", PROFILE, &[config()], &[], &credentials)
                .is_err()
        );
    }
    #[test]
    fn run_binding_inconsistent_service_metadata_is_rejected() {
        let mut captured = capture("run-a");
        captured.context.resources[0].service = "different-service".into();
        assert!(
            captured
                .select("run-a", PROFILE, &[config()], &[], &credentials)
                .is_err()
        );
    }
}
