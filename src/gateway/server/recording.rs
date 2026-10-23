//! Lossy request samples and reliable, bounded source selection use separate paths.
use super::*;
use std::sync::Mutex;
use tokio::sync::Notify;

mod recovery;

pub(super) type Sample = (Arc<Source>, String, Record);
type Key = (String, String);

pub(super) struct Source {
    key: Key,
    fallback: AtomicBool,
    persisted: AtomicBool,
    wake: Arc<Notify>,
}
impl Source {
    pub fn fallback(&self) {
        if !self.fallback.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                event = "gateway_cli_source_selected",
                scope = self.key.0,
                run_id = self.key.1
            );
            self.wake.notify_one();
        }
    }
    fn pending(&self) -> bool {
        self.fallback.load(Ordering::Acquire) && !self.persisted.load(Ordering::Acquire)
    }
    #[cfg(test)]
    pub fn is_fallback(&self) -> bool {
        self.fallback.load(Ordering::Acquire)
    }
}
#[derive(Default)]
pub(super) struct Sources {
    entries: Mutex<HashMap<Key, Arc<Source>>>,
    wake: Arc<Notify>,
}
impl Sources {
    pub fn reserve(&self, scope: &str, run: &str) -> Option<Arc<Source>> {
        let mut entries = self.entries.lock().expect("source registry");
        let key = (scope.to_owned(), run.to_owned());
        if let Some(source) = entries.get(&key) {
            return Some(source.clone());
        }
        // Include retired runs whose source decision has not yet reached storage.
        if entries.len() >= 256 {
            return None;
        }
        let source = Arc::new(Source {
            key: key.clone(),
            fallback: AtomicBool::new(false),
            persisted: AtomicBool::new(false),
            wake: self.wake.clone(),
        });
        entries.insert(key, source.clone());
        Some(source)
    }
    #[cfg(test)]
    fn get(&self, scope: &str, run: &str) -> Option<Arc<Source>> {
        self.entries
            .lock()
            .expect("source registry")
            .get(&(scope.to_owned(), run.to_owned()))
            .cloned()
    }
    // Routes, registrations, queued samples, and the writer hold references.
    // Unowned states are safe to release only after their source is persisted.
    fn collect(&self) {
        self.entries
            .lock()
            .expect("source registry")
            .retain(|_, source| Arc::strong_count(source) > 1 || source.pending());
    }
    fn pending(&self) -> Vec<Arc<Source>> {
        self.entries
            .lock()
            .expect("source registry")
            .values()
            .filter(|s| s.pending())
            .cloned()
            .collect()
    }
    async fn flush(&self, service: Arc<crate::performance::Service>) -> bool {
        let pending = self.pending();
        if pending.is_empty() {
            return true;
        }
        let keys: Vec<_> = pending.iter().map(|s| s.key.clone()).collect();
        let result =
            tokio::task::spawn_blocking(move || service.mark_gateway_cli_sources(&keys)).await;
        if matches!(result, Ok(Ok(()))) {
            for source in pending {
                source.persisted.store(true, Ordering::Release);
            }
            true
        } else {
            tracing::error!(
                event = "gateway_cli_source_write_failed",
                runs = pending.len(),
                retry = true
            );
            false
        }
    }
}

pub(super) async fn write(
    mut receiver: mpsc::Receiver<Sample>,
    sources: Arc<Sources>,
    service: Arc<crate::performance::Service>,
    directory: PathBuf,
) {
    let recovery = recovery::Recovery::new(&directory);
    let mut retry = tokio::time::interval(Duration::from_secs(1));
    let mut closing = None;
    let mut storage_failed = false;
    loop {
        if receiver.is_closed() && closing.is_none() {
            closing = Some(tokio::time::Instant::now());
        }
        if closing.is_some_and(|started| {
            receiver.is_empty() || storage_failed || started.elapsed() >= Duration::from_millis(250)
        }) {
            // One final source transaction, never one blocked DB write per queued sample.
            // CLI selection covers all queued samples, including runs with earlier saved samples.
            let mut dropped = 0;
            while let Ok((source, _, _)) = receiver.try_recv() {
                source.fallback();
                dropped += 1;
            }
            if dropped > 0 {
                tracing::warn!(
                    event = "gateway_shutdown_samples_deferred",
                    samples = dropped
                );
            }
            sources.flush(service.clone()).await;
            let pending: Vec<_> = sources.pending().iter().map(|s| s.key.clone()).collect();
            if !pending.is_empty() {
                let count = pending.len();
                match tokio::task::spawn_blocking(move || recovery.save(pending)).await {
                    Ok(Ok(())) => {
                        tracing::warn!(event = "gateway_cli_sources_deferred", runs = count)
                    }
                    result => tracing::error!(
                        event = "gateway_cli_sources_recovery_save_failed",
                        runs = count,
                        reason = result
                            .ok()
                            .and_then(Result::err)
                            .unwrap_or("recovery_task_failed"),
                        action = "release_gateway_lock"
                    ),
                }
            }
            break;
        }
        sources.flush(service.clone()).await;
        sources.collect();
        tokio::select! {
            sample = receiver.recv() => {
                let Some((source, upstream, record)) = sample else {
                    continue;
                };
                // The queued sample owns its source even after the route expires.
                let scope = source.key.0.clone();
                let request_id = record.id.clone();
                let run_id = record.run.clone();
                let service = service.clone();
                let result = tokio::task::spawn_blocking(move || service.record_gateway(&scope, &upstream, &record)).await;
                storage_failed = !matches!(result, Ok(Ok(())));
                if storage_failed {
                    source.fallback();
                    tracing::error!(event = "gateway_record_failed", request_id, run_id);
                }
            }
            _ = sources.wake.notified() => {},
            _ = retry.tick() => {
                let recovery = recovery.clone();
                let service = service.clone();
                if !matches!(tokio::task::spawn_blocking(move || recovery.replay_one(&service)).await, Ok(Ok(_))) {
                    tracing::warn!(event = "gateway_cli_sources_recovery_failed", retry = true);
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) struct Fixture {
        pub(super) root: PathBuf,
        pub(super) path: PathBuf,
        pub(super) service: Arc<crate::performance::Service>,
    }
    impl Fixture {
        pub(super) fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("usage-recording-{}", uuid::Uuid::new_v4()));
            let path = root.join("index/performance.sqlite3");
            let service = Arc::new(crate::performance::Service::at(path.clone()));
            Self {
                root,
                path,
                service,
            }
        }
        pub(super) fn markers(&self) -> i64 {
            rusqlite::Connection::open(&self.path)
                .unwrap()
                .query_row("SELECT COUNT(*) FROM perf_gateway_native", [], |r| r.get(0))
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    #[tokio::test]
    async fn source_retry_survives_storage_failure_and_remains_idempotent() {
        let f = Fixture::new();
        std::fs::create_dir_all(&f.root).unwrap();
        std::fs::write(f.root.join("index"), b"block storage").unwrap();
        let sources = Sources::default();
        let source = sources.reserve("scope", "run").unwrap();
        source.fallback();
        assert!(!sources.flush(f.service.clone()).await);
        assert!(source.pending());
        sources.collect();
        assert!(sources.get("scope", "run").is_some());
        std::fs::remove_file(f.root.join("index")).unwrap();
        assert!(sources.flush(f.service.clone()).await);
        assert!(!source.pending());
        source.fallback();
        assert!(sources.flush(f.service.clone()).await);
        assert_eq!(f.markers(), 1);
        drop(source);
        sources.collect();
        assert!(sources.get("scope", "run").is_none());
    }
    #[tokio::test]
    async fn healthy_shutdown_preserves_queued_gateway_samples() {
        let f = Fixture::new();
        let sources = Arc::new(Sources::default());
        let source = sources.reserve("scope", "run").unwrap();
        let (tx, rx) = mpsc::channel(2);
        for id in ["first", "second"] {
            tx.try_send((
                source.clone(),
                "openai".into(),
                Record {
                    id: id.into(),
                    run: "run".into(),
                    ..Default::default()
                },
            ))
            .ok()
            .unwrap();
        }
        drop(tx);
        tokio::time::timeout(
            Duration::from_secs(5),
            write(rx, sources, f.service.clone(), f.root.join("gateway")),
        )
        .await
        .unwrap();
        assert!(!source.is_fallback());
        assert_eq!(f.markers(), 0);
        let db = rusqlite::Connection::open(&f.path).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM perf_gateway_calls", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn permanent_storage_failure_releases_shutdown_and_replays_after_restart() {
        let f = Fixture::new();
        std::fs::create_dir_all(&f.root).unwrap();
        std::fs::write(f.root.join("index"), b"block storage").unwrap();
        let sources = Arc::new(Sources::default());
        let scope = "a".repeat(64);
        let run = uuid::Uuid::new_v4().to_string();
        let source = sources.reserve(&scope, &run).unwrap();
        let (tx, rx) = mpsc::channel(1);
        tx.try_send((
            source.clone(),
            "openai".into(),
            Record {
                run,
                ..Default::default()
            },
        ))
        .ok()
        .unwrap();
        let directory = f.root.join("gateway");
        private_dir(&directory).unwrap();
        let lock_path = directory.join("service.lock");
        let lock = private_file(&lock_path).unwrap();
        lock.try_lock_exclusive().unwrap();
        let writer = tokio::spawn({
            let service = f.service.clone();
            let directory = directory.clone();
            async move {
                write(rx, sources, service, directory).await;
                drop(lock);
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !source.is_fallback() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), writer)
            .await
            .unwrap()
            .unwrap();
        let next_lock = private_file(&lock_path).unwrap();
        next_lock
            .try_lock_exclusive()
            .expect("another gateway must be able to start while metrics remain unavailable");
        let recovery = recovery::Recovery::new(&directory);
        assert!(recovery.replay_one(&f.service).is_err());
        std::fs::remove_file(f.root.join("index")).unwrap();
        assert!(recovery.replay_one(&f.service).unwrap());
        assert!(!recovery.replay_one(&f.service).unwrap());
        assert_eq!(f.markers(), 1);
    }
    #[test]
    fn registry_is_bounded_and_keeps_sources_until_queued_samples_finish() {
        let sources = Sources::default();
        let (tx, mut rx) = mpsc::channel(256);
        for run in 0..256 {
            let source = sources.reserve("scope", &run.to_string()).unwrap();
            tx.try_send((source, String::new(), Record::default()))
                .ok()
                .unwrap();
        }
        sources.collect();
        assert!(sources.reserve("scope", "overflow").is_none());
        let in_flight = rx.try_recv().unwrap();
        while rx.try_recv().is_ok() {}
        sources.collect();
        assert!(sources.get("scope", "0").is_some());
        assert!(sources.get("scope", "1").is_none());
        in_flight.0.fallback();
        drop(in_flight);
        sources.collect();
        assert!(sources.get("scope", "0").unwrap().pending());
        assert!(sources.reserve("scope", "new").is_some());
    }
    #[test]
    fn dropped_sample_selects_cli_without_waiting_for_full_or_closed_queue() {
        for closed in [false, true] {
            let sources = Sources::default();
            let source = sources.reserve("scope", "run").unwrap();
            let (tx, rx) = mpsc::channel(1);
            if closed {
                drop(rx);
            } else {
                tx.try_send((source.clone(), String::new(), Record::default()))
                    .ok()
                    .unwrap();
            }
            assert_eq!(tx.is_closed(), closed);
            let route = Arc::new(Route {
                source: source.clone(),
                active: AtomicUsize::new(0),
                registration: Registration {
                    observe_websocket: false,
                    run: "run".into(),
                    scope: "scope".into(),
                    provider: "provider".into(),
                    upstream: "http://localhost".into(),
                    model_provider: "openai".into(),
                    auth: RouteAuth::ApiKey { key: "key".into() },
                    token: "capability".into(),
                    lease: PathBuf::new(),
                },
            });
            drop(Capture::new(
                route.clone(),
                tx,
                "session".into(),
                "model".into(),
                "http",
            ));
            assert!(source.is_fallback());
            assert_eq!(route.active.load(Ordering::Relaxed), 0);
        }
    }
}
