//! Discovery, cache reuse and independent queries form one refresh operation.
use super::{Catalog, CatalogService, Models, Plan};
use crate::{http::HttpClient, providers::QUERY_TIMEOUT, resources::ResourceSnapshot};
use std::{path::PathBuf, sync::Arc};

pub struct RefreshRequest {
    pub config_path: PathBuf,
    pub previous: Option<Catalog>,
    pub refresh_models: bool,
}

#[derive(Debug)]
pub enum RefreshUpdate {
    Loaded(Result<(Catalog, usize), String>),
    Models {
        target: String,
        models: Models,
    },
    Resource {
        target: String,
        snapshot: ResourceSnapshot,
    },
    Finished,
}

impl CatalogService {
    /// Publish inventory first, then independent model/resource results and completion.
    /// A false callback result closes the consumer. Dropping this future also cancels
    /// outstanding async queries; local discovery already running may finish on its worker.
    pub async fn refresh(
        self: Arc<Self>,
        request: RefreshRequest,
        publish: impl Fn(RefreshUpdate) -> bool + Send + Sync,
    ) {
        let service = self.clone();
        let plan = tokio::task::spawn_blocking(move || {
            let config = crate::config::load_config(&request.config_path, false)
                .map_err(|_| "首页配置读取失败，请检查配置文件".to_string())?;
            let mut plan = service.load(&config)?;
            if let Some(previous) = request.previous {
                plan.reuse(&previous, request.refresh_models);
            }
            Ok::<_, String>(plan)
        })
        .await
        .unwrap_or_else(|_| Err("首页读取任务中断".into()));
        match plan {
            Ok(plan) => collect(plan, self.http.clone(), publish).await,
            Err(error) => {
                publish(RefreshUpdate::Loaded(Err(error)));
            }
        }
    }
}

async fn collect(
    plan: Plan,
    http: Arc<dyn HttpClient>,
    publish: impl Fn(RefreshUpdate) -> bool + Send + Sync,
) {
    let Plan {
        catalog,
        queries,
        resources,
    } = plan;
    if !publish(RefreshUpdate::Loaded(Ok((
        catalog,
        queries.len() + resources.len(),
    )))) {
        return;
    }
    let closed = tokio::sync::Notify::new();
    let accepting = std::sync::atomic::AtomicBool::new(true);
    let emit = |update| {
        if accepting.load(std::sync::atomic::Ordering::Relaxed) && !publish(update) {
            accepting.store(false, std::sync::atomic::Ordering::Relaxed);
            closed.notify_one();
        }
    };
    tokio::select! {
        biased;
        _ = closed.notified() => {},
        _ = async {
            tokio::join!(
                super::collect_models(http, queries, |target, models| {
                    emit(RefreshUpdate::Models { target, models });
                }),
                crate::resources::collect_with_updates(&resources, QUERY_TIMEOUT, |index, snapshot| {
                    let reader = &resources[index];
                    emit(RefreshUpdate::Resource {
                        target: reader.binding().cache_key(reader.service()),
                        snapshot: snapshot.clone(),
                    });
                }),
            );
        } => {
            if accepting.load(std::sync::atomic::Ordering::Relaxed) {
                publish(RefreshUpdate::Finished);
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{FakeHttp, HttpResponse};
    use crate::resources::{ResourceBinding, ResourceReader, ResourceScope, ResourceState};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::{Semaphore, mpsc};

    struct Reader {
        binding: ResourceBinding,
        gate: Arc<Semaphore>,
        started: mpsc::UnboundedSender<()>,
        stopped: Arc<AtomicUsize>,
        panic: bool,
    }
    struct Stopped(Arc<AtomicUsize>);
    impl Drop for Stopped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl ResourceReader for Reader {
        fn service(&self) -> &str {
            "synthetic"
        }
        fn binding(&self) -> &ResourceBinding {
            &self.binding
        }
        async fn read(&self) -> ResourceSnapshot {
            let _stopped = Stopped(self.stopped.clone());
            self.started.send(()).unwrap();
            assert!(!self.panic, "synthetic reader panic");
            self.gate.acquire().await.unwrap().forget();
            ResourceSnapshot {
                binding: self.binding.clone(),
                service: self.service().into(),
                scope: ResourceScope::Account,
                state: ResourceState::Ready,
                metrics: Vec::new(),
                queried_at: None,
                message: None,
            }
        }
    }
    fn http() -> Arc<FakeHttp> {
        let http = Arc::new(FakeHttp::new());
        http.route(
            "https://models.invalid/v1/models",
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: br#"{"data":[{"id":"m"}]}"#.to_vec(),
            }),
        );
        http
    }
    fn plan(resources: Vec<Arc<dyn ResourceReader>>) -> Plan {
        Plan {
            catalog: Catalog::default(),
            queries: vec![
                super::super::ModelQuery::new(
                    "provider-a".into(),
                    "https://models.invalid/v1",
                    "synthetic".into(),
                )
                .unwrap(),
            ],
            resources,
        }
    }
    async fn next(rx: &mut mpsc::UnboundedReceiver<RefreshUpdate>) -> RefreshUpdate {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }
    #[tokio::test]
    async fn inventory_precedes_independent_queries_and_completion() {
        let (started, mut ready) = mpsc::unbounded_channel();
        let gate = Arc::new(Semaphore::new(0));
        let reader = Arc::new(Reader {
            binding: ResourceBinding {
                account: "account-a".into(),
                revision: "1".into(),
            },
            gate: gate.clone(),
            started,
            stopped: Arc::new(AtomicUsize::new(0)),
            panic: false,
        });
        let target = reader.binding.cache_key(reader.service());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(collect(plan(vec![reader]), http(), move |update| {
            tx.send(update).is_ok()
        }));
        assert!(matches!(
            next(&mut rx).await,
            RefreshUpdate::Loaded(Ok((_, 2)))
        ));
        assert!(
            matches!(next(&mut rx).await, RefreshUpdate::Models { target, models: Models::Ready(m) } if target == "provider-a" && m == ["m"])
        );
        ready.recv().await.unwrap();
        assert!(
            rx.try_recv().is_err(),
            "slow resources must not block model publication"
        );
        gate.add_permits(1);
        assert!(
            matches!(next(&mut rx).await, RefreshUpdate::Resource { target: actual, snapshot } if actual == target && snapshot.state == ResourceState::Ready)
        );
        assert!(matches!(next(&mut rx).await, RefreshUpdate::Finished));
        task.await.unwrap();
    }
    #[tokio::test]
    async fn closing_consumer_cancels_slow_queries_without_a_completion_event() {
        let (started, mut ready) = mpsc::unbounded_channel();
        let stopped = Arc::new(AtomicUsize::new(0));
        let gates: Vec<_> = (0..2).map(|_| Arc::new(Semaphore::new(0))).collect();
        let readers: Vec<Arc<dyn ResourceReader>> = gates
            .iter()
            .enumerate()
            .map(|(i, gate)| {
                Arc::new(Reader {
                    binding: ResourceBinding {
                        account: i.to_string(),
                        revision: String::new(),
                    },
                    gate: gate.clone(),
                    started: started.clone(),
                    stopped: stopped.clone(),
                    panic: false,
                }) as Arc<dyn ResourceReader>
            })
            .collect();
        let mut request = plan(readers);
        request.queries.clear();
        let updates = Arc::new(AtomicUsize::new(0));
        let seen = updates.clone();
        let task = tokio::spawn(collect(request, http(), move |update| {
            seen.fetch_add(1, Ordering::SeqCst);
            matches!(update, RefreshUpdate::Loaded(_))
        }));
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(2), ready.recv())
                .await
                .unwrap()
                .unwrap();
        }
        gates[0].add_permits(1);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while stopped.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            updates.load(Ordering::SeqCst),
            2,
            "only inventory and the rejected resource result are delivered"
        );
    }
    #[tokio::test]
    async fn panicking_reader_still_publishes_identity_and_finishes_refresh() {
        let (started, _ready) = mpsc::unbounded_channel();
        let reader = Arc::new(Reader {
            binding: ResourceBinding {
                account: "failed".into(),
                revision: "2".into(),
            },
            gate: Arc::new(Semaphore::new(0)),
            started,
            stopped: Arc::new(AtomicUsize::new(0)),
            panic: true,
        });
        let target = reader.binding.cache_key(reader.service());
        let (tx, mut rx) = mpsc::unbounded_channel();
        collect(plan(vec![reader]), http(), move |update| {
            tx.send(update).is_ok()
        })
        .await;
        assert!(matches!(
            next(&mut rx).await,
            RefreshUpdate::Loaded(Ok((_, 2)))
        ));
        let mut failed = false;
        for _ in 0..2 {
            if let RefreshUpdate::Resource {
                target: actual,
                snapshot,
            } = next(&mut rx).await
            {
                assert_eq!(actual, target);
                assert_eq!(snapshot.state, ResourceState::TransientFailure);
                failed = true;
            }
        }
        assert!(failed);
        assert!(matches!(next(&mut rx).await, RefreshUpdate::Finished));
    }

    #[tokio::test]
    async fn invalid_configuration_finishes_with_one_safe_error_and_no_queries() {
        let root =
            std::env::temp_dir().join(format!("usage-refresh-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        std::fs::write(&path, "private-sentinel = [").unwrap();
        let http = http();
        let service = Arc::new(
            CatalogService::new(
                root.join("accounts"),
                http.clone(),
                Arc::new(|_| Err("unused".into())),
            )
            .with_plugin_root(root.join("plugins")),
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        service
            .refresh(
                RefreshRequest {
                    config_path: path,
                    previous: None,
                    refresh_models: false,
                },
                move |update| tx.send(update).is_ok(),
            )
            .await;
        let RefreshUpdate::Loaded(Err(error)) = next(&mut rx).await else {
            panic!("missing failure result")
        };
        assert!(!error.contains("private-sentinel"));
        assert!(rx.recv().await.is_none());
        assert!(http.calls.lock().unwrap().is_empty());
        assert!(!root.join("accounts").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
