//! Background loading policy shared by consumers; no terminal or UI state lives here.
use super::{Binding, Period, Service, Snapshot};
use crate::background::{Cancellation, Task};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

/// One immutable query, including its time window, across all import batches.
pub struct LoadRequest {
    pub binding: Binding,
    pub period: Period,
    pub model: Option<String>,
    pub page: u32,
    pub now: i64,
    pub rediscover: bool,
}

pub struct LoadUpdate {
    pub result: Result<Snapshot, String>,
    pub loading: bool,
}

/// Owns both the async coordinator and cancellation of an already-running disk batch.
/// Dropping a plain JoinHandle would detach it, and abort alone cannot stop spawn_blocking.
pub struct LoadTask {
    task: Task,
}
impl LoadTask {
    pub fn cancel(&self) {
        self.task.abort();
    }
    #[cfg(test)]
    pub(crate) fn cancellation(&self) -> Cancellation {
        self.task.cancellation()
    }
}

impl Service {
    /// Publish cached data before indexing, then throttled progress and a final result.
    /// Returning false from `publish` closes the consumer and stops further work.
    pub fn start_load(
        self: &Arc<Self>,
        handle: &Handle,
        request: LoadRequest,
        publish: impl FnMut(LoadUpdate) -> bool + Send + 'static,
    ) -> LoadTask {
        let service = self.clone();
        let task = Task::spawn(handle, move |cancel| async move {
            load(service, request, cancel, publish).await;
        });
        LoadTask { task }
    }
}

async fn snapshot(
    service: Arc<Service>,
    request: &LoadRequest,
    cancel: Cancellation,
) -> Result<Snapshot, String> {
    let (binding, period, model, page, now) = (
        request.binding.clone(),
        request.period,
        request.model.clone(),
        request.page,
        request.now,
    );
    tokio::task::spawn_blocking(move || {
        if cancel.is_cancelled() {
            return Err("已取消".into());
        }
        service.snapshot(&binding, period, model.as_deref(), page, now)
    })
    .await
    .unwrap_or_else(|_| Err("性能索引读取中断".into()))
}

async fn load(
    service: Arc<Service>,
    request: LoadRequest,
    cancel: Cancellation,
    mut publish: impl FnMut(LoadUpdate) -> bool,
) {
    let cached = snapshot(service.clone(), &request, cancel.clone()).await;
    if cancel.is_cancelled()
        || !publish(LoadUpdate {
            result: cached,
            loading: true,
        })
    {
        return;
    }
    if request.rediscover {
        service.rediscover(&request.binding);
    }
    let (mut bytes, mut files, mut skipped) = (0, 0, 0);
    let mut reported = Instant::now();
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let (svc, binding, flag) = (service.clone(), request.binding.clone(), cancel.clone());
        let batch =
            tokio::task::spawn_blocking(move || svc.refresh(&binding, binding.source, flag.flag()))
                .await
                .unwrap_or_else(|_| Err("性能采集任务中断".into()));
        // A cancelled blocking batch may still return an error. It must not publish late data.
        if cancel.is_cancelled() {
            return;
        }
        let batch = match batch {
            Ok(batch) => batch,
            Err(error) => {
                publish(LoadUpdate {
                    result: Err(error),
                    loading: false,
                });
                return;
            }
        };
        bytes += batch.bytes;
        files += batch.files;
        skipped += batch.skipped;
        if !batch.more || reported.elapsed() >= Duration::from_millis(250) {
            let mut result = snapshot(service.clone(), &request, cancel.clone()).await;
            if cancel.is_cancelled() {
                return;
            }
            if let Ok(snapshot) = &mut result {
                snapshot.read_bytes = bytes;
                snapshot.files_checked = files;
                snapshot.skipped_lines = skipped;
                snapshot.partial = batch.more;
            }
            let loading = batch.more && result.is_ok();
            if !publish(LoadUpdate { result, loading }) || !loading {
                return;
            }
            reported = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
