//! Home read model. Discovery/identity validation happens off the UI thread;
//! only verified, deduplicated resource readers cross into async queries.
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::codex_manager::{AuthMode, Manager};
use crate::config::AppConfig;
use crate::resources::connections::ResourceService;
use crate::resources::{CredentialResolver, ResourceBinding, ResourceReader, ResourceSnapshot};
use crate::runs::{IdentityState, LinkState, RunState};

pub const RUN_LIMIT: usize = 200;

#[derive(Debug, Clone)]
pub struct RunOverview {
    pub id: String,
    pub project: PathBuf,
    pub model: Option<String>,
    pub started_at: String,
    pub state: RunState,
    pub identity_reason: Option<String>,
    pub resources: Vec<RunResource>,
}

#[derive(Debug, Clone)]
pub struct RunResource {
    pub label: String,
    pub target: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct AccountOverview {
    pub id: String,
    pub name: String,
    pub agent: String,
    pub auth_mode: AuthMode,
    pub runs: Vec<RunOverview>,
}

#[derive(Debug, Clone)]
pub enum ResourceStatus {
    Unbound,
    Loading,
    Unavailable(String),
    Queried(ResourceSnapshot),
}

#[derive(Debug, Clone)]
pub struct ResourceOverview {
    pub target: String,
    pub label: String,
    pub service: String,
    pub binding: ResourceBinding,
    /// Only runs that pass the current revision and credential proofs are included.
    pub runs: Vec<String>,
    pub status: ResourceStatus,
}

#[derive(Debug, Clone, Default)]
pub struct OverviewSnapshot {
    pub accounts: Vec<AccountOverview>,
    pub resources: Vec<ResourceOverview>,
    pub observed_at: Option<DateTime<Utc>>,
    pub warnings: Vec<String>,
}

impl OverviewSnapshot {
    /// Never route a result by its tool type or accept another account's response.
    pub fn apply_resource(&mut self, target: &str, snapshot: ResourceSnapshot) -> bool {
        let Some(resource) = self
            .resources
            .iter_mut()
            .find(|resource| resource.target == target)
        else {
            return false;
        };
        if !matches!(resource.status, ResourceStatus::Loading) {
            return false;
        }
        if resource.binding != snapshot.binding || resource.service != snapshot.service {
            resource.status = ResourceStatus::Unavailable("资源响应身份不匹配".into());
        } else {
            resource.status = ResourceStatus::Queried(snapshot);
        }
        true
    }
}

pub struct OverviewPlan {
    pub snapshot: OverviewSnapshot,
    pub readers: Vec<Arc<dyn ResourceReader>>,
}

pub struct OverviewService {
    root: PathBuf,
    resources: ResourceService,
    credentials: Arc<CredentialResolver>,
}

impl OverviewService {
    pub fn new(
        root: PathBuf,
        http: Arc<dyn crate::http::HttpClient>,
        credentials: Arc<CredentialResolver>,
    ) -> Self {
        Self {
            root,
            resources: ResourceService::new(
                crate::resources::builtin_registry(),
                http,
                credentials.clone(),
                Arc::new(Utc::now),
            ),
            credentials,
        }
    }

    pub fn from_environment() -> Self {
        Self::new(
            crate::codex_manager::default_root(),
            Arc::new(crate::http::ReqwestClient::new()),
            Arc::new(|reference| {
                let name = reference.strip_prefix("env:").ok_or("无效的凭据引用")?;
                std::env::var(name).map_err(|_| "凭据环境变量未设置".into())
            }),
        )
    }

    pub fn load(&self, config: &AppConfig) -> Result<OverviewPlan, String> {
        let mut plan = OverviewPlan {
            snapshot: OverviewSnapshot {
                observed_at: Some(Utc::now()),
                ..Default::default()
            },
            readers: Vec::new(),
        };
        for connection in &config.resources {
            if plan
                .snapshot
                .resources
                .iter()
                .any(|resource| resource.binding.account == connection.id)
            {
                return Err("资源连接 ID 重复".into());
            }
            plan.snapshot.resources.push(ResourceOverview {
                target: connection.binding().cache_key(&connection.service),
                label: connection
                    .label
                    .clone()
                    .unwrap_or_else(|| connection.id.clone()),
                service: connection.service.clone(),
                binding: connection.binding(),
                runs: Vec::new(),
                status: ResourceStatus::Unbound,
            });
        }
        let Some(manager) =
            Manager::open_read_only(self.root.clone()).map_err(|_| "读取托管账号库失败")?
        else {
            return Ok(plan);
        };
        let profiles = manager.list().map_err(|_| "读取托管账号失败")?;
        let runs = manager
            .recent_runs(RUN_LIMIT)
            .map_err(|_| "读取最近运行记录失败")?;
        if runs.len() == RUN_LIMIT {
            plan.snapshot
                .warnings
                .push(format!("仅展示最近 {RUN_LIMIT} 条运行，未结束记录优先"));
        }
        let credentials = |name: &str| (self.credentials)(&format!("env:{name}")).ok();
        for profile in profiles {
            let mut account = AccountOverview {
                id: profile.id.clone(),
                name: profile.name,
                agent: "Codex".into(),
                auth_mode: profile.auth_mode,
                runs: Vec::new(),
            };
            let mut recent = 0;
            for run in runs.iter().filter(|run| run.profile_id == profile.id) {
                let state = RunState::observe(run);
                if !matches!(state, RunState::Running | RunState::Unknown) {
                    recent += 1;
                    if recent > 3 {
                        continue;
                    }
                }
                let mut view = RunOverview {
                    id: run.id.clone(),
                    project: run.cwd.clone(),
                    model: run
                        .context
                        .as_ref()
                        .and_then(|context| context.identity.model.clone()),
                    started_at: run.started_at.clone(),
                    state,
                    identity_reason: None,
                    resources: Vec::new(),
                };
                if let Some(context) = &run.context {
                    view.identity_reason = if context.identity.state != IdentityState::Resolved {
                        Some(
                            context
                                .identity
                                .reason
                                .clone()
                                .unwrap_or_else(|| "启动身份未解析".into()),
                        )
                    } else {
                        context.resources_error.clone()
                    };
                    for link in &context.resources {
                        let connection = config
                            .resources
                            .iter()
                            .find(|connection| connection.id == link.binding.account);
                        let label = connection
                            .and_then(|connection| connection.label.as_ref())
                            .unwrap_or(&link.binding.account)
                            .clone();
                        let selected = if link.state == LinkState::Matched {
                            manager
                                .resources_for_run_with(
                                    &run.id,
                                    &config.resources,
                                    std::slice::from_ref(&link.binding.account),
                                    &credentials,
                                )
                                .map_err(|error| error.to_string())
                        } else {
                            Err(link_message(&link.state).into())
                        };
                        let result = selected.and_then(|pinned| {
                            let connection = pinned.configs.first().ok_or("运行未绑定资源")?;
                            let target = connection.binding().cache_key(&connection.service);
                            let resource = plan
                                .snapshot
                                .resources
                                .iter_mut()
                                .find(|resource| resource.target == target)
                                .ok_or("资源连接已变更")?;
                            if !plan.readers.iter().any(|reader| {
                                reader.binding() == &connection.binding()
                                    && reader.service() == connection.service
                            }) {
                                plan.readers.push(self.resources_with_credentials(
                                    pinned.credential_resolver(),
                                    connection,
                                )?);
                            }
                            resource.runs.push(run.id.clone());
                            resource.status = ResourceStatus::Loading;
                            Ok(target)
                        });
                        let (target, message) = match result {
                            Ok(target) => (Some(target), "已核对启动关联".into()),
                            Err(message) => {
                                if let Some(resource) =
                                    plan.snapshot.resources.iter_mut().find(|resource| {
                                        resource.binding.account == link.binding.account
                                    })
                                    && resource.runs.is_empty()
                                {
                                    resource.status = ResourceStatus::Unavailable(message.clone());
                                }
                                (None, message)
                            }
                        };
                        view.resources.push(RunResource {
                            label,
                            target,
                            message,
                        });
                    }
                } else {
                    view.identity_reason = Some("旧运行未记录启动身份".into());
                }
                account.runs.push(view);
            }
            plan.snapshot.accounts.push(account);
        }
        Ok(plan)
    }

    fn resources_with_credentials(
        &self,
        credentials: Arc<CredentialResolver>,
        config: &crate::resources::connections::ConnectionConfig,
    ) -> Result<Arc<dyn ResourceReader>, String> {
        // Share HTTP/registry through the service; pin credentials per reader, never globally.
        self.resources
            .prepare_with_credentials(config, credentials)
            .map_err(|_| "资源服务配置无效或尚未支持".into())
    }
}

fn link_message(state: &LinkState) -> &'static str {
    match state {
        LinkState::Matched => "已关联",
        LinkState::UnresolvedIdentity => "启动身份未解析",
        LinkState::MissingInferenceBinding => "未配置推理身份绑定",
        LinkState::EndpointMismatch => "启动端点不匹配",
        LinkState::CredentialMismatch => "启动凭据不匹配",
        LinkState::MissingCredential => "启动时凭据缺失",
        LinkState::InvalidResourceConfiguration => "资源服务配置无效或尚未支持",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{FakeHttp, HttpResponse};
    use crate::resources::connections::{ConnectionConfig, InferenceBinding};
    use crate::resources::{ResourceMetric, ResourceScope, ResourceState};
    use crate::runs::{CapturedContext, LaunchIdentity};
    use rusqlite::{Connection, params};
    use serde_json::json;

    struct Fixture {
        root: PathBuf,
        manager: Manager,
        config: AppConfig,
        profile: String,
        http: Arc<FakeHttp>,
    }
    fn credentials(name: &str) -> Option<String> {
        match name {
            "INFERENCE_ONLY" => Some("synthetic-inference".into()),
            "QUERY_ONLY" => Some("synthetic-query".into()),
            _ => None,
        }
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("usage-overview-{}", uuid::Uuid::new_v4()));
            let mut manager = Manager::open(root.clone()).unwrap();
            let text = crate::codex_manager::config::new_config(
                "https://a.invalid/v1",
                "saved-model",
                None,
            )
            .unwrap();
            let profile = manager
                .add("Work", &text, "synthetic-inference")
                .unwrap()
                .id;
            let connection = ConnectionConfig {
                id: "balance".into(),
                label: Some("工作余额".into()),
                service: "new_api_account".into(),
                base_url: "https://a.invalid".into(),
                credential_env: "QUERY_ONLY".into(),
                revision: String::new(),
                options: json!({"user_id":1,"currency":"CNY","quota_per_unit":"500000"}),
                profile_ids: vec![profile.clone()],
                inference: Some(InferenceBinding {
                    base_url: "https://a.invalid/v1".into(),
                    credential_env: "INFERENCE_ONLY".into(),
                }),
            };
            let http = Arc::new(FakeHttp::new());
            http.route(
                "https://a.invalid/api/user/self",
                Ok(HttpResponse {
                    status: 200,
                    headers: Default::default(),
                    body: br#"{"success":true,"data":{"id":1,"quota":1172839,"used_quota":10}}"#
                        .to_vec(),
                }),
            );
            Self {
                root,
                manager,
                profile,
                http,
                config: AppConfig {
                    resources: vec![connection],
                    ..Default::default()
                },
            }
        }
        fn service(&self) -> OverviewService {
            OverviewService::new(
                self.root.clone(),
                self.http.clone(),
                Arc::new(|reference| {
                    credentials(reference.strip_prefix("env:").unwrap()).ok_or("missing".into())
                }),
            )
        }
        fn record(&self, id: &str, profile: &str, configs: &[ConnectionConfig]) {
            let captured = CapturedContext::capture(
                id,
                profile,
                LaunchIdentity {
                    state: IdentityState::Resolved,
                    agent: "codex".into(),
                    auth_mode: "byok".into(),
                    endpoint: Some("https://a.invalid/v1".into()),
                    model: Some("launch-model".into()),
                    model_provider: Some("usage_byok".into()),
                    credential_source: Some("managed_revision".into()),
                    reason: None,
                },
                Some("synthetic-inference"),
                Ok(configs),
                &credentials,
                &|_| Some(ResourceScope::Account),
            );
            let conn = Connection::open(self.root.join("manager.sqlite3")).unwrap();
            conn.execute("INSERT INTO runs VALUES(?1,?2,1,'/synthetic-project','2026-10-16T00:00:00Z','2026-10-16T00:01:00Z',NULL,0)",params![id,profile]).unwrap();
            conn.execute(
                "INSERT INTO run_contexts VALUES(?1,?2)",
                params![id, serde_json::to_string(&captured).unwrap()],
            )
            .unwrap();
        }
        fn run(&self, id: &str) {
            self.record(id, &self.profile, &self.config.resources);
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn shared_resource_is_queried_once_with_distinct_run_associations() {
        let f = Fixture::new();
        f.run("first");
        f.run("second");
        let mut plan = f.service().load(&f.config).unwrap();
        assert_eq!(plan.readers.len(), 1);
        assert_eq!(plan.snapshot.resources[0].runs.len(), 2);
        assert_eq!(plan.snapshot.accounts[0].runs.len(), 2);
        assert_eq!(
            plan.snapshot.accounts[0].runs[0].model.as_deref(),
            Some("launch-model")
        );
        let reader = &plan.readers[0];
        let snapshot = reader.read().await;
        assert_eq!(snapshot.state, ResourceState::Ready);
        assert!(snapshot.metrics.iter().any(|metric| matches!(metric,ResourceMetric::Balance { remaining } if remaining.amount=="2.345678" && remaining.currency=="CNY")));
        let target = reader.binding().cache_key(reader.service());
        assert!(plan.snapshot.apply_resource(&target, snapshot.clone()));
        assert!(!plan.snapshot.apply_resource(&target, snapshot));
        assert_eq!(f.http.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn unbound_or_reconfigured_resources_do_not_create_readers() {
        let mut f = Fixture::new();
        assert!(f.service().load(&f.config).unwrap().readers.is_empty());
        f.run("before-edit");
        f.config.resources[0].base_url = "https://different.invalid".into();
        let plan = f.service().load(&f.config).unwrap();
        assert!(plan.readers.is_empty());
        assert!(
            matches!(&plan.snapshot.resources[0].status,ResourceStatus::Unavailable(message) if message.contains("配置已变更"))
        );
        assert!(
            plan.snapshot.accounts[0].runs[0].resources[0]
                .target
                .is_none()
        );
        assert!(f.http.calls.lock().unwrap().is_empty());
        f.config.resources.clear();
        let plan = f.service().load(&f.config).unwrap();
        assert!(plan.snapshot.resources.is_empty());
        assert!(
            plan.snapshot.accounts[0].runs[0].resources[0]
                .message
                .contains("已删除")
        );
    }

    #[test]
    fn rotated_credentials_do_not_fall_back_to_current_account() {
        let f = Fixture::new();
        f.run("before-rotation");
        let service = OverviewService::new(
            f.root.clone(),
            f.http.clone(),
            Arc::new(|_| Ok("rotated".into())),
        );
        let plan = service.load(&f.config).unwrap();
        assert!(plan.readers.is_empty());
        assert!(
            plan.snapshot.accounts[0].runs[0].resources[0]
                .message
                .contains("已轮换")
        );
        assert!(f.http.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn invalid_run_cannot_hide_healthy_shared_resource() {
        let f = Fixture::new();
        f.run("a-healthy");
        let mut old = f.config.resources.clone();
        old[0].revision = "old".into();
        f.record("z-stale", &f.profile, &old);
        let plan = f.service().load(&f.config).unwrap();
        assert_eq!(plan.readers.len(), 1);
        assert_eq!(plan.snapshot.resources[0].runs, vec!["a-healthy"]);
        assert!(matches!(
            plan.snapshot.resources[0].status,
            ResourceStatus::Loading
        ));
        let runs = &plan.snapshot.accounts[0].runs;
        assert!(
            runs.iter().find(|r| r.id == "z-stale").unwrap().resources[0]
                .target
                .is_none()
        );
        assert!(
            runs.iter().find(|r| r.id == "a-healthy").unwrap().resources[0]
                .target
                .is_some()
        );
    }

    #[tokio::test]
    async fn different_accounts_never_share_resource_results() {
        let mut f = Fixture::new();
        f.run("work-run");
        let text = crate::codex_manager::config::new_config("https://a.invalid/v1", "other", None)
            .unwrap();
        let profile = f
            .manager
            .add("Other", &text, "synthetic-inference")
            .unwrap()
            .id;
        let mut other = f.config.resources[0].clone();
        other.id = "other-balance".into();
        other.profile_ids = vec![profile.clone()];
        other.base_url = "https://b.invalid".into();
        f.record("other-run", &profile, std::slice::from_ref(&other));
        f.config.resources.push(other);
        f.http.route(
            "https://b.invalid/api/user/self",
            Ok(HttpResponse {
                status: 401,
                headers: Default::default(),
                body: Vec::new(),
            }),
        );
        let mut plan = f.service().load(&f.config).unwrap();
        assert_eq!(plan.readers.len(), 2);
        for reader in &plan.readers {
            let result = reader.read().await;
            assert!(
                plan.snapshot
                    .apply_resource(&reader.binding().cache_key(reader.service()), result)
            );
        }
        let first = plan
            .snapshot
            .resources
            .iter()
            .find(|r| r.binding.account == "balance")
            .unwrap();
        let second = plan
            .snapshot
            .resources
            .iter()
            .find(|r| r.binding.account == "other-balance")
            .unwrap();
        assert_eq!(first.runs, vec!["work-run"]);
        assert_eq!(second.runs, vec!["other-run"]);
        assert!(
            matches!(&first.status,ResourceStatus::Queried(s) if s.state==ResourceState::Ready)
        );
        assert!(
            matches!(&second.status,ResourceStatus::Queried(s) if s.state==ResourceState::AuthenticationFailed)
        );
        assert_eq!(f.http.calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn snapshot_identity_mismatch_is_an_explicit_failure() {
        let f = Fixture::new();
        f.run("work-run");
        let mut plan = f.service().load(&f.config).unwrap();
        let resource = &plan.snapshot.resources[0];
        let target = resource.target.clone();
        let mut wrong_binding = resource.binding.clone();
        wrong_binding.account = "other".into();
        let snapshot = ResourceSnapshot {
            binding: wrong_binding,
            service: resource.service.clone(),
            scope: ResourceScope::Account,
            state: ResourceState::Ready,
            metrics: Vec::new(),
            queried_at: None,
            message: None,
        };
        assert!(!plan.snapshot.apply_resource("unexpected", snapshot.clone()));
        assert!(plan.snapshot.apply_resource(&target, snapshot));
        assert!(
            matches!(&plan.snapshot.resources[0].status,ResourceStatus::Unavailable(message) if message.contains("身份不匹配"))
        );
    }
}
