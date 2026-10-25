use super::*;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::Write;

struct Fixture {
    root: PathBuf,
    service: Service,
    binding: Binding,
}
impl Fixture {
    fn new(agent: &str) -> Self {
        let root = std::env::temp_dir().join(format!("usage-performance-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let binding = Binding {
            provider: "work".into(),
            source: source(agent).unwrap(),
            agent: agent.into(),
            home,
            revision: "1".into(),
            expected_upstream: None,
            gateway_observations: true,
        };
        let service = Service::at(root.join("index/performance.sqlite3"));
        Self {
            root,
            service,
            binding,
        }
    }
    fn file(&self, name: &str, text: &str) -> PathBuf {
        let dir = self.binding.home.join(if self.binding.agent == "claude" {
            "projects"
        } else {
            "sessions"
        });
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path
    }
    fn update(&self) -> Batch {
        let mut all = Batch::default();
        for _ in 0..1000 {
            let batch = self
                .service
                .refresh(&self.binding, self.binding.source, &AtomicBool::new(false))
                .unwrap();
            all.bytes += batch.bytes;
            all.files += batch.files;
            all.skipped += batch.skipped;
            if !batch.more {
                return all;
            }
        }
        panic!("bounded import did not converge")
    }
    fn snapshot(&self) -> Snapshot {
        self.service
            .snapshot(&self.binding, Period::Month, None, 0, at(30))
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn at(sec: u32) -> i64 {
    chrono::DateTime::parse_from_rfc3339(&format!("2026-10-18T00:00:{sec:02}Z"))
        .unwrap()
        .timestamp_millis()
}
fn line(sec: u32, kind: &str, payload: serde_json::Value) -> String {
    json!({"timestamp":format!("2026-10-18T00:00:{sec:02}Z"),"type":kind,"payload":payload})
        .to_string()
        + "\n"
}
fn codex_session(session: &str, model: &str) -> String {
    line(
        0,
        "session_meta",
        json!({"id":session,"model_provider":"relay"}),
    ) + &line(0, "turn_context", json!({"model":model}))
        + &line(
            0,
            "event_msg",
            json!({"type":"task_started","turn_id":"turn-a"}),
        )
        + &line(
            10,
            "event_msg",
            json!({"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":500,"reasoning_output_tokens":200},"last_token_usage":{"output_tokens":500,"reasoning_output_tokens":200}}}),
        )
}
fn complete() -> String {
    line(
        11,
        "event_msg",
        json!({"type":"task_complete","turn_id":"turn-a","duration_ms":10000,"time_to_first_token_ms":2000}),
    )
}

fn load_request(f: &Fixture) -> LoadRequest {
    LoadRequest {
        binding: f.binding.clone(),
        period: Period::Month,
        model: None,
        page: 0,
        now: at(30),
        rediscover: false,
    }
}

async fn load_update(rx: &mut tokio::sync::mpsc::UnboundedReceiver<LoadUpdate>) -> LoadUpdate {
    tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn loader_publishes_cached_then_updated_data_for_one_fixed_query() {
    let f = Fixture::new("codex");
    let path = f.file("a.jsonl", &codex_session("a", "model-a"));
    f.update();
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(complete().as_bytes())
        .unwrap();
    let service = std::sync::Arc::new(Service::at(f.root.join("index/performance.sqlite3")));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _task = service.start_load(
        &tokio::runtime::Handle::current(),
        load_request(&f),
        move |update| tx.send(update).is_ok(),
    );
    let cached = load_update(&mut rx).await;
    assert!(cached.loading);
    assert_eq!(cached.result.unwrap().models[0].timed_samples, 0);
    let complete_update = load_update(&mut rx).await;
    assert!(!complete_update.loading);
    let snapshot = complete_update.result.unwrap();
    assert_eq!(snapshot.models[0].timed_samples, 1);
    assert_eq!(snapshot.read_bytes, complete().len() as u64);
    assert!(!snapshot.partial);
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn closing_loader_consumer_after_cached_data_prevents_log_import() {
    let f = Fixture::new("codex");
    f.file("a.jsonl", &codex_session("a", "model-a"));
    let path = f.root.join("index/performance.sqlite3");
    let service = std::sync::Arc::new(Service::at(path.clone()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _task = service.start_load(
        &tokio::runtime::Handle::current(),
        load_request(&f),
        move |update| {
            tx.send(update).unwrap();
            false
        },
    );
    assert!(load_update(&mut rx).await.loading);
    assert!(rx.recv().await.is_none());
    let db = rusqlite::Connection::open(path).unwrap();
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM perf_files", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn dropping_loader_before_first_poll_never_starts_disk_work() {
    let f = Fixture::new("codex");
    let path = f.root.join("index/performance.sqlite3");
    let service = std::sync::Arc::new(Service::at(path.clone()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = service.start_load(
        &tokio::runtime::Handle::current(),
        load_request(&f),
        move |update| tx.send(update).is_ok(),
    );
    let cancel = task.cancellation();
    drop(task);
    assert!(cancel.is_cancelled());
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!path.exists());
}

#[tokio::test]
async fn loader_storage_errors_and_parser_panics_both_finish_loading() {
    for panic in [false, true] {
        let mut f = Fixture::new("codex");
        f.file("a.jsonl", &codex_session("a", "model-a"));
        if panic {
            f.binding.source.parser = |_| panic!("synthetic parser panic");
        } else {
            fs::write(f.root.join("index"), b"block index").unwrap();
        }
        let service = std::sync::Arc::new(Service::at(f.root.join("index/performance.sqlite3")));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _task = service.start_load(
            &tokio::runtime::Handle::current(),
            load_request(&f),
            move |update| tx.send(update).is_ok(),
        );
        assert!(load_update(&mut rx).await.loading);
        let final_update = load_update(&mut rx).await;
        assert!(!final_update.loading);
        assert!(final_update.result.is_err());
        assert!(rx.recv().await.is_none());
    }
}

#[tokio::test]
async fn dropping_loader_cancels_an_in_flight_blocking_batch() {
    struct PausedParser(Option<PathBuf>);
    impl RequestParser for PausedParser {
        fn feed(&mut self, line: &[u8]) -> Result<Vec<Observation>, String> {
            let paths: Vec<PathBuf> = serde_json::from_slice(line).unwrap();
            self.0 = Some(paths[2].clone());
            writeln!(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&paths[0])
                    .unwrap(),
                "read"
            )
            .unwrap();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !paths[1].exists() && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(Vec::new())
        }
        fn checkpoint(&self) -> serde_json::Value {
            serde_json::Value::Null
        }
    }
    impl Drop for PausedParser {
        fn drop(&mut self) {
            if let Some(path) = &self.0 {
                let _ = fs::write(path, b"finished");
            }
        }
    }
    let mut f = Fixture::new("codex");
    f.binding.source.parser = |_| Box::new(PausedParser(None));
    let paths = [
        f.root.join("started"),
        f.root.join("release"),
        f.root.join("finished"),
    ];
    let text = serde_json::to_string(&paths).unwrap() + "\n";
    f.file("a.jsonl", &text.repeat(3));
    let service = std::sync::Arc::new(Service::at(f.root.join("index/performance.sqlite3")));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = service.start_load(
        &tokio::runtime::Handle::current(),
        load_request(&f),
        move |update| tx.send(update).is_ok(),
    );
    assert!(load_update(&mut rx).await.loading);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !paths[0].exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(task);
    fs::write(&paths[1], b"release worker").unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !paths[2].exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fs::read_to_string(&paths[0]).unwrap().lines().count(), 1);
    assert!(
        rx.recv().await.is_none(),
        "no progress is published after cancellation"
    );
}

#[test]
fn codex_incremental_native_timing_restart_and_archive_copy_count_once() {
    let f = Fixture::new("codex");
    let path = f.file("a.jsonl", &codex_session("session-a", "observed-model"));
    let first = f.update();
    assert!(first.bytes > 0);
    let before = f.snapshot();
    assert_eq!(before.models[0].calls, 1);
    assert_eq!(before.models[0].timed_samples, 0);
    assert_eq!(f.update().bytes, 0, "unchanged logs must not be reread");
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(complete().as_bytes())
        .unwrap();
    let reopened = Service::at(f.root.join("index/performance.sqlite3"));
    let batch = reopened
        .refresh(&f.binding, f.binding.source, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(batch.bytes, complete().len() as u64);
    let after = reopened
        .snapshot(&f.binding, Period::Day, None, 0, at(30))
        .unwrap();
    let m = &after.models[0];
    assert_eq!(m.model, "observed-model");
    assert_eq!(m.calls, 1);
    assert_eq!(m.mean_ttft_ms, Some(2000.0));
    assert_eq!(m.timed_samples, 1);
    assert!(
        m.speed().is_none(),
        "reasoning without first-text timing is not measured speed"
    );
    assert_eq!(
        m.floor(),
        Some(37.5),
        "300 answer tokens over the 8-second post-first-token window"
    );
    let archived = f.binding.home.join("archived_sessions");
    fs::create_dir_all(&archived).unwrap();
    fs::copy(&path, archived.join("copy.jsonl")).unwrap();
    f.service.rediscover(&f.binding);
    f.update();
    assert_eq!(f.snapshot().models[0].calls, 1);
}

#[test]
fn pending_lines_replacement_cancellation_and_large_lines_are_bounded() {
    let f = Fixture::new("codex");
    let data = codex_session("session-a", "model-a");
    let path = f.file("a.jsonl", &data[..data.len() - 1]);
    f.update();
    assert!(f.snapshot().models.is_empty());
    assert_eq!(f.update().bytes, 0);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    f.update();
    assert_eq!(f.snapshot().models[0].calls, 1);
    fs::write(&path, codex_session("session-b", "model-b")).unwrap();
    f.update();
    assert_eq!(
        f.snapshot().models.len(),
        2,
        "replacement must reset parser state, retaining indexed history"
    );
    let huge = f.file(
        "huge.jsonl",
        &("x".repeat(5 * 1024 * 1024) + "\n" + &codex_session("session-c", "model-c")),
    );
    f.service.rediscover(&f.binding);
    let batch = f
        .service
        .refresh(&f.binding, f.binding.source, &AtomicBool::new(false))
        .unwrap();
    assert!(batch.bytes <= 4 * 1024 * 1024);
    assert!(batch.more);
    let stopped = f
        .service
        .refresh(&f.binding, f.binding.source, &AtomicBool::new(true))
        .unwrap();
    assert_eq!(stopped.bytes, 0);
    f.update();
    assert!(f.snapshot().models.iter().any(|m| m.model == "model-c"));
    fs::remove_file(huge).unwrap();
    f.update();
}

#[test]
fn compressed_logs_share_plain_identity_and_import_without_a_plain_twin() {
    let f = Fixture::new("codex");
    let text = codex_session("compressed-session", "model-a");
    let plain = f.file("a.jsonl", &text);
    let archive = plain.with_extension("jsonl.zst");
    fs::write(&archive, b"still writing").unwrap();
    f.update();
    assert_eq!(f.snapshot().models[0].calls, 1);
    assert_eq!(f.snapshot().models[0].timed_samples, 0);
    assert_eq!(f.update().bytes, 0);
    let finished = text + &complete();
    fs::write(
        &archive,
        ruzstd::encoding::compress_to_vec(
            finished.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        ),
    )
    .unwrap();
    fs::remove_file(&plain).unwrap();
    assert_eq!(f.update().bytes, finished.len() as u64);
    assert_eq!(f.snapshot().models[0].calls, 1);
    assert_eq!(f.snapshot().models[0].timed_samples, 1);
    assert_eq!(f.update().bytes, 0);
    let fresh = Service::at(f.root.join("fresh.sqlite3"));
    let imported = fresh
        .refresh(&f.binding, f.binding.source, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(imported.bytes, finished.len() as u64);
    assert_eq!(
        fresh
            .snapshot(&f.binding, Period::Month, None, 0, at(30))
            .unwrap()
            .models[0]
            .calls,
        1
    );
    let reopened = Service::at(f.root.join("index/performance.sqlite3"));
    assert_eq!(
        reopened
            .refresh(&f.binding, f.binding.source, &AtomicBool::new(false))
            .unwrap()
            .bytes,
        0
    );
}

#[test]
fn compressed_batches_resume_skip_large_lines_and_replay_safely_after_restart() {
    let mut f = Fixture::new("codex");
    let text = codex_session("before", "model-a")
        + &"x".repeat(5 * 1024 * 1024)
        + "\n"
        + &codex_session("after", "model-b")
        + &complete();
    let path = f.file("large.jsonl.zst", "");
    fs::write(
        path,
        ruzstd::encoding::compress_to_vec(
            text.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        ),
    )
    .unwrap();
    let stopped = f
        .service
        .refresh(&f.binding, f.binding.source, &AtomicBool::new(true))
        .unwrap();
    assert_eq!(stopped.bytes, 0);
    let first = f
        .service
        .refresh(&f.binding, f.binding.source, &AtomicBool::new(false))
        .unwrap();
    assert!(first.more);
    assert!(first.bytes > 0 && first.bytes <= 4 * 1024 * 1024);
    f.service = Service::at(f.root.join("index/performance.sqlite3"));
    let replay = f.update();
    assert_eq!(replay.bytes, text.len() as u64);
    assert_eq!(replay.skipped, 1);
    let result = f.snapshot();
    assert_eq!(result.models.len(), 2);
    assert_eq!(result.models.iter().map(|m| m.calls).sum::<u64>(), 2);
    assert_eq!(f.update().bytes, 0);
}

#[test]
fn provider_home_and_upstream_identity_are_isolated() {
    let a = Fixture::new("codex");
    let b = Fixture::new("codex");
    a.file(
        "a.jsonl",
        &(codex_session("same-session", "model-a") + &complete()),
    );
    b.file(
        "b.jsonl",
        &(codex_session("same-session", "model-b") + &complete()),
    );
    a.update();
    b.update();
    assert_ne!(a.binding.scope(), b.binding.scope());
    assert_eq!(a.snapshot().models[0].model, "model-a");
    assert_eq!(b.snapshot().models[0].model, "model-b");
    let mut wrong = a.binding.clone();
    wrong.expected_upstream = Some("other-relay".into());
    let result = a
        .service
        .snapshot(&wrong, Period::Day, None, 0, at(30))
        .unwrap();
    assert!(result.models.is_empty());
    assert_eq!(result.unmatched, 1);
    let mut right = a.binding.clone();
    right.expected_upstream = Some("relay".into());
    assert_eq!(
        a.service
            .snapshot(&right, Period::Day, None, 0, at(30))
            .unwrap()
            .models[0]
            .calls,
        1
    );
    let db = rusqlite::Connection::open(a.root.join("index/performance.sqlite3")).unwrap();
    let cursor: String = db
        .query_row("SELECT cursor FROM perf_files", [], |r| r.get(0))
        .unwrap();
    let checkpoint: serde_json::Value = serde_json::from_str(&cursor).unwrap();
    assert_eq!(
        checkpoint["fingerprint"].as_array().unwrap().len(),
        32,
        "only a digest of log boundaries may be persisted"
    );
}

#[test]
fn multiple_calls_do_not_inherit_turn_first_token_and_bad_timings_are_unknown() {
    let mut parser = (source("codex").unwrap().parser)(serde_json::Value::Null);
    let mut calls = Vec::new();
    for ln in codex_session("s", "m").lines() {
        calls.extend(parser.feed(ln.as_bytes()).unwrap());
    }
    calls.extend(parser.feed(line(12,"event_msg",json!({"type":"token_count","info":{"total_token_usage":{"output_tokens":800},"last_token_usage":{"output_tokens":300}}})).as_bytes()).unwrap());
    assert!(parser.feed(complete().as_bytes()).unwrap().is_empty());
    assert!(calls.iter().all(|c| c.ttft_ms.is_none()));
    let call = Observation {
        output: 500,
        duration_ms: Some(10),
        ..Default::default()
    };
    assert!(call.speed().is_none());
}

#[test]
fn claude_chunks_merge_without_inventing_first_token_timing() {
    let f = Fixture::new("claude");
    let user=json!({"type":"user","uuid":"user-a","parentUuid":null,"timestamp":"2026-10-18T00:00:00Z","message":{"content":"large user text is not retained"}}).to_string();
    let first=json!({"type":"assistant","uuid":"part-a","parentUuid":"user-a","sessionId":"s","timestamp":"2026-10-18T00:00:05Z","apiBlockIndex":0,"thinkingDurationMs":2000,"message":{"id":"msg-a","model":"claude-model","content":[{"type":"thinking","thinking":"PRIVATE-THINKING"}],"usage":{"output_tokens":200}}}).to_string();
    let last=json!({"type":"assistant","uuid":"part-b","parentUuid":"part-a","sessionId":"s","timestamp":"2026-10-18T00:00:10Z","apiBlockIndex":1,"message":{"id":"msg-a","model":"claude-model","content":[{"type":"text","text":"PRIVATE-ANSWER"}],"usage":{"output_tokens":600}}}).to_string();
    f.file("a.jsonl", &(user + "\n" + &first + "\n" + &last + "\n"));
    f.update();
    let snapshot = f.snapshot();
    let m = &snapshot.models[0];
    assert_eq!(m.calls, 1);
    assert!(m.mean_ttft_ms.is_none());
    assert_eq!(m.timed_samples, 0);
    assert!(m.speed().is_none());
    assert!(
        m.floor().is_none(),
        "short local replies do not establish a whole-request floor"
    );
    let db = rusqlite::Connection::open(f.root.join("index/performance.sqlite3")).unwrap();
    let state: String = db
        .query_row("SELECT cursor FROM perf_files", [], |r| r.get(0))
        .unwrap();
    assert!(!state.contains("PRIVATE"));
    assert!(!state.contains("large user text"));
}

#[test]
fn means_and_request_pages_keep_generation_and_floor_samples_separate() {
    let f = Fixture::new("codex");
    let mut text = String::new();
    for i in 0..45 {
        text += &codex_session(&format!("s{i}"), "m");
        text += &line(
            11,
            "event_msg",
            json!({"type":"task_complete","turn_id":"turn-a","duration_ms":if i<25 {2000}else{10000},"time_to_first_token_ms":if i<25{1000}else{3000}}),
        );
    }
    f.file("many.jsonl", &text);
    f.update();
    let data = f.snapshot();
    assert_eq!(data.models[0].calls, 45);
    assert!((data.models[0].mean_ttft_ms.unwrap() - (85_000 / 45) as f64).abs() < 0.001);
    assert!(data.models[0].speed().is_none());
    assert!((data.models[0].floor().unwrap() - 13_500_000.0 / 165_000.0).abs() < 0.001);
    let mean = f
        .service
        .summary(&f.binding, Period::Day, at(30))
        .unwrap()
        .unwrap();
    assert_eq!(mean.calls, 45);
    assert_eq!(mean.timed_samples, 45);
    assert!((mean.mean_ttft_ms.unwrap() - (85_000 / 45) as f64).abs() < 0.001);
    assert!(mean.speed().is_none());
    assert_eq!(mean.floor(), data.models[0].floor());
    assert_eq!(mean.speed_samples, 0);
    assert_eq!(mean.floor_samples, 45);
    assert_eq!(data.summary.mean_ttft_ms, mean.mean_ttft_ms);
    let mut wrong = f.binding.clone();
    wrong.expected_upstream = Some("other".into());
    let mean = f
        .service
        .summary(&wrong, Period::Day, at(30))
        .unwrap()
        .unwrap();
    assert_eq!(mean.calls, 0);
    assert!(mean.mean_ttft_ms.is_none());
    assert!(mean.speed().is_none());
    assert_eq!(data.recent.len(), 40);
    let page = f
        .service
        .snapshot(&f.binding, Period::Day, Some("m"), 1, at(30))
        .unwrap();
    assert_eq!(page.recent.len(), 5);
    assert!(
        !page
            .recent
            .iter()
            .any(|c| data.recent.iter().any(|a| a.id == c.id))
    );
}

#[test]
fn homepage_summary_does_not_create_index_or_discover_logs() {
    let f = Fixture::new("codex");
    f.file("unindexed.jsonl", &(codex_session("s", "m") + &complete()));
    assert!(
        f.service
            .summary(&f.binding, Period::Day, at(30))
            .unwrap()
            .is_none()
    );
    assert!(!f.root.join("index").exists());
    f.update();
    let db = rusqlite::Connection::open(f.root.join("index/performance.sqlite3")).unwrap();
    let before: (String, i64) = db
        .query_row("SELECT cursor,checked FROM perf_files", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    f.file(
        "not-discovered.jsonl",
        &(codex_session("new", "new-model") + &complete()),
    );
    assert_eq!(
        f.service
            .summary(&f.binding, Period::Day, at(30))
            .unwrap()
            .unwrap()
            .calls,
        1
    );
    let after: (String, i64) = db
        .query_row("SELECT cursor,checked FROM perf_files", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(
        before, after,
        "homepage must not advance or discover cursors"
    );
}

#[test]
fn ten_thousand_requests_are_indexed_in_bounded_batches_and_idle_reads_no_body() {
    let f = Fixture::new("codex");
    let mut text = String::new();
    for i in 0..10_000 {
        text += &codex_session(&format!("session-{i}"), &format!("model-{}", i % 5));
        text += &complete();
    }
    f.file("large.jsonl", &text);
    let began = std::time::Instant::now();
    let mut bytes = 0;
    let mut batches = 0;
    loop {
        let batch = f
            .service
            .refresh(&f.binding, f.binding.source, &AtomicBool::new(false))
            .unwrap();
        assert!(batch.bytes <= 4 * 1024 * 1024);
        bytes += batch.bytes;
        batches += 1;
        if !batch.more {
            break;
        }
        assert!(batches < 1000);
    }
    let import_ms = began.elapsed().as_millis();
    let began = std::time::Instant::now();
    let data = f.snapshot();
    let query_ms = began.elapsed().as_millis();
    assert_eq!(data.models.len(), 5);
    assert_eq!(data.models.iter().map(|m| m.calls).sum::<u64>(), 10_000);
    assert_eq!(
        data.models.iter().map(|m| m.timed_samples).sum::<u64>(),
        10_000
    );
    assert_eq!(data.recent.len(), 40);
    assert_eq!(bytes, text.len() as u64);
    assert!(batches > 1);
    let idle = f.update();
    assert_eq!(idle.bytes, 0);
    eprintln!(
        "performance fixture: {} bytes, {batches} batches, import {import_ms}ms, indexed query {query_ms}ms, unchanged body read {} bytes",
        text.len(),
        idle.bytes
    );
}

#[test]
fn inherited_fork_usage_and_missing_first_token_evidence_are_not_samples() {
    let mut parser = (source("codex").unwrap().parser)(serde_json::Value::Null);
    parser
        .feed(
            line(
                20,
                "session_meta",
                json!({"id":"fork","forked_from_id":"parent","model_provider":"relay"}),
            )
            .as_bytes(),
        )
        .unwrap();
    parser
        .feed(line(0, "turn_context", json!({"model":"m"})).as_bytes())
        .unwrap();
    let inherited = line(
        10,
        "event_msg",
        json!({"type":"token_count","info":{"total_token_usage":{"output_tokens":500},"last_token_usage":{"output_tokens":500}}}),
    );
    assert!(parser.feed(inherited.as_bytes()).unwrap().is_empty());
    let next = line(
        25,
        "event_msg",
        json!({"type":"token_count","info":{"total_token_usage":{"output_tokens":800}}}),
    );
    let calls = parser.feed(next.as_bytes()).unwrap();
    assert_eq!(calls[0].output, 300);
    assert!(calls[0].ttft_ms.is_none());
    let mut claude = (source("claude").unwrap().parser)(serde_json::Value::Null);
    let event = json!({"type":"assistant","uuid":"part","parentUuid":null,"sessionId":"s","timestamp":"2026-10-18T00:00:05Z","message":{"id":"msg","model":"m","content":[{"type":"text","text":"not a first-token timestamp"}],"usage":{"output_tokens":500}}});
    let calls = claude.feed(event.to_string().as_bytes()).unwrap();
    assert!(calls[0].ttft_ms.is_none());
    assert!(calls[0].duration_ms.is_none());
    assert!(calls[0].speed().is_none());
}

#[test]
fn claude_interleaved_messages_keep_their_start_and_missing_first_blocks_stay_unknown() {
    let mut parser = (source("claude").unwrap().parser)(serde_json::Value::Null);
    parser.feed(json!({"type":"user","uuid":"user","parentUuid":null,"timestamp":"2026-10-18T00:00:00Z"}).to_string().as_bytes()).unwrap();
    let block = |sec: u32, id: &str, uuid: &str, parent: &str, index: u32, output: u64| {
        json!({"type":"assistant","uuid":uuid,"parentUuid":parent,"sessionId":"s","timestamp":format!("2026-10-18T00:00:{sec:02}Z"),"apiBlockIndex":index,"message":{"id":id,"model":"m","content":[{"type":"text","text":"not retained"}],"usage":{"output_tokens":output}}}).to_string()
    };
    parser
        .feed(block(2, "m1", "p1", "user", 0, 200).as_bytes())
        .unwrap();
    parser
        .feed(block(4, "m2", "p2", "p1", 0, 300).as_bytes())
        .unwrap();
    // m1 returns after m2, including across a persisted checkpoint.
    let mut parser = (source("claude").unwrap().parser)(parser.checkpoint());
    let calls = parser
        .feed(block(10, "m1", "p3", "p1", 1, 600).as_bytes())
        .unwrap();
    assert_eq!(calls[0].duration_ms, Some(10000));
    let calls = parser
        .feed(block(12, "m2", "p4", "p2", 1, 800).as_bytes())
        .unwrap();
    assert_eq!(
        calls[0].duration_ms,
        Some(10000),
        "m2 began at p1, not the earlier user prompt"
    );
    let calls = parser
        .feed(block(15, "incomplete", "p5", "p4", 2, 900).as_bytes())
        .unwrap();
    assert!(calls[0].duration_ms.is_none());
    let calls = parser
        .feed(block(20, "incomplete", "p6", "p5", 3, 1000).as_bytes())
        .unwrap();
    assert!(calls[0].speed().is_none());
    let synthetic = block(21, "synthetic", "p7", "p6", 0, 0)
        .replace("\"model\":\"m\"", "\"model\":\"<synthetic>\"");
    assert!(parser.feed(synthetic.as_bytes()).unwrap().is_empty());
}

fn insert_timing(
    f: &Fixture,
    id: &str,
    output: u64,
    reasoning: u64,
    ms: i64,
    ttft: Option<i64>,
    native: bool,
) {
    f.snapshot(); // Create the private test index, never a user database.
    let db = rusqlite::Connection::open(f.root.join("index/performance.sqlite3")).unwrap();
    db.execute("INSERT INTO perf_calls(scope,id,session,at,model,output,reasoning,duration,ttft,native) VALUES(?1,?2,'fixture',?3,?2,?4,?5,?6,?7,?8)",
        rusqlite::params![f.binding.scope(), id, at(10), output, reasoning, ms, ttft, native]).unwrap();
}

#[test]
fn magpie_codex_speed_and_floor_reference_cases_match_in_detail_and_aggregates() {
    // Reference vectors: Magpie internal/usage/speed_count_test.go,
    // TestCodexRowsTellAFloor and TestCodexTurnMeasuredOnlyOverASecondAndAHundredTokens.
    let f = Fixture::new("codex");
    let cases = [
        (496, 224, 11055, 10228, None, None),
        (1300, 200, 11500, 11000, None, Some((1300, 11500))),
        (1200, 200, 12000, 2000, None, Some((1000, 10000))),
        (260, 200, 5000, 2000, None, None),
        (500, 0, 5000, 1000, Some((500, 4000)), None),
        (500, 100, 2050, 2000, None, None),
        (300, 300, 9000, 2000, None, None),
        (1000, 1000, 9000, 2000, None, Some((1000, 9000))),
        (1500, 600, 30000, 0, None, Some((1500, 30000))),
        (800, 100, 20000, 0, None, None),
        (380, 0, 7000, 6800, None, None),
        (1200, 0, 10000, 9500, None, Some((1200, 10000))),
        (60, 0, 3000, 1000, None, None),
        (100, 0, 7800, 6800, Some((100, 1000)), None),
        (99, 0, 7800, 6800, None, None),
        (400, 0, 7799, 6800, None, None),
        (1000, 0, 7799, 6800, None, Some((1000, 7799))),
        // Wide upper speed cap, inclusive at exactly 10,000 tok/s.
        (10000, 0, 2000, 1000, Some((10000, 1000)), None),
        (20001, 0, 2000, 1000, None, None),
    ];
    let mut decode = (0, 0, 0);
    let mut floor = (0, 0, 0);
    for (i, (output, reasoning, ms, ttft, generation, lower)) in cases.into_iter().enumerate() {
        let name = format!("case-{i}");
        insert_timing(
            &f,
            &name,
            output,
            reasoning,
            ms,
            (ttft > 0).then_some(ttft),
            ttft > 0,
        );
        let snapshot = f.snapshot();
        let row = snapshot.recent.iter().find(|r| r.id == name).unwrap();
        assert_eq!(row.speed(), generation, "generation: {name}");
        assert_eq!(row.floor(), lower, "floor: {name}");
        let stats = snapshot.models.iter().find(|m| m.model == name).unwrap();
        assert_eq!(
            stats.speed(),
            generation.map(|(n, w)| n as f64 * 1000.0 / w as f64)
        );
        assert_eq!(
            stats.floor(),
            lower.map(|(n, w)| n as f64 * 1000.0 / w as f64)
        );
        if let Some((n, w)) = generation {
            decode.0 += n;
            decode.1 += w;
            decode.2 += 1;
        }
        if let Some((n, w)) = lower {
            floor.0 += n;
            floor.1 += w;
            floor.2 += 1;
        }
    }
    let s = f
        .service
        .summary(&f.binding, Period::Day, at(30))
        .unwrap()
        .unwrap();
    assert_eq!((s.speed_tokens, s.speed_ms, s.speed_samples), decode);
    assert_eq!((s.floor_tokens, s.floor_ms, s.floor_samples), floor);
    assert_eq!(s.mean_ttft_ms, f.snapshot().summary.mean_ttft_ms);
}

#[test]
fn magpie_claude_floor_and_legacy_inferred_ttft_are_handled_without_rebuilding_index() {
    let f = Fixture::new("claude");
    for (name, out, ms, first, native, generation, floor) in [
        ("large", 2000, 20000, None, false, None, Some((2000, 20000))),
        ("small", 815, 18000, None, false, None, None),
        (
            "minimum",
            1000,
            12000,
            None,
            false,
            None,
            Some((1000, 12000)),
        ),
        ("too-fast", 20001, 1000, None, false, None, None),
        (
            "legacy-inference",
            1500,
            10000,
            Some(3000),
            false,
            None,
            Some((1500, 10000)),
        ),
        // Magpie's general 100ms decode gate applies to non-Codex native timing.
        ("timed", 380, 7000, Some(6800), true, Some((380, 200)), None),
    ] {
        insert_timing(&f, name, out, 0, ms, first, native);
        let snapshot = f.snapshot();
        let row = snapshot.recent.iter().find(|r| r.id == name).unwrap();
        assert_eq!(row.speed(), generation, "{name}");
        assert_eq!(row.floor(), floor, "{name}");
        if !native {
            assert!(row.ttft_ms.is_none());
        }
    }
    let summary = f
        .service
        .summary(&f.binding, Period::Day, at(30))
        .unwrap()
        .unwrap();
    assert_eq!(summary.timed_samples, 1);
    assert_eq!(summary.mean_ttft_ms, Some(6800.0));
    assert_eq!(summary.speed(), Some(1900.0));
    assert_eq!(summary.floor_samples, 3);
    assert_eq!(summary.floor(), Some(4500.0 * 1000.0 / 42000.0));
}

#[test]
fn delayed_codex_usage_excludes_tool_runtime_and_survives_checkpoint() {
    let f = Fixture::new("codex");
    let text = line(
        0,
        "session_meta",
        json!({"id":"tool-session","model_provider":"relay"}),
    ) + &line(0, "turn_context", json!({"model":"m","turn_id":"t"}))
        + &line(0, "event_msg", json!({"type":"task_started","turn_id":"t"}))
        + &line(5, "response_item", json!({"type":"function_call"}))
        + &line(10, "response_item", json!({"type":"function_call_output"}));
    let path = f.file("tool.jsonl", &text);
    f.update();
    // Resume the parser through its persisted checkpoint, after the tool completed.
    OpenOptions::new().append(true).open(path).unwrap().write_all(line(11,"event_msg",json!({"type":"token_count","info":{"total_token_usage":{"output_tokens":1500},"last_token_usage":{"output_tokens":1500}}})).as_bytes()).unwrap();
    f.update();
    let s = f.snapshot();
    assert_eq!(s.recent[0].duration_ms, Some(5000));
    assert_eq!(s.recent[0].floor(), Some((1500, 5000)));
    assert!(s.recent[0].speed().is_none());
}

fn item(sec: u32, kind: &str) -> String {
    line(
        sec,
        "event_msg",
        json!({"type":"item_completed","item":{"type":kind},"started_at_ms":at(sec),"completed_at_ms":at(sec)}),
    )
}
fn usage_record(sec: u32, output: u64) -> String {
    line(
        sec,
        "token_usage_record",
        json!({"response_id":format!("resp-{sec}"),"usage":{"output_tokens":output}}),
    )
}
fn token_count(sec: u32, total: u64, last: u64) -> String {
    line(
        sec,
        "event_msg",
        json!({"type":"token_count","info":{"total_token_usage":{"output_tokens":total},"last_token_usage":{"output_tokens":last}}}),
    )
}

/// Mirrors a Codex 0.16x rollout: each response ends with `token_usage_record`, output items
/// carry their start time, and `token_count` follows only after the tool ran.
#[test]
fn recorded_codex_requests_get_their_own_first_token_and_end_across_a_checkpoint() {
    let f = Fixture::new("codex");
    let first = line(
        0,
        "session_meta",
        json!({"id":"rec","model_provider":"relay"}),
    ) + &line(0, "turn_context", json!({"model":"m","turn_id":"t"}))
        + &line(0, "event_msg", json!({"type":"task_started","turn_id":"t"}))
        + &line(0, "response_item", json!({"type":"message","role":"user"}))
        + &item(0, "UserMessage")
        + &item(2, "AgentMessage")
        + &line(
            3,
            "response_item",
            json!({"type":"message","role":"assistant"}),
        )
        + &line(4, "response_item", json!({"type":"custom_tool_call"}))
        + &usage_record(5, 152);
    let path = f.file("recorded.jsonl", &first);
    f.update();
    // The tool's runtime (5 s → 6 s) and the later token_count are not part of the request.
    let rest = item(6, "CommandExecution")
        + &line(6, "response_item", json!({"type":"custom_tool_call_output"}))
        + &token_count(6, 152, 152)
        // A tool-call-only response has no output item, so its first token stays unknown.
        + &line(9, "response_item", json!({"type":"custom_tool_call"}))
        + &usage_record(10, 300)
        + &line(10, "response_item", json!({"type":"custom_tool_call_output"}))
        + &token_count(11, 452, 300)
        + &item(12, "Reasoning")
        + &item(13, "AgentMessage")
        + &line(20, "response_item", json!({"type":"message","role":"assistant"}))
        + &usage_record(20, 1200)
        + &token_count(20, 1652, 1200)
        + &line(21, "event_msg", json!({"type":"task_complete","turn_id":"t","duration_ms":21000,"time_to_first_token_ms":2000}));
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(rest.as_bytes())
        .unwrap();
    f.update();
    let s = f.snapshot();
    let timing: Vec<_> = s
        .recent
        .iter()
        .rev()
        .map(|c| (c.output, c.duration_ms, c.ttft_ms, c.native_timing))
        .collect();
    assert_eq!(
        timing,
        [
            (152, Some(5000), Some(2000), true),
            (300, Some(4000), None, true),
            (1200, Some(9000), Some(2000), true),
        ]
    );
    // The reasoning item does not count as the first token; the answer at 13 s does.
    // The last request now yields a generation sample: 1200 tokens over 9 s − 2 s.
    assert_eq!(s.recent[0].speed(), Some((1200, 7000)));
    assert_eq!(s.summary.mean_ttft_ms, Some(2000.0));
}

/// Codex writes token_count when a sampling attempt ends; one without new usage is an attempt
/// that failed (here a stream that went idle for five minutes) and the retry is timed alone.
#[test]
fn a_failed_attempt_does_not_inflate_the_retried_request() {
    let mut parser = (source("codex").unwrap().parser)(serde_json::Value::Null);
    let text = line(0, "session_meta", json!({"id":"retry","model_provider":"relay"}))
        + &line(0, "turn_context", json!({"model":"m","turn_id":"t"}))
        + &line(0, "event_msg", json!({"type":"task_started","turn_id":"t"}))
        + &token_count(1, 100, 100)
        + &line(2, "response_item", json!({"type":"reasoning"}))
        // The abandoned attempt: same totals again, no token_usage_record.
        + &token_count(30, 100, 100)
        + &item(40, "AgentMessage")
        + &line(45, "response_item", json!({"type":"message","role":"assistant"}))
        + &usage_record(45, 600)
        + &token_count(46, 700, 600);
    let calls: Vec<_> = text
        .lines()
        .flat_map(|ln| parser.feed(ln.as_bytes()).unwrap())
        .collect();
    let retried = calls.last().unwrap();
    assert_eq!(
        (retried.duration_ms, retried.ttft_ms),
        (Some(15000), Some(10000))
    );
}

#[test]
fn a_recorded_single_call_turn_keeps_request_timing_over_turn_timing() {
    let mut parser = (source("codex").unwrap().parser)(serde_json::Value::Null);
    let text = line(
        0,
        "session_meta",
        json!({"id":"one","model_provider":"relay"}),
    ) + &line(0, "turn_context", json!({"model":"m","turn_id":"turn-a"}))
        + &line(
            0,
            "event_msg",
            json!({"type":"task_started","turn_id":"turn-a"}),
        )
        + &item(3, "AgentMessage")
        + &line(
            8,
            "response_item",
            json!({"type":"message","role":"assistant"}),
        )
        + &usage_record(8, 500)
        + &token_count(9, 500, 500)
        + &complete();
    let calls: Vec<_> = text
        .lines()
        .flat_map(|ln| parser.feed(ln.as_bytes()).unwrap())
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "task_complete adds nothing to a recorded request"
    );
    assert_eq!(
        (calls[0].duration_ms, calls[0].ttft_ms),
        (Some(8000), Some(3000))
    );
}

#[test]
fn old_cursor_is_reparsed_once_without_duplicating_requests_or_losing_deleted_history() {
    let f = Fixture::new("codex");
    let text = codex_session("s", "m") + &complete();
    let file = f.file("old.jsonl", &text);
    f.update();
    insert_timing(&f, "deleted-log", 1000, 0, 5000, None, false);
    let db = rusqlite::Connection::open(f.root.join("index/performance.sqlite3")).unwrap();
    let state: String = db
        .query_row("SELECT cursor FROM perf_files LIMIT 1", [], |r| r.get(0))
        .unwrap();
    let mut state: serde_json::Value = serde_json::from_str(&state).unwrap();
    state.as_object_mut().unwrap().remove("format");
    db.execute("UPDATE perf_files SET cursor=?1", [state.to_string()])
        .unwrap();
    assert_eq!(f.update().bytes, text.len() as u64);
    assert_eq!(f.snapshot().summary.calls, 2);
    assert_eq!(f.update().bytes, 0);
    fs::remove_file(file).unwrap();
    f.update();
    assert_eq!(f.snapshot().summary.calls, 2);
}

#[test]
fn gateway_supersedes_cli_only_inside_its_recorded_launch_and_keeps_other_sessions() {
    let f = Fixture::new("codex");
    let scope = f.binding.scope();
    let run = "test-run";
    f.service
        .register_gateway_run(&scope, run, "owner")
        .unwrap();
    let db = f.service.open().unwrap();
    db.execute(
        "UPDATE perf_gateway_runs SET started=?1,ended=?2 WHERE run=?3",
        rusqlite::params![at(5), at(15), run],
    )
    .unwrap();
    for (id, session, sec) in [
        ("original", "shared", 10),
        ("resumed", "shared", 20),
        ("independent", "other", 10),
    ] {
        super::store::save(
            &db,
            &scope,
            &Observation {
                id: id.into(),
                session: session.into(),
                at: at(sec),
                model: "model".into(),
                output: 200,
                duration_ms: Some(2000),
                ttft_ms: Some(500),
                native_timing: true,
                ..Default::default()
            },
        )
        .unwrap();
    }
    drop(db);
    let record = crate::gateway::Record {
        id: "gateway-request".into(),
        run: run.into(),
        session: "shared".into(),
        at: at(10),
        model: "model".into(),
        requested_model: "model".into(),
        response_id: Some("response".into()),
        output: Some(300),
        reasoning: 100,
        duration: 1500,
        ttft: Some(200),
        decode: Some((200, 1000)),
        status: 200,
        complete: true,
        outcome: "finished".into(),
        ..Default::default()
    };
    f.service.record_gateway(&scope, "custom", &record).unwrap();
    f.service.record_gateway(&scope, "custom", &record).unwrap();
    let snapshot = f.snapshot();
    assert_eq!(snapshot.summary.calls, 3);
    assert_eq!(snapshot.summary.speed_samples, 3);
    assert!(
        snapshot
            .recent
            .iter()
            .any(|r| r.gateway && r.speed() == Some((200, 1000)))
    );
    assert!(!snapshot.recent.iter().any(|r| r.id == "original"));
    let summary = f
        .service
        .summary(&f.binding, Period::Month, at(30))
        .unwrap()
        .unwrap();
    assert_eq!(summary.calls, snapshot.summary.calls);
    assert_eq!(summary.speed_tokens, snapshot.summary.speed_tokens);

    // Official OAuth reads CLI evidence even when this Home has old gateway rows.
    let mut direct = f.binding.clone();
    direct.gateway_observations = false;
    assert_eq!(direct.scope(), f.binding.scope());
    assert_ne!(direct.cache_key(), f.binding.cache_key());
    let direct_snapshot = f
        .service
        .snapshot(&direct, Period::Month, None, 0, at(30))
        .unwrap();
    assert_eq!(direct_snapshot.summary.calls, 3);
    assert_eq!(direct_snapshot.models[0].calls, 3);
    assert!(direct_snapshot.recent.iter().any(|r| r.id == "original"));
    assert!(direct_snapshot.recent.iter().all(|r| !r.gateway));
    assert_eq!(direct_snapshot.summary.mean_ttft_ms, Some(500.0));
    let direct_summary = f
        .service
        .summary(&direct, Period::Month, at(30))
        .unwrap()
        .unwrap();
    assert_eq!(
        direct_summary.mean_ttft_ms,
        direct_snapshot.summary.mean_ttft_ms
    );

    let mut unbound = record.clone();
    unbound.id = "unbound".into();
    unbound.run = "unbound-run".into();
    unbound.session.clear();
    f.service
        .record_gateway(&scope, "custom", &unbound)
        .unwrap();
    f.service
        .mark_gateway_cli_sources(&[(scope, run.into())])
        .unwrap();
    let direct_snapshot = f
        .service
        .snapshot(&direct, Period::Month, None, 0, at(30))
        .unwrap();
    assert_eq!(direct_snapshot.unmatched, 0);
    assert!(!direct_snapshot.native_transport);
    assert_eq!(direct_snapshot.summary.calls, 3);
}

#[test]
fn gateway_without_session_is_unmatched_until_explicit_lifecycle_binding() {
    let f = Fixture::new("codex");
    let scope = f.binding.scope();
    f.service
        .register_gateway_run(&scope, "run", "owner")
        .unwrap();
    let record = crate::gateway::Record {
        id: "unknown".into(),
        run: "run".into(),
        session: String::new(),
        at: at(10),
        model: "m".into(),
        requested_model: "m".into(),
        response_id: None,
        output: None,
        reasoning: 0,
        duration: 1500,
        ttft: None,
        decode: None,
        status: 502,
        complete: false,
        outcome: "upstream_error".into(),
        ..Default::default()
    };
    f.service.record_gateway(&scope, "custom", &record).unwrap();
    let before = f.snapshot();
    assert_eq!(before.summary.calls, 0);
    assert_eq!(before.unmatched, 1);
    f.service.link_gateway_session("run", "session").unwrap();
    let after = f.snapshot();
    assert_eq!(after.summary.calls, 1);
    assert_eq!(after.summary.speed_samples, 0);
    assert!(after.recent[0].output_unknown);
    assert_eq!(after.unmatched, 0);
}

#[test]
fn recovering_one_gateway_does_not_close_other_manager_roots() {
    let f = Fixture::new("codex");
    let scope = f.binding.scope();
    f.service
        .register_gateway_run(&scope, "one", "root-one")
        .unwrap();
    f.service
        .register_gateway_run(&scope, "two", "root-two")
        .unwrap();
    f.service.recover_gateway_runs("root-one").unwrap();
    let db = f.service.open().unwrap();
    let rows: Vec<(String, bool)> = db
        .prepare("SELECT run,ended IS NOT NULL FROM perf_gateway_runs ORDER BY run")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, vec![("one".into(), true), ("two".into(), false)]);
}

#[test]
fn prepared_but_unused_gateway_does_not_hide_direct_cli_observations() {
    let f = Fixture::new("codex");
    let scope = f.binding.scope();
    f.service
        .register_gateway_run(&scope, "unused", "owner")
        .unwrap();
    f.service.link_gateway_session("unused", "session").unwrap();
    let db = f.service.open().unwrap();
    db.execute("UPDATE perf_gateway_runs SET started=?1", [at(1)])
        .unwrap();
    super::store::save(
        &db,
        &scope,
        &Observation {
            id: "direct".into(),
            session: "session".into(),
            at: at(10),
            model: "m".into(),
            output: 200,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        f.snapshot().summary.calls,
        1,
        "a failed optional registration cannot suppress actual direct traffic"
    );
}

#[test]
fn unobservable_websocket_uses_cli_source_for_the_whole_run_without_double_counting() {
    let f = Fixture::new("codex");
    let scope = f.binding.scope();
    f.service
        .register_gateway_run(&scope, "run", "owner")
        .unwrap();
    let db = f.service.open().unwrap();
    db.execute("UPDATE perf_gateway_runs SET started=?1", [at(1)])
        .unwrap();
    super::store::save(
        &db,
        &scope,
        &Observation {
            id: "cli".into(),
            session: "session".into(),
            at: at(10),
            model: "m".into(),
            output: 200,
            ..Default::default()
        },
    )
    .unwrap();
    super::store::save(
        &db,
        &scope,
        &Observation {
            id: "cli-second".into(),
            session: "session".into(),
            at: at(11),
            model: "m".into(),
            output: 300,
            ..Default::default()
        },
    )
    .unwrap();
    let record = crate::gateway::Record {
        transport: "websocket".into(),
        id: "http".into(),
        run: "run".into(),
        session: "session".into(),
        at: at(10),
        model: "m".into(),
        output: Some(200),
        complete: true,
        ..Default::default()
    };
    f.service.record_gateway(&scope, "openai", &record).unwrap();
    assert_eq!(f.snapshot().summary.calls, 1);
    assert!(f.snapshot().recent[0].gateway);
    assert!(!f.snapshot().native_transport);
    f.service
        .record_gateway(
            &scope,
            "openai",
            &crate::gateway::Record {
                run: "run".into(),
                native_transport: true,
                ..Default::default()
            },
        )
        .unwrap();
    let snapshot = f.snapshot();
    assert_eq!(snapshot.summary.calls, 2);
    assert!(snapshot.recent.iter().all(|r| !r.gateway));
    assert!(snapshot.native_transport);
    assert_eq!(
        f.service
            .summary(&f.binding, Period::Month, at(30))
            .unwrap()
            .unwrap()
            .calls,
        2
    );
}
