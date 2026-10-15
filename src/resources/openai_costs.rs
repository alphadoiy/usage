//! Official organization costs. An Admin API key is independent of inference credentials.
use super::api::{ApiClient, ApiProtocol, QueryFailure, QueryResult, invalid, number, options};
use super::*;
use crate::registry::RegistryError;
use async_trait::async_trait;
use chrono::Duration;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CostOptions {
    days: u32,
    project_id: Option<String>,
    api_key_id: Option<String>,
}
impl Default for CostOptions {
    fn default() -> Self {
        Self {
            days: 30,
            project_id: None,
            api_key_id: None,
        }
    }
}

pub(super) struct Costs(CostOptions);
#[async_trait]
impl ApiProtocol for Costs {
    const SERVICE: &'static str = "openai_costs";
    const AUTHENTICATION_HINT: &'static str =
        "费用查询认证失败或权限不足，需要 OpenAI Admin API Key";
    fn configure(value: &Value) -> Result<Self, RegistryError> {
        let opts: CostOptions = options(value)?;
        if !(1..=180).contains(&opts.days)
            || (opts.project_id.is_some() && opts.api_key_id.is_some())
            || [&opts.project_id, &opts.api_key_id]
                .into_iter()
                .flatten()
                .any(|id| id.is_empty() || id.chars().any(char::is_control))
        {
            return Err(RegistryError::Factory(
                "费用 days 需在 1–180 之间，project_id 与 api_key_id 最多指定一个且不能为空".into(),
            ));
        }
        Ok(Self(opts))
    }
    fn scope(&self) -> ResourceScope {
        if let Some(id) = &self.0.project_id {
            ResourceScope::Project { id: id.clone() }
        } else if let Some(id) = &self.0.api_key_id {
            ResourceScope::ApiKey { id: id.clone() }
        } else {
            ResourceScope::Organization
        }
    }
    async fn query(&self, client: &ApiClient, token: &str) -> QueryResult<Vec<ResourceMetric>> {
        let options = &self.0;
        let end = (client.connection.now)();
        let start = end - Duration::days(i64::from(options.days));
        let mut cursor: Option<String> = None;
        let mut cursors = HashSet::new();
        let mut totals: BTreeMap<String, Decimal> = BTreeMap::new();
        // Bound pagination even if a service returns a different cursor forever.
        for _ in 0..32 {
            let mut url = client.endpoint("organization/costs");
            {
                let mut query = url.query_pairs_mut();
                query
                    .append_pair("start_time", &start.timestamp().to_string())
                    .append_pair("end_time", &end.timestamp().to_string())
                    .append_pair("limit", "180");
                if let Some(project) = &options.project_id {
                    query.append_pair("project_ids", project);
                }
                if let Some(key) = &options.api_key_id {
                    query.append_pair("api_key_ids", key);
                }
                if let Some(page) = &cursor {
                    query.append_pair("page", page);
                }
            }
            let value = client.get(url, token, &[]).await?;
            if value.get("object").and_then(Value::as_str) != Some("page") {
                return Err(invalid());
            }
            let buckets = value
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(invalid)?;
            for bucket in buckets {
                let rows = bucket
                    .get("results")
                    .and_then(Value::as_array)
                    .ok_or_else(invalid)?;
                for row in rows {
                    if row.get("object").and_then(Value::as_str)
                        != Some("organization.costs.result")
                    {
                        return Err(invalid());
                    }
                    let amount = row.get("amount").ok_or_else(invalid)?;
                    let currency = amount
                        .get("currency")
                        .and_then(Value::as_str)
                        .filter(|s| s.len() == 3 && s.bytes().all(|c| c.is_ascii_alphabetic()))
                        .ok_or_else(invalid)?
                        .to_ascii_uppercase();
                    let total = totals.entry(currency).or_default();
                    *total = total
                        .checked_add(number(amount.get("value"))?)
                        .ok_or_else(invalid)?;
                }
            }
            match value.get("has_more").and_then(Value::as_bool) {
                Some(false) => {
                    return Ok(totals
                        .into_iter()
                        .map(|(currency, amount)| ResourceMetric::Cost {
                            used: Money {
                                amount: amount.normalize().to_string(),
                                currency,
                            },
                            period_start: start,
                            period_end: end,
                        })
                        .collect());
                }
                Some(true) => {
                    let next = value
                        .get("next_page")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(invalid)?;
                    if !cursors.insert(next.to_owned()) {
                        return Err(invalid());
                    }
                    cursor = Some(next.to_owned());
                }
                None => return Err(invalid()),
            }
        }
        Err(QueryFailure(
            ResourceState::Failure,
            "费用查询分页超过上限，未返回不完整的合计",
        ))
    }
}
