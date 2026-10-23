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
        if value.get("isValid") == Some(&Value::Bool(false)) {
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
            .unwrap_or("sub2api_quota");
        if unit.is_empty() || unit.len() > 32 || unit.chars().any(char::is_control) {
            return Err(invalid());
        }
        let mut metrics = Vec::new();
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
                } else if metrics.is_empty() {
                    metrics.push(ResourceMetric::Allowance {
                        remaining: Some(remaining.normalize().to_string()),
                        used: None,
                        unit: unit.into(),
                        unlimited: false,
                    });
                }
            } else if remaining == Decimal::NEGATIVE_ONE && metrics.is_empty() {
                metrics.push(ResourceMetric::Allowance {
                    remaining: None,
                    used: None,
                    unit: unit.into(),
                    unlimited: true,
                });
            } else if remaining < Decimal::NEGATIVE_ONE {
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
        "api.stepfun.com" | "api.stepfun.ai" => "stepfun_balance",
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
    super::api::register::<DeepSeek>(registry);
    super::api::register::<StepFun>(registry);
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
        let raw = read("sub2api_usage", "v1/usage", json!({"remaining":10})).await;
        assert!(
            !raw.metrics
                .iter()
                .any(|m| matches!(m, ResourceMetric::Balance { .. }))
        );
        assert!(
            raw.metrics
                .iter()
                .any(|m| matches!(m,ResourceMetric::Allowance{unit,..} if unit=="sub2api_quota"))
        );
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
