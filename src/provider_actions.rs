//! Optional, account-scoped provider operations. Factories capture references, never tokens.
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;

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
