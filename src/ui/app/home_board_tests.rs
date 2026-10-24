use super::super::home::Focus as HomeFocus;
use super::*;
use crate::config::ResumeMode;
use crate::sessions::{AgentKind, Session};

fn app() -> Dashboard {
    let mut app = Dashboard::new(
        Vec::new(),
        isolated_config(),
        "/nonexistent/home-boards.toml".into(),
    );
    app.service_state.http = Arc::new(crate::http::FakeHttp::new());
    app
}
fn session(instance: &str, project: &str, recency: i64) -> Session {
    Session {
        agent: None,
        provider: AgentKind::CODEX,
        instance: instance.into(),
        id: "123e4567-e89b-12d3-a456-426614174001".into(),
        cwd: format!("/synthetic/{project}").into(),
        title: format!("{project} 会话"),
        recency_at_ms: recency,
        home: format!("/synthetic/{instance}").into(),
    }
}
fn text(buffer: &Buffer) -> String {
    buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .split_whitespace()
        .collect()
}

#[test]
fn default_home_renders_three_boards_with_projects_and_sessions() {
    let mut app = app();
    app.home_sessions_state
        .browser
        .refresh(vec![session("account-a", "ProjectA", 1)]);
    for width in [28, 52, 80, 120] {
        let area = Rect::new(0, 0, width, 22);
        let mut buffer = Buffer::empty(area);
        render_home(area, &mut buffer, &app);
        let output = text(&buffer);
        for expected in [
            "Provider",
            "官方API",
            "OpenAI",
            "Claude",
            "Grok",
            "项目与会话",
            "ProjectA",
        ] {
            assert!(
                output.contains(expected),
                "missing {expected} at {width}: {output}"
            );
        }
        assert!(!output.contains("123e4567"));
        if width >= 80 {
            assert!(output.contains("ProjectA会话"), "{output}");
        }
    }
    for (width, height) in [(0, 0), (1, 1), (12, 4)] {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        render_home(buffer.area, &mut buffer, &app);
    }
}

#[tokio::test]
async fn status_details_select_vendor_scroll_and_return_without_launching_sessions() {
    let mut app = app();
    let handle = Handle::current();
    for key in [KeyCode::Tab, KeyCode::Down, KeyCode::Enter] {
        assert!(app.handle_home_key(key, &handle, (80, 24)));
    }
    assert_eq!(app.home_focus, HomeFocus::Connections);
    assert!(app.home_connections.is_details());
    let area = Rect::new(0, 0, 80, 22);
    let mut buffer = Buffer::empty(area);
    render_home(area, &mut buffer, &app);
    let status_area = super::super::home::areas(area)[1];
    let output = (status_area.y..status_area.bottom())
        .flat_map(|y| (status_area.x..status_area.right()).map(move |x| (x, y)))
        .map(|position| buffer[position].symbol())
        .collect::<String>();
    assert!(output.contains("Claude"));
    assert!(!output.contains("OpenAI"));
    assert!(app.browser_state.resume_request.is_none());
    for _ in 0..100 {
        app.handle_home_key(KeyCode::Down, &handle, (80, 24));
    }
    assert!(app.handle_home_key(KeyCode::Esc, &handle, (80, 24)));
    assert!(!app.home_connections.is_details());
    render_home(area, &mut buffer, &app);
    assert!(text(&buffer).contains(">○Claude"));
    assert!(text(&buffer).contains("OpenAI"));
    assert!(!app.handle_home_key(KeyCode::Esc, &handle, (80, 24)));
}

#[tokio::test]
async fn quick_resume_preserves_instance_home_and_session_identity() {
    let mut app = app();
    app.home_sessions_state.browser.resume_mode = ResumeMode::Current;
    app.home_sessions_state.browser.refresh(vec![
        session("account-b", "ProjectB", 2),
        session("account-a", "ProjectA", 1),
    ]);
    let handle = Handle::current();
    app.handle_home_key(KeyCode::Tab, &handle, (100, 30));
    assert_eq!(app.home_focus, HomeFocus::Connections);
    app.handle_home_key(KeyCode::Tab, &handle, (100, 30));
    assert_eq!(app.home_focus, HomeFocus::Projects);
    app.handle_home_key(KeyCode::Down, &handle, (100, 30));
    app.handle_home_key(KeyCode::Enter, &handle, (100, 30));
    assert!(app.browser_state.resume_request.is_none());
    app.handle_home_key(KeyCode::Enter, &handle, (100, 30));
    let requested = app.browser_state.resume_request.as_ref().unwrap();
    assert_eq!(requested.instance, "account-a");
    assert_eq!(requested.home, std::path::Path::new("/synthetic/account-a"));
    assert_eq!(requested.cwd, std::path::Path::new("/synthetic/ProjectA"));
    assert_eq!(requested.id, "123e4567-e89b-12d3-a456-426614174001");
    app.handle_home_key(KeyCode::Down, &handle, (100, 30));
    app.handle_home_key(KeyCode::Enter, &handle, (100, 30));
    assert_eq!(
        app.browser_state.resume_request.as_ref().unwrap().instance,
        "account-a"
    );
    assert!(app.home_sessions_state.browser.launching);
}

#[test]
fn compact_sessions_show_account_names_and_hide_identifier_fallback_titles() {
    let mut app = app();
    let id = "123e4567-e89b-12d3-a456-426614174002";
    let mut session = session(&format!("managed-{id}-r2"), "ProjectA", 1);
    session.title = session.id.clone();
    app.home_sessions_state.browser.refresh(vec![session]);
    app.home_activity.sessions.insert(
        app.home_sessions_state.browser.sessions[0].key(),
        crate::runs::SessionRuntime {
            runs: vec![crate::runs::SessionRun {
                run_id: "private-run-uuid".into(),
                process: crate::runs::RunState::Running,
                activity: crate::runs::SessionActivity::Working,
                model: Some("reported-model".into()),
            }],
        },
    );
    app.overview_state.catalog = Some(crate::provider_catalog::Catalog {
        providers: vec![crate::provider_catalog::Provider {
            operations: Vec::new(),
            id: id.into(),
            name: "工作账号".into(),
            agent: Some("Codex".into()),
            kind: crate::provider_catalog::ProviderKind::CompatibleApi,
            configured_model: None,
            models: crate::provider_catalog::Models::Ready(Vec::new()),
            usage: Default::default(),
            performance: None,
        }],
        ..Default::default()
    });
    let area = Rect::new(0, 0, 120, 30);
    let mut buffer = Buffer::empty(area);
    render_home(area, &mut buffer, &app);
    let project = super::super::home::areas(area)[2];
    let output = (project.y..project.bottom())
        .flat_map(|y| (project.x..project.right()).map(move |x| (x, y)))
        .map(|position| buffer[position].symbol())
        .collect::<String>()
        .split_whitespace()
        .collect::<String>();
    assert!(output.contains("未命名会话"));
    assert!(output.contains("工作账号"));
    assert!(output.contains("r2"));
    assert!(output.contains("执行中"));
    assert!(output.contains("ProjectA(1)"));
    assert!(!output.contains("(1)·0"));
    assert!(!output.contains("private-run-uuid"));
    assert!(!output.contains("123e4567"));
}

#[tokio::test]
async fn search_does_not_open_menu_and_stale_events_do_not_replace_selection_or_unlock_launch() {
    let mut app = app();
    app.home_focus = HomeFocus::Projects;
    app.home_sessions_state
        .browser
        .refresh(vec![session("account-a", "ProjectA", 1)]);
    app.handle_home_key(KeyCode::Char('/'), &Handle::current(), (80, 24));
    assert_eq!(app.handle_navigation_key(KeyCode::Char('m')), None);
    app.handle_home_key(KeyCode::Char('m'), &Handle::current(), (80, 24));
    assert_eq!(app.home_sessions_state.browser.query, "m");
    assert!(app.menu.is_none());
    app.home_sessions_state.browser.clear_search();
    app.home_sessions_state.generation = 2;
    app.home_sessions_state.browser.launching = true;
    app.tx
        .send(AppEvent::HomeSessionsLoaded {
            generation: 1,
            catalog: sessions::SessionCatalog::default(),
            projects: Ok(Vec::new()),
        })
        .unwrap();
    app.tx
        .send(AppEvent::HomeSessionLaunched {
            generation: 1,
            result: Ok(()),
        })
        .unwrap();
    app.drain_events();
    assert!(app.home_sessions_state.browser.launching);
    assert_eq!(app.home_sessions_state.browser.sessions.len(), 1);
}

#[tokio::test]
async fn runtime_poll_is_read_only_rejects_stale_events_and_does_not_refresh_provider() {
    let root = std::env::temp_dir().join(format!("usage-runtime-ui-{}", uuid::Uuid::new_v4()));
    let mut app = app();
    app.home_activity.root = root.clone();
    app.home_sessions_state
        .browser
        .refresh(vec![session("account-a", "ProjectA", 1)]);
    let selected = app
        .home_sessions_state
        .browser
        .selected_session()
        .unwrap()
        .key();
    let handle = Handle::current();
    app.refresh_seconds = 0;
    app.maybe_refresh_home_activity(&handle);
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.home_activity.pending {
            app.drain_events();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!root.exists());
    assert_eq!(app.overview_state.generation, 0);
    assert!(app.home_activity.sessions.is_empty());
    let generation = app.home_activity.generation;
    app.maybe_refresh_home_activity(&handle);
    assert_eq!(app.home_activity.generation, generation);
    let activity = crate::runs::SessionRuntime {
        runs: vec![crate::runs::SessionRun {
            run_id: "internal-id".into(),
            process: crate::runs::RunState::Running,
            activity: crate::runs::SessionActivity::Working,
            model: None,
        }],
    };
    app.tx
        .send(AppEvent::HomeActivityLoaded {
            generation,
            result: Ok(HashMap::from([(selected.clone(), activity.clone())])),
        })
        .unwrap();
    assert!(app.drain_events().0);
    app.tx
        .send(AppEvent::HomeActivityLoaded {
            generation,
            result: Ok(HashMap::from([(selected.clone(), activity)])),
        })
        .unwrap();
    assert!(
        !app.drain_events().0,
        "identical local status invalidated the UI"
    );
    app.home_activity.generation += 1;
    app.tx
        .send(AppEvent::HomeActivityLoaded {
            generation,
            result: Err("old error".into()),
        })
        .unwrap();
    app.drain_events();
    assert!(!app.home_activity.sessions.is_empty());
    app.tx
        .send(AppEvent::HomeActivityLoaded {
            generation: app.home_activity.generation,
            result: Err("运行状态读取失败".into()),
        })
        .unwrap();
    app.drain_events();
    assert!(app.home_activity.sessions.is_empty());
    assert!(app.home_activity.error.is_some());
    assert_eq!(
        app.home_sessions_state
            .browser
            .selected_session()
            .unwrap()
            .key(),
        selected
    );
}

#[tokio::test]
async fn home_discovers_sessions_in_background_and_keeps_selection_on_refresh() {
    fn load(home: &std::path::Path) -> Result<Vec<sessions::SessionRecord>, String> {
        Ok(vec![sessions::SessionRecord {
            id: "123e4567-e89b-12d3-a456-426614174001".into(),
            cwd: home.join("project"),
            title: std::fs::read_to_string(home.join("title")).unwrap(),
            recency_at_ms: 1,
        }])
    }
    let root = std::env::temp_dir().join(format!("usage-home-sessions-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("project")).unwrap();
    std::fs::write(root.join("title"), "before").unwrap();
    let mut app = app();
    app.browser_state.homes = sessions::SessionHomes::new().with("codex", root.clone());
    app.browser_state.registry = sessions::SessionRegistry::new();
    app.browser_state
        .registry
        .register(sessions::SessionSource {
            kind: AgentKind::CODEX,
            load,
        })
        .unwrap();
    for title in ["before", "after"] {
        std::fs::write(root.join("title"), title).unwrap();
        app.start_home_sessions_load(&Handle::current());
        let generation = app.home_sessions_state.generation;
        app.start_home_sessions_load(&Handle::current());
        assert_eq!(app.home_sessions_state.generation, generation);
        tokio::time::timeout(Duration::from_secs(1), async {
            while app.home_sessions_state.loading {
                app.drain_events();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            app.home_sessions_state
                .browser
                .selected_session()
                .unwrap()
                .title,
            title
        );
        app.home_sessions_state.browser.focus_sessions();
    }
    assert_eq!(app.home_sessions_state.browser.focus(), Focus::Sessions);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
