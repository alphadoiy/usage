//! Launch-time identity and pinned resource associations. No raw arguments or keys are stored.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::resources::ResourceBinding;
use crate::resources::connections::ConnectionConfig;

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
