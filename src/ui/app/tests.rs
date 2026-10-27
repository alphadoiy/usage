use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::Duration as ChronoDuration;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

use super::*;
use crate::model::{QuotaTier, snapshot_not_found, snapshot_ok};
use crate::providers::UsageReset;
use crate::sessions::{AgentKind, Session};

struct ResetTestProvider {
    target: &'static str,
    available: u32,
    consumed: Arc<AtomicUsize>,
}

#[async_trait]
impl UsageProvider for ResetTestProvider {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn cache_key(&self) -> String {
        self.target.into()
    }

    async fn query(&self) -> UsageSnapshot {
        snapshot_not_found("codex", "Codex", None)
    }

    fn reset(&self) -> Option<&dyn UsageReset> {
        Some(self)
    }
}

#[async_trait]
impl UsageReset for ResetTestProvider {
    async fn preview_reset(&self) -> Result<ResetPreview, ProviderError> {
        Ok(ResetPreview {
            available: self.available,
            next_expires_at: None,
            account_label: Some("account-123".into()),
        })
    }

    async fn consume_reset(&self) -> Result<(), ProviderError> {
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.consumed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn reset_dashboard(available: u32, consumed: Arc<AtomicUsize>) -> Dashboard {
    let mut app = Dashboard::new(
        vec![Arc::new(ResetTestProvider {
            target: "codex",
            available,
            consumed,
        })],
        isolated_config(),
        "/tmp/usage-reset-test.toml".into(),
    );
    app.overview_state.catalog = Some(reset_catalog(&app.providers));
    app.service_state.http = Arc::new(crate::http::FakeHttp::new());
    app
}

fn reset_catalog(providers: &[Arc<dyn UsageProvider>]) -> crate::provider_catalog::Catalog {
    use crate::provider_catalog::{
        Catalog, Models, Provider, ProviderKind, ProviderUsage, ResetHandle,
    };
    Catalog {
        providers: providers
            .iter()
            .map(|provider| Provider {
                operations: Vec::new(),
                id: provider.cache_key(),
                name: provider.display_name().into(),
                agent: Some("Codex".into()),
                kind: ProviderKind::Subscription,
                configured_model: None,
                models: Models::Ready(Vec::new()),
                performance: None,
                usage: ProviderUsage {
                    reset: ResetHandle::new(provider.clone()),
                    ..Default::default()
                },
            })
            .collect(),
        ..Default::default()
    }
}

async fn drain_until(app: &mut Dashboard, condition: impl Fn(&Dashboard) -> bool) -> (bool, bool) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let outcome = app.drain_events();
            if condition(app) {
                return outcome;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reset event")
}

fn dashboard() -> Dashboard {
    let now = Utc::now();
    let mut app = Dashboard::new(Vec::new(), isolated_config(), "/tmp/usage-test.toml".into());
    app.service_state.http = Arc::new(crate::http::FakeHttp::new());
    app.cards = vec![
        CardSlot {
            target: "claude".into(),
            loading: false,
            snapshot: Some(snapshot_not_found("claude", "Claude Code", None)),
            warning: None,
        },
        CardSlot {
            target: "codex".into(),
            loading: false,
            snapshot: Some(snapshot_ok(
                "codex",
                "Codex",
                vec![
                    QuotaTier {
                        name: "five_hour".into(),
                        utilization: 89.0,
                        resets_at: Some(now + ChronoDuration::hours(1)),
                    },
                    QuotaTier {
                        name: "seven_day".into(),
                        utilization: 29.0,
                        resets_at: Some(now + ChronoDuration::days(5)),
                    },
                ],
                None,
                now,
            )),
            warning: None,
        },
        CardSlot {
            target: "grok".into(),
            loading: false,
            snapshot: Some(snapshot_not_found("grok", "Grok", None)),
            warning: None,
        },
    ];
    app
}

#[tokio::test]
async fn browser_navigates_projects_and_searches_without_launching() {
    let mut app = dashboard();
    app.browser_state.panel = Some(SessionBrowser::new(vec![
        Session {
            agent: None,
            provider: AgentKind::CLAUDE,
            instance: "claude".into(),
            id: "123e4567-e89b-12d3-a456-426614174000".into(),
            cwd: "/tmp/project".into(),
            title: "Claude task".into(),
            recency_at_ms: 0,
            home: "/tmp/claude".into(),
        },
        Session {
            agent: None,
            provider: AgentKind::CODEX,
            instance: "codex".into(),
            id: "123e4567-e89b-12d3-a456-426614174001".into(),
            cwd: "/tmp/other".into(),
            title: "Codex task".into(),
            recency_at_ms: 0,
            home: "/tmp/codex".into(),
        },
    ]));
    let handle = Handle::current();
    assert_eq!(
        app.browser_state.panel.as_ref().unwrap().focus(),
        Focus::Projects
    );
    app.handle_browser_key(KeyCode::Enter, &handle);
    assert_eq!(
        app.browser_state.panel.as_ref().unwrap().focus(),
        Focus::Sessions
    );
    app.handle_browser_key(KeyCode::Char('/'), &handle);
    app.handle_browser_key(KeyCode::Char('t'), &handle);
    assert_eq!(app.browser_state.panel.as_ref().unwrap().visible_count(), 2);
    app.handle_browser_key(KeyCode::Esc, &handle);
    assert!(!app.browser_state.panel.as_ref().unwrap().searching);
    app.handle_browser_key(KeyCode::Esc, &handle);
    assert_eq!(
        app.browser_state.panel.as_ref().unwrap().focus(),
        Focus::Projects
    );
    app.handle_browser_key(KeyCode::Esc, &handle);
    assert!(app.browser_state.panel.is_none());
}

#[tokio::test]
async fn browser_load_runs_in_background_and_reports_empty_result() {
    let mut app = dashboard();
    let missing = std::env::temp_dir().join(format!("usage-missing-{}", uuid::Uuid::new_v4()));
    app.browser_state.homes = sessions::SessionHomes::new()
        .with("claude", missing.join("claude"))
        .with("codex", missing.join("codex"))
        .with("grok", missing.join("grok"));
    let handle = Handle::current();
    app.open_browser(&handle);
    assert!(app.browser_state.loading);
    assert!(app.browser_state.panel.is_some());
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.browser_state.loading {
            app.drain_events();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(app.browser_state.panel.is_none());
    assert_eq!(app.notice.as_deref(), Some("未找到可恢复的本机会话"));
}

#[tokio::test]
async fn closing_browser_discards_pending_catalog() {
    let mut app = dashboard();
    let missing = std::env::temp_dir().join(format!("usage-missing-{}", uuid::Uuid::new_v4()));
    app.browser_state.homes = sessions::SessionHomes::new()
        .with("claude", missing.join("claude"))
        .with("codex", missing.join("codex"))
        .with("grok", missing.join("grok"));
    let handle = Handle::current();
    app.open_browser(&handle);
    app.handle_browser_key(KeyCode::Esc, &handle);
    assert!(!app.browser_state.loading);
    assert!(app.browser_state.panel.is_none());
    tokio::time::sleep(Duration::from_millis(10)).await;
    app.drain_events();
    assert!(app.browser_state.panel.is_none());
    assert!(app.notice.is_none());
}

fn custom_sessions(home: &std::path::Path) -> Result<Vec<sessions::SessionRecord>, String> {
    Ok(vec![sessions::SessionRecord {
        id: "123e4567-e89b-12d3-a456-426614174004".into(),
        cwd: home.join("project"),
        title: "Custom task".into(),
        recency_at_ms: 42,
    }])
}

#[tokio::test]
async fn dashboard_uses_injected_session_registry() {
    let root = std::env::temp_dir().join(format!("usage-dashboard-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("project")).unwrap();
    let kind = AgentKind::new(
        "custom",
        "Custom Agent",
        Some((180, 120, 210)),
        "custom resume",
        "CUSTOM_HOME",
        3,
    );
    let mut registry = sessions::SessionRegistry::new();
    registry
        .register(sessions::SessionSource {
            kind,
            load: custom_sessions,
        })
        .unwrap();
    let mut app = Dashboard::new_with_sessions(
        Vec::new(),
        isolated_config(),
        root.join("config.toml"),
        registry,
        sessions::SessionHomes::new().with("custom", &root),
    );
    let handle = Handle::current();
    app.open_browser(&handle);
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.browser_state.loading {
            app.drain_events();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        app.browser_state
            .panel
            .as_ref()
            .unwrap()
            .selected_session()
            .unwrap()
            .provider,
        kind
    );
    std::fs::remove_dir_all(root).unwrap();
}

fn row_text(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area().width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

#[test]
fn idle_redraws_only_on_clock_change_or_invalidation() {
    let mut redraw = RedrawSchedule::new();
    assert!(redraw.take(100));
    for _ in 0..750 {
        assert!(!redraw.take(100));
    }
    assert!(redraw.take(101));
    assert!(!redraw.take(101));
    redraw.invalidate();
    assert!(redraw.take(101));
}

#[test]
fn only_current_query_results_trigger_redraw() {
    let mut app = dashboard();
    app.generation = 2;
    app.pending = 1;
    app.tx
        .send(AppEvent::Quota(Box::new(RefreshEvent {
            generation: 1,
            target: "codex".into(),
            snapshot: snapshot_not_found("codex", "Codex", None),
        })))
        .unwrap();
    assert_eq!(app.drain_events(), (false, false));
    assert_eq!(app.pending, 1);
    app.tx
        .send(AppEvent::Quota(Box::new(RefreshEvent {
            generation: 2,
            target: "codex".into(),
            snapshot: snapshot_not_found("codex", "Codex", None),
        })))
        .unwrap();
    assert_eq!(app.drain_events(), (true, false));
    assert_eq!(app.pending, 0);
}

#[tokio::test]
async fn reset_requires_confirmation_and_consumes_once() {
    let consumed = Arc::new(AtomicUsize::new(0));
    let mut app = reset_dashboard(2, Arc::clone(&consumed));
    let handle = Handle::current();
    app.open_reset(&handle);
    drain_until(&mut app, |app| {
        app.reset
            .as_ref()
            .is_some_and(|flow| matches!(flow.stage, ResetStage::Confirm(_)))
    })
    .await;
    assert_eq!(consumed.load(Ordering::SeqCst), 0);
    assert!(app.handle_reset_key(KeyCode::Esc, &handle));
    assert!(app.reset.is_none());
    assert_eq!(consumed.load(Ordering::SeqCst), 0);

    app.open_reset(&handle);
    drain_until(&mut app, |app| {
        app.reset
            .as_ref()
            .is_some_and(|flow| matches!(flow.stage, ResetStage::Confirm(_)))
    })
    .await;
    assert!(app.handle_reset_key(KeyCode::Char('y'), &handle));
    assert!(app.handle_reset_key(KeyCode::Char('y'), &handle));
    let outcome = drain_until(&mut app, |app| app.reset.is_none()).await;
    assert_eq!(outcome, (true, true));
    assert_eq!(consumed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reset_consumes_the_confirmed_account_even_when_another_has_the_same_tool() {
    let first = Arc::new(AtomicUsize::new(0));
    let second = Arc::new(AtomicUsize::new(0));
    let mut app = Dashboard::new(
        vec![
            Arc::new(ResetTestProvider {
                target: "a",
                available: 0,
                consumed: first.clone(),
            }),
            Arc::new(ResetTestProvider {
                target: "b",
                available: 2,
                consumed: second.clone(),
            }),
        ],
        isolated_config(),
        "/tmp/usage-reset-accounts.toml".into(),
    );
    app.overview_state.catalog = Some(reset_catalog(&app.providers));
    app.home_provider = Some("b".into());
    app.open_reset(&Handle::current());
    drain_until(&mut app, |app| {
        app.reset
            .as_ref()
            .is_some_and(|flow| matches!(flow.stage, ResetStage::Confirm(_)))
    })
    .await;
    assert_eq!(app.reset.as_ref().unwrap().target, "b");
    // A selection change or legacy-list replacement cannot retarget an open confirmation.
    app.home_provider = Some("a".into());
    app.providers.clear();
    app.handle_reset_key(KeyCode::Char('y'), &Handle::current());
    drain_until(&mut app, |app| app.reset.is_none()).await;
    assert_eq!(first.load(Ordering::SeqCst), 0);
    assert_eq!(second.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn zero_reset_credits_never_open_confirmation() {
    let consumed = Arc::new(AtomicUsize::new(0));
    let mut app = reset_dashboard(0, Arc::clone(&consumed));
    app.open_reset(&Handle::current());
    drain_until(&mut app, |app| app.reset.is_none()).await;
    assert!(app.notice.as_deref().unwrap().contains("没有可用"));
    assert_eq!(consumed.load(Ordering::SeqCst), 0);
}

#[test]
fn reset_never_falls_back_to_default_when_home_provider_is_missing_or_unsupported() {
    let consumed = Arc::new(AtomicUsize::new(0));
    let mut app = reset_dashboard(2, consumed.clone());
    app.overview_state.catalog = None;
    // No task is spawned on either rejection path.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    app.open_reset(runtime.handle());
    assert!(app.reset.is_none());
    assert!(app.notice.as_deref().unwrap().contains("选择首页 Provider"));
    let mut catalog = reset_catalog(&app.providers);
    catalog.providers[0].usage.reset = None;
    app.overview_state.catalog = Some(catalog);
    app.open_reset(runtime.handle());
    assert!(app.reset.is_none());
    assert!(app.notice.as_deref().unwrap().contains("不支持主动重置"));
    assert_eq!(consumed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn reset_preview_updates_only_its_provider_board_including_zero() {
    use crate::provider_catalog::{QueryState, UsageSource};
    use crate::resources::{
        ResourceBinding, ResourceMetric, ResourceScope, ResourceSnapshot, ResourceState,
    };
    for (available, initial) in [(2, 0), (0, 2)] {
        let mut app = reset_dashboard(available, Arc::new(AtomicUsize::new(0)));
        let catalog = app.overview_state.catalog.as_mut().unwrap();
        catalog.providers[0].usage.sources.push(UsageSource {
            target: "codex".into(),
            label: "官方订阅额度".into(),
            service: "chatgpt_subscription".into(),
            binding: ResourceBinding {
                account: "codex".into(),
                revision: "1".into(),
            },
            state: QueryState::Result(ResourceSnapshot {
                binding: ResourceBinding {
                    account: "codex".into(),
                    revision: "1".into(),
                },
                service: "chatgpt_subscription".into(),
                scope: ResourceScope::Account,
                state: ResourceState::Ready,
                metrics: vec![ResourceMetric::ResetCredits { available: initial }],
                queried_at: Some(Utc::now()),
                message: None,
            }),
        });
        let mut other = catalog.providers[0].clone();
        other.id = "other".into();
        other.usage.sources[0].target = "other".into();
        catalog.providers.push(other);
        let text = |app: &Dashboard| {
            crate::ui::overview::lines(
                app.overview_state.catalog.as_ref(),
                false,
                None,
                true,
                Some("codex"),
                None,
                Default::default(),
            )
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>()
        };
        assert!(text(&app).contains(&format!("主动重置 {initial} 次")));
        app.open_reset(&Handle::current());
        drain_until(&mut app, |app| {
            app.reset
                .as_ref()
                .is_none_or(|flow| matches!(flow.stage, ResetStage::Confirm(_)))
        })
        .await;
        assert!(text(&app).contains(&format!("主动重置 {available} 次")));
        let QueryState::Result(other) = &app.overview_state.catalog.as_ref().unwrap().providers[1]
            .usage
            .sources[0]
            .state
        else {
            panic!("other provider lost its snapshot")
        };
        assert!(
            matches!(other.metrics.as_slice(), [ResourceMetric::ResetCredits { available }] if *available == initial)
        );
    }
}

#[test]
fn reset_confirmation_is_visible_over_dashboard() {
    let mut app = reset_dashboard(2, Arc::new(AtomicUsize::new(0)));
    app.reset = Some(ResetFlow {
        provider_id: "codex".into(),
        provider: app.providers[0].clone(),
        target: "codex".into(),
        provider_name: "Codex".into(),
        generation: 1,
        stage: ResetStage::Confirm(ResetPreview {
            available: 2,
            next_expires_at: Some(Utc::now() + ChronoDuration::days(1)),
            account_label: Some("123e4567-e89b-12d3-a456-426614174000".into()),
        }),
    });
    for width in [30, 80] {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = (0..24)
            .map(|y| row_text(terminal.backend().buffer(), y))
            .collect::<Vec<_>>()
            .join("\n");
        let compact = rendered.split_whitespace().collect::<String>();
        assert!(compact.contains("Codex主动重置"), "{rendered}");
        assert!(compact.contains("可用次数：2"), "{rendered}");
        assert!(compact.contains("账号：123e4567"), "{rendered}");
        assert!(compact.contains("426614174000"), "{rendered}");
        assert!(compact.contains("将消耗1次"), "{rendered}");
        assert!(compact.contains("按y确认"), "{rendered}");
    }
}

#[test]
fn provider_add_shortcut_is_scoped_to_the_provider_board() {
    let mut app = reset_dashboard(1, Arc::new(AtomicUsize::new(0)));
    app.home_focus = super::super::home::Focus::Providers;
    assert_eq!(
        app.handle_navigation_key(KeyCode::Char('a')),
        Some(MenuCommand::Activate(MenuAction::AddProvider))
    );
    assert_eq!(
        app.handle_navigation_key(KeyCode::F(5)),
        Some(MenuCommand::Activate(MenuAction::AddProvider))
    );
    app.home_focus = super::super::home::Focus::Projects;
    assert_eq!(app.handle_navigation_key(KeyCode::Char('a')), None);
    app.home_focus = super::super::home::Focus::Providers;
    app.history_state.panel = Some(HistoryPanel::new(vec!["codex".into()], 2026, true));
    assert_eq!(app.handle_navigation_key(KeyCode::Char('a')), None);
}

#[test]
fn main_menu_routes_codex_and_dismisses_without_changing_the_current_page() {
    let mut app = reset_dashboard(2, Arc::new(AtomicUsize::new(0)));
    assert_eq!(
        app.handle_navigation_key(KeyCode::Char('m')),
        Some(MenuCommand::Stay)
    );
    assert!(app.menu.is_some());
    for _ in 0..3 {
        assert_eq!(
            app.handle_navigation_key(KeyCode::Down),
            Some(MenuCommand::Stay)
        );
    }
    assert_eq!(
        app.handle_navigation_key(KeyCode::Enter),
        Some(MenuCommand::Activate(MenuAction::Providers))
    );
    assert!(app.menu.is_none());
    assert_eq!(
        app.handle_navigation_key(KeyCode::F(4)),
        Some(MenuCommand::Activate(MenuAction::Providers))
    );
    app.history_state.panel = Some(HistoryPanel::new(vec!["codex".into()], 2026, true));
    assert_eq!(app.handle_navigation_key(KeyCode::Char('m')), None);
    assert_eq!(
        app.handle_navigation_key(KeyCode::F(10)),
        Some(MenuCommand::Stay)
    );
    assert_eq!(
        app.handle_navigation_key(KeyCode::Esc),
        Some(MenuCommand::Dismiss)
    );
    assert!(app.menu.is_none());
    assert!(app.history_state.panel.as_ref().unwrap().monthly);
}

#[test]
fn menu_does_not_intercept_reset_confirmation() {
    let mut app = reset_dashboard(2, Arc::new(AtomicUsize::new(0)));
    app.reset = Some(ResetFlow {
        provider_id: "codex".into(),
        provider: app.providers[0].clone(),
        target: "codex".into(),
        provider_name: "Codex".into(),
        generation: 1,
        stage: ResetStage::Loading,
    });
    assert_eq!(app.handle_navigation_key(KeyCode::F(10)), None);
    assert!(app.menu.is_none());
}

#[tokio::test]
async fn homepage_status_updates_without_quota_credentials_and_marks_failed_refresh() {
    let mut app = dashboard();
    let http = Arc::new(crate::http::FakeHttp::new());
    http.route(
        service_status::CLAUDE.endpoint,
        Ok(crate::http::HttpResponse {
            status: 200,
            headers: Default::default(),
            body: br#"{"status":{"indicator":"none","description":"OK"},"components":[]}"#.to_vec(),
        }),
    );
    app.service_state.http = http.clone();
    app.refresh_service_statuses(&Handle::current());
    app.refresh_service_statuses(&Handle::current());
    drain_until(&mut app, |a| a.service_state.pending == 0).await;
    assert_eq!(http.calls.lock().unwrap().len(), 3);
    assert_eq!(app.pending, 0);
    assert!(!app.cards[0].snapshot.as_ref().unwrap().success);
    let time = app.service_state.statuses["claude"].fetched_at;
    for width in [28, 72, 110] {
        let area = Rect::new(0, 0, width, 32);
        let mut buffer = Buffer::empty(area);
        render_home(area, &mut buffer, &app);
        let text = buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        let compact = text.split_whitespace().collect::<String>();
        assert!(
            compact.contains(if width >= 60 {
                "Claude·正常"
            } else {
                "●Claude"
            }),
            "{width}: {text}"
        );
    }
    app.service_state.http = Arc::new(crate::http::FakeHttp::new());
    app.refresh_service_statuses(&Handle::current());
    drain_until(&mut app, |a| a.service_state.pending == 0).await;
    assert_eq!(app.service_state.statuses["claude"].fetched_at, time);
    assert!(app.service_state.statuses["claude"].stale);
    let area = Rect::new(0, 0, 72, 32);
    let mut buffer = Buffer::empty(area);
    render_home(area, &mut buffer, &app);
    let text = buffer
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(
        text.split_whitespace()
            .collect::<String>()
            .contains("更新失败"),
        "{text}"
    );
    let stale_cell = buffer.content.iter().find(|c| c.symbol() == "○").unwrap();
    assert_eq!(stale_cell.fg, Color::DarkGray);
}

#[tokio::test]
async fn old_status_events_do_not_replace_the_current_generation() {
    let mut app = dashboard();
    app.service_state.generation = 2;
    app.service_state.pending = 1;
    let snapshot =
        service_status::query(&crate::http::FakeHttp::new(), service_status::CLAUDE).await;
    app.tx
        .send(AppEvent::ServiceStatus {
            generation: 1,
            snapshot,
        })
        .unwrap();
    assert_eq!(app.drain_events(), (false, false));
    assert!(app.service_state.statuses.is_empty());
    assert_eq!(app.service_state.pending, 1);
}

struct CountingQuota(Arc<AtomicUsize>);
#[async_trait]
impl UsageProvider for CountingQuota {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn display_name(&self) -> &'static str {
        "Codex"
    }
    async fn query(&self) -> UsageSnapshot {
        self.0.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        snapshot_not_found("codex", "Codex", None)
    }
}
#[tokio::test]
async fn repeated_quota_refresh_joins_pending_work() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut app = Dashboard::new(
        vec![Arc::new(CountingQuota(calls.clone()))],
        isolated_config(),
        "/tmp/test-config".into(),
    );
    app.service_state.http = Arc::new(crate::http::FakeHttp::new());
    let handle = Handle::current();
    app.refresh(&handle);
    app.refresh(&handle);
    app.refresh(&handle);
    drain_until(&mut app, |a| a.pending == 0).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
struct PanickingQuota;
#[async_trait]
impl UsageProvider for PanickingQuota {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn display_name(&self) -> &'static str {
        "Codex"
    }
    async fn query(&self) -> UsageSnapshot {
        panic!("synthetic provider panic")
    }
}
#[tokio::test]
async fn a_panicking_quota_query_answers_and_later_refreshes_still_run() {
    let mut app = Dashboard::new(
        vec![Arc::new(PanickingQuota)],
        isolated_config(),
        "/tmp/test-config".into(),
    );
    app.service_state.http = Arc::new(crate::http::FakeHttp::new());
    let handle = Handle::current();
    app.refresh(&handle);
    drain_until(&mut app, |a| a.pending == 0).await;
    let generation = app.generation;
    app.refresh(&handle);
    assert_ne!(
        app.generation, generation,
        "the next refresh is not skipped"
    );
    drain_until(&mut app, |a| a.pending == 0).await;
}
#[tokio::test]
async fn switching_history_preserves_date_source_and_monthly_view() {
    use chrono::Datelike;
    let root = std::env::temp_dir().join(format!("usage-ui-{}", uuid::Uuid::new_v4()));
    let mut app = dashboard();
    app.history_state.config.providers.clear();
    app.history_state.cache = Some(crate::history::HistoryCache::at(
        root.join("history.sqlite3"),
    ));
    let date = chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
    app.history_state.panel = Some(HistoryPanel::new(
        vec!["codex".into(), "claude".into()],
        date.year(),
        true,
    ));
    let panel = app.history_state.panel.as_mut().unwrap();
    panel.selected = date;
    panel.key(KeyCode::Char('1'));
    app.close_history();
    app.open_history(&Handle::current());
    let panel = app.history_state.panel.as_ref().unwrap();
    assert_eq!(panel.selected, date);
    assert!(panel.monthly);
    assert_eq!(panel.sources(), vec!["codex"]);
    drain_until(&mut app, |a| !a.history_state.loading).await;
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn database_history_report_and_picker_finish_through_background_events() {
    let root = std::env::temp_dir().join(format!("usage-ui-query-{}", uuid::Uuid::new_v4()));
    let mut app = dashboard();
    app.history_state.config.providers = vec!["codex".into()];
    app.history_state.config.codex_dir = Some(root.join("codex"));
    app.history_state.cache = Some(crate::history::HistoryCache::at(
        root.join("history.sqlite3"),
    ));
    app.history_state.panel = Some(HistoryPanel::new(vec!["codex".into()], 2026, false));
    let handle = Handle::current();
    app.start_history_load(&handle);
    drain_until(&mut app, |app| !app.history_state.loading).await;
    assert!(
        app.history_state
            .panel
            .as_ref()
            .unwrap()
            .report_request()
            .is_some()
    );
    let rect = Rect::new(0, 0, 90, 24);
    super::super::history::render(
        rect,
        &mut Buffer::empty(rect),
        app.history_state.panel.as_ref().unwrap(),
    );
    assert!(
        app.history_state
            .panel
            .as_ref()
            .unwrap()
            .report_request()
            .is_some(),
        "render must not run the SQLite query"
    );
    app.ensure_history_report(&handle);
    drain_until(&mut app, |app| app.history_state.report_task.is_none()).await;
    assert!(
        app.history_state
            .panel
            .as_ref()
            .unwrap()
            .report_request()
            .is_none()
    );

    app.history_state
        .panel
        .as_mut()
        .unwrap()
        .key(KeyCode::Char('f'));
    assert!(
        app.history_state
            .panel
            .as_ref()
            .unwrap()
            .picker_request()
            .is_some()
    );
    app.ensure_history_picker(&handle);
    drain_until(&mut app, |app| app.history_state.picker_task.is_none()).await;
    assert!(
        app.history_state
            .panel
            .as_ref()
            .unwrap()
            .picker_request()
            .is_none()
    );
    assert!(app.history_state.panel.as_ref().unwrap().is_editing());
    app.history_state.panel = None;
    app.browser_state.panel = Some(SessionBrowser::new(Vec::new()));
    app.apply_browser_usage();
    app.ensure_browser_usage(&handle);
    assert!(app.browser_state.usage_pending.is_some());
    drain_until(&mut app, |app| app.browser_state.usage_task.is_none()).await;
    assert!(app.browser_state.usage_cache.is_some());
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn drilldown_returns_to_saved_history_and_uses_selected_day_usage() {
    let mut app = dashboard();
    let selected = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
    let mut panel = HistoryPanel::new(vec!["codex".into()], 2026, false);
    panel.selected = selected;
    let record = crate::history::Record {
        source: "codex".into(),
        instance: "codex".into(),
        id: "event".into(),
        session: "session".into(),
        project: "/tmp/p".into(),
        model: "gpt-test".into(),
        time: selected
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .single()
            .unwrap()
            .with_timezone(&Utc),
        tokens: crate::history::Tokens {
            input: 123,
            ..Default::default()
        },
    };
    Arc::make_mut(&mut app.history_state.loaded)
        .records
        .push(record);
    app.browser_state.filter = Some((panel.sources(), panel.session_query()));
    app.history_state.saved = Some(panel);
    app.browser_state.return_to_history = true;
    app.browser_state.panel = Some(SessionBrowser::new(vec![Session {
        agent: None,
        provider: AgentKind::CODEX,
        instance: "codex".into(),
        id: "session".into(),
        cwd: "/tmp/p".into(),
        title: "task".into(),
        recency_at_ms: 0,
        home: "/tmp".into(),
    }]));
    app.apply_browser_usage();
    assert_eq!(app.browser_state.panel.as_ref().unwrap().visible_count(), 1);
    app.close_browser(true);
    assert_eq!(app.history_state.panel.as_ref().unwrap().selected, selected);
}
#[tokio::test]
async fn history_auto_refresh_scans_locally_and_skips_quota_network() {
    let root = std::env::temp_dir().join(format!("usage-auto-{}", uuid::Uuid::new_v4()));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut app = Dashboard::new(
        vec![Arc::new(CountingQuota(calls.clone()))],
        isolated_config(),
        "/tmp/test-config".into(),
    );
    app.history_state.config.providers.clear();
    app.history_state.cache = Some(crate::history::HistoryCache::at(
        root.join("history.sqlite3"),
    ));
    app.history_state.panel = Some(HistoryPanel::new(Vec::new(), 2026, false));
    app.refresh_seconds = 1;
    app.history_state.last_refresh = std::time::Instant::now() - Duration::from_secs(2);
    assert!(app.maybe_auto_refresh(&Handle::current()));
    assert!(app.history_state.loading);
    drain_until(&mut app, |a| !a.history_state.loading).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    app.refresh_seconds = 0;
    app.history_state.last_refresh = std::time::Instant::now() - Duration::from_secs(2);
    assert!(!app.maybe_auto_refresh(&Handle::current()));
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn current_resume_is_requested_only_after_session_enter() {
    let mut app = dashboard();
    app.browser_state.panel = Some(SessionBrowser::new(vec![sessions::Session {
        agent: None,
        provider: AgentKind::CODEX,
        instance: "work".into(),
        id: "123e4567-e89b-12d3-a456-426614174001".into(),
        cwd: "/tmp/project".into(),
        home: "/tmp/work".into(),
        title: "Work".into(),
        recency_at_ms: 0,
    }]));
    app.browser_state.panel.as_mut().unwrap().resume_mode = crate::config::ResumeMode::Current;
    app.handle_browser_key(KeyCode::Enter, &Handle::current());
    assert!(app.browser_state.resume_request.is_none());
    app.handle_browser_key(KeyCode::Enter, &Handle::current());
    assert_eq!(
        app.browser_state.resume_request.as_ref().unwrap().instance,
        "work"
    );
    assert!(app.browser_state.panel.as_ref().unwrap().launching);
}
#[test]
fn quota_events_queue_one_alert_and_failures_do_not_emit_recovery() {
    let mut app = dashboard();
    app.history_state.config.alerts.enabled = true;
    let now = Utc::now();
    let snapshot = snapshot_ok(
        "codex",
        "Codex",
        vec![crate::model::QuotaTier {
            name: "primary".into(),
            utilization: 90.0,
            resets_at: None,
        }],
        None,
        now,
    );
    app.tx
        .send(AppEvent::Quota(Box::new(RefreshEvent {
            generation: app.generation,
            target: "codex".into(),
            snapshot,
        })))
        .unwrap();
    app.drain_events();
    assert_eq!(app.alerts_queue.len(), 1);
    assert!(app.notice.as_deref().unwrap().contains("剩余额度"));
    app.alerts_queue.clear();
    app.tx
        .send(AppEvent::Quota(Box::new(RefreshEvent {
            generation: app.generation,
            target: "codex".into(),
            snapshot: crate::model::snapshot_transient("codex", "Codex", "network"),
        })))
        .unwrap();
    app.drain_events();
    assert!(app.alerts_queue.is_empty());
    let snapshot = snapshot_ok(
        "codex",
        "Codex",
        vec![crate::model::QuotaTier {
            name: "primary".into(),
            utilization: 10.0,
            resets_at: None,
        }],
        None,
        now + ChronoDuration::seconds(1),
    );
    app.tx
        .send(AppEvent::Quota(Box::new(RefreshEvent {
            generation: app.generation,
            target: "codex".into(),
            snapshot,
        })))
        .unwrap();
    app.drain_events();
    assert!(app.alerts_queue[0].recovered);
}
