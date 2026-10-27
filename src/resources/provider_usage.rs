//! Provider-native balances and Sub2API usage, matching CC Switch's endpoint semantics.
use super::api::{ApiClient, ApiProtocol, QueryFailure, QueryResult, invalid, number, options};
use super::*;
use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoOptions {}

fn balance(amount: Decimal, currency: &str) -> QueryResult<ResourceMetric> {
    if currency.len() != 3 || !currency.bytes().all(|c| c.is_ascii_uppercase()) {
        return Err(invalid());
    }
    Ok(ResourceMetric::Balance {
        remaining: Money {
            amount: amount.normalize().to_string(),
            currency: currency.into(),
        },
    })
}

fn rejected(value: &Value) -> bool {
    value.get("success") == Some(&Value::Bool(false))
        || value.get("status") == Some(&Value::Bool(false))
        || value.get("code") == Some(&Value::Bool(false))
        || value
            .get("code")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0 && code != 200)
}

pub(super) struct Sub2Api;
#[async_trait]
impl ApiProtocol for Sub2Api {
    const SERVICE: &'static str = "sub2api_usage";
    fn configure(value: &Value) -> Result<Self, crate::registry::RegistryError> {
        let _: NoOptions = options(value)?;
        Ok(Self)
    }
    fn scope(&self) -> ResourceScope {
        ResourceScope::Token
    }
    async fn query(&self, client: &ApiClient, token: &str) -> QueryResult<Vec<ResourceMetric>> {
        let value = client.get(client.endpoint("v1/usage"), token, &[]).await?;
        if value.get("isValid") == Some(&Value::Bool(false)) || rejected(&value) {
            return Err(QueryFailure(
                ResourceState::Failure,
                "Sub2API 用量状态无效或套餐不可用",
            ));
        }
        let quota = value.get("quota");
        let unit = value
            .get("unit")
            .or_else(|| quota.and_then(|q| q.get("unit")))
            .and_then(Value::as_str)
            .unwrap_or("USD");
        if unit.is_empty() || unit.len() > 32 || unit.chars().any(char::is_control) {
            return Err(invalid());
        }
        let mut metrics = sub2api_windows(&value, unit)?;
        let limit = quota
            .and_then(|q| q.get("limit"))
            .map(|v| number(Some(v)))
            .transpose()?;
        let used = quota
            .and_then(|q| q.get("used"))
            .map(|v| number(Some(v)))
            .transpose()?;
        if let Some(limit) = limit.filter(|limit| *limit > Decimal::ZERO) {
            let used = used
                .filter(|used| *used >= Decimal::ZERO)
                .ok_or_else(invalid)?;
            let remaining = limit.checked_sub(used).ok_or_else(invalid)?;
            metrics.push(ResourceMetric::Allowance {
                remaining: Some(remaining.normalize().to_string()),
                used: Some(used.normalize().to_string()),
                unit: unit.into(),
                unlimited: false,
            });
        }
        if let Some(remaining) = value.get("remaining") {
            let remaining = number(Some(remaining))?;
            if remaining >= Decimal::ZERO {
                if unit.len() == 3 && unit.bytes().all(|c| c.is_ascii_uppercase()) {
                    metrics.push(balance(remaining, unit)?);
                } else if !metrics
                    .iter()
                    .any(|m| matches!(m, ResourceMetric::Allowance { .. }))
                {
                    metrics.push(ResourceMetric::Allowance {
                        remaining: Some(remaining.normalize().to_string()),
                        used: None,
                        unit: unit.into(),
                        unlimited: false,
                    });
                }
            } else if remaining == Decimal::NEGATIVE_ONE {
                if !metrics
                    .iter()
                    .any(|m| matches!(m, ResourceMetric::Allowance { .. }))
                {
                    metrics.push(ResourceMetric::Allowance {
                        remaining: None,
                        used: None,
                        unit: unit.into(),
                        unlimited: true,
                    });
                }
            } else {
                return Err(invalid());
            }
        }
        if metrics.is_empty() {
            return Err(QueryFailure(
                ResourceState::Failure,
                "Sub2API 未返回可确认的额度数据",
            ));
        }
        Ok(metrics)
    }
}

fn sub2api_windows(value: &Value, unit: &str) -> QueryResult<Vec<ResourceMetric>> {
    let Some(windows) = value.get("rate_limits").filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let windows = windows.as_array().ok_or_else(invalid)?;
    let mut metrics = Vec::new();
    for (window, name) in [("5h", "5 hours"), ("1d", "1 day"), ("7d", "7 days")] {
        let Some(row) = windows
            .iter()
            .find(|row| row["window"].as_str() == Some(window))
        else {
            continue;
        };
        let limit = number(row.get("limit"))?;
        if limit <= Decimal::ZERO {
            continue;
        }
        let used = number(row.get("used"))?;
        if used < Decimal::ZERO {
            return Err(invalid());
        }
        let remaining = limit.checked_sub(used).ok_or_else(invalid)?;
        let resets_at = match row.get("reset_at").filter(|v| !v.is_null()) {
            None => None,
            Some(value) => Some(
                chrono::DateTime::parse_from_rfc3339(value.as_str().ok_or_else(invalid)?)
                    .map_err(|_| invalid())?
                    .with_timezone(&chrono::Utc),
            ),
        };
        metrics.push(ResourceMetric::AllowanceWindow {
            name: name.into(),
            used: used.normalize().to_string(),
            limit: limit.normalize().to_string(),
            remaining: remaining.normalize().to_string(),
            unit: unit.into(),
            resets_at,
        });
    }
    Ok(metrics)
}

pub(super) struct Sub2ApiAccount;
#[async_trait]
impl ApiProtocol for Sub2ApiAccount {
    const SERVICE: &'static str = "sub2api_account";
    fn configure(value: &Value) -> Result<Self, crate::registry::RegistryError> {
        let _: NoOptions = options(value)?;
        Ok(Self)
    }
    fn scope(&self) -> ResourceScope {
        ResourceScope::Account
    }
    async fn query(&self, client: &ApiClient, token: &str) -> QueryResult<Vec<ResourceMetric>> {
        let value = client
            .get(client.endpoint("api/v1/user/profile"), token, &[])
            .await?;
        if rejected(&value) {
            return Err(QueryFailure(
                if matches!(value.get("code").and_then(Value::as_i64), Some(401 | 403)) {
                    ResourceState::AuthenticationFailed
                } else {
                    ResourceState::Failure
                },
                "Sub2API 账号接口拒绝查询，请检查登录 Token 和接口状态",
            ));
        }
        Ok(vec![balance(
            number(value.pointer("/data/balance"))?,
            "USD",
        )?])
    }
}

pub(super) struct CreditGrants;
#[async_trait]
impl ApiProtocol for CreditGrants {
    const SERVICE: &'static str = "openai_credit_grants";
    fn configure(value: &Value) -> Result<Self, crate::registry::RegistryError> {
        let _: NoOptions = options(value)?;
        Ok(Self)
    }
    fn scope(&self) -> ResourceScope {
        ResourceScope::Token
    }
    async fn query(&self, client: &ApiClient, token: &str) -> QueryResult<Vec<ResourceMetric>> {
        let value = client
            .get(
                client.endpoint("dashboard/billing/credit_grants"),
                token,
                &[],
            )
            .await?;
        if rejected(&value) {
            return Err(QueryFailure(ResourceState::Failure, "账单接口拒绝查询"));
        }
        Ok(vec![balance(number(value.get("total_available"))?, "USD")?])
    }
}

// Each service has its own factory ID; all share credential and transport lifecycle.
macro_rules! native {
    ($name:ident,$id:literal,$path:literal,$parse:ident) => {
        pub(super) struct $name;
        #[async_trait]
        impl ApiProtocol for $name {
            const SERVICE: &'static str = $id;
            fn configure(value: &Value) -> Result<Self, crate::registry::RegistryError> {
                let _: NoOptions = options(value)?;
                Ok(Self)
            }
            fn scope(&self) -> ResourceScope {
                ResourceScope::Account
            }
            async fn query(
                &self,
                client: &ApiClient,
                token: &str,
            ) -> QueryResult<Vec<ResourceMetric>> {
                $parse(&client.get(client.endpoint($path), token, &[]).await?)
            }
        }
    };
}
fn deepseek(v: &Value) -> QueryResult<Vec<ResourceMetric>> {
    let infos = v
        .get("balance_infos")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty())
        .ok_or_else(invalid)?;
    infos
        .iter()
        .map(|row| {
            balance(
                number(row.get("total_balance"))?,
                row.get("currency")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?,
            )
        })
        .collect()
}
fn stepfun(v: &Value) -> QueryResult<Vec<ResourceMetric>> {
    Ok(vec![balance(number(v.get("balance"))?, "CNY")?])
}
fn stepfun_en(v: &Value) -> QueryResult<Vec<ResourceMetric>> {
    Ok(vec![balance(number(v.get("balance"))?, "USD")?])
}
fn moonshot(v: &Value) -> QueryResult<Vec<ResourceMetric>> {
    moonshot_balance(v, "CNY")
}
fn moonshot_en(v: &Value) -> QueryResult<Vec<ResourceMetric>> {
    moonshot_balance(v, "USD")
}
fn moonshot_balance(v: &Value, currency: &str) -> QueryResult<Vec<ResourceMetric>> {
    if rejected(v) {
        return Err(QueryFailure(
            ResourceState::Failure,
            "Moonshot 余额接口拒绝查询",
        ));
    }
    Ok(vec![balance(
        number(v.pointer("/data/available_balance"))?,
        currency,
    )?])
}
fn siliconflow(v: &Value) -> QueryResult<Vec<ResourceMetric>> {
    Ok(vec![balance(
        number(v.pointer("/data/totalBalance"))?,
        "CNY",
    )?])
}
fn siliconflow_en(v: &Value) -> QueryResult<Vec<ResourceMetric>> {
    Ok(vec![balance(
        number(v.pointer("/data/totalBalance"))?,
        "USD",
    )?])
}
fn openrouter(v: &Value) -> QueryResult<Vec<ResourceMetric>> {
    let data = v.get("data").unwrap_or(v);
    let total = number(data.get("total_credits"))?;
    let used = number(data.get("total_usage"))?;
    Ok(vec![
        balance(total.checked_sub(used).ok_or_else(invalid)?, "USD")?,
        ResourceMetric::Allowance {
            remaining: Some(
                total
                    .checked_sub(used)
                    .ok_or_else(invalid)?
                    .normalize()
                    .to_string(),
            ),
            used: Some(used.normalize().to_string()),
            unit: "USD".into(),
            unlimited: false,
        },
    ])
}
fn novita(v: &Value) -> QueryResult<Vec<ResourceMetric>> {
    Ok(vec![balance(
        number(v.get("availableBalance"))?
            .checked_div(Decimal::from(10000))
            .ok_or_else(invalid)?,
        "USD",
    )?])
}
native!(DeepSeek, "deepseek_balance", "user/balance", deepseek);
native!(StepFun, "stepfun_balance", "v1/accounts", stepfun);
native!(StepFunEn, "stepfun_en_balance", "v1/accounts", stepfun_en);
native!(
    Moonshot,
    "moonshot_balance",
    "v1/users/me/balance",
    moonshot
);
native!(
    MoonshotEn,
    "moonshot_en_balance",
    "v1/users/me/balance",
    moonshot_en
);
native!(
    SiliconFlow,
    "siliconflow_balance",
    "v1/user/info",
    siliconflow
);
native!(
    SiliconFlowEn,
    "siliconflow_en_balance",
    "v1/user/info",
    siliconflow_en
);
native!(
    OpenRouter,
    "openrouter_balance",
    "api/v1/credits",
    openrouter
);
native!(Novita, "novita_balance", "v3/user/balance", novita);

pub(crate) fn balance_endpoint(base: &str) -> Option<(&'static str, String)> {
    let mut url = super::api::validate_base_url(base).ok()?;
    if url.scheme() != "https" || url.port_or_known_default() != Some(443) {
        return None;
    }
    let service = match url.host_str()? {
        "api.deepseek.com" => "deepseek_balance",
        "api.stepfun.com" => "stepfun_balance",
        "api.stepfun.ai" => "stepfun_en_balance",
        "api.moonshot.cn" => "moonshot_balance",
        "api.moonshot.ai" => "moonshot_en_balance",
        "api.siliconflow.cn" => "siliconflow_balance",
        "api.siliconflow.com" => "siliconflow_en_balance",
        "openrouter.ai" => "openrouter_balance",
        "api.novita.ai" => "novita_balance",
        _ => return None,
    };
    url.set_path("/");
    Some((service, url.to_string()))
}

pub(super) fn register(registry: &mut ResourceRegistry) {
    super::api::register::<Sub2Api>(registry);
    super::api::register::<Sub2ApiAccount>(registry);
    super::api::register::<CreditGrants>(registry);
    super::api::register::<DeepSeek>(registry);
    super::api::register::<StepFun>(registry);
    super::api::register::<StepFunEn>(registry);
    super::api::register::<Moonshot>(registry);
    super::api::register::<MoonshotEn>(registry);
    super::api::register::<SiliconFlow>(registry);
    super::api::register::<SiliconFlowEn>(registry);
    super::api::register::<OpenRouter>(registry);
    super::api::register::<Novita>(registry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{FakeHttp, HttpResponse};
    use crate::resources::connections::{ConnectionConfig, ResourceService};
    use serde_json::json;

    #[tokio::test]
    async fn sub2api_window_only_keys_preserve_amounts_and_resets_without_inventing_a_wallet() {
        let snapshot = read("sub2api_usage", "v1/usage", json!({"rate_limits":[
            {"window":"7d","limit":"800","used":"12.123456789012345678","reset_at":"2026-11-01T12:00:00Z"},
            {"window":"5h","limit":10,"used":"2.5","reset_at":null},
            {"window":"1d","limit":100,"used":"100.25"}
        ]})).await;
        assert_eq!(snapshot.state, ResourceState::Ready);
        assert_eq!(snapshot.scope, ResourceScope::Token);
        assert_eq!(snapshot.metrics.len(), 3);
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["metrics"][0]["kind"], "allowance_window");
        assert_eq!(json["metrics"][2]["used"], "12.123456789012345678");
        let output = crate::resources::connections::render_text(&[
            crate::resources::connections::ResourceReport {
                connection: crate::resources::connections::ConnectionDescriptor {
                    id: "fixture".into(),
                    label: "fixture".into(),
                    service: "sub2api_usage".into(),
                    profile_ids: Vec::new(),
                },
                snapshot: snapshot.clone(),
            },
        ]);
        assert!(output.contains("5 hours · 已用 2.5 / 10 USD · 剩余 7.5"));
        assert!(!output.contains("余额"));
        assert!(
            matches!(&snapshot.metrics[0], ResourceMetric::AllowanceWindow { name, used, remaining, unit, resets_at: None, .. }
            if name == "5 hours" && used == "2.5" && remaining == "7.5" && unit == "USD")
        );
        assert!(
            matches!(&snapshot.metrics[1], ResourceMetric::AllowanceWindow { remaining, .. } if remaining == "-0.25")
        );
        assert!(
            matches!(&snapshot.metrics[2], ResourceMetric::AllowanceWindow { used, resets_at: Some(_), .. } if used == "12.123456789012345678")
        );
        assert!(
            !snapshot
                .metrics
                .iter()
                .any(|m| matches!(m, ResourceMetric::Balance { .. }))
        );
        let combined = read(
            "sub2api_usage",
            "v1/usage",
            json!({"remaining":"42.5","rate_limits":[{"window":"5h","limit":10,"used":2}]}),
        )
        .await;
        assert_eq!(combined.metrics.len(), 2);
        for value in [
            json!({"rate_limits":[{"window":"5h","limit":10,"used":-1}]}),
            json!({"rate_limits":[{"window":"5h","limit":10,"used":1,"reset_at":"bad"}]}),
            json!({"rate_limits":[{"window":"5h","limit":10}]}),
            json!({"rate_limits":{}}),
            json!({"rate_limits":[{"window":"5h","limit":0,"used":0}]}),
            json!({"remaining":"-0.5","rate_limits":[{"window":"5h","limit":10,"used":1}]}),
            json!({"remaining":"-2","rate_limits":[{"window":"5h","limit":10,"used":1}]}),
        ] {
            let failed = read("sub2api_usage", "v1/usage", value).await;
            assert_ne!(failed.state, ResourceState::Ready);
            assert!(failed.metrics.is_empty());
        }
    }

    #[tokio::test]
    async fn account_wallets_and_legacy_key_balances_keep_their_credential_scopes() {
        for (service, path, body, scope, amount) in [
            (
                "sub2api_account",
                "api/v1/user/profile",
                json!({"code":0,"data":{"balance":"12.34567890123456789"}}),
                ResourceScope::Account,
                "12.34567890123456789",
            ),
            (
                "openai_credit_grants",
                "dashboard/billing/credit_grants",
                json!({"total_available":0}),
                ResourceScope::Token,
                "0",
            ),
        ] {
            let snapshot = read(service, path, body).await;
            assert_eq!(snapshot.state, ResourceState::Ready);
            assert_eq!(snapshot.scope, scope);
            assert!(snapshot.metrics.contains(&ResourceMetric::Balance {
                remaining: Money {
                    amount: amount.into(),
                    currency: "USD".into()
                }
            }));
            let failed = read(service, path, json!({"error":"private-token"})).await;
            assert_ne!(failed.state, ResourceState::Ready);
            assert!(failed.metrics.is_empty());
            assert!(!format!("{failed:?}").contains("private-token"));
        }
        let failed = read(
            "sub2api_account",
            "api/v1/user/profile",
            json!({"code":1,"data":{"balance":999}}),
        )
        .await;
        assert_ne!(failed.state, ResourceState::Ready);
        assert!(failed.metrics.is_empty());
    }

    async fn read(service: &str, path: &str, body: Value) -> ResourceSnapshot {
        let http = Arc::new(FakeHttp::new());
        http.route(
            format!("https://fixture.invalid/{path}"),
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: serde_json::to_vec(&body).unwrap(),
            }),
        );
        let config = ConnectionConfig {
            id: "work".into(),
            label: None,
            service: service.into(),
            base_url: "https://fixture.invalid".into(),
            credential_env: "FIXTURE_KEY".into(),
            revision: String::new(),
            options: json!({}),
            profile_ids: Vec::new(),
            inference: None,
        };
        ResourceService::new(
            Arc::new(builtin_registry()),
            http,
            Arc::new(|_| Ok("synthetic-only".into())),
            Arc::new(chrono::Utc::now),
        )
        .prepare(&config)
        .unwrap()
        .read()
        .await
    }

    #[tokio::test]
    async fn native_balances_preserve_decimal_currencies_and_do_not_turn_missing_data_into_zero() {
        for (service, path, body, amount, currency) in [
            (
                "deepseek_balance",
                "user/balance",
                json!({"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"12.34567890123456789"}]}),
                "12.34567890123456789",
                "CNY",
            ),
            (
                "stepfun_balance",
                "v1/accounts",
                json!({"balance":"1.23"}),
                "1.23",
                "CNY",
            ),
            (
                "stepfun_en_balance",
                "v1/accounts",
                json!({"balance":"1.23"}),
                "1.23",
                "USD",
            ),
            (
                "moonshot_balance",
                "v1/users/me/balance",
                json!({"code":0,"data":{"available_balance":"9.8"}}),
                "9.8",
                "CNY",
            ),
            (
                "moonshot_en_balance",
                "v1/users/me/balance",
                json!({"code":0,"data":{"available_balance":"9.8"}}),
                "9.8",
                "USD",
            ),
            (
                "siliconflow_balance",
                "v1/user/info",
                json!({"data":{"totalBalance":"8.77"}}),
                "8.77",
                "CNY",
            ),
            (
                "siliconflow_en_balance",
                "v1/user/info",
                json!({"data":{"totalBalance":"8.77"}}),
                "8.77",
                "USD",
            ),
            (
                "openrouter_balance",
                "api/v1/credits",
                json!({"data":{"total_credits":"100","total_usage":"105.25"}}),
                "-5.25",
                "USD",
            ),
            (
                "novita_balance",
                "v3/user/balance",
                json!({"availableBalance":"12345"}),
                "1.2345",
                "USD",
            ),
        ] {
            let snapshot = read(service, path, body).await;
            assert_eq!(snapshot.state, ResourceState::Ready, "{service}");
            assert!(snapshot.metrics.contains(&ResourceMetric::Balance {
                remaining: Money {
                    amount: amount.into(),
                    currency: currency.into()
                }
            }));
            let malformed = read(
                service,
                path,
                json!({"error":{"message":"synthetic-secret"}}),
            )
            .await;
            assert_ne!(malformed.state, ResourceState::Ready);
            assert!(malformed.metrics.is_empty());
            assert!(!format!("{malformed:?}").contains("synthetic-secret"));
        }
        assert!(balance_endpoint("https://api.deepseek.com.attacker.invalid").is_none());
        assert!(balance_endpoint("https://api.deepseek.com:444/v1").is_none());
    }

    #[tokio::test]
    async fn sub2api_distinguishes_wallet_balance_key_quota_and_unlimited_groups() {
        let snapshot = read(
            "sub2api_usage",
            "v1/usage",
            json!({"isValid":true,"remaining":12.5,"unit":"USD","quota":{"limit":20,"used":7.5}}),
        )
        .await;
        assert_eq!(snapshot.state, ResourceState::Ready);
        assert!(snapshot.metrics.contains(&ResourceMetric::Balance {
            remaining: Money {
                amount: "12.5".into(),
                currency: "USD".into()
            }
        }));
        assert!(snapshot.metrics.iter().any(|m|matches!(m,ResourceMetric::Allowance{remaining:Some(n),used:Some(u),unlimited:false,..} if n=="12.5" && u=="7.5")));
        let unlimited = read(
            "sub2api_usage",
            "v1/usage",
            json!({"remaining":-1,"unit":"USD"}),
        )
        .await;
        assert_eq!(unlimited.state, ResourceState::Ready);
        assert!(
            !unlimited
                .metrics
                .iter()
                .any(|m| matches!(m, ResourceMetric::Balance { .. }))
        );
        assert!(unlimited.metrics.iter().any(|m| matches!(
            m,
            ResourceMetric::Allowance {
                unlimited: true,
                ..
            }
        )));
        let default_currency = read("sub2api_usage", "v1/usage", json!({"remaining":10})).await;
        assert!(default_currency.metrics.contains(&ResourceMetric::Balance {
            remaining: Money {
                amount: "10".into(),
                currency: "USD".into()
            }
        }));
        let invalid = read(
            "sub2api_usage",
            "v1/usage",
            json!({"isValid":false,"remaining":9999,"unit":"USD"}),
        )
        .await;
        assert_ne!(invalid.state, ResourceState::Ready);
        assert!(invalid.metrics.is_empty());
    }
}
