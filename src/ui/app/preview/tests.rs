use super::*;

#[tokio::test]
async fn selection_changes_cancel_old_previews_and_reject_results_from_other_homes() {
    let mut app = Dashboard::new(
        Vec::new(),
        isolated_config(),
        "/nonexistent/preview.toml".into(),
    );
    let first = sessions::Session {
        agent: None,
        provider: sessions::AgentKind::CODEX,
        instance: "first".into(),
        id: uuid::Uuid::new_v4().to_string(),
        cwd: "/synthetic/project".into(),
        title: "First".into(),
        recency_at_ms: 1,
        home: "/synthetic/first".into(),
    };
    let mut second = first.clone();
    second.instance = "second".into();
    second.home = "/synthetic/second".into();
    app.browser_state.panel = Some(SessionBrowser::new(vec![first.clone(), second.clone()]));
    app.browser_state.panel.as_mut().unwrap().focus_sessions();
    app.ensure_session_preview(&Handle::current());
    let generation = app.preview.generation;
    let old_request = app.preview.request.clone().unwrap();
    app.browser_state.panel.as_mut().unwrap().move_by(1);
    app.ensure_session_preview(&Handle::current());
    let result = Ok(sessions::preview::Preview {
        request: Some("wrong instance".into()),
        response: None,
    });
    assert!(!app.accept_session_preview(generation, old_request, result.clone()));
    let mut wrong_home = app.preview.request.clone().unwrap();
    wrong_home.home = first.home;
    assert!(!app.accept_session_preview(app.preview.generation, wrong_home, result));
    assert!(app.browser_state.panel.as_ref().unwrap().preview.is_none());
    let current = app.preview.request.clone().unwrap();
    assert!(app.accept_session_preview(
        app.preview.generation,
        current,
        Ok(sessions::preview::Preview {
            request: Some("right instance".into()),
            response: Some("reply".into())
        })
    ));
    assert_eq!(
        app.browser_state
            .panel
            .as_ref()
            .unwrap()
            .preview
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .request
            .as_deref(),
        Some("right instance")
    );
    app.close_browser(false);
    app.ensure_session_preview(&Handle::current());
    assert!(app.preview.task.is_none());
}
