use super::*;
use presence::Presence;
use std::io::Write;

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/support/bark_server.rs"
));

const USER_THREAD: &str = "01a135e0-bb6a-75e3-94a5-fbcdbf483966";

#[derive(Clone)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Fixture {
    root: PathBuf,
    context: Context,
}

impl Fixture {
    fn new(server: &BarkServer) -> Self {
        let root =
            std::env::temp_dir().join(format!("usage-bark-callback-{}", uuid::Uuid::new_v4()));
        save(&root, &from_url(&server.url, true, true).unwrap()).unwrap();
        let home = root.join("home");
        std::fs::create_dir(&home).unwrap();
        let conn = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, source TEXT, thread_source TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads VALUES (?1,'cli','user')",
            [USER_THREAD],
        )
        .unwrap();
        let context = Context {
            root: root.clone(),
            home: Some(home),
            account: "Fixture".into(),
            run_id: uuid::Uuid::new_v4().to_string(),
            previous_notify: vec![],
        };
        Self { root, context }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn complete(turn: &str) -> String {
    json!({"type":"agent-turn-complete", "thread-id":USER_THREAD, "turn-id":turn,
        "cwd":"/private/project", "input-messages":["private prompt"],
        "last-assistant-message":"private assistant output"})
    .to_string()
}

#[tokio::test]
async fn ephemeral_title_and_subagent_completions_are_filtered_by_source() {
    let server = BarkServer::new();
    let f = Fixture::new(&server);
    let title = "01a135e1-229a-71e1-8fb4-5495b317c2f4";
    let subagent = "01a135e0-3102-7241-a54f-b05d239e2832";
    let conn = rusqlite::Connection::open(f.context.home.as_ref().unwrap().join("state_5.sqlite"))
        .unwrap();
    conn.execute(
        "INSERT INTO threads VALUES (?1,'cli','subagent')",
        [subagent],
    )
    .unwrap();
    for threads in [
        [title, subagent, USER_THREAD],
        [USER_THREAD, title, subagent],
    ] {
        for thread in threads {
            let mut data: Value = serde_json::from_str(&complete("same-turn")).unwrap();
            data["thread-id"] = thread.into();
            handle_event(&f.context, Event::Complete, &data.to_string(), || {
                Presence::Away
            })
            .await
            .unwrap();
        }
        let message = server.receive();
        assert!(
            message["body"]
                .as_str()
                .unwrap()
                .contains("private assistant output")
        );
        assert!(!message.to_string().contains(&USER_THREAD[..12]));
        assert!(server.messages.try_recv().is_err());
    }
    let context = Context {
        home: None,
        ..f.context.clone()
    };
    handle_event(&context, Event::Complete, &complete("later"), || {
        Presence::Away
    })
    .await
    .unwrap();
    assert!(server.messages.try_recv().is_err());
}

#[test]
fn source_lookup_handles_rollouts_archives_and_old_index_schema_read_only() {
    let server = BarkServer::new();
    let f = Fixture::new(&server);
    let home = f.context.home.as_ref().unwrap();
    let archived = home.join("archived_sessions");
    let sessions = home.join("sessions/2026/10/13");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&archived).unwrap();
    let thread = "01a135e1-229a-71e1-8fb4-5495b317c2f4";
    let rollout = sessions.join(format!("rollout-2026-10-13T23-28-34-{thread}.jsonl"));
    let write_meta = |thread_source: Value, source: Value| {
        std::fs::write(&rollout, format!("{}\nprivate transcript is never parsed", json!({
            "type":"session_meta", "payload":{"id":thread, "thread_source":thread_source, "source":source}
        }))).unwrap();
    };
    write_meta("user".into(), "cli".into());
    assert_eq!(source::classify(Some(home), thread), source::Origin::User);
    write_meta("thread_title".into(), "cli".into());
    assert_eq!(
        source::classify(Some(home), thread),
        source::Origin::Internal
    );
    write_meta(
        Value::Null,
        json!({"subagent":{"thread_spawn":{"parent_thread_id":USER_THREAD}}}),
    );
    assert_eq!(
        source::classify(Some(home), thread),
        source::Origin::Internal
    );
    write_meta(Value::Null, "exec".into());
    std::fs::rename(&rollout, archived.join(rollout.file_name().unwrap())).unwrap();
    assert_eq!(source::classify(Some(home), thread), source::Origin::User);
    std::fs::remove_file(home.join("state_5.sqlite")).unwrap();
    assert_eq!(source::classify(Some(home), thread), source::Origin::User);
    assert!(!home.join("state_5.sqlite").exists());
    let conn = rusqlite::Connection::open(home.join("state_1.sqlite")).unwrap();
    conn.execute_batch("CREATE TABLE threads (id TEXT,source TEXT);")
        .unwrap();
    conn.execute("INSERT INTO threads VALUES (?1,'exec')", [USER_THREAD])
        .unwrap();
    drop(conn);
    let before = std::fs::read(home.join("state_1.sqlite")).unwrap();
    assert_eq!(
        source::classify(Some(home), USER_THREAD),
        source::Origin::User
    );
    assert_eq!(std::fs::read(home.join("state_1.sqlite")).unwrap(), before);
    assert!(!home.join("state_1.sqlite-journal").exists());
}

#[test]
fn source_lookup_rejects_mismatched_metadata_invalid_ids_and_unknown_sources() {
    let server = BarkServer::new();
    let f = Fixture::new(&server);
    let home = f.context.home.as_ref().unwrap();
    assert_eq!(source::classify(None, USER_THREAD), source::Origin::Unknown);
    assert_eq!(
        source::classify(Some(home), "../../config.toml"),
        source::Origin::Unknown
    );
    let thread = "01a135e1-229a-71e1-8fb4-5495b317c2f4";
    assert_eq!(
        source::classify(Some(home), thread),
        source::Origin::Untracked
    );
    let sessions = home.join("sessions/2026/10/13");
    std::fs::create_dir_all(&sessions).unwrap();
    let rollout = sessions.join(format!("rollout-2026-10-13T23-28-34-{thread}.jsonl"));
    for payload in [
        json!({"id":USER_THREAD,"source":"cli"}),
        json!({"id":thread,"source":"unknown"}),
    ] {
        std::fs::write(
            &rollout,
            json!({"type":"session_meta","payload":payload}).to_string(),
        )
        .unwrap();
        assert_eq!(
            source::classify(Some(home), thread),
            source::Origin::Unknown
        );
    }
    std::fs::write(&rollout, "partial metadata").unwrap();
    assert_eq!(
        source::classify(Some(home), thread),
        source::Origin::Unknown
    );
    let conn = rusqlite::Connection::open(home.join("state_6.sqlite")).unwrap();
    conn.execute_batch("CREATE TABLE threads (id TEXT,source TEXT,thread_source TEXT);")
        .unwrap();
    conn.execute(
        "INSERT INTO threads VALUES (?1,'cli','guardian_review')",
        [USER_THREAD],
    )
    .unwrap();
    assert_eq!(
        source::classify(Some(home), USER_THREAD),
        source::Origin::Internal
    );
}

#[tokio::test]
async fn bright_or_unknown_state_suppresses_both_events_and_manual_test_bypasses_gate() {
    let server = BarkServer::new();
    let f = Fixture::new(&server);
    for state in [Presence::Present, Presence::Unknown] {
        for (event, payload) in [
            (Event::Complete, complete("first")),
            (
                Event::Approval,
                json!({"hook_event_name":"PermissionRequest","session_id":"fixture-thread"})
                    .to_string(),
            ),
        ] {
            handle_event(&f.context, event, &payload, || state)
                .await
                .unwrap();
        }
    }
    assert!(server.messages.try_recv().is_err());
    assert!(!f.root.join("bark-deliveries.sqlite3").exists());
    dispatch(&f.root, BarkCommand::Test).await.unwrap();
    assert_eq!(server.receive()["group"], "usage-test");
    assert!(server.messages.try_recv().is_err());
}

#[tokio::test]
async fn completion_callbacks_send_immediately_without_coalescing_or_deduplication() {
    let server = BarkServer::new();
    let f = Fixture::new(&server);
    let first = complete("same-id");
    let last = complete("other-id");
    let started = Instant::now();
    let (a, b) = tokio::join!(
        handle_event(&f.context, Event::Complete, &first, || Presence::Away),
        handle_event(&f.context, Event::Complete, &last, || Presence::Away)
    );
    a.unwrap();
    b.unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    for _ in 0..2 {
        let message = server.receive();
        assert_eq!(message["level"], "active");
        assert!(!message.to_string().contains("private prompt"));
        assert!(message.to_string().contains("private assistant output"));
        assert_eq!(message["title"], "Codex · project · 本轮完成");
        assert!(!message.to_string().contains(USER_THREAD));
    }
    handle_event(&f.context, Event::Complete, &first, || Presence::Away)
        .await
        .unwrap();
    server.receive();
    assert!(!f.root.join("bark-deliveries.sqlite3").exists());
    assert!(server.messages.try_recv().is_err());
}

#[tokio::test]
async fn independent_instances_and_later_turns_keep_their_notifications() {
    let server = BarkServer::new();
    let a = Fixture::new(&server);
    let b = Context {
        root: a.root.clone(),
        home: a.context.home.clone(),
        account: "Fixture".into(),
        run_id: uuid::Uuid::new_v4().to_string(),
        previous_notify: vec![],
    };
    let payload = complete("first");
    let (first, second) = tokio::join!(
        handle_event(&a.context, Event::Complete, &payload, || Presence::Away),
        handle_event(&b, Event::Complete, &payload, || Presence::Away)
    );
    first.unwrap();
    second.unwrap();
    let first_group = server.receive()["group"].clone();
    let second_group = server.receive()["group"].clone();
    assert_ne!(first_group, second_group);
    handle_event(&a.context, Event::Complete, &complete("later"), || {
        Presence::Away
    })
    .await
    .unwrap();
    server.receive();
    assert!(server.messages.try_recv().is_err());
}

#[tokio::test]
async fn approval_failure_can_retry_and_callbacks_are_not_deduplicated() {
    let server = BarkServer::new();
    let f = Fixture::new(&server);
    let approval = json!({"hook_event_name":"PermissionRequest", "session_id":"fixture-session",
        "turn_id":"fixture-turn", "cwd":"/private/project", "tool_name":"exec_command",
        "tool_input":{"command":"secret-command", "justification":"需要下载依赖以运行验证"}})
    .to_string();
    server.code.store(400, std::sync::atomic::Ordering::Relaxed);
    assert!(
        handle_event(&f.context, Event::Approval, &approval, || Presence::Away)
            .await
            .is_err()
    );
    server.receive();
    server.code.store(200, std::sync::atomic::Ordering::Relaxed);
    handle_event(&f.context, Event::Approval, &approval, || Presence::Away)
        .await
        .unwrap();
    let message = server.receive();
    assert_eq!(message["level"], "timeSensitive");
    assert_eq!(message["title"], "Codex · project · 需要审批");
    assert!(
        message["body"]
            .as_str()
            .unwrap()
            .contains("exec_command 请求权限审批：需要下载依赖以运行验证")
    );
    assert!(!message.to_string().contains("secret-command"));
    handle_event(&f.context, Event::Approval, &approval, || Presence::Away)
        .await
        .unwrap();
    server.receive();
    handle_event(&f.context, Event::Complete, &complete("same"), || {
        Presence::Away
    })
    .await
    .unwrap();
    server.receive();
    let aliases = json!({"type":"agent-turn-complete", "thread_id":USER_THREAD, "turn_id":"same"})
        .to_string();
    handle_event(&f.context, Event::Complete, &aliases, || Presence::Away)
        .await
        .unwrap();
    server.receive();
    assert!(server.messages.try_recv().is_err());
}

#[tokio::test]
async fn logs_correlate_screen_decisions_and_transport_failures_without_private_content() {
    // Match production's separate callback process and isolate tracing's global
    // callsite registration from concurrent tests using other subscribers.
    if std::env::var_os("USAGE_BARK_LOG_CAPTURE_TEST").is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "codex_manager::bark::tests::logs_correlate_screen_decisions_and_transport_failures_without_private_content"])
            .env("USAGE_BARK_LOG_CAPTURE_TEST", "1")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::prelude::*;
    let server = BarkServer::new();
    let f = Fixture::new(&server);
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = LogCapture(buffer.clone());
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::filter::Targets::new().with_target("usage", tracing::Level::TRACE),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(move || writer.clone()),
        );
    let payload = json!({"hook_event_name":"PermissionRequest", "session_id":"private-thread", "turn_id":"approval-turn",
        "cwd":"/private/secret-project", "tool_input":{"command":"private-command"},
        "tool_name":"exec_command",
        "last_assistant_message":"private-answer"})
    .to_string();
    async {
        let internal = json!({"type":"agent-turn-complete", "thread-id":"01a135e1-229a-71e1-8fb4-5495b317c2f4", "turn-id":"title-turn", "client":"codex-tui", "last-assistant-message":"{\"title\":\"修复通知\"}"});
        handle_event(&f.context, Event::Complete, &internal.to_string(), || Presence::Away)
            .await.unwrap();
        handle_event(&f.context, Event::Complete, &complete("main-turn"), || Presence::Present)
            .await.unwrap();
        handle_event(&f.context, Event::Approval, &payload, || Presence::Present)
            .await
            .unwrap();
        server.code.store(400, std::sync::atomic::Ordering::Relaxed);
        assert!(
            handle_event(&f.context, Event::Approval, &payload, || Presence::Away)
                .await
                .is_err()
        );
    }
    .with_subscriber(subscriber)
    .await;
    server.receive();
    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    for private in [
        "/private/secret-project",
        "private-command",
        "private-answer",
        "fixture-bark-device-secret",
    ] {
        assert!(!logs.contains(private));
    }
    let events: Vec<Value> = logs
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for name in [
        "bark_event_received",
        "bark_thread_source",
        "bark_source_lookup",
        "bark_source_index",
        "bark_source_metadata",
        "bark_completion_content",
        "bark_notification_prepared",
        "bark_suppressed",
        "bark_request_started",
        "bark_response_received",
        "bark_request_failed",
        "operation_failed",
    ] {
        assert!(
            events
                .iter()
                .any(|row| row["fields"]["event"] == name
                    && row["span"]["run_id"] == f.context.run_id),
            "missing correlated event {name}; metadata: {:?}",
            events
                .iter()
                .map(|row| (&row["fields"]["event"], &row["span"]["run_id"]))
                .collect::<Vec<_>>()
        );
    }
    assert!(
        events
            .iter()
            .any(|row| row["fields"]["reason"] == "screen_active")
    );
    assert!(
        events
            .iter()
            .any(|row| row["fields"]["reason"] == "untracked_thread")
    );
    assert!(
        events
            .iter()
            .any(|row| row["fields"]["backend_code"] == 400)
    );
    assert!(
        events
            .iter()
            .any(|row| row["fields"]["stage"] == "bark_request")
    );
    assert!(events.iter().any(|row| {
        let fields = &row["fields"];
        fields["thread_id"] == "private-thread"
            && fields["turn_id"] == "approval-turn"
            && fields["tool_name"] == "exec_command"
            && fields["callback_type"] == "PermissionRequest"
    }));
    assert!(
        events
            .iter()
            .any(|row| row["fields"]["client"] == "codex-tui")
    );
    assert!(
        events
            .iter()
            .any(|row| row["fields"]["assistant_preview"] == "{\"title\":\"修复通知\"}")
    );
    assert!(events.iter().any(|row| {
        let fields = &row["fields"];
        fields["basis"] == "index"
            && fields["thread_source"] == "user"
            && fields["session_source"] == "\"cli\""
    }));
    for basis in ["index", "rollout"] {
        assert!(
            events.iter().any(|row| row["fields"]["basis"] == basis
                && row["fields"]["result"] == "thread_not_found")
        );
    }
}

#[tokio::test]
async fn completion_excerpt_is_unicode_safe_bounded_and_has_a_missing_reply_fallback() {
    let server = BarkServer::new();
    let f = Fixture::new(&server);
    let reply = format!("已完成通知修复。\n\t{}", "验证通过🎉".repeat(100));
    let payload = json!({"type":"agent-turn-complete", "thread_id":USER_THREAD,
        "turn_id":"unicode", "cwd":"/private/项目", "last_assistant_message":reply});
    handle_event(&f.context, Event::Complete, &payload.to_string(), || {
        Presence::Away
    })
    .await
    .unwrap();
    let message = server.receive();
    assert_eq!(message["title"], "Codex · 项目 · 本轮完成");
    let body = message["body"].as_str().unwrap();
    let excerpt = body.strip_suffix("\n账号：Fixture").unwrap();
    assert_eq!(excerpt.chars().count(), 400);
    assert!(excerpt.starts_with("已完成通知修复。 验证通过🎉"));
    assert!(excerpt.ends_with('…'));
    for empty in [Value::Null, " \n\t ".into()] {
        let mut payload = payload.clone();
        payload["last_assistant_message"] = empty;
        handle_event(&f.context, Event::Complete, &payload.to_string(), || {
            Presence::Away
        })
        .await
        .unwrap();
        assert!(
            server.receive()["body"]
                .as_str()
                .unwrap()
                .contains("未提供回复摘要")
        );
    }
    assert_eq!(content::excerpt("中文🎉", 0), "");
    assert_eq!(content::excerpt("中文🎉", 3), "中文🎉");
}
