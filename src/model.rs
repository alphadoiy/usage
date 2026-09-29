//! 各提供方收成同一份用量快照，界面只依赖这份结构。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Valid,
    Expired,
    NotFound,
    ParseError,
}

/// 一个用量窗口。`utilization` 是已用百分比，范围 0–100。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaTier {
    pub name: String,
    pub utilization: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtraUsage {
    pub is_enabled: bool,
    pub monthly_limit: Option<f64>,
    pub used_credits: Option<f64>,
    pub utilization: Option<f64>,
    pub currency: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub tool: String,
    pub display_name: String,
    pub credential_status: CredentialStatus,
    pub success: bool,
    pub tiers: Vec<QuotaTier>,
    pub extra_usage: Option<ExtraUsage>,
    #[serde(default)]
    pub reset_credits_available: Option<u32>,
    pub error: Option<String>,
    pub credential_message: Option<String>,
    pub queried_at: Option<DateTime<Utc>>,
    /// 网络抖动。界面保留上一份成功结果。
    pub transient: bool,
}

impl UsageSnapshot {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.tool,
            "displayName": self.display_name,
            "credentialStatus": serde_json::to_value(self.credential_status).unwrap_or(Value::Null),
            "success": self.success,
            "transient": self.transient,
            "tiers": self.tiers.iter().map(|tier| json!({
                "name": tier.name,
                "utilization": tier.utilization,
                "resetsAt": tier.resets_at.map(iso),
            })).collect::<Vec<_>>(),
            "extraUsage": self.extra_usage.as_ref().map(|extra| json!({
                "isEnabled": extra.is_enabled,
                "monthlyLimit": extra.monthly_limit,
                "usedCredits": extra.used_credits,
                "utilization": extra.utilization,
                "currency": extra.currency,
            })),
            "resetCreditsAvailable": self.reset_credits_available,
            "error": self.error,
            "credentialMessage": self.credential_message,
            "queriedAt": self.queried_at.map(iso),
        })
    }
}

pub fn snapshot_not_found(
    tool: &str,
    display_name: &str,
    message: Option<String>,
) -> UsageSnapshot {
    UsageSnapshot {
        tool: tool.to_string(),
        display_name: display_name.to_string(),
        credential_status: CredentialStatus::NotFound,
        credential_message: message,
        success: false,
        tiers: Vec::new(),
        extra_usage: None,
        reset_credits_available: None,
        error: None,
        queried_at: None,
        transient: false,
    }
}

pub fn snapshot_error(
    tool: &str,
    display_name: &str,
    status: CredentialStatus,
    message: impl Into<String>,
    now: DateTime<Utc>,
) -> UsageSnapshot {
    let message = message.into();
    UsageSnapshot {
        tool: tool.to_string(),
        display_name: display_name.to_string(),
        credential_status: status,
        credential_message: Some(message.clone()),
        success: false,
        tiers: Vec::new(),
        extra_usage: None,
        reset_credits_available: None,
        error: Some(message),
        queried_at: Some(now),
        transient: false,
    }
}

pub fn snapshot_transient(
    tool: &str,
    display_name: &str,
    message: impl Into<String>,
) -> UsageSnapshot {
    UsageSnapshot {
        tool: tool.to_string(),
        display_name: display_name.to_string(),
        credential_status: CredentialStatus::Valid,
        success: false,
        tiers: Vec::new(),
        extra_usage: None,
        reset_credits_available: None,
        error: Some(message.into()),
        credential_message: None,
        queried_at: Some(Utc::now()),
        transient: true,
    }
}

pub fn snapshot_ok(
    tool: &str,
    display_name: &str,
    tiers: Vec<QuotaTier>,
    extra_usage: Option<ExtraUsage>,
    now: DateTime<Utc>,
) -> UsageSnapshot {
    UsageSnapshot {
        tool: tool.to_string(),
        display_name: display_name.to_string(),
        credential_status: CredentialStatus::Valid,
        success: true,
        tiers,
        extra_usage,
        reset_credits_available: None,
        error: None,
        credential_message: None,
        queried_at: Some(now),
        transient: false,
    }
}

fn iso(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
