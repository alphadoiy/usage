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
    let homes = sessions::SessionHomes::new().with("claude", &session.home);
    let mut app = Dashboard::new_with_sessions(
        Vec::new(),
        config,
        "/nonexistent/cleanup-ui.toml".into(),
        sessions::builtin_registry(),
        homes,
    );
    app.cleanup_store = crate::session_cleanup::Store::at(session.cwd.join("trash"));
    app.cleanup_opens = crate::session_cleanup::opens::Store::at(session.cwd.join("opens.sqlite3"));
    app.home_sessions_state.project_store = std::sync::Arc::new(crate::projects::Store::at(
        session.cwd.join("projects.sqlite3"),
    ));
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

#[test]
fn cleanup_dialog_text_keeps_contrast_without_the_terminal_ansi_palette() {
    fn luminance(rgb: (u8, u8, u8)) -> f64 {
        let linear = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(rgb.0) + 0.7152 * linear(rgb.1) + 0.0722 * linear(rgb.2)
    }

    let (root, session) = fixture();
    let store = crate::session_cleanup::Store::at(root.join("trash"));
    store.delete(&session).unwrap();
    let entries = store.list().unwrap();
    let plan = policy::Plan {
        stale_days: 30,
        candidates: vec![policy::Candidate {
            session: session.clone(),
            reason: policy::Reason::Empty,
        }],
        errors: vec!["损坏日志，保留会话".into()],
        ..Default::default()
    };
    for (mode, selected) in [
        (Mode::Smart(SmartPlan::Ready(plan.clone())), 0),
        (Mode::Smart(SmartPlan::Ready(plan.clone())), 1),
        (Mode::Smart(SmartPlan::Ready(plan)), 6),
        (Mode::Smart(SmartPlan::Ready(policy::Plan::default())), 0),
        (Mode::Smart(SmartPlan::Failed("读取失败".into())), 0),
        (Mode::Loading, 0),
        (Mode::Working, 0),
        (Mode::Result("处理完成".into()), 0),
        (Mode::Confirm(Action::Delete(vec![session])), 0),
        (Mode::Confirm(Action::Restore(entries[0].key.clone())), 0),
        (
            Mode::Confirm(Action::Purge(vec![entries[0].key.clone()])),
            0,
        ),
        (Mode::List, 0),
    ] {
        let mut panel = Panel {
            entries: entries.clone(),
            selected,
            mode,
            trash: true,
            task: None,
        };
        for empty_trash in [false, true] {
            if empty_trash {
                panel.entries.clear();
            }
            for (width, height) in [(120, 30), (44, 14)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| panel.render(frame, store.path()))
                    .unwrap();
                let mut inspected = 0;
                for cell in &terminal.backend().buffer().content {
                    if cell.symbol().trim().is_empty() {
                        continue;
                    }
                    inspected += 1;
                    // Herdr can remap ANSI White/LightBlue/Yellow for a light theme.
                    // A fixed dark popup must use palette-independent colors throughout.
                    let (Color::Rgb(r, g, b), Color::Rgb(br, bg, bb)) = (cell.fg, cell.bg) else {
                        panic!(
                            "{:?}: glyph {:?} depends on terminal colors: {:?}",
                            panel.mode,
                            cell.symbol(),
                            cell.style()
                        );
                    };
                    let fg = luminance((r, g, b));
                    let bg = luminance((br, bg, bb));
                    let contrast = (fg.max(bg) + 0.05) / (fg.min(bg) + 0.05);
                    assert!(
                        contrast >= 4.5,
                        "{:?}: glyph {:?} has contrast {contrast:.2}",
                        panel.mode,
                        cell.symbol()
                    );
                    assert!(!cell.modifier.intersects(
                        ratatui::style::Modifier::DIM | ratatui::style::Modifier::REVERSED
                    ));
                }
                assert!(inspected > 10);
            }
        }
    }
    fs::remove_dir_all(root).unwrap();
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
    app.handle_browser_key(KeyCode::Char('s'), &handle);
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
async fn smart_cleanup_previews_and_deletes_only_stale_sessions() {
    let (root, session) = fixture();
    let mut stale = session.clone();
    stale.id = uuid::Uuid::new_v4().to_string();
    stale.title = "stale fixture".into();
    stale.recency_at_ms = 1;
    fs::write(
        log_path(&stale),
        format!(
            "{}\n",
            serde_json::json!({"type":"user","sessionId":stale.id,"cwd":root,"message":{"content":"old fixture"}})
        ),
    )
    .unwrap();
    fs::File::open(log_path(&stale))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(120)))
        .unwrap();
    let mut recent = session.clone();
    recent.recency_at_ms = chrono::Utc::now().timestamp_millis();
    let mut app = dashboard(recent.clone());
    app.cleanup_opens.record(&stale.key(), 1).unwrap();
    app.browser_state
        .panel
        .as_mut()
        .unwrap()
        .refresh(vec![recent, stale.clone()]);
    let handle = Handle::current();
    app.handle_browser_key(KeyCode::Char('C'), &handle);
    assert!(matches!(app.cleanup.as_ref().unwrap().mode, Mode::Loading));
    settle(&mut app).await;
    let plan = match app.cleanup.as_ref().unwrap().mode {
        Mode::Smart(SmartPlan::Ready(ref plan)) => plan.clone(),
        ref other => panic!("unexpected cleanup mode: {other:?}"),
    };
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.candidates[0].session.id, stale.id);
    assert_eq!(
        plan.candidates[0].reason,
        crate::session_cleanup::policy::Reason::Stale
    );
    assert!(log_path(&stale).exists());
    app.handle_cleanup_key(KeyCode::Char('y'), &handle);
    settle(&mut app).await;
    assert!(!log_path(&stale).exists());
    assert!(
        matches!(&app.cleanup.as_ref().unwrap().mode, Mode::Result(s) if s.contains("成功处理 1"))
    );
    drop(app);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn smart_cleanup_honors_search_and_rejects_cancelled_results() {
    let (root, session) = fixture();
    let mut other = session.clone();
    other.id = uuid::Uuid::new_v4().to_string();
    other.title = "excluded".into();
    let mut app = dashboard(session.clone());
    let browser = app.browser_state.panel.as_mut().unwrap();
    browser.refresh(vec![session.clone(), other]);
    for ch in "fixture".chars() {
        browser.type_char(ch);
    }
    let handle = Handle::current();
    app.open_smart_cleanup(&handle);
    let generation = app.cleanup_generation;
    let cancel = app
        .cleanup
        .as_ref()
        .unwrap()
        .task
        .as_ref()
        .unwrap()
        .cancellation();
    app.handle_cleanup_key(KeyCode::Esc, &handle);
    assert!(cancel.is_cancelled());
    app.open_smart_cleanup(&handle);
    assert!(!app.accept_smart_cleanup(generation, SmartPlan::Failed("late result".into())));
    settle(&mut app).await;
    let Mode::Smart(SmartPlan::Ready(plan)) = &app.cleanup.as_ref().unwrap().mode else {
        panic!("expected plan");
    };
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.candidates[0].session.id, session.id);
    assert!(plan.errors.is_empty());
    for (width, height) in [(120, 30), (44, 14), (12, 4), (1, 1)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
    }
    assert!(log_path(&session).exists());
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
