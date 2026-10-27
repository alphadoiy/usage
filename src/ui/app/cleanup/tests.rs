use super::*;
use ratatui::{Terminal, backend::TestBackend};
use std::fs;
use std::time::SystemTime;

fn fixture() -> (PathBuf, sessions::Session) {
    let root = std::env::temp_dir().join(format!("usage-cleanup-ui-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("home/projects/project")).unwrap();
    let root = root.canonicalize().unwrap();
    let session = sessions::Session {
        agent: None,
        provider: sessions::AgentKind::CLAUDE,
        instance: "claude".into(),
        id: uuid::Uuid::new_v4().to_string(),
        cwd: root.clone(),
        title: "fixture".into(),
        recency_at_ms: 1,
        home: root.join("home"),
    };
    let log = log_path(&session);
    fs::write(&log, format!("{}\n", serde_json::json!({"type":"user","sessionId":session.id,"cwd":root,"message":{"content":"fixture"}}))).unwrap();
    fs::File::open(log)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(120)))
        .unwrap();
    (root, session)
}

fn log_path(session: &sessions::Session) -> PathBuf {
    session
        .home
        .join(format!("projects/project/{}.jsonl", session.id))
}

fn dashboard(session: sessions::Session) -> Dashboard {
    let mut config = isolated_config();
    config.claude_dir = Some(session.home.clone());
    let mut app = Dashboard::new(Vec::new(), config, "/nonexistent/cleanup-ui.toml".into());
    app.cleanup_store = crate::session_cleanup::Store::at(session.cwd.join("trash"));
    app.home_activity.root = session.cwd.join("no-managed-runs");
    app.browser_state.panel = Some(SessionBrowser::new(vec![session]));
    app.browser_state.panel.as_mut().unwrap().focus_sessions();
    app
}

async fn settle(app: &mut Dashboard) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while app
            .cleanup
            .as_ref()
            .is_some_and(|p| matches!(p.mode, Mode::Working | Mode::Loading))
        {
            app.drain_events();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn delete_cancel_restore_and_refresh_work_through_browser_keys() {
    let (root, session) = fixture();
    let log = log_path(&session);
    let mut app = dashboard(session.clone());
    let handle = Handle::current();
    app.handle_browser_key(KeyCode::Char('d'), &handle);
    assert!(matches!(
        app.cleanup.as_ref().unwrap().mode,
        Mode::Confirm(_)
    ));
    assert!(log.exists());
    assert!(app.handle_cleanup_key(KeyCode::Char('n'), &handle));
    assert!(app.cleanup.is_none() && log.exists());
    app.handle_browser_key(KeyCode::Char(' '), &handle);
    app.handle_browser_key(KeyCode::Delete, &handle);
    app.handle_cleanup_key(KeyCode::Char('y'), &handle);
    app.handle_cleanup_key(KeyCode::Esc, &handle);
    assert!(matches!(app.cleanup.as_ref().unwrap().mode, Mode::Working));
    settle(&mut app).await;
    assert!(!log.exists());
    assert!(
        matches!(&app.cleanup.as_ref().unwrap().mode, Mode::Result(s) if s.contains("成功处理 1"))
    );
    let generation = app.browser_state.generation;
    assert!(app.refresh_after_cleanup(&handle));
    assert!(app.browser_state.generation > generation);
    assert!(app.browser_state.panel.as_ref().unwrap().marked.is_empty());
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.browser_state.loading {
            app.drain_events();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        app.browser_state
            .panel
            .as_ref()
            .unwrap()
            .sessions
            .is_empty()
    );
    app.handle_cleanup_key(KeyCode::Enter, &handle);
    app.handle_browser_key(KeyCode::Char('b'), &handle);
    settle(&mut app).await;
    let panel = app.cleanup.as_mut().unwrap();
    panel.selected = panel
        .entries
        .iter()
        .position(|e| e.id == session.id)
        .unwrap();
    app.handle_cleanup_key(KeyCode::Enter, &handle);
    assert!(!log.exists());
    app.handle_cleanup_key(KeyCode::Char('y'), &handle);
    settle(&mut app).await;
    assert!(log.exists());
    app.refresh_after_cleanup(&handle);
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.browser_state.loading {
            app.drain_events();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(app.browser_state.panel.as_ref().unwrap().sessions.len(), 1);
    drop(app);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn batch_delete_reports_active_failure_and_trash_purge_requires_confirmation() {
    let (root, session) = fixture();
    let mut active = session.clone();
    active.id = uuid::Uuid::new_v4().to_string();
    active.title = "active fixture".into();
    fs::write(log_path(&active), "active").unwrap();
    let mut app = dashboard(session.clone());
    app.browser_state
        .panel
        .as_mut()
        .unwrap()
        .refresh(vec![session.clone(), active.clone()]);
    let handle = Handle::current();
    app.handle_browser_key(KeyCode::Char('a'), &handle);
    assert_eq!(app.browser_state.panel.as_ref().unwrap().marked.len(), 2);
    app.handle_browser_key(KeyCode::Char('d'), &handle);
    app.handle_cleanup_key(KeyCode::Char('y'), &handle);
    settle(&mut app).await;
    assert!(!log_path(&session).exists() && log_path(&active).exists());
    assert!(
        matches!(&app.cleanup.as_ref().unwrap().mode, Mode::Result(s) if s.contains("成功处理 1") && s.contains("1 项失败"))
    );
    app.handle_cleanup_key(KeyCode::Enter, &handle);
    app.handle_browser_key(KeyCode::Char('b'), &handle);
    settle(&mut app).await;
    let key = app.cleanup.as_ref().unwrap().entries[0].key.clone();
    app.handle_cleanup_key(KeyCode::Char('e'), &handle);
    assert_eq!(app.cleanup_store.list().unwrap().len(), 1);
    app.handle_cleanup_key(KeyCode::Esc, &handle);
    assert_eq!(app.cleanup_store.list().unwrap().len(), 1);
    app.handle_cleanup_key(KeyCode::Char('d'), &handle);
    app.handle_cleanup_key(KeyCode::Char('y'), &handle);
    settle(&mut app).await;
    assert!(app.cleanup_store.list().unwrap().is_empty());
    assert!(app.cleanup_store.restore(&key).is_err());
    assert!(log_path(&active).exists());
    drop(app);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_cleanup_respects_search_and_dialogs_render_in_small_terminals() {
    let (root, session) = fixture();
    let mut other = session.clone();
    other.id = uuid::Uuid::new_v4().to_string();
    other.title = "exclude".into();
    let mut app = dashboard(session.clone());
    app.browser_state
        .panel
        .as_mut()
        .unwrap()
        .refresh(vec![session, other]);
    for ch in "fixture".chars() {
        app.browser_state.panel.as_mut().unwrap().type_char(ch);
    }
    app.open_cleanup_delete(true);
    assert!(
        matches!(&app.cleanup.as_ref().unwrap().mode, Mode::Confirm(Action::Delete(s)) if s.len() == 1)
    );
    for (width, height) in [(120, 30), (44, 14), (12, 4), (1, 1)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
    }
    drop(app);
    fs::remove_dir_all(root).unwrap();
}
