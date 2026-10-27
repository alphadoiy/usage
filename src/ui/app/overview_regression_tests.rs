use super::*;
use crate::provider_catalog::{Catalog, Models, Provider, ProviderKind};

#[tokio::test]
async fn old_refresh_completion_cannot_clear_current_work() {
    let mut app = Dashboard::new(
        Vec::new(),
        isolated_config(),
        "/nonexistent/refresh.toml".into(),
    );
    app.overview_state.generation = 4;
    app.overview_state.pending = 2;
    app.overview_state.loading = true;
    let current = Task::spawn(&Handle::current(), |_| std::future::pending::<()>());
    let abort = current.abort_handle();
    app.overview_state.task = Some(current);
    app.tx
        .send(AppEvent::OverviewFinished { generation: 3 })
        .unwrap();
    app.drain_events();
    assert!(app.overview_state.loading);
    assert_eq!(app.overview_state.pending, 2);
    assert!(app.overview_state.task.is_some());
    app.tx
        .send(AppEvent::OverviewFinished { generation: 4 })
        .unwrap();
    app.drain_events();
    assert!(!app.overview_state.loading);
    assert_eq!(app.overview_state.pending, 0);
    assert!(app.overview_state.task.is_none());
    abort.abort();
}

fn overview() -> Catalog {
    Catalog {
        providers: vec![Provider {
            operations: Vec::new(),
            id: "hidden-profile-uuid".into(),
            name: "工作 Provider".into(),
            agent: Some("Codex".into()),
            kind: ProviderKind::CompatibleApi,
            configured_model: Some("default-model".into()),
            models: Models::Loading,
            usage: Default::default(),
            performance: None,
        }],
        ..Default::default()
    }
}
fn dashboard() -> Dashboard {
    Dashboard::new(
        Vec::new(),
        isolated_config(),
        "/nonexistent/usage-overview.toml".into(),
    )
}
fn rendered(app: &Dashboard, width: u16, height: u16) -> String {
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    render_provider_contents(buffer.area, &mut buffer, app);
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn provider_refresh_keeps_content_rows_and_scroll_position() {
    let mut app = dashboard();
    let mut catalog = overview();
    catalog.providers[0].models =
        Models::Ready((0..12).map(|index| format!("model-{index}")).collect());
    app.overview_state.catalog = Some(catalog);
    for expanded in [false, true] {
        app.home_models_expanded = expanded;
        for scroll in [0, 3] {
            app.home_scroll = scroll;
            for (width, height) in [(100, 20), (24, 6)] {
                app.overview_state.loading = false;
                let idle = rendered(&app, width, height);
                app.overview_state.loading = true;
                assert_eq!(rendered(&app, width, height), idle);
                assert_eq!(app.home_scroll, scroll);
            }
        }
    }
    let area = Rect::new(0, 0, 120, 40);
    let mut buffer = Buffer::empty(area);
    render_home(area, &mut buffer, &app);
    let provider_area = crate::ui::home::areas(area)[0];
    let title: String = (provider_area.x..provider_area.right())
        .map(|x| buffer[(x, provider_area.y)].symbol())
        .collect();
    assert!(
        title
            .split_whitespace()
            .collect::<String>()
            .contains("Provider·1/1·更新中")
    );

    app.overview_state.catalog = None;
    assert!(
        rendered(&app, 40, 6)
            .split_whitespace()
            .collect::<String>()
            .contains("正在读取Provider…")
    );
}

#[tokio::test]
async fn automatic_refresh_keeps_content_and_skips_models_while_manual_refresh_requeries() {
    use crate::http::{HttpClient, HttpResponse, TransportError};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RefreshHttp {
        models: AtomicUsize,
        balances: AtomicUsize,
        release: tokio::sync::Semaphore,
    }
    #[async_trait::async_trait]
    impl HttpClient for RefreshHttp {
        async fn request(
            &self,
            _: &str,
            url: &str,
            _: &[(&str, String)],
            _: Option<Vec<u8>>,
        ) -> Result<HttpResponse, TransportError> {
            let (status, body) = if url.ends_with("/models") {
                self.models.fetch_add(1, Ordering::SeqCst);
                (200, r#"{"data":[{"id":"kept-model"}]}"#.to_string())
            } else {
                let round = self.balances.fetch_add(1, Ordering::SeqCst);
                if round > 0 {
                    self.release.acquire().await.unwrap().forget();
                }
                if round > 1 {
                    (503, "temporary failure".into())
                } else {
                    (
                        200,
                        format!(
                            r#"{{"code":true,"data":{{"object":"token_usage","total_used":0,"total_available":{},"unlimited_quota":true}}}}"#,
                            (round + 1) * 2500000
                        ),
                    )
                }
            };
            Ok(HttpResponse {
                status,
                headers: Default::default(),
                body: body.into_bytes(),
            })
        }
    }
    async fn settle(app: &mut Dashboard) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while app.overview_state.loading {
                app.drain_events();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    async fn waiting(app: &mut Dashboard, http: &RefreshHttp, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while http.balances.load(Ordering::SeqCst) < count {
                app.drain_events();
                tokio::task::yield_now().await;
            }
            app.drain_events();
        })
        .await
        .unwrap();
    }
    let root = std::env::temp_dir().join(format!("usage-home-refresh-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("usage.toml");
    std::fs::write(
        &path,
        r#"[[resources]]
id="relay"
service="new_api_token"
base_url="https://relay.invalid"
credential_env="FAKE_KEY"
options={quota_per_unit="500000",currency="USD"}
inference={base_url="https://relay.invalid/v1",credential_env="FAKE_KEY"}
"#,
    )
    .unwrap();
    let http = Arc::new(RefreshHttp {
        models: AtomicUsize::new(0),
        balances: AtomicUsize::new(0),
        release: tokio::sync::Semaphore::new(0),
    });
    let mut app = dashboard();
    app.config_path = path;
    app.service_state.http = Arc::new(crate::http::FakeHttp::new());
    app.overview_state.service = Arc::new(crate::provider_catalog::CatalogService::new(
        root.join("absent-manager"),
        http.clone(),
        Arc::new(|_| Ok("synthetic-key".into())),
    ));
    let handle = Handle::current();
    app.refresh(&handle);
    settle(&mut app).await;
    assert_eq!(http.models.load(Ordering::SeqCst), 1);
    assert!(
        rendered(&app, 100, 30)
            .split_whitespace()
            .collect::<String>()
            .contains("余额5USD")
    );
    app.home_models_expanded = true;
    let selected = app.home_provider.clone();
    app.refresh_seconds = 1;
    app.last_refresh = std::time::Instant::now() - Duration::from_secs(2);
    assert!(app.maybe_auto_refresh(&handle));
    waiting(&mut app, &http, 2).await;
    let output = rendered(&app, 100, 30)
        .split_whitespace()
        .collect::<String>();
    assert!(output.contains("余额5USD"));
    assert!(output.contains("kept-model"));
    assert!(!output.contains("查询中"));
    assert_eq!(http.models.load(Ordering::SeqCst), 1);
    assert_eq!(app.home_provider, selected);
    assert!(app.home_models_expanded);
    let generation = app.overview_state.generation;
    app.refresh_background(&handle);
    assert_eq!(app.overview_state.generation, generation);
    http.release.add_permits(1);
    settle(&mut app).await;
    assert!(
        rendered(&app, 100, 30)
            .split_whitespace()
            .collect::<String>()
            .contains("余额10USD")
    );

    app.refresh(&handle);
    waiting(&mut app, &http, 3).await;
    assert!(
        rendered(&app, 100, 30)
            .split_whitespace()
            .collect::<String>()
            .contains("余额10USD")
    );
    http.release.add_permits(1);
    settle(&mut app).await;
    assert_eq!(http.models.load(Ordering::SeqCst), 2);
    let output = rendered(&app, 100, 30)
        .split_whitespace()
        .collect::<String>();
    assert!(!output.contains("余额10USD"));
    assert!(output.contains("kept-model"));
    assert!(
        app.overview_state.catalog.as_ref().unwrap().providers[0]
            .usage
            .sources
            .iter()
            .any(|s| {
                matches!(&s.state, crate::provider_catalog::QueryState::Result(result)
                if result.state == crate::resources::ResourceState::TransientFailure)
            })
    );
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn background_refresh_uses_selected_config_and_does_not_create_store() {
    let root = std::env::temp_dir().join(format!("usage-home-events-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("usage.toml");
    std::fs::write(
        &path,
        r#"[[resources]]
id="billing-only"
service="new_api_account"
base_url="https://unused.invalid"
credential_env="SYNTHETIC_ONLY"
options={user_id=1}
"#,
    )
    .unwrap();
    let http = Arc::new(crate::http::FakeHttp::new());
    let mut app = dashboard();
    app.config_path = path.clone();
    app.overview_state.service = Arc::new(crate::provider_catalog::CatalogService::new(
        root.join("absent-manager"),
        http.clone(),
        Arc::new(|_| panic!("billing-only resource resolved credentials")),
    ));
    app.refresh_overview(&Handle::current());
    tokio::time::timeout(Duration::from_secs(1), async {
        while app.overview_state.loading {
            app.drain_events();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        app.overview_state
            .catalog
            .as_ref()
            .unwrap()
            .providers
            .is_empty()
    );
    assert!(http.calls.lock().unwrap().is_empty());
    assert!(!root.join("absent-manager").exists());
    std::fs::write(path, "invalid [ TOML").unwrap();
    app.refresh_overview(&Handle::current());
    tokio::time::timeout(Duration::from_secs(1), async {
        while app.overview_state.loading {
            app.drain_events();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(app.overview_state.catalog.is_none());
    assert!(app.overview_state.error.is_some());
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn model_refresh_rejects_old_generation_duplicate_and_misrouted_results() {
    let mut app = dashboard();
    app.overview_state.generation = 2;
    app.overview_state.loading = true;
    app.tx
        .send(AppEvent::OverviewLoaded {
            generation: 1,
            result: Ok((overview(), 1)),
        })
        .unwrap();
    app.drain_events();
    assert!(app.overview_state.catalog.is_none());
    app.tx
        .send(AppEvent::OverviewLoaded {
            generation: 2,
            result: Ok((overview(), 1)),
        })
        .unwrap();
    app.drain_events();
    for (generation, target) in [(1, "hidden-profile-uuid"), (2, "unexpected")] {
        app.tx
            .send(AppEvent::ProviderModels {
                generation,
                target: target.into(),
                models: Models::Ready(vec!["wrong-model".into()]),
            })
            .unwrap();
    }
    app.drain_events();
    assert_eq!(app.overview_state.pending, 1);
    for model in ["correct-model", "duplicate-model"] {
        app.tx
            .send(AppEvent::ProviderModels {
                generation: 2,
                target: "hidden-profile-uuid".into(),
                models: Models::Ready(vec![model.into()]),
            })
            .unwrap();
    }
    app.drain_events();
    assert_eq!(app.overview_state.pending, 0);
    assert!(!app.overview_state.loading);
    assert_eq!(
        app.overview_state.catalog.as_ref().unwrap().providers[0].models,
        Models::Ready(vec!["correct-model".into()])
    );
    app.tx
        .send(AppEvent::OverviewLoaded {
            generation: 2,
            result: Err("配置读取失败".into()),
        })
        .unwrap();
    app.drain_events();
    assert!(app.overview_state.catalog.is_none());
    assert_eq!(app.overview_state.error.as_deref(), Some("配置读取失败"));
}

#[test]
fn provider_budget_updates_are_scoped_by_generation_and_identity_and_render_above_models() {
    use crate::provider_catalog::{ProviderUsage, QueryState, UsageSource};
    use crate::resources::{
        Money, ResourceBinding, ResourceMetric, ResourceScope, ResourceSnapshot, ResourceState,
    };
    let binding = ResourceBinding {
        account: "shared-wallet".into(),
        revision: "r1".into(),
    };
    let target = binding.cache_key("new_api_token");
    let mut catalog = overview();
    catalog.providers[0].usage = ProviderUsage {
        sources: vec![UsageSource {
            target: target.clone(),
            label: "工作令牌".into(),
            service: "new_api_token".into(),
            binding: binding.clone(),
            state: QueryState::Loading,
        }],
        hint: None,
        reset: None,
    };
    let mut app = dashboard();
    app.overview_state.catalog = Some(catalog);
    app.overview_state.generation = 2;
    app.overview_state.pending = 1;
    app.overview_state.loading = true;
    let result = ResourceSnapshot {
        binding,
        service: "new_api_token".into(),
        scope: ResourceScope::Token,
        state: ResourceState::Ready,
        metrics: vec![
            ResourceMetric::Balance {
                remaining: Money {
                    amount: "12.345678".into(),
                    currency: "USD".into(),
                },
            },
            ResourceMetric::Allowance {
                remaining: Some("6172839".into()),
                used: Some("1000000".into()),
                unit: "new_api_quota".into(),
                unlimited: true,
            },
        ],
        queried_at: Some(Utc::now()),
        message: None,
    };
    app.tx
        .send(AppEvent::ProviderResource {
            generation: 1,
            target: target.clone(),
            snapshot: result.clone(),
        })
        .unwrap();
    app.drain_events();
    assert_eq!(app.overview_state.pending, 1);
    app.tx
        .send(AppEvent::ProviderResource {
            generation: 2,
            target: target.clone(),
            snapshot: result.clone(),
        })
        .unwrap();
    app.drain_events();
    assert_eq!(app.overview_state.pending, 0);
    assert!(!app.overview_state.loading);
    let mut duplicate = result;
    duplicate.metrics.clear();
    app.tx
        .send(AppEvent::ProviderResource {
            generation: 2,
            target,
            snapshot: duplicate,
        })
        .unwrap();
    app.drain_events();
    let output = rendered(&app, 100, 30)
        .split_whitespace()
        .collect::<String>();
    assert!(output.contains("余额12.345678USD"));
    assert!(output.contains("USD（接口返回）"));
    assert!(output.contains("令牌不限额"));
    assert!(output.contains("原始额度"));
    assert!(output.contains("6172839"));
    assert!(output.contains("1000000"));
    assert!(!output.contains("账号余额需配置"));
    assert!(output.contains("工作令牌·令牌"));
    assert!(output.find("余额12.345678USD").unwrap() < output.find("默认模型").unwrap());
    for hidden in ["启动", "会话", "shared-wallet", "hidden-profile-uuid"] {
        assert!(!output.contains(hidden));
    }
}

#[tokio::test]
async fn provider_board_shows_only_current_provider_and_scrolls_its_details() {
    let mut app = dashboard();
    let mut catalog = overview();
    catalog.providers[0].models =
        Models::Ready((0..24).map(|n| format!("available-model-{n}")).collect());
    catalog.providers.push(Provider {
        operations: Vec::new(),
        id: "hidden-oauth-id".into(),
        name: "个人订阅".into(),
        agent: Some("Codex".into()),
        kind: ProviderKind::Subscription,
        configured_model: None,
        models: Models::Unavailable("OAuth 模型列表认证失败或权限不足".into()),
        usage: Default::default(),
        performance: None,
    });
    app.overview_state.catalog = Some(catalog);
    let initial = rendered(&app, 160, 40)
        .split_whitespace()
        .collect::<String>();
    assert_eq!(initial.matches("工作Provider").count(), 1);
    assert!(!rendered(&app, 100, 30).contains("available-model-23"));
    assert!(
        rendered(&app, 100, 30)
            .split_whitespace()
            .collect::<String>()
            .contains("另21个")
    );
    app.handle_home_key(KeyCode::Enter, &Handle::current(), (100, 32));
    assert!(app.home_models_expanded);
    assert!(app.browser_state.resume_request.is_none());
    app.cards.push(CardSlot {
        target: "hidden-card".into(),
        loading: true,
        snapshot: None,
        warning: None,
    });
    for width in [28, 52, 100] {
        let mut pages = String::new();
        app.home_scroll = 0;
        for _ in 0..100 {
            pages.push_str(&rendered(&app, width, 12));
            let old = app.home_scroll;
            app.scroll_home(1, (width, 14));
            if old == app.home_scroll {
                break;
            }
        }
        let compact = pages.split_whitespace().collect::<String>();
        for expected in [
            "工作Provider",
            "兼容API·BYOK",
            "default-model",
            "available-model-23",
        ] {
            assert!(
                compact.contains(expected),
                "missing {expected} at {width}: {pages}"
            );
        }
        for removed in [
            "启动",
            "运行中",
            "会话",
            "Legacy",
            "hidden-profile",
            "hidden-oauth",
        ] {
            assert!(!compact.contains(removed), "unexpected {removed}: {pages}");
        }
        app.scroll_home(i16::MIN, (width, 14));
        assert_eq!(app.home_scroll, 0);
    }
    app.handle_home_key(KeyCode::Right, &Handle::current(), (100, 32));
    assert_eq!(app.home_scroll, 0);
    assert!(!app.home_models_expanded);
    let output = rendered(&app, 100, 30)
        .split_whitespace()
        .collect::<String>();
    assert!(output.contains("个人订阅"));
    assert!(!output.contains("工作Provider"));
    assert!(!output.contains("available-model"));
    app.handle_home_key(KeyCode::Left, &Handle::current(), (100, 32));
    assert!(
        rendered(&app, 100, 30)
            .split_whitespace()
            .collect::<String>()
            .contains("工作Provider")
    );
    app.handle_home_key(KeyCode::Enter, &Handle::current(), (100, 32));
    assert!(app.handle_home_key(KeyCode::Esc, &Handle::current(), (100, 32)));
    assert!(!app.home_models_expanded);
    assert!(!rendered(&app, 100, 30).contains("available-model-23"));
    for (width, height) in [(0, 0), (1, 1), (2, 1)] {
        rendered(&app, width, height);
    }
}

#[tokio::test]
async fn provider_switch_keeps_identity_on_refresh_and_recovers_when_removed() {
    let mut app = dashboard();
    app.overview_state.generation = 9;
    let a = overview().providers.remove(0);
    let mut b = a.clone();
    b.id = "provider-b".into();
    b.name = "其他 Provider".into();
    b.models = Models::Ready((0..30).map(|n| format!("b-model-{n}")).collect());
    app.tx
        .send(AppEvent::OverviewLoaded {
            generation: 9,
            result: Ok((
                Catalog {
                    providers: vec![a.clone(), b.clone()],
                    ..Default::default()
                },
                0,
            )),
        })
        .unwrap();
    app.drain_events();
    assert_eq!(app.home_provider.as_deref(), Some(a.id.as_str()));
    app.handle_home_key(KeyCode::Right, &Handle::current(), (100, 30));
    assert_eq!(app.home_provider.as_deref(), Some("provider-b"));
    app.handle_home_key(KeyCode::Enter, &Handle::current(), (100, 30));
    app.handle_home_key(KeyCode::Down, &Handle::current(), (100, 30));
    let scroll = app.home_scroll;
    assert!(scroll > 0);
    app.tx
        .send(AppEvent::OverviewLoaded {
            generation: 9,
            result: Ok((
                Catalog {
                    providers: vec![b, a.clone()],
                    ..Default::default()
                },
                0,
            )),
        })
        .unwrap();
    app.drain_events();
    assert_eq!(app.home_provider.as_deref(), Some("provider-b"));
    assert_eq!(app.home_scroll, scroll);
    assert!(app.home_models_expanded);
    app.handle_home_key(KeyCode::Left, &Handle::current(), (100, 30));
    assert_eq!(app.home_provider.as_deref(), Some(a.id.as_str()));
    assert_eq!(app.home_scroll, 0);
    assert!(!app.home_models_expanded);
    app.handle_home_key(KeyCode::Right, &Handle::current(), (100, 30));
    app.tx
        .send(AppEvent::OverviewLoaded {
            generation: 9,
            result: Ok((
                Catalog {
                    providers: vec![a.clone()],
                    ..Default::default()
                },
                0,
            )),
        })
        .unwrap();
    app.drain_events();
    assert_eq!(app.home_provider.as_deref(), Some(a.id.as_str()));
    app.tx
        .send(AppEvent::OverviewLoaded {
            generation: 9,
            result: Ok((Catalog::default(), 0)),
        })
        .unwrap();
    app.drain_events();
    assert!(app.home_provider.is_none());
    for key in [KeyCode::Left, KeyCode::Right] {
        assert!(app.handle_home_key(key, &Handle::current(), (100, 30)));
    }
    assert!(app.browser_state.resume_request.is_none());
}

#[tokio::test]
async fn overview_refresh_cancels_previous_task_and_drop_cancels_current_task() {
    let mut app = dashboard();
    let previous = Task::spawn(&Handle::current(), |_| std::future::pending::<()>());
    let previous_abort = previous.abort_handle();
    app.overview_state.task = Some(previous);
    app.refresh_overview(&Handle::current());
    tokio::task::yield_now().await;
    assert!(previous_abort.is_finished());
    app.overview_state.task.take().unwrap().abort();
    let current = Task::spawn(&Handle::current(), |_| std::future::pending::<()>());
    let current_abort = current.abort_handle();
    app.overview_state.task = Some(current);
    drop(app);
    tokio::task::yield_now().await;
    assert!(current_abort.is_finished());
}

#[tokio::test]
async fn provider_changes_reload_inventory_while_old_queries_are_pending() {
    use crate::http::{HttpClient, HttpResponse, TransportError};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct SlowModels {
        calls: AtomicUsize,
        release: tokio::sync::Semaphore,
    }
    #[async_trait::async_trait]
    impl HttpClient for SlowModels {
        async fn request(
            &self,
            _: &str,
            url: &str,
            _: &[(&str, String)],
            _: Option<Vec<u8>>,
        ) -> Result<HttpResponse, TransportError> {
            if !url.contains("/v1/models") {
                // Balance probes finish immediately; only model requests are gated.
                return Ok(HttpResponse {
                    status: 404,
                    headers: Default::default(),
                    body: Vec::new(),
                });
            }
            if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
                self.release.acquire().await.unwrap().forget();
            }
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: br#"{"data":[{"id":"fixture-model"}]}"#.to_vec(),
            })
        }
    }
    async fn until(app: &mut Dashboard, condition: impl Fn(&Dashboard) -> bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                app.drain_events();
                if condition(app) {
                    break;
                }
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }

    let root = std::env::temp_dir().join(format!("usage-provider-change-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let config_path = root.join("usage.toml");
    std::fs::write(&config_path, "").unwrap();
    let mut manager = crate::codex_manager::Manager::open(root.join("manager")).unwrap();
    let config = crate::codex_manager::config::new_config(
        "https://models.invalid/v1",
        "fixture-model",
        None,
    )
    .unwrap();
    let original = manager
        .add("Original", &config, "synthetic-original")
        .unwrap();
    let http = Arc::new(SlowModels {
        calls: AtomicUsize::new(0),
        release: tokio::sync::Semaphore::new(0),
    });
    let mut app = dashboard();
    app.config_path = config_path;
    app.service_state.http = Arc::new(crate::http::FakeHttp::new());
    app.overview_state.service = Arc::new(
        crate::provider_catalog::CatalogService::new(
            root.join("manager"),
            http.clone(),
            Arc::new(|_| panic!("unexpected env lookup")),
        )
        .with_plugin_root(root.join("plugins")),
    );
    let handle = Handle::current();
    app.refresh_overview(&handle);
    until(&mut app, |app| !app.overview_state.loading).await;
    let old = app.overview_state.catalog.clone().unwrap();
    app.refresh_overview(&handle);
    until(&mut app, |_| http.calls.load(Ordering::SeqCst) == 2).await;
    let stale_generation = app.overview_state.generation;

    let added = manager.add("Added", &config, "synthetic-added").unwrap();
    app.refresh_after_provider_change(&handle);
    app.home_provider = Some(added.id.clone());
    app.notice = Some("已添加 Provider".into());
    assert!(app.overview_state.generation > stale_generation);
    until(&mut app, |app| {
        app.overview_state.catalog.as_ref().unwrap().providers.len() == 2
    })
    .await;
    assert!(
        app.overview_state.loading,
        "new Provider must appear before model queries finish; loading={} pending={}",
        app.overview_state.loading, app.overview_state.pending
    );
    assert_eq!(app.home_provider.as_deref(), Some(added.id.as_str()));
    assert_eq!(app.notice.as_deref(), Some("已添加 Provider"));
    assert!(rendered(&app, 100, 30).contains("Added"));
    assert!(matches!(
        app.overview_state
            .catalog
            .as_ref()
            .unwrap()
            .providers
            .iter()
            .find(|provider| provider.id == original.id)
            .unwrap()
            .models,
        Models::Ready(_)
    ));
    app.tx
        .send(AppEvent::OverviewLoaded {
            generation: stale_generation,
            result: Ok((old, 0)),
        })
        .unwrap();
    app.tx
        .send(AppEvent::OverviewFinished {
            generation: stale_generation,
        })
        .unwrap();
    app.drain_events();
    assert_eq!(
        app.overview_state.catalog.as_ref().unwrap().providers.len(),
        2
    );
    assert!(app.overview_state.loading);

    manager.remove(&original.id).unwrap();
    app.refresh_after_provider_change(&handle);
    until(&mut app, |app| {
        app.overview_state.catalog.as_ref().unwrap().providers.len() == 1
    })
    .await;
    assert_eq!(
        app.overview_state.catalog.as_ref().unwrap().providers[0].id,
        added.id
    );
    assert_eq!(app.home_provider.as_deref(), Some(added.id.as_str()));
    http.release.add_permits(4);
    until(&mut app, |app| !app.overview_state.loading).await;
    drop(app);
    drop(manager);
    std::fs::remove_dir_all(root).unwrap();
}
