use super::*;
use crate::resources::*;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

enum Reading {
    Quotas,
    Usage,
}

/// One query per resource and gateway credential scope, shared by its launch presets.
pub(crate) struct GatewayReader {
    connection: Connection,
    http: Arc<dyn HttpClient>,
    binding: ResourceBinding,
    reading: Reading,
}
impl GatewayReader {
    pub fn quotas(connection: Connection, http: Arc<dyn HttpClient>) -> Self {
        Self::new(connection, http, Reading::Quotas)
    }
    pub fn usage(connection: Connection, http: Arc<dyn HttpClient>) -> Self {
        Self::new(connection, http, Reading::Usage)
    }
    fn new(connection: Connection, http: Arc<dyn HttpClient>, reading: Reading) -> Self {
        let binding = ResourceBinding {
            account: connection.identity(),
            revision: "magpie-v1".into(),
        };
        Self {
            connection,
            http,
            binding,
            reading,
        }
    }
}
#[async_trait]
impl ResourceReader for GatewayReader {
    fn service(&self) -> &str {
        match self.reading {
            Reading::Quotas => "magpie_quotas",
            Reading::Usage => "magpie_usage",
        }
    }
    fn binding(&self) -> &ResourceBinding {
        &self.binding
    }
    fn scope(&self) -> ResourceScope {
        ResourceScope::Unspecified
    }
    async fn read(&self) -> ResourceSnapshot {
        let mut snapshot = ResourceSnapshot {
            binding: self.binding.clone(),
            service: self.service().into(),
            scope: self.scope(),
            state: ResourceState::Ready,
            metrics: Vec::new(),
            queried_at: Some(Utc::now()),
            message: None,
        };
        // Keep these separate: restricted keys or an older gateway may expose only one API.
        let result = match self.reading {
            Reading::Quotas => self
                .connection
                .get(self.http.as_ref(), "v1/magpie/quotas", &[])
                .await
                .and_then(|v| quota_metrics(&v)),
            Reading::Usage => self
                .connection
                .get(
                    self.http.as_ref(),
                    "v1/magpie/usage",
                    &[("period", "today")],
                )
                .await
                .and_then(|v| usage_metrics(&v)),
        };
        match result {
            Ok((metrics, messages)) => {
                snapshot.metrics = metrics;
                snapshot.message = (!messages.is_empty()).then(|| messages.join(" · "));
            }
            Err(error) => {
                snapshot.state = ResourceState::TransientFailure;
                snapshot.message = Some(error);
            }
        }
        snapshot
    }
}
pub(super) fn quota_metrics(value: &Value) -> Result<(Vec<ResourceMetric>, Vec<String>)> {
    let rows = value["data"].as_array().ok_or("Magpie 额度目录格式无效")?;
    let mut metrics = Vec::new();
    let mut notes = Vec::new();
    for row in rows {
        let label = [
            row["name"].as_str().unwrap_or("Magpie"),
            row["user"].as_str().unwrap_or(""),
        ]
        .join(" ");
        // A vendor's error or deferred read is not a zero quota.
        if row["error"].as_str().is_some_and(|v| !v.is_empty()) {
            notes.push(format!("{label}：额度读取失败，请在 Magpie 查看"));
            continue;
        }
        if row["readDeferred"] == true && row["readAt"].is_null() && row["asOf"].is_null() {
            notes.push(format!("{label}：额度尚未读取"));
            continue;
        }
        if let Some(windows) = row["windows"].as_array() {
            for w in windows {
                let name = format!("{} · {}", label, w["name"].as_str().unwrap_or("额度"));
                let resets_at = w["resetsAt"]
                    .as_str()
                    .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
                    .map(|v| v.with_timezone(&Utc));
                let unit = w["unit"].as_str().unwrap_or("");
                if w["unlimited"] == true {
                    metrics.push(ResourceMetric::Allowance {
                        remaining: None,
                        used: None,
                        unit: name,
                        unlimited: true,
                    });
                } else if w["text"] == true {
                    if let Some(text) = w["display"].as_str() {
                        notes.push(format!("{name}：{text}"));
                    }
                } else if let Some(balance) = decimal(w.get("balance")) {
                    metrics.push(ResourceMetric::Allowance {
                        remaining: Some(balance),
                        used: None,
                        unit: format!("{name} {unit}"),
                        unlimited: false,
                    });
                } else if !unit.is_empty()
                    && let (Some(used), Some(limit)) = (
                        decimal(w.get("amount")).or_else(|| {
                            // Magpie omits its numeric amount field when it is zero, while
                            // its used percentage is always present. Missing usage stays unknown.
                            (w["used"].as_f64() == Some(0.0)).then(|| "0".into())
                        }),
                        decimal(w.get("limit")),
                    )
                {
                    if let (Ok(u), Ok(l)) = (
                        used.parse::<rust_decimal::Decimal>(),
                        limit.parse::<rust_decimal::Decimal>(),
                    ) {
                        metrics.push(ResourceMetric::AllowanceWindow {
                            name,
                            used,
                            limit,
                            remaining: (l - u).to_string(),
                            unit: unit.into(),
                            resets_at,
                        });
                    }
                } else if let Some(used) = w["used"].as_f64().filter(|v| v.is_finite() && *v >= 0.0)
                {
                    metrics.push(ResourceMetric::QuotaWindow {
                        name,
                        used_percent: used,
                        resets_at,
                    });
                }
            }
        }
        // Balance strings may contain credits, currencies or vendor prose. Preserve their unit.
        if let Some(balance) = row["balance"].as_str().filter(|v| !v.is_empty()) {
            metrics.push(ResourceMetric::ReportedBalance {
                name: label.trim().into(),
                display: balance.into(),
            });
        }
    }
    if rows.is_empty() {
        notes.push("Magpie 尚无可查询额度的账号".into());
    }
    Ok((metrics, notes))
}

pub(super) fn usage_metrics(value: &Value) -> Result<(Vec<ResourceMetric>, Vec<String>)> {
    #[derive(Deserialize)]
    struct Totals {
        period: String,
        calls: u64,
        errors: u64,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        reasoning: u64,
        unpriced: u64,
    }
    let totals: Totals =
        serde_json::from_value(value.clone()).map_err(|_| "Magpie 用量汇总格式无效")?;
    if totals.period != "today" || totals.errors > totals.calls || totals.unpriced > totals.calls {
        return Err("Magpie 用量汇总格式无效".into());
    }
    let mut notes =
        vec!["范围：Magpie 全部记录，可能包含其他客户端和本地会话；非当前预设独占".into()];
    if let Some(cost) = decimal(value.get("cost"))
        && cost
            .parse::<rust_decimal::Decimal>()
            .is_ok_and(|v| v >= rust_decimal::Decimal::ZERO)
    {
        notes.push(format!(
            "今日估算费用 {cost} USD（已定价部分，非账号实际扣费）"
        ));
    }
    if totals.unpriced > 0 {
        notes.push(format!("{} 次请求未定价，未计入估算费用", totals.unpriced));
    }
    Ok((
        vec![ResourceMetric::RequestUsage {
            name: "今日".into(),
            requests: totals.calls,
            failed_requests: totals.errors,
            input_tokens: totals.input,
            output_tokens: totals.output,
            cache_read_tokens: totals.cache_read,
            cache_write_tokens: totals.cache_write,
            reasoning_tokens: totals.reasoning,
        }],
        notes,
    ))
}
fn decimal(value: Option<&Value>) -> Option<String> {
    let v = value?;
    if v.is_number() {
        Some(v.to_string())
    } else {
        v.as_str()
            .filter(|s| s.parse::<rust_decimal::Decimal>().is_ok())
            .map(str::to_owned)
    }
}
