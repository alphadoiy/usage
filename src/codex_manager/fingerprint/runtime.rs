//! Scheduling only: two independent account jobs, owned by the interactive app.
use super::*;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{runtime::Handle, task::JoinSet};

pub(crate) struct Runtime {
    cancel: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.task.abort();
    }
}
impl Runtime {
    pub fn start(handle: &Handle, service: Arc<FingerprintService>) -> Self {
        Self::with_interval(handle, service, Duration::from_secs(30))
    }
    fn with_interval(handle: &Handle, service: Arc<FingerprintService>, poll: Duration) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let task = handle.spawn(async move {
            let mut interval = tokio::time::interval(poll);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut tasks: JoinSet<Result<Option<Report>>> = JoinSet::new();
            let mut accounts = HashMap::new();
            let mut queue = VecDeque::new();
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let service = service.clone();
                        match tokio::task::spawn_blocking(move || service.jobs()).await {
                            Ok(Ok(jobs)) => { queue = jobs.into(); },
                            _ => tracing::warn!(event="fingerprint_schedule_failed", reason="account_discovery_failed"),
                        }
                    },
                    Some(result) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                        let (id, failed) = match result { Ok((id, result)) => (id, result.is_err()), Err(error) => (error.id(), true) };
                        if let Some(account) = accounts.remove(&id) && failed {
                            tracing::warn!(event="fingerprint_schedule_failed", provider_id=account, reason="account_test_failed");
                        }
                    },
                }
                let active: HashSet<_> = accounts.values().cloned().collect();
                while tasks.len() < 2 {
                    let Some(id) = queue.pop_front() else { break; };
                    if active.contains(&id) { continue; }
                    let service = service.clone();
                    let cancel = worker_cancel.clone();
                    let account = id.clone();
                    let task = tasks.spawn_blocking(move || service.run_scheduled(&account, &cancel));
                    accounts.insert(task.id(), id);
                }
            }
        });
        Self { cancel, task }
    }
}

#[cfg(all(test, unix))]
mod tests;
