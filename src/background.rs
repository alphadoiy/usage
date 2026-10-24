//! Owned background work with cooperative cancellation across async and blocking boundaries.
use std::future::Future;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    runtime::Handle,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
};
use tracing::Instrument;

#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn flag(&self) -> &AtomicBool {
        &self.0
    }
}

struct CancelOnDrop(Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// One admission queue and one execution limit for each kind of background work.
/// A dropped waiter releases its queue slot, and a running task releases its slot
/// when it finishes or is cancelled. Queue wait is outside the operation timeout.
struct Budget {
    running: Arc<Semaphore>,
    queue: Arc<Semaphore>,
}

const QUERY_CONCURRENCY: usize = 8;
const QUERY_QUEUE: usize = 64;
const BLOCKING_CONCURRENCY: usize = 4;
const BLOCKING_QUEUE: usize = 32;

impl Budget {
    fn new(running: usize, queued: usize) -> Self {
        Self {
            running: Arc::new(Semaphore::new(running)),
            queue: Arc::new(Semaphore::new(queued)),
        }
    }

    async fn admit(&self) -> Result<OwnedSemaphorePermit, &'static str> {
        let queued = self
            .queue
            .clone()
            .try_acquire_owned()
            .map_err(|_| "后台任务排队已满")?;
        let running = self
            .running
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "后台任务队列已关闭")?;
        drop(queued);
        Ok(running)
    }

    async fn run<T>(&self, work: impl Future<Output = T>) -> Result<T, &'static str> {
        let _permit = self.admit().await?;
        Ok(work.await)
    }
}

fn query_budget() -> &'static Budget {
    static BUDGET: OnceLock<Budget> = OnceLock::new();
    BUDGET.get_or_init(|| Budget::new(QUERY_CONCURRENCY, QUERY_QUEUE))
}

fn blocking_budget() -> &'static Budget {
    static BUDGET: OnceLock<Budget> = OnceLock::new();
    BUDGET.get_or_init(|| Budget::new(BLOCKING_CONCURRENCY, BLOCKING_QUEUE))
}

pub(crate) async fn query<T>(work: impl Future<Output = T>) -> Result<T, &'static str> {
    query_budget().run(work).await
}

pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, &'static str> {
    blocking_with(blocking_budget(), work).await
}

async fn blocking_with<T: Send + 'static>(
    budget: &Budget,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, &'static str> {
    let permit = budget.admit().await?;
    tokio::task::spawn_blocking(move || {
        // The worker owns the permit so aborting its async coordinator cannot
        // free capacity while non-interruptible disk work is still running.
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| "后台任务异常结束")
}

/// Keep this handle for as long as its result is needed. Disk work already running
/// must check the token: aborting a Tokio coordinator alone cannot interrupt it.
#[must_use]
pub(crate) struct Task {
    cancel: Cancellation,
    task: JoinHandle<()>,
}
impl Task {
    pub(crate) fn spawn<F: Future<Output = ()> + Send + 'static>(
        handle: &Handle,
        work: impl FnOnce(Cancellation) -> F + Send + 'static,
    ) -> Self {
        let cancel = Cancellation::new();
        let flag = cancel.clone();
        let task = handle.spawn(
            async move {
                let _cancel = CancelOnDrop(flag.clone());
                if !flag.is_cancelled() {
                    work(flag).await;
                }
            }
            .in_current_span(),
        );
        Self { cancel, task }
    }

    /// Async reads always produce a terminal result, including unwinding adapters.
    pub(crate) fn query<T: Send + 'static>(
        handle: &Handle,
        future: impl Future<Output = Result<T, String>> + Send + 'static,
        complete: impl FnOnce(Result<T, String>) + Send + 'static,
    ) -> Self {
        use futures_util::FutureExt;
        Self::spawn(handle, move |cancel| async move {
            let result = std::panic::AssertUnwindSafe(future)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err("后台任务异常结束".into()));
            if !cancel.is_cancelled() {
                complete(result);
            }
        })
    }

    /// Always report errors (including a panicked reader) while the owner is live.
    /// Suppress results when the owner has cancelled or been dropped.
    pub(crate) fn blocking<T: Send + 'static>(
        handle: &Handle,
        work: impl FnOnce(Cancellation) -> Result<T, String> + Send + 'static,
        complete: impl FnOnce(Result<T, String>) + Send + 'static,
    ) -> Self {
        Self::spawn(handle, move |cancel| async move {
            let flag = cancel.clone();
            let result = blocking(move || {
                if flag.is_cancelled() {
                    return Err("已取消".into());
                }
                work(flag)
            })
            .await
            .unwrap_or_else(|error| Err(error.into()));
            if !cancel.is_cancelled() {
                complete(result);
            }
        })
    }
    pub(crate) fn abort(&self) {
        self.cancel.cancel();
        self.task.abort();
    }
    pub(crate) fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
    #[cfg(test)]
    pub(crate) fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }
    #[cfg(test)]
    pub(crate) fn abort_handle(&self) -> tokio::task::AbortHandle {
        self.task.abort_handle()
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.abort();
    }
}

#[derive(Default)]
pub(crate) struct Tasks(Vec<Task>);
impl Tasks {
    pub(crate) fn push(&mut self, task: Task) {
        self.0.retain(|task| !task.is_finished());
        self.0.push(task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn budget_limits_running_and_waiting_work() {
        let budget = Budget::new(1, 1);
        let running = budget.admit().await.unwrap();
        let mut waiting = Box::pin(budget.admit());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                .await
                .is_err()
        );
        assert!(matches!(budget.admit().await, Err("后台任务排队已满")));
        drop(running);
        let admitted = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        drop(admitted);
    }

    #[tokio::test]
    async fn cancelled_waiter_frees_queue_slot() {
        let budget = Budget::new(1, 1);
        let running = budget.admit().await.unwrap();
        let mut waiting = Box::pin(budget.admit());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                .await
                .is_err()
        );
        drop(waiting);
        drop(running);
        assert!(budget.admit().await.is_ok());
    }

    #[tokio::test]
    async fn aborted_coordinator_keeps_blocking_worker_in_budget() {
        let budget = Arc::new(Budget::new(1, 1));
        let worker_budget = budget.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let coordinator = tokio::spawn(async move {
            blocking_with(&worker_budget, move || {
                let _ = started.send(());
                let _ = released.blocking_recv();
            })
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), ready)
            .await
            .unwrap()
            .unwrap();
        coordinator.abort();
        let mut next = Box::pin(budget.admit());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut next)
                .await
                .is_err()
        );
        let _ = release.send(());
        let _permit = tokio::time::timeout(Duration::from_secs(1), next)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn queued_work_is_cancelled_before_it_starts() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = Task::blocking(
            &Handle::current(),
            |_| -> Result<(), String> { panic!("must not start") },
            move |result| {
                let _ = tx.send(result);
            },
        );
        let token = task.cancellation();
        drop(task);
        assert!(token.is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_secs(2), rx)
                .await
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn panicking_reader_reports_a_safe_terminal_error() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _task = Task::blocking(
            &Handle::current(),
            |_| -> Result<(), String> { panic!("private-sentinel") },
            move |result| {
                let _ = tx.send(result);
            },
        );
        let result = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err(), "后台任务异常结束");
    }

    #[tokio::test]
    async fn dropping_task_cancels_running_blocking_work_and_suppresses_its_result() {
        let (started, ready) = tokio::sync::oneshot::channel();
        let (stopped, done) = tokio::sync::oneshot::channel();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = Task::blocking(
            &Handle::current(),
            move |token| {
                let _ = started.send(());
                let until = std::time::Instant::now() + Duration::from_secs(2);
                while !token.is_cancelled() && std::time::Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let _ = stopped.send(token.is_cancelled());
                Ok(())
            },
            move |result| {
                let _ = tx.send(result);
            },
        );
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        drop(task);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), done)
                .await
                .unwrap()
                .unwrap()
        );
        assert!(rx.await.is_err());
    }
}
