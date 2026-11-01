//! Managed Agent profiles and explicit model/resource connections.
use super::{
    CatalogService, ModelQuery, Models, Plan, Provider, ProviderKind, ProviderUsage, kind,
    unreadable_query, usage,
};
use crate::codex_manager::{AuthMode, Manager};
use crate::config::AppConfig;
use std::collections::HashSet;
impl CatalogService {
    pub(super) fn add_managed_profiles(
        &self,
        plan: &mut Plan,
        config: &AppConfig,
    ) -> Result<(), String> {
        let manager =
            Manager::open_read_only(self.root.clone()).map_err(|_| "读取 Provider 账号库失败")?;
        if self.builtin_magpie {
            self.add_builtin_magpie(plan, manager.as_ref())?;
        }
        let Some(manager) = manager else {
            return Ok(());
        };
        for profile in manager.list().map_err(|_| "读取 Provider 配置失败")? {
            if profile.model_provider == crate::magpie::PROVIDER {
                continue;
            }
            let legacy = crate::magpie::legacy_endpoint(&profile.base_url);
            let query = if legacy {
                None
            } else {
                manager.model_catalog_query(&profile).ok()
            };
            let models = if legacy {
                super::retired_plugin()
            } else if query.is_some() {
                Models::Loading
            } else {
                unreadable_query(&profile)
            };
            plan.catalog.providers.push(Provider {
                id: profile.id.clone(),
                name: profile.name.clone(),
                agent: Some("Codex".into()),
                kind: if legacy {
                    ProviderKind::Plugin
                } else if profile.auth_mode == AuthMode::Oauth {
                    ProviderKind::Subscription
                } else {
                    kind(&profile.base_url)
                },
                configured_model: Some(profile.model.clone()),
                models,
                operations: Vec::new(),
                usage: ProviderUsage::default(),
                performance: if legacy {
                    None
                } else {
                    manager.performance_binding(&profile, profile.revision).ok()
                },
            });
            if let Some(query) = query {
                plan.queries.push(query);
            }
            if !legacy {
                self.add_profile_usage(plan, &manager, &profile, config);
            }
        }
        Ok(())
    }
    fn add_builtin_magpie(&self, plan: &mut Plan, manager: Option<&Manager>) -> Result<(), String> {
        let profile = crate::magpie::builtin_profile(manager)?;
        let connection = crate::magpie::builtin_connection(manager);
        let query = connection
            .as_ref()
            .ok()
            .and_then(|c| c.model_query(crate::magpie::BUILTIN_ID).ok());
        plan.catalog.providers.push(Provider {
            id: crate::magpie::BUILTIN_ID.into(),
            name: crate::magpie::BUILTIN_NAME.into(),
            agent: Some("Codex".into()),
            kind: ProviderKind::Magpie,
            configured_model: (profile.revision > 0).then_some(profile.model),
            models: if query.is_some() {
                Models::Loading
            } else {
                Models::Unavailable("Magpie 连接配置不可读".into())
            },
            usage: ProviderUsage {
                hint: Some("内置 Magpie · e 配置模型与路由".into()),
                ..Default::default()
            },
            operations: Vec::new(),
            performance: None,
        });
        if let Some(query) = query {
            plan.queries.push(query);
        }
        if let Ok(connection) = connection {
            for (label, reader) in [
                (
                    "Magpie 共享额度与余额",
                    crate::magpie::GatewayReader::quotas(connection.clone(), self.http.clone()),
                ),
                (
                    "Magpie 今日用量（全部记录）",
                    crate::magpie::GatewayReader::usage(connection, self.http.clone()),
                ),
            ] {
                usage::attach_reader(
                    plan,
                    crate::magpie::BUILTIN_ID,
                    label.into(),
                    std::sync::Arc::new(reader),
                );
            }
        }
        Ok(())
    }
    /// Explicit inference connections, except billing connections of a listed Provider.
    pub(super) fn add_resource_connections(
        &self,
        plan: &mut Plan,
        config: &AppConfig,
    ) -> Result<(), String> {
        let listed: HashSet<String> = plan
            .catalog
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect();
        for connection in &config.resources {
            let Some(inference) = &connection.inference else {
                continue;
            };
            if connection.profile_ids.iter().any(|id| listed.contains(id)) {
                continue;
            }
            let id = format!("resource:{}", connection.id);
            let query = (self.credentials)(&format!("env:{}", inference.credential_env))
                .map_err(|_| "推理凭据环境变量未设置")
                .and_then(|key| ModelQuery::new(id.clone(), &inference.base_url, key));
            let models = match query {
                Ok(query) => {
                    plan.queries.push(query);
                    Models::Loading
                }
                Err(error) => Models::Unavailable(error.into()),
            };
            let performance = inference
                .performance_instance
                .as_deref()
                .map(|instance_id| {
                    let instance = config
                        .instances
                        .iter()
                        .find(|i| i.id == instance_id)
                        .ok_or("性能日志实例不存在")?;
                    let source = crate::performance::source(&instance.provider)
                        .ok_or("此 Agent 尚不支持性能日志")?;
                    Ok::<_, &str>(crate::performance::local_binding(
                        &id,
                        &instance.provider,
                        &instance.path,
                        source,
                    ))
                })
                .transpose()?;
            plan.catalog.providers.push(Provider {
                id,
                name: connection
                    .label
                    .clone()
                    .unwrap_or_else(|| connection.id.clone()),
                agent: None,
                kind: kind(&inference.base_url),
                configured_model: None,
                models,
                operations: Vec::new(),
                usage: ProviderUsage::default(),
                performance,
            });
        }
        Ok(())
    }
}
