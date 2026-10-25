use super::*;
use crate::performance::{Binding, ModelStats, Snapshot};
use crate::provider_catalog::{Catalog, Models, Provider, ProviderKind};

#[tokio::test]
async fn homepage_performance_is_cached_per_binding_and_expires_after_five_minutes() {
    let root =
        std::env::temp_dir().join(format!("usage-home-performance-{}", uuid::Uuid::new_v4()));
    let mut app = Dashboard::new(Vec::new(), AppConfig::default(), root.join("config"));
    app.performance.service = Arc::new(crate::performance::Service::at(root.join("index.sqlite3")));
    let a = panel().binding.clone();
    let mut b = a.clone();
    b.provider = "provider-b".into();
    b.home = root.join("other-home");
    app.overview_state.catalog = Some(Catalog {
        providers: [a.clone(), b.clone()]
            .into_iter()
            .map(|binding| Provider {
                operations: Vec::new(),
                id: binding.provider.clone(),
                name: binding.provider.clone(),
                agent: None,
                kind: ProviderKind::CompatibleApi,
                configured_model: None,
                models: Models::Ready(Vec::new()),
                usage: Default::default(),
                performance: Some(binding),
            })
            .collect(),
        ..Default::default()
    });
    let handle = Handle::current();
    assert!(app.refresh_home_performance(&handle));
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.home_performance.pending {
            app.drain_events();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !root.exists(),
        "homepage must not create an index or scan directories"
    );
    assert!(!app.refresh_home_performance(&handle));
    app.home_performance.cached.insert(
        a.cache_key(),
        super::super::performance::CachedSummary {
            result: Ok(Some(crate::performance::Summary {
                calls: 100,
                mean_ttft_ms: Some(2000.0),
                timed_samples: 90,
                floor_tokens: 1000,
                floor_ms: 10000,
                floor_samples: 4,
                speed_tokens: 600,
                speed_ms: 10000,
                speed_samples: 80,
            })),
            updated: std::time::Instant::now(),
        },
    );
    let text = |app: &Dashboard| {
        home_lines(app, 100)
            .iter()
            .flat_map(|l| &l.spans)
            .map(|s| s.content.to_string())
            .collect::<String>()
    };
    assert!(text(&app).contains("100 次请求 · 平均首字 2.00s"));
    assert!(text(&app).contains("速度下限 ≥100.0 tok/s"));
    assert!(!text(&app).contains("样本）"));
    assert!(text(&app).contains("60.0 tok/s"));
    app.home_provider = Some(b.provider.clone());
    assert!(!text(&app).contains("60.0 tok/s"));
    app.home_performance.cached.insert(
        b.cache_key(),
        super::super::performance::CachedSummary {
            result: Ok(None),
            updated: std::time::Instant::now(),
        },
    );
    assert!(!app.refresh_home_performance(&handle));
    app.home_provider = Some(a.provider.clone());
    assert!(!app.refresh_home_performance(&handle));
    app.home_performance
        .cached
        .get_mut(&a.cache_key())
        .unwrap()
        .updated -= Duration::from_secs(301);
    assert!(app.refresh_home_performance(&handle));
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.home_performance.pending {
            app.drain_events();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let old = a.cache_key();
    app.overview_state.catalog.as_mut().unwrap().providers[0]
        .performance
        .as_mut()
        .unwrap()
        .revision = "changed".into();
    app.tx
        .send(AppEvent::HomePerformance {
            generation: app.home_performance.generation,
            key: old.clone(),
            result: Err("wrong revision".into()),
        })
        .unwrap();
    app.drain_events();
    assert!(
        app.home_performance
            .cached
            .get(&old)
            .unwrap()
            .result
            .is_ok()
    );
    assert!(!text(&app).contains("wrong revision"));
    assert!(
        app.refresh_home_performance(&handle),
        "new account revision cannot reuse cached averages"
    );
}

async fn loaded(app: &mut Dashboard) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            app.drain_events();
            let panel = app.performance.panel.as_ref().unwrap();
            if !panel.loading {
                assert!(panel.error.is_none(), "{:?}", panel.error);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("performance dialog never finished");
}

#[tokio::test]
async fn performance_dialog_loads_local_index_refreshes_without_body_reads_and_cancels() {
    let root = std::env::temp_dir().join(format!("usage-performance-ui-{}", uuid::Uuid::new_v4()));
    let home = root.join("home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    let at = (Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
    let end = (Utc::now() - chrono::Duration::seconds(20)).to_rfc3339();
    let text = [
        serde_json::json!({"timestamp":at,"type":"session_meta","payload":{"id":"fixture","model_provider":"relay"}}),
        serde_json::json!({"timestamp":at,"type":"turn_context","payload":{"model":"fixture-model"}}),
        serde_json::json!({"timestamp":at,"type":"event_msg","payload":{"type":"task_started","turn_id":"t"}}),
        serde_json::json!({"timestamp":end,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"output_tokens":500},"last_token_usage":{"output_tokens":500}}}}),
        serde_json::json!({"timestamp":end,"type":"event_msg","payload":{"type":"task_complete","turn_id":"t","duration_ms":10000,"time_to_first_token_ms":2000}}),
    ].into_iter().map(|v| v.to_string()+"\n").collect::<String>();
    std::fs::write(home.join("sessions/fixture.jsonl"), &text).unwrap();
    let mut app = Dashboard::new(Vec::new(), AppConfig::default(), root.join("config.toml"));
    app.performance.service = Arc::new(crate::performance::Service::at(
        root.join("index/performance.sqlite3"),
    ));
    let mut p = panel();
    p.binding.home = home;
    p.snapshot = None;
    app.performance.panel = Some(p);
    let handle = Handle::current();
    app.load_performance(&handle, false);
    loaded(&mut app).await;
    let data = app
        .performance
        .panel
        .as_ref()
        .unwrap()
        .snapshot
        .as_ref()
        .unwrap();
    assert_eq!(data.models[0].model, "fixture-model");
    assert_eq!(data.models[0].mean_ttft_ms, Some(2000.0));
    assert_eq!(data.read_bytes, text.len() as u64);
    let key = app.performance.panel.as_ref().unwrap().binding.cache_key();
    let mean = app
        .home_performance
        .cached
        .get(&key)
        .unwrap()
        .result
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap();
    assert_eq!(mean.mean_ttft_ms, Some(2000.0));
    app.handle_performance_key(KeyCode::Char('r'), &handle);
    loaded(&mut app).await;
    let data = app
        .performance
        .panel
        .as_ref()
        .unwrap()
        .snapshot
        .as_ref()
        .unwrap();
    assert_eq!(data.read_bytes, 0);
    assert_eq!(data.models[0].calls, 1);
    let cancel = app
        .performance
        .panel
        .as_ref()
        .unwrap()
        .task
        .as_ref()
        .unwrap()
        .cancellation();
    app.handle_performance_key(KeyCode::Esc, &handle);
    assert!(cancel.is_cancelled());
    assert!(app.performance.panel.is_none());
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

fn panel() -> super::super::performance::Panel {
    let binding = Binding {
        provider: "provider-a".into(),
        source: crate::performance::source("codex").unwrap(),
        agent: "codex".into(),
        home: "/nonexistent/usage-performance-fixture".into(),
        revision: "1".into(),
        expected_upstream: Some("relay".into()),
        gateway_observations: true,
    };
    let mut panel = super::super::performance::Panel::new(binding, "Work".into());
    panel.generation = 7;
    panel.loading = false;
    panel.snapshot = Some(Snapshot {
        models: vec![ModelStats {
            model: "observed-model".into(),
            calls: 8,
            mean_ttft_ms: Some(2000.0),
            timed_samples: 3,
            floor_tokens: 1000,
            floor_ms: 10000,
            floor_samples: 2,
            speed_tokens: 600,
            speed_ms: 10000,
            speed_samples: 4,
        }],
        ..Default::default()
    });
    panel
}
#[test]
fn performance_modal_shows_provenance_samples_and_intercepts_parent_actions() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut app = Dashboard::new(
        Vec::new(),
        AppConfig::default(),
        "/nonexistent/perf-config".into(),
    );
    app.performance.panel = Some(panel());
    // x, Enter and navigation belong to the modal, never to resets or parent sessions.
    assert!(app.handle_performance_key(KeyCode::Char('x'), runtime.handle()));
    assert!(app.reset.is_none());
    for width in [42, 100] {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 28)).unwrap();
        terminal
            .draw(|frame| {
                super::super::performance::draw(frame, app.performance.panel.as_ref().unwrap())
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        let compact = text.split_whitespace().collect::<String>().replace('│', "");
        assert!(compact.contains("Work·性能"));
        assert!(compact.contains("平均首字"));
        assert!(compact.contains("速度下限"));
        assert!(compact.contains("observed-model"));
        assert!(compact.contains("60.0tok/s"), "width={width}: {compact}");
    }
    assert!(app.handle_performance_key(KeyCode::Esc, runtime.handle()));
    assert!(app.performance.panel.is_none());
}
#[test]
fn official_oauth_performance_modal_only_describes_session_logs() {
    let mut panel = panel();
    panel.binding.gateway_observations = false;
    for width in [42, 100] {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 28)).unwrap();
        terminal
            .draw(|frame| super::super::performance::draw(frame, &panel))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        let compact = text.split_whitespace().collect::<String>().replace('│', "");
        assert!(compact.contains("仅使用会话日志"), "{compact}");
        assert!(!compact.contains("网关"), "{compact}");
    }
}

#[test]
fn performance_results_cannot_cross_dialog_generations_or_provider_selection() {
    let mut app = Dashboard::new(
        Vec::new(),
        AppConfig::default(),
        "/nonexistent/perf-config".into(),
    );
    app.performance.panel = Some(panel());
    app.home_provider = Some("provider-b".into());
    app.tx
        .send(AppEvent::Performance {
            generation: 6,
            result: Err("stale".into()),
            loading: false,
        })
        .unwrap();
    app.drain_events();
    assert!(app.performance.panel.as_ref().unwrap().error.is_none());
    assert_eq!(
        app.performance.panel.as_ref().unwrap().binding.provider,
        "provider-a"
    );
    app.tx
        .send(AppEvent::Performance {
            generation: 7,
            result: Ok(Snapshot::default()),
            loading: false,
        })
        .unwrap();
    app.drain_events();
    assert!(
        app.performance
            .panel
            .as_ref()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .models
            .is_empty()
    );
    app.performance.panel = None;
    app.tx
        .send(AppEvent::Performance {
            generation: 7,
            result: Err("closed".into()),
            loading: false,
        })
        .unwrap();
    app.drain_events();
    assert!(app.performance.panel.is_none());
}
#[test]
fn an_unbound_provider_never_scans_default_agent_logs() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut app = Dashboard::new(
        Vec::new(),
        AppConfig::default(),
        "/nonexistent/perf-config".into(),
    );
    app.overview_state.catalog = Some(Catalog {
        providers: vec![Provider {
            operations: Vec::new(),
            id: "api".into(),
            name: "API".into(),
            agent: None,
            kind: ProviderKind::OfficialApi,
            configured_model: None,
            models: Models::Ready(Vec::new()),
            usage: Default::default(),
            performance: None,
        }],
        ..Default::default()
    });
    app.open_performance(runtime.handle());
    assert!(app.performance.panel.is_none());
    assert!(app.notice.as_deref().unwrap().contains("尚未绑定"));
}
