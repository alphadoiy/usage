//! Optional, account-scoped provider operations. Factories capture references, never tokens.
use crate::registry::{FactoryRegistry, RegistryError};
use async_trait::async_trait;
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
pub struct Context {
    pub root: PathBuf,
    pub account: String,
    pub automatic_status: bool,
    pub day_offset_seconds: i32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Ready,
    Claimed,
    Done,
    Ineligible,
    Inactive,
    Captcha,
    OwnApp,
    Expired,
    Failed,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Report {
    pub outcome: Outcome,
    pub credit: Option<f64>,
    pub streak: Option<u32>,
    #[serde(default)]
    pub delayed: bool,
}
impl Report {
    pub fn message(&self) -> String {
        let text = match self.outcome {
            Outcome::Ready => "确认后执行今日签到",
            Outcome::Claimed if self.delayed => "签到成功，奖励稍后到账",
            Outcome::Claimed => "签到成功",
            Outcome::Done => "今日已签到，无需重复领取",
            Outcome::Ineligible => "该账号不符合签到活动条件",
            Outcome::Inactive => "当前没有进行中的签到活动",
            Outcome::Captcha => "签到需要验证，请在服务商客户端完成",
            Outcome::OwnApp => "签到需要服务商客户端的设备验证；请在客户端签到",
            Outcome::Expired => "服务商拒绝当前登录，请重新登录",
            Outcome::Failed => "签到请求未能确认结果，请重新查询今日状态后再操作",
        };
        let mut text = text.to_string();
        if let Some(credit) = self.credit.filter(|v| v.is_finite() && *v >= 0.0) {
            text.push_str(&format!(
                " · {} {credit:.2} Credits",
                if self.outcome == Outcome::Ready {
                    "预计奖励"
                } else {
                    "奖励"
                }
            ));
        }
        if let Some(streak) = self.streak {
            text.push_str(&format!(" · 连续 {streak} 天"));
        }
        text
    }
}
#[async_trait]
pub trait AccountOperation: Send + Sync {
    fn id(&self) -> &str;
    fn label(&self) -> &str;
    fn cache_key(&self) -> String;
    fn automatic_status(&self) -> bool {
        false
    }
    fn status_day(&self, now: chrono::DateTime<chrono::Utc>) -> chrono::NaiveDate {
        now.date_naive()
    }
    async fn preview(&self) -> Result<Report, String>;
    async fn execute(&self) -> Result<Report, String>;
}
#[derive(Clone)]
pub struct OperationHandle(pub Arc<dyn AccountOperation>);
impl std::fmt::Debug for OperationHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OperationHandle")
            .field(&self.0.id())
            .finish()
    }
}
struct PluginCheckin(Context);
impl PluginCheckin {
    async fn call(&self, phase: &str) -> Result<Report, String> {
        tracing::info!(event="provider_action_started", account=%self.0.account, action="checkin", phase);
        let result = crate::plugins::call_at(&self.0.root, serde_json::json!({"operation":"action", "action":"checkin", "phase":phase, "account":self.0.account})).await
            .and_then(|value|serde_json::from_value::<Report>(value).map_err(|_|"签到返回的数据格式不兼容".to_string()));
        match &result {
            Ok(report) => {
                tracing::info!(event="provider_action_completed", account=%self.0.account, action="checkin", phase, outcome=?report.outcome)
            }
            Err(_) => {
                tracing::warn!(event="provider_action_failed", account=%self.0.account, action="checkin", phase)
            }
        }
        result
    }
}
#[async_trait]
impl AccountOperation for PluginCheckin {
    fn id(&self) -> &str {
        "checkin"
    }
    fn label(&self) -> &str {
        "每日签到"
    }
    fn cache_key(&self) -> String {
        use sha2::Digest;
        format!(
            "checkin:{:x}",
            sha2::Sha256::digest(format!(
                "{}\0{}",
                self.0.root.to_string_lossy(),
                self.0.account
            ))
        )
    }
    fn automatic_status(&self) -> bool {
        self.0.automatic_status
    }
    fn status_day(&self, now: chrono::DateTime<chrono::Utc>) -> chrono::NaiveDate {
        (now + chrono::Duration::seconds(i64::from(self.0.day_offset_seconds))).date_naive()
    }
    async fn preview(&self) -> Result<Report, String> {
        self.call("preview").await
    }
    async fn execute(&self) -> Result<Report, String> {
        self.call("execute").await
    }
}
pub fn registry() -> FactoryRegistry<Context, OperationHandle> {
    let mut registry = FactoryRegistry::new();
    registry
        .register("plugin_checkin", |context: Context| {
            if context.account.is_empty() {
                return Err(RegistryError::Factory("账号操作缺少账号绑定".into()));
            }
            Ok(OperationHandle(Arc::new(PluginCheckin(context))))
        })
        .expect("unique provider action factory");
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn action_factories_are_lazy_and_results_do_not_invent_rewards() {
        let r = registry();
        let context = Context {
            root: "/nonexistent/action-fixture".into(),
            account: "fixture".into(),
            automatic_status: false,
            day_offset_seconds: 0,
        };
        let action = r.create("plugin_checkin", context).unwrap();
        assert_eq!(action.0.id(), "checkin");
        let report: Report = serde_json::from_value(
            serde_json::json!({"outcome":"done","message":"private-vendor-prose"}),
        )
        .unwrap();
        assert!(!report.message().contains("private-vendor-prose"));
        assert!(!report.message().contains("奖励"));
        assert!(
            serde_json::from_value::<Report>(serde_json::json!({"outcome":"unknown"})).is_err()
        );
    }
}
