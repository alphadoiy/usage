use super::*;
use crate::sessions::AgentKind;

struct Fixture {
    root: PathBuf,
    session: Session,
}
impl Fixture {
    fn new(provider: AgentKind) -> Self {
        let root = std::env::temp_dir().join(format!("usage-preview-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        Self {
            session: Session {
                agent: None,
                provider,
                instance: provider.id().into(),
                id: uuid::Uuid::new_v4().to_string(),
                cwd: root.clone(),
                title: "fixture".into(),
                recency_at_ms: 1,
                home: root.clone(),
            },
            root,
        }
    }
    fn write(&self, relative: impl AsRef<Path>, rows: &[Value]) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            rows.iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn claude_preview_ignores_tool_results_thoughts_and_metadata_without_writing_logs() {
    let f = Fixture::new(AgentKind::CLAUDE);
    let path = f.write(format!("projects/project/{}.jsonl", f.session.id), &[
        serde_json::json!({"type":"user","message":{"content":"修复布局"}}),
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"thinking","thinking":"private reasoning"},{"type":"text","text":"已完成\n布局修复"}]}}),
        serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","content":"private tool output"}]}}),
        serde_json::json!({"type":"user","isMeta":true,"message":{"content":"internal instructions"}}),
    ]);
    let before = fs::read(&path).unwrap();
    let preview = load(&f.session, &Cancellation::new()).unwrap();
    assert_eq!(preview.request.as_deref(), Some("修复布局"));
    assert_eq!(preview.response.as_deref(), Some("已完成 布局修复"));
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn codex_preview_resolves_packed_rollouts_by_metadata_and_does_not_leak_another_session() {
    let f = Fixture::new(AgentKind::CODEX);
    let rows = [
        serde_json::json!({"type":"session_meta","payload":{"id":f.session.id}}),
        serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"latest question"}}),
        serde_json::json!({"type":"event_msg","payload":{"type":"agent_message","message":"latest answer"}}),
    ];
    let text = rows.iter().map(|r| format!("{r}\n")).collect::<String>();
    let dir = f.root.join("sessions/day");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("opaque.jsonl.zst"),
        ruzstd::encoding::compress_to_vec(
            text.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        ),
    )
    .unwrap();
    f.write("sessions/day/wrong.jsonl", &[serde_json::json!({"type":"session_meta","payload":{"id":uuid::Uuid::new_v4().to_string()}}), serde_json::json!({"type":"event_msg","payload":{"type":"agent_message","message":"other account"}})]);
    let preview = load(&f.session, &Cancellation::new()).unwrap();
    assert_eq!(preview.request.as_deref(), Some("latest question"));
    assert_eq!(preview.response.as_deref(), Some("latest answer"));
}

#[test]
fn new_request_clears_previous_reply_even_when_the_question_is_repeated() {
    let f = Fixture::new(AgentKind::CODEX);
    f.write("sessions/day/session.jsonl", &[
        serde_json::json!({"type":"session_meta","payload":{"id":f.session.id}}),
        serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"again"}]}}),
        serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"old answer"}]}}),
        serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"again"}}),
    ]);
    let preview = load(&f.session, &Cancellation::new()).unwrap();
    assert_eq!(preview.request.as_deref(), Some("again"));
    assert_eq!(preview.response, None);
}

#[test]
fn grok_preview_combines_text_chunks_and_keeps_only_the_latest_exchange() {
    let f = Fixture::new(AgentKind::GROK);
    f.write(
        "sessions/project/opaque/summary.json",
        &[serde_json::json!({"info":{"id":f.session.id}})],
    );
    let chunk = |kind, text| serde_json::json!({"params":{"update":{"sessionUpdate":kind,"content":{"type":"text","text":text}}}});
    f.write(
        "sessions/project/opaque/updates.jsonl",
        &[
            chunk("user_message_chunk", "old"),
            chunk("agent_message_chunk", "old response"),
            chunk("user_message_chunk", "最新"),
            chunk("user_message_chunk", "请求"),
            chunk("agent_message_chunk", "新的"),
            chunk("agent_message_chunk", "回复"),
        ],
    );
    let preview = load(&f.session, &Cancellation::new()).unwrap();
    assert_eq!(preview.request.as_deref(), Some("最新请求"));
    assert_eq!(preview.response.as_deref(), Some("新的回复"));
}

#[test]
fn oversized_records_are_skipped_and_cancellation_stops_discovery() {
    let mut text = "x".repeat(LINE_BYTES as usize + 1).into_bytes();
    text.extend_from_slice(b"\n{\"ok\":true}\n");
    let mut reader = std::io::Cursor::new(text);
    assert!(bounded_line(&mut reader).unwrap().is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&bounded_line(&mut reader).unwrap()).unwrap()["ok"],
        true
    );
    let f = Fixture::new(AgentKind::CLAUDE);
    let cancel = Cancellation::new();
    cancel.cancel();
    assert!(load(&f.session, &cancel).is_err());
}
