//! Account-keyed, read-only status queries for the visible Provider. No execution calls here.
use super::*;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
type Reply = (String, u64, Result<Report, String>);
struct Entry {
    at: DateTime<Utc>,
    day: chrono::NaiveDate,
    epoch: u64,
    result: Option<Result<Report, String>>,
    task: Option<crate::background::Task>,
}
pub(crate) struct Statuses {
    entries: HashMap<String, Entry>,
    tx: mpsc::Sender<Reply>,
    rx: mpsc::Receiver<Reply>,
    epoch: u64,
}
impl Default for Statuses {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            entries: HashMap::new(),
            tx,
            rx,
            epoch: 0,
        }
    }
}
fn fresh(entry: &Entry, operation: &OperationHandle, now: DateTime<Utc>) -> bool {
    if entry.day != operation.0.status_day(now) || now < entry.at {
        return false;
    }
    let ttl = match &entry.result {
        Some(Ok(report)) if matches!(report.outcome, Outcome::Failed | Outcome::Expired) => 60,
        Some(Err(_)) => 60,
        Some(Ok(report)) if matches!(report.outcome, Outcome::Done | Outcome::Claimed) => 1800,
        _ => 300,
    };
    now - entry.at < chrono::Duration::seconds(if entry.task.is_some() { 35 } else { ttl })
}
impl Statuses {
    pub fn ensure(
        &mut self,
        operation: &OperationHandle,
        now: DateTime<Utc>,
        handle: &Handle,
    ) -> bool {
        if !operation.0.automatic_status() {
            return false;
        }
        let key = operation.0.cache_key();
        if self
            .entries
            .get(&key)
            .is_some_and(|entry| fresh(entry, operation, now))
        {
            return false;
        }
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        let tx = self.tx.clone();
        let action = operation.clone();
        let target = key.clone();
        let task = crate::background::Task::query(
            handle,
            async move {
                tokio::time::timeout(Duration::from_secs(30), action.0.preview())
                    .await
                    .unwrap_or_else(|_| Err("签到状态查询超时".into()))
            },
            move |result| {
                let _ = tx.send((target, epoch, result));
            },
        );
        self.entries.insert(
            key,
            Entry {
                at: now,
                day: operation.0.status_day(now),
                epoch,
                result: None,
                task: Some(task),
            },
        );
        true
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok((key, epoch, result)) = self.rx.try_recv() {
            if let Some(entry) = self.entries.get_mut(&key)
                && entry.epoch == epoch
            {
                entry.task = None;
                entry.result = Some(result);
                changed = true;
            }
        }
        changed
    }
    pub fn record(
        &mut self,
        operation: &OperationHandle,
        at: DateTime<Utc>,
        result: Result<Report, String>,
    ) {
        self.epoch = self.epoch.wrapping_add(1);
        self.entries.insert(
            operation.0.cache_key(),
            Entry {
                at,
                day: operation.0.status_day(at),
                epoch: self.epoch,
                result: Some(result),
                task: None,
            },
        );
    }
    pub fn invalidate(&mut self, operation: &OperationHandle) {
        self.entries.remove(&operation.0.cache_key());
    }
    pub fn retain(&mut self, operations: impl Iterator<Item = OperationHandle>) {
        let keys: std::collections::HashSet<_> = operations.map(|op| op.0.cache_key()).collect();
        self.entries.retain(|key, _| keys.contains(key));
    }
    pub fn summary(&self, operation: &OperationHandle, now: DateTime<Utc>) -> String {
        let Some(entry) = self.entries.get(&operation.0.cache_key()) else {
            return if operation.0.automatic_status() {
                "签到：正在查询今日状态…".into()
            } else {
                "签到：c 手动操作".into()
            };
        };
        if entry.day != operation.0.status_day(now) {
            return if operation.0.automatic_status() {
                "签到：正在更新今日状态…".into()
            } else {
                "签到：c 手动操作".into()
            };
        }
        match &entry.result {
            None => "签到：正在查询今日状态…".into(),
            Some(Err(_)) => "签到：状态查询失败 · c 重试".into(),
            Some(Ok(report)) => match report.outcome {
                Outcome::Ready => "签到：今日未签到 · c 领取奖励".into(),
                Outcome::Done | Outcome::Claimed => "签到：今日已签到".into(),
                Outcome::Failed => "签到：状态查询失败 · c 重试".into(),
                _ => format!("签到：{}", report.message()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_actions::AccountOperation;
    use async_trait::async_trait;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct ReadOnly {
        key: &'static str,
        reads: Arc<AtomicUsize>,
        auto: bool,
    }
    #[async_trait]
    impl AccountOperation for ReadOnly {
        fn id(&self) -> &str {
            "checkin"
        }
        fn label(&self) -> &str {
            "签到"
        }
        fn cache_key(&self) -> String {
            self.key.into()
        }
        fn automatic_status(&self) -> bool {
            self.auto
        }
        fn status_day(&self, now: DateTime<Utc>) -> chrono::NaiveDate {
            (now + chrono::Duration::hours(8)).date_naive()
        }
        async fn preview(&self) -> Result<Report, String> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(report(Outcome::Ready))
        }
        async fn execute(&self) -> Result<Report, String> {
            panic!("automatic status must never execute an action")
        }
    }
    fn report(outcome: Outcome) -> Report {
        Report {
            outcome,
            credit: None,
            streak: None,
            delayed: false,
        }
    }
    fn time() -> DateTime<Utc> {
        "2026-10-18T10:00:00Z".parse().unwrap()
    }
    fn operation(key: &'static str, reads: Arc<AtomicUsize>) -> OperationHandle {
        OperationHandle(Arc::new(ReadOnly {
            key,
            reads,
            auto: true,
        }))
    }
    async fn poll(statuses: &mut Statuses) {
        for _ in 0..200 {
            if statuses.poll() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("status query did not finish");
    }
    #[tokio::test]
    async fn visible_cards_sharing_an_account_coalesce_and_respect_cache_expiry() {
        let reads = Arc::new(AtomicUsize::new(0));
        let action = operation("account-a", reads.clone());
        let mut statuses = Statuses::default();
        let now = time();
        assert!(statuses.ensure(&action, now, &Handle::current()));
        assert!(!statuses.ensure(&action.clone(), now, &Handle::current()));
        poll(&mut statuses).await;
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert!(statuses.summary(&action, now).contains("今日未签到"));
        assert!(!statuses.ensure(
            &action,
            now + chrono::Duration::seconds(299),
            &Handle::current()
        ));
        assert!(statuses.ensure(
            &action,
            now + chrono::Duration::seconds(300),
            &Handle::current()
        ));
        poll(&mut statuses).await;
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn beijing_midnight_and_failed_queries_get_fresh_readonly_status() {
        let action = operation("account-a", Arc::new(AtomicUsize::new(0)));
        let mut statuses = Statuses::default();
        let before = "2026-10-18T15:59:59Z".parse().unwrap();
        statuses.record(&action, before, Ok(report(Outcome::Done)));
        assert!(statuses.ensure(
            &action,
            before + chrono::Duration::seconds(2),
            &Handle::current()
        ));
        poll(&mut statuses).await;
        let now = time();
        statuses.record(&action, now, Err("synthetic failure".into()));
        assert!(!statuses.ensure(
            &action,
            now + chrono::Duration::seconds(59),
            &Handle::current()
        ));
        assert!(statuses.ensure(
            &action,
            now + chrono::Duration::seconds(60),
            &Handle::current()
        ));
    }
    #[test]
    fn late_background_results_cannot_overwrite_manual_success_or_other_accounts() {
        let reads = Arc::new(AtomicUsize::new(0));
        let a = operation("account-a", reads.clone());
        let b = operation("account-b", reads);
        let mut statuses = Statuses::default();
        let now = time();
        statuses.record(&a, now, Ok(report(Outcome::Ready)));
        let old = statuses.entries["account-a"].epoch;
        statuses.record(&a, now, Ok(report(Outcome::Claimed)));
        statuses.record(&b, now, Ok(report(Outcome::Ready)));
        statuses
            .tx
            .send(("account-a".into(), old, Ok(report(Outcome::Ready))))
            .unwrap();
        assert!(!statuses.poll());
        assert!(statuses.summary(&a, now).contains("今日已签到"));
        assert!(statuses.summary(&b, now).contains("今日未签到"));
    }
    #[tokio::test]
    async fn hooks_without_explicit_readonly_status_are_never_queried_automatically() {
        let reads = Arc::new(AtomicUsize::new(0));
        let action = OperationHandle(Arc::new(ReadOnly {
            key: "manual",
            reads: reads.clone(),
            auto: false,
        }));
        let mut statuses = Statuses::default();
        assert!(!statuses.ensure(&action, time(), &Handle::current()));
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(statuses.summary(&action, time()).contains("手动操作"));
        statuses.record(&action, time(), Ok(report(Outcome::Claimed)));
        assert!(
            statuses
                .summary(&action, time() + chrono::Duration::days(1))
                .contains("手动操作")
        );
    }
    #[tokio::test]
    async fn a_panicking_status_query_finishes_and_uses_failure_retry_ttl() {
        // A dedicated automatically-queryable adapter delegates to the panic fixture.
        struct Automatic;
        #[async_trait]
        impl AccountOperation for Automatic {
            fn id(&self) -> &str {
                "checkin"
            }
            fn label(&self) -> &str {
                "签到"
            }
            fn cache_key(&self) -> String {
                "panic-status".into()
            }
            fn automatic_status(&self) -> bool {
                true
            }
            async fn preview(&self) -> Result<Report, String> {
                panic!("private-status-sentinel")
            }
            async fn execute(&self) -> Result<Report, String> {
                panic!("never execute")
            }
        }
        let operation = OperationHandle(Arc::new(Automatic));
        let mut statuses = Statuses::default();
        assert!(statuses.ensure(&operation, time(), &Handle::current()));
        poll(&mut statuses).await;
        assert!(statuses.summary(&operation, time()).contains("查询失败"));
        assert!(statuses.entries["panic-status"].task.is_none());
        assert!(!statuses.ensure(
            &operation,
            time() + chrono::Duration::seconds(59),
            &Handle::current()
        ));
        assert!(statuses.ensure(
            &operation,
            time() + chrono::Duration::seconds(60),
            &Handle::current()
        ));
        let cancel = statuses.entries["panic-status"]
            .task
            .as_ref()
            .unwrap()
            .cancellation();
        statuses.invalidate(&operation);
        assert!(cancel.is_cancelled());
    }
}
