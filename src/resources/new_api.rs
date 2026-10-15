//! New API token quotas and account wallets are distinct credential scopes.
use super::api::{ApiClient, ApiProtocol, QueryFailure, QueryResult, invalid, number, options};
use super::*;
use crate::registry::RegistryError;
use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct QuotaOptions {
    quota_per_unit: Option<String>,
    currency: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountOptions {
    user_id: u64,
    #[serde(default)]
    quota_per_unit: Option<String>,
    #[serde(default)]
    currency: Option<String>,
}
fn validate_conversion(ratio: Option<&str>, currency: Option<&str>) -> Result<(), RegistryError> {
    match (ratio, currency) {
        (None, None) => Ok(()),
        (Some(ratio), Some(currency))
            if Decimal::from_str_exact(ratio).is_ok_and(|n| n > Decimal::ZERO)
                && currency.len() == 3
                && currency.bytes().all(|c| c.is_ascii_uppercase()) =>
        {
            Ok(())
        }
        _ => Err(RegistryError::Factory(
            "quota_per_unit 必须为正十进制字符串，并同时指定三位大写 currency".into(),
        )),
    }
}

pub(super) struct Token(QuotaOptions);
pub(super) struct Account(AccountOptions);
#[async_trait]
impl ApiProtocol for Token {
    const SERVICE: &'static str = "new_api_token";
    fn configure(value: &Value) -> Result<Self, RegistryError> {
        let opts: QuotaOptions = options(value)?;
        validate_conversion(opts.quota_per_unit.as_deref(), opts.currency.as_deref())?;
        Ok(Self(opts))
    }
    fn scope(&self) -> ResourceScope {
        ResourceScope::Token
    }
    async fn query(&self, client: &ApiClient, token: &str) -> QueryResult<Vec<ResourceMetric>> {
        let options = &self.0;
        let value = client
            .get(client.endpoint("api/usage/token"), token, &[])
            .await?;
        let data = new_api_data(&value)?;
        if data.get("object").and_then(Value::as_str) != Some("token_usage") {
            return Err(invalid());
        }
        if let Some(expires_at) = data.get("expires_at") {
            let expires_at = expires_at.as_i64().ok_or_else(invalid)?;
            if expires_at > 0 && expires_at <= (client.connection.now)().timestamp() {
                return Err(QueryFailure(
                    ResourceState::AuthenticationFailed,
                    "令牌已过期，查询结果不能代表可用额度",
                ));
            }
        }
        let unlimited = data
            .get("unlimited_quota")
            .and_then(Value::as_bool)
            .ok_or_else(invalid)?;
        let used = number(data.get("total_used"))?;
        let remaining = number(data.get("total_available"))?;
        quota_metrics(
            used,
            remaining,
            unlimited,
            options.quota_per_unit.as_deref(),
            options.currency.as_deref(),
        )
    }
}
#[async_trait]
impl ApiProtocol for Account {
    const SERVICE: &'static str = "new_api_account";
    fn configure(value: &Value) -> Result<Self, RegistryError> {
        let opts: AccountOptions = options(value)?;
        if opts.user_id == 0 {
            return Err(RegistryError::Factory("user_id 必须大于零".into()));
        }
        validate_conversion(opts.quota_per_unit.as_deref(), opts.currency.as_deref())?;
        Ok(Self(opts))
    }
    fn scope(&self) -> ResourceScope {
        ResourceScope::Account
    }
    async fn query(&self, client: &ApiClient, token: &str) -> QueryResult<Vec<ResourceMetric>> {
        let options = &self.0;
        let value = client
            .get(
                client.endpoint("api/user/self"),
                token,
                &[("New-Api-User", options.user_id.to_string())],
            )
            .await?;
        let data = new_api_data(&value)?;
        if data.get("id").and_then(Value::as_u64) != Some(options.user_id) {
            return Err(QueryFailure(
                ResourceState::AuthenticationFailed,
                "资源返回的账号与配置的 user_id 不匹配",
            ));
        }
        quota_metrics(
            number(data.get("used_quota"))?,
            number(data.get("quota"))?,
            false,
            options.quota_per_unit.as_deref(),
            options.currency.as_deref(),
        )
    }
}
fn new_api_data(value: &Value) -> QueryResult<&Value> {
    if value.get("success") == Some(&Value::Bool(false))
        || value.get("code") == Some(&Value::Bool(false))
    {
        return Err(QueryFailure(
            ResourceState::Failure,
            "资源服务拒绝查询，请检查凭据类型和权限",
        ));
    }
    if value.get("success") != Some(&Value::Bool(true))
        && value.get("code") != Some(&Value::Bool(true))
    {
        return Err(invalid());
    }
    value
        .get("data")
        .filter(|v| v.is_object())
        .ok_or_else(invalid)
}
fn quota_metrics(
    used: Decimal,
    remaining: Decimal,
    unlimited: bool,
    ratio: Option<&str>,
    currency: Option<&str>,
) -> QueryResult<Vec<ResourceMetric>> {
    if used < Decimal::ZERO {
        return Err(invalid());
    }
    let mut metrics = vec![ResourceMetric::Allowance {
        remaining: (!unlimited).then(|| remaining.normalize().to_string()),
        used: Some(used.normalize().to_string()),
        unit: "new_api_quota".into(),
        unlimited,
    }];
    if !unlimited && let (Some(ratio), Some(currency)) = (ratio, currency) {
        let amount = remaining
            .checked_div(Decimal::from_str_exact(ratio).map_err(|_| invalid())?)
            .ok_or_else(invalid)?;
        metrics.push(ResourceMetric::Balance {
            remaining: Money {
                amount: amount.normalize().to_string(),
                currency: currency.into(),
            },
        });
    }
    Ok(metrics)
}
