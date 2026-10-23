//! The three Provider sources of the home board: managed Codex accounts, plugin accounts and
//! explicit resource connections. Each contributes Providers, model queries and usage readers.
use std::collections::{HashMap, HashSet};

use super::{
    CatalogService, ModelQuery, Models, Plan, Provider, ProviderKind, ProviderUsage, kind, presets,
    unavailable_plugin, unreadable_query, usage,
};
use crate::codex_manager::{AuthMode, Manager};
use crate::config::AppConfig;
use crate::plugins::store::{Account, Plugin};

/// Plugin accounts whose plugin is installed and enabled.
pub(super) struct PluginSnapshot {
    accounts: Vec<Account>,
    plugins: HashMap<String, Plugin>,
}
impl PluginSnapshot {
    fn available(&self, account: &str) -> bool {
        self.accounts
            .iter()
            .any(|a| a.id == account && self.plugins.contains_key(&a.plugin))
    }
    fn plugin_of(&self, account: &str) -> Option<&Plugin> {
        self.accounts
            .iter()
            .find(|a| a.id == account)
            .and_then(|a| self.plugins.get(&a.plugin))
    }
}

/// Managed profile IDs bound to each plugin account.
type Bindings = HashMap<String, Vec<String>>;

impl CatalogService {
    /// An unreadable plugin store hides plugin Providers; that is logged rather than silent.
    pub(super) fn plugin_snapshot(&self) -> PluginSnapshot {
        let accounts = self.plugins.accounts().unwrap_or_else(|error| {
            tracing::warn!(event = "catalog_plugin_accounts_unreadable", error = %error);
            Vec::new()
        });
        let plugins = self
            .plugins
            .plugins()
            .unwrap_or_else(|error| {
                tracing::warn!(event = "catalog_plugins_unreadable", error = %error);
                Vec::new()
            })
            .into_iter()
            .filter(|p| p.enabled)
            .map(|p| (p.id.clone(), p))
            .collect();
        PluginSnapshot { accounts, plugins }
    }

    pub(super) fn add_managed_profiles(
        &self,
        plan: &mut Plan,
        config: &AppConfig,
        plugins: &PluginSnapshot,
    ) -> Result<Bindings, String> {
        let mut bindings = Bindings::new();
        let Some(manager) =
            Manager::open_read_only(self.root.clone()).map_err(|_| "读取 Provider 账号库失败")?
        else {
            return Ok(bindings);
        };
        for profile in manager.list().map_err(|_| "读取 Provider 配置失败")? {
            let plugin_account = crate::plugins::endpoint_account(&profile.base_url);
            let models = if let Some(account) = &plugin_account {
                bindings
                    .entry(account.clone())
                    .or_default()
                    .push(profile.id.clone());
                if plugins.available(account) {
                    Models::Loading
                } else {
                    unavailable_plugin()
                }
            } else {
                match manager.model_catalog_query(&profile) {
                    Ok(query) => {
                        plan.queries.push(query);
                        Models::Loading
                    }
                    Err(_) => unreadable_query(&profile),
                }
            };
            let provider_kind = if let Some(account_id) = &plugin_account {
                plugins
                    .plugin_of(account_id)
                    .and_then(|p| presets::for_package(&p.package))
                    .map_or(ProviderKind::Plugin, |preset| preset.kind)
            } else if profile.auth_mode == AuthMode::Oauth {
                ProviderKind::Subscription
            } else {
                kind(&profile.base_url)
            };
            plan.catalog.providers.push(Provider {
                id: profile.id.clone(),
                name: profile.name.clone(),
                agent: Some(crate::agents::AgentKind::CODEX.name().into()),
                kind: provider_kind,
                configured_model: Some(profile.model.clone()),
                models,
                operations: Vec::new(),
                usage: ProviderUsage::default(),
                performance: manager.performance_binding(&profile, profile.revision).ok(),
            });
            self.add_profile_usage(plan, &manager, &profile, config);
        }
        Ok(bindings)
    }

    /// A plugin account appears as its own Provider unless managed profiles already bind it.
    pub(super) fn add_plugin_accounts(
        &self,
        plan: &mut Plan,
        plugins: &PluginSnapshot,
        mut bindings: Bindings,
    ) {
        let mut descriptors: HashMap<String, Option<serde_json::Value>> = HashMap::new();
        for account in &plugins.accounts {
            let Some(plugin) = plugins.plugins.get(&account.plugin) else {
                continue;
            };
            let preset = presets::for_package(&plugin.package);
            let existing = bindings.remove(&account.id).unwrap_or_default();
            let descriptor = descriptors.entry(plugin.id.clone()).or_insert_with(|| {
                crate::plugins::store::read::<serde_json::Value>(
                    &self
                        .plugins
                        .root
                        .join("descriptors")
                        .join(format!("{}.json", plugin.id)),
                )
                .ok()
                .filter(|d| d["revision"].as_u64() == Some(plugin.revision))
            });
            let supports_usage = descriptor
                .as_ref()
                .and_then(|d| {
                    d["providers"]
                        .as_array()?
                        .iter()
                        .find(|p| p["id"] == account.provider)
                        .map(|p| p["capabilities"]["usage"] == true)
                })
                .unwrap_or(true);
            let ids = if existing.is_empty() {
                let id = format!("plugin:{}", account.id);
                plan.catalog.providers.push(Provider {
                    id: id.clone(),
                    name: preset.map_or_else(
                        || account.name.clone(),
                        |p| p.account_name(&account.provider, &account.name),
                    ),
                    agent: None,
                    kind: preset.map_or(ProviderKind::Plugin, |p| p.kind),
                    configured_model: None,
                    models: Models::Loading,
                    operations: Vec::new(),
                    usage: ProviderUsage::default(),
                    performance: None,
                });
                vec![id]
            } else {
                existing
            };
            for id in ids {
                let query = self.plugin_query(id.clone(), plugin, &account.id);
                if supports_usage {
                    let reader = crate::plugins::adapters::resource(
                        &self.plugins.root,
                        account,
                        format!(
                            "{}:{}:{}",
                            plugin.version,
                            plugin.revision,
                            plugin.entry.display()
                        ),
                    );
                    let label =
                        preset.map_or_else(|| "账号额度".into(), |p| format!("{} 额度", p.name));
                    usage::attach_reader(plan, &id, label, reader);
                } else if let Some(p) = plan.catalog.providers.iter_mut().find(|p| p.id == id) {
                    p.usage.hint = Some("此服务暂未提供用量查询".into());
                }
                if let Some(checkin) = crate::plugins::capabilities::checkin(
                    &plugin.package,
                    &account.provider,
                    descriptor.as_ref(),
                ) && let Some(provider) = plan.catalog.providers.iter_mut().find(|p| p.id == id)
                {
                    // A malformed account skips its operation; it must not fail the whole board.
                    match crate::provider_actions::registry().create(
                        "plugin_checkin",
                        crate::provider_actions::Context {
                            root: self.plugins.root.clone(),
                            account: account.id.clone(),
                            automatic_status: checkin.automatic_status,
                            day_offset_seconds: checkin.day_offset_seconds,
                        },
                    ) {
                        Ok(operation) => provider.operations.push(operation),
                        Err(error) => tracing::warn!(
                            event = "catalog_operation_skipped",
                            account_id = account.id,
                            error = %error
                        ),
                    }
                }
                plan.queries.push(query);
            }
        }
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
