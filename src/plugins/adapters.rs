//! Bridges plugin-provided capabilities into the existing resource registry.
use super::*;
use crate::resources::*;
use async_trait::async_trait;
use chrono::Utc;
use std::sync::Arc;
struct Reader {
    context: ResourceContext,
    root: PathBuf,
    account: String,
}
#[async_trait]
impl ResourceReader for Reader {
    fn service(&self) -> &str {
        "plugin_account"
    }
    fn binding(&self) -> &ResourceBinding {
        &self.context.binding
    }
    fn scope(&self) -> ResourceScope {
        ResourceScope::Account
    }
    async fn read(&self) -> ResourceSnapshot {
        let result = crate::plugins::call_at(
            &self.root,
            serde_json::json!({"operation":"usage","account":self.account}),
        )
        .await;
        self.snapshot(result)
    }
}
impl Reader {
    fn snapshot(&self, result: Result<Value>) -> ResourceSnapshot {
        let mut snapshot = ResourceSnapshot {
            binding: self.context.binding.clone(),
            service: self.service().into(),
            scope: self.scope(),
            state: ResourceState::Ready,
            metrics: Vec::new(),
            queried_at: Some(Utc::now()),
            message: None,
        };
        match result {
            Ok(v) if v["signIn"] == "expired" => {
                snapshot.state = ResourceState::AuthenticationFailed;
                snapshot.message = Some("账号登录已过期，请重新登录".into());
            }
            Ok(v) if v["error"].as_str().is_none() => {
                if let Some(windows) = v["windows"].as_array() {
                    for w in windows {
                        if let Some(used) = w["used"].as_f64() {
                            snapshot.metrics.push(ResourceMetric::QuotaWindow {
                                name: w["name"].as_str().unwrap_or("额度").into(),
                                used_percent: used,
                                resets_at: w["resetsAt"]
                                    .as_str()
                                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                                    .map(|t| t.with_timezone(&Utc)),
                            });
                        }
                        if let (Some(amount), Some(limit), Some(unit)) =
                            (w.get("amount"), w.get("limit"), w["unit"].as_str())
                        {
                            let used = amount
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| amount.to_string());
                            let total = limit
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| limit.to_string());
                            let remaining = used
                                .parse::<rust_decimal::Decimal>()
                                .ok()
                                .zip(total.parse::<rust_decimal::Decimal>().ok())
                                .map(|(u, l)| (l - u).to_string());
                            snapshot.metrics.push(ResourceMetric::Allowance {
                                remaining,
                                used: Some(used),
                                unit: unit.into(),
                                unlimited: false,
                            });
                        }
                    }
                }
                snapshot.message = v["plan"].as_str().map(str::to_owned);
            }
            _ => {
                snapshot.state = ResourceState::TransientFailure;
                snapshot.message = Some("用量查询暂不可用；未知额度不会显示为零".into());
            }
        }
        snapshot
    }
}
pub(crate) fn register(registry: &mut ResourceRegistry) {
    registry
        .register("plugin_account", |context| {
            let ResourceConnection::Plugin { root, account } = &context.connection else {
                return Err(crate::registry::RegistryError::Factory(
                    "插件资源连接类型不匹配".into(),
                ));
            };
            Ok(Arc::new(Reader {
                root: root.clone(),
                account: account.clone(),
                context,
            }) as Arc<dyn ResourceReader>)
        })
        .expect("unique plugin resource factory");
}
pub(crate) fn resource(
    root: &Path,
    account: &store::Account,
    revision: String,
) -> Arc<dyn ResourceReader> {
    crate::resources::builtin_registry()
        .create(
            "plugin_account",
            ResourceContext {
                binding: ResourceBinding {
                    account: account.id.clone(),
                    revision,
                },
                connection: ResourceConnection::Plugin {
                    root: root.to_owned(),
                    account: account.id.clone(),
                },
            },
        )
        .expect("validated plugin resource factory")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reader() -> Reader {
        Reader {
            context: ResourceContext {
                binding: ResourceBinding {
                    account: "fixture".into(),
                    revision: "1".into(),
                },
                connection: ResourceConnection::Plugin {
                    root: PathBuf::new(),
                    account: "fixture".into(),
                },
            },
            root: PathBuf::new(),
            account: "fixture".into(),
        }
    }

    #[test]
    fn expired_signin_takes_precedence_over_error_prose_and_metrics() {
        for value in [
            json!({"signIn":"expired"}),
            json!({"signIn":"expired","error":"fixture"}),
            json!({"signIn":"expired","windows":[{"used":25}]}),
        ] {
            let snapshot = reader().snapshot(Ok(value));
            assert_eq!(snapshot.state, ResourceState::AuthenticationFailed);
            assert!(snapshot.metrics.is_empty());
        }
        let snapshot = reader().snapshot(Err("offline".into()));
        assert_eq!(snapshot.state, ResourceState::TransientFailure);
        assert!(snapshot.metrics.is_empty());
    }

    #[test]
    fn credits_keep_their_unit_and_decimal_precision() {
        let snapshot = reader().snapshot(Ok(json!({"plan":"Fixture","windows":[{
            "name":"Credits","used":25,"amount":"0.100000000000001",
            "limit":"1.100000000000002","unit":"credits"
        }]})));
        assert_eq!(snapshot.state, ResourceState::Ready);
        assert!(snapshot.metrics.iter().any(|m| matches!(m,
            ResourceMetric::Allowance { remaining:Some(remaining),used:Some(used),unit,unlimited:false }
            if remaining == "1.000000000000001" && used == "0.100000000000001" && unit == "credits"
        )));
    }
}
