//! Lossy request samples and reliable, bounded source selection use separate paths.
use super::*;
use std::sync::Mutex;
use tokio::sync::Notify;

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
    async fn flush(&self, service: Arc<crate::performance::Service>) -> bool {
        let pending: Vec<_> = self
            .entries
            .lock()
            .expect("source registry")
            .values()
            .filter(|s| s.pending())
            .cloned()
            .collect();
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
) {
    let mut retry = tokio::time::interval(Duration::from_secs(1));
    let mut closed = false;
    loop {
        let flushed = sources.flush(service.clone()).await;
        sources.collect();
        if closed && flushed {
            break;
        }
        tokio::select! {
            sample = receiver.recv(), if !closed => {
                let Some((source, upstream, record)) = sample else {
                    closed = true;
                    continue;
                };
                // The queued sample owns its source even after the route expires.
                let scope = source.key.0.clone();
                let request_id = record.id.clone();
                let run_id = record.run.clone();
                let service = service.clone();
                let result = tokio::task::spawn_blocking(move || service.record_gateway(&scope, &upstream, &record)).await;
                if !matches!(result, Ok(Ok(()))) {
                    source.fallback();
                    tracing::error!(event = "gateway_record_failed", request_id, run_id);
                }
            }
            _ = sources.wake.notified() => {},
            _ = retry.tick() => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        path: PathBuf,
        service: Arc<crate::performance::Service>,
    }
    impl Fixture {
        fn new() -> Self {
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
        fn markers(&self) -> i64 {
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
    async fn sample_write_failure_selects_cli_and_shutdown_waits_for_source_retry() {
        let f = Fixture::new();
        std::fs::create_dir_all(&f.root).unwrap();
        std::fs::write(f.root.join("index"), b"block storage").unwrap();
        let sources = Arc::new(Sources::default());
        let source = sources.reserve("scope", "run").unwrap();
        let (tx, rx) = mpsc::channel(1);
        tx.try_send((
            source.clone(),
            "openai".into(),
            Record {
                run: "run".into(),
                ..Default::default()
            },
        ))
        .ok()
        .unwrap();
        drop(tx);
        let writer = tokio::spawn(write(rx, sources, f.service.clone()));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !source.is_fallback() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!writer.is_finished());
        std::fs::remove_file(f.root.join("index")).unwrap();
        tokio::time::timeout(Duration::from_secs(5), writer)
            .await
            .unwrap()
            .unwrap();
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
