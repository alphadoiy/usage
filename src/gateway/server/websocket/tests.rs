use super::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn tracker() -> (Tracker, mpsc::Receiver<recording::Sample>) {
    let (records, receiver) = mpsc::channel(128);
    let route = Arc::new(Route {
        source: recording::Sources::default()
            .reserve("scope", "run")
            .unwrap(),
        active: AtomicUsize::new(0),
        registration: Registration {
            observe_websocket: true,
            run: "run".into(),
            scope: "scope".into(),
            provider: "provider".into(),
            upstream: "http://127.0.0.1".into(),
            model_provider: "openai".into(),
            auth: RouteAuth::NativeOauth {
                account_id: "workspace".into(),
            },
            token: "capability".into(),
            lease: PathBuf::new(),
        },
    });
    (
        Tracker {
            route,
            records,
            session: "session".into(),
            connection: "connection".into(),
            pending: HashMap::new(),
            finished: VecDeque::new(),
            disabled: false,
        },
        receiver,
    )
}
fn client(t: &mut Tracker, value: Value) {
    t.message(true, value.to_string().as_bytes());
}
fn server(t: &mut Tracker, mut value: Value, lane: &str, at: u64) {
    if !lane.is_empty() {
        value["stream_id"] = json!(lane);
    }
    if let Some(sample) = t.pending.get_mut(lane).and_then(|q| q.front_mut()) {
        sample.start = Instant::now() - Duration::from_millis(at);
    }
    t.message(false, value.to_string().as_bytes());
}
fn created(t: &mut Tracker, lane: &str, id: &str) {
    server(
        t,
        json!({"type":"response.created","response":{"id":id,"model":"served"}}),
        lane,
        1,
    );
}
fn finish(t: &mut Tracker, lane: &str, id: &str) {
    for n in 1..=7 {
        server(
            t,
            json!({"type":"response.output_text.delta","delta":"text"}),
            lane,
            n * 100,
        );
    }
    server(
        t,
        json!({"type":"response.completed","response":{"id":id,"model":"served","usage":{"input_tokens":10,"output_tokens":140}}}),
        lane,
        800,
    );
}
#[test]
fn per_request_timing_excludes_socket_idle_and_persists_before_disconnect() {
    let (mut t, mut rx) = tracker();
    for id in ["first", "second"] {
        client(&mut t, json!({"type":"response.create","model":"asked"}));
        created(&mut t, "", id);
        finish(&mut t, "", id);
        let (_, _, r) = rx.try_recv().unwrap();
        assert_eq!(r.transport, "websocket");
        assert_eq!(r.connection_id.as_deref(), Some("connection"));
        assert_eq!(r.response_id.as_deref(), Some(id));
        assert!(r.complete && !r.native_transport);
        assert_eq!(r.output, Some(140));
        assert_eq!(r.ttft, Some(100));
        assert_eq!(r.decode, Some((140, 700)));
        assert!(t.pending.is_empty());
        // Repeating a terminal identity never consumes another request or writes another row.
        server(
            &mut t,
            json!({"type":"response.completed","response":{"id":id}}),
            "",
            900,
        );
        assert!(rx.try_recv().is_err());
        assert!(!t.disabled);
    }
    assert_eq!(t.route.active.load(Ordering::Relaxed), 0);
}
#[test]
fn multiplexed_lanes_and_fifo_requests_keep_usage_and_response_identity_separate() {
    let (mut t, mut rx) = tracker();
    for lane in ["a", "b", "a"] {
        client(
            &mut t,
            json!({"type":"response.create","stream_id":lane,"model":lane}),
        );
    }
    created(&mut t, "a", "a1");
    created(&mut t, "b", "b1");
    for n in 1..=7 {
        for lane in ["a", "b"] {
            server(
                &mut t,
                json!({"type":"response.output_text.delta","delta":"flow"}),
                lane,
                n * 100,
            );
        }
    }
    for (lane, id, count) in [("b", "b1", 210), ("a", "a1", 140)] {
        server(
            &mut t,
            json!({"type":"response.completed","response":{"id":id,"usage":{"output_tokens":count}}}),
            lane,
            800,
        );
        let (_, _, r) = rx.try_recv().unwrap();
        assert_eq!(r.response_id.as_deref(), Some(id));
        assert_eq!(r.requested_model, lane);
        assert_eq!(r.output, Some(count));
        assert!(r.decode.is_some());
    }
    created(&mut t, "a", "a2");
    finish(&mut t, "a", "a2");
    let (_, _, r) = rx.try_recv().unwrap();
    assert_eq!(r.response_id.as_deref(), Some("a2"));
    assert!(t.pending.is_empty());
    assert!(!t.disabled);
}
#[test]
fn warmups_errors_and_disconnects_never_become_successful_speed_samples() {
    let (mut t, mut rx) = tracker();
    client(&mut t, json!({"type":"response.create","generate":false}));
    created(&mut t, "", "warmup");
    finish(&mut t, "", "warmup");
    assert!(rx.try_recv().is_err());
    client(&mut t, json!({"type":"response.create"}));
    server(
        &mut t,
        json!({"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}),
        "",
        20,
    );
    let (_, _, r) = rx.try_recv().unwrap();
    assert_eq!(r.status, 429);
    assert!(!r.complete);
    assert!(r.decode.is_none());
    client(&mut t, json!({"type":"response.create"}));
    created(&mut t, "", "partial");
    server(
        &mut t,
        json!({"type":"response.output_text.delta","delta":"partial"}),
        "",
        100,
    );
    drop(t);
    let (_, _, r) = rx.try_recv().unwrap();
    assert_eq!(r.outcome, "websocket_disconnected");
    assert!(!r.complete);
    assert!(r.ttft.is_none() && r.decode.is_none());
}
#[test]
fn ambiguous_attribution_and_limits_select_cli_source_instead_of_guessing() {
    for mode in ["unmatched", "queued_error", "steering", "limit"] {
        let (mut t, mut rx) = tracker();
        match mode {
            "unmatched" => server(
                &mut t,
                json!({"type":"response.output_text.delta","delta":"unmatched"}),
                "",
                1,
            ),
            "queued_error" => {
                for _ in 0..2 {
                    client(&mut t, json!({"type":"response.create"}));
                }
                server(&mut t, json!({"type":"error"}), "", 1);
            }
            "steering" => client(&mut t, json!({"type":"response.steer"})),
            _ => {
                for _ in 0..=MAX_PENDING {
                    client(&mut t, json!({"type":"response.create"}));
                }
            }
        }
        assert!(t.disabled);
        assert!(t.pending.is_empty());
        assert!(t.route.source.is_fallback());
        assert!(rx.try_recv().is_err());
    }
}

fn frame(opcode: u8, fin: bool, payload: &[u8], masked: bool) -> Vec<u8> {
    let mut out = vec![opcode | if fin { 0x80 } else { 0 }];
    let mask = if masked { 0x80 } else { 0 };
    if payload.len() < 126 {
        out.push(mask | payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        out.push(mask | 126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        out.push(mask | 127);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    let key = [1, 2, 3, 4];
    if masked {
        out.extend_from_slice(&key);
    }
    out.extend(
        payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ if masked { key[i % 4] } else { 0 }),
    );
    out
}
#[test]
fn frame_observation_handles_masking_fragmentation_controls_and_extended_lengths() {
    for size in [30, 200, 70_000] {
        let payload = vec![b'x'; size];
        let mut raw = frame(1, false, &payload[..size / 2], true);
        raw.extend(frame(9, true, b"ping", true));
        raw.extend(frame(0, true, &payload[size / 2..], true));
        let mut parser = frames::Frames::default();
        let mut messages = Vec::new();
        for chunk in raw.chunks(3) {
            assert!(
                parser
                    .feed(chunk, true, |m| messages.push(m.to_vec()))
                    .is_none()
            );
        }
        assert_eq!(messages, [payload]);
    }
}
#[test]
fn unsupported_encoding_and_oversized_frames_stop_only_observation() {
    let mut compressed = frame(1, true, b"compressed", false);
    compressed[0] |= 0x40;
    assert_eq!(
        frames::Frames::default().feed(&compressed, false, |_| panic!("compressed")),
        Some("websocket_extension_encoding")
    );
    let mut oversized = vec![0x81, 127];
    oversized.extend_from_slice(&(2_u64 * 1024 * 1024).to_be_bytes());
    assert_eq!(
        frames::Frames::default().feed(&oversized, false, |_| panic!("oversized")),
        Some("websocket_message_limit")
    );
    assert_eq!(
        frames::Frames::default().feed(&frame(0, true, b"orphan", false), false, |_| panic!(
            "orphan"
        )),
        Some("websocket_unmatched_continuation")
    );
}

#[tokio::test]
async fn native_websocket_preserves_wire_bytes_and_records_real_socket_events() {
    native_socket(false).await;
}
#[tokio::test]
async fn deflated_native_websocket_preserves_wire_bytes_and_records_real_socket_events() {
    native_socket(true).await;
}
async fn native_socket(compressed: bool) {
    let (mut base_tracker, mut rx) = tracker();
    // Build a fresh route instead of taking credentials from a live account.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let create = json!({"type":"response.create","model":"asked"}).to_string();
    let mut encoder = flate2::Compress::new(flate2::Compression::fast(), false);
    let create = if compressed {
        compress(&mut encoder, create.as_bytes())
    } else {
        create.into_bytes()
    };
    let middle = create.len() / 2;
    let mut client_bytes = frame(1, false, &create[..middle], true);
    if compressed {
        client_bytes[0] |= 0x40;
    }
    client_bytes.extend(frame(9, true, b"ping", true));
    client_bytes.extend(frame(0, true, &create[middle..], true));
    let expected_client = client_bytes.clone();
    let mut events =
        vec![json!({"type":"response.created","response":{"id":"response-one","model":"served"}})];
    for _ in 0..7 {
        events.push(json!({"type":"response.output_text.delta","delta":"text"}));
    }
    events.push(json!({"type":"response.completed","response":{"id":"response-one","model":"served","usage":{"output_tokens":200}}}));
    let mut encoder = flate2::Compress::new(flate2::Compression::fast(), false);
    let server_frames: Vec<Vec<u8>> = events
        .into_iter()
        .map(|v| {
            let payload = v.to_string();
            if compressed {
                let mut bytes = frame(1, true, &compress(&mut encoder, payload.as_bytes()), false);
                bytes[0] |= 0x40;
                bytes
            } else {
                frame(1, true, payload.as_bytes(), false)
            }
        })
        .collect();
    let expected_reply: Vec<u8> = server_frames.iter().flatten().copied().collect();
    let app = Router::new().route(
        "/responses",
        get(move |mut request: axum::extract::Request| {
            let expected_client = expected_client.clone();
            let frames = server_frames.clone();
            async move {
                assert_eq!(
                    request.headers()["authorization"],
                    "Bearer rotated-native-token"
                );
                assert_eq!(request.headers()["chatgpt-account-id"], "workspace");
                assert_eq!(
                    request.headers()["sec-websocket-key"],
                    "dGhlIHNhbXBsZSBub25jZQ=="
                );
                let upgrade = hyper::upgrade::on(&mut request);
                tokio::spawn(async move {
                    let mut io = hyper_util::rt::TokioIo::new(upgrade.await.unwrap());
                    let mut bytes = vec![0; expected_client.len()];
                    io.read_exact(&mut bytes).await.unwrap();
                    assert_eq!(bytes, expected_client);
                    for bytes in frames {
                        io.write_all(&bytes).await.unwrap();
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    // Keep the socket open: the terminal event must already have persisted metrics.
                    let mut end = [0; 1];
                    let _ = io.read(&mut end).await;
                });
                let mut response = Response::builder()
                    .status(101)
                    .header("connection", "Upgrade")
                    .header("upgrade", "websocket")
                    .header("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
                if compressed {
                    response = response.header("sec-websocket-extensions", "permessage-deflate");
                }
                response.body(Body::empty()).unwrap()
            }
        }),
    );
    let upstream_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Arc::get_mut(&mut base_tracker.route)
        .unwrap()
        .registration
        .upstream = upstream;
    let route = base_tracker.route.clone();
    let lease = route.clone();
    let state = Arc::new(Shared {
        plugins: crate::plugins::Runtime::new(crate::plugins::store::root(), 0),
        sources: Arc::new(recording::Sources::default()),
        directory: PathBuf::new(),
        token: "admin".into(),
        port: 0,
        stopping: AtomicBool::new(false),
        routes: RwLock::new(HashMap::from([("route".into(), route)])),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        idle_timeout: Duration::from_secs(2),
        records: base_tracker.records.clone(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/r/route/capability/backend-api/codex/responses",
        listener.local_addr().unwrap()
    );
    let app = Router::new()
        .route("/r/{id}/{*path}", any(super::super::forward))
        .with_state(state);
    let gateway_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(endpoint)
        .header("authorization", "Bearer rotated-native-token")
        .header("chatgpt-account-id", "workspace")
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-extensions", "permessage-deflate")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 101);
    assert_eq!(
        response.headers()["sec-websocket-accept"],
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
    let mut io = response.upgrade().await.unwrap();
    for chunk in client_bytes.chunks(3) {
        io.write_all(chunk).await.unwrap();
    }
    let mut reply = vec![0; expected_reply.len()];
    tokio::time::timeout(Duration::from_secs(2), io.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, expected_reply);
    let (_, _, record) = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(record.complete && record.decode.is_some());
    assert_eq!(record.transport, "websocket");
    assert_eq!(record.output, Some(200));
    assert!(record.ttft.unwrap() > 0);
    assert!(!record.native_transport);
    assert_eq!(lease.active.load(Ordering::Relaxed), 1);
    drop(io);
    for _ in 0..50 {
        if lease.active.load(Ordering::Relaxed) == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(lease.active.load(Ordering::Relaxed), 0);
    assert!(rx.try_recv().is_err());
    gateway_task.abort();
    upstream_task.abort();
}

#[test]
fn incomplete_and_failed_responses_preserve_usage_without_speed_samples() {
    for kind in [
        "response.incomplete",
        "response.failed",
        "response.cancelled",
    ] {
        let (mut t, mut rx) = tracker();
        client(&mut t, json!({"type":"response.create"}));
        created(&mut t, "", "failure");
        server(
            &mut t,
            json!({"type":"response.output_text.delta","delta":"partial"}),
            "",
            100,
        );
        server(
            &mut t,
            json!({"type":kind,"response":{"id":"failure","usage":{"output_tokens":10}}}),
            "",
            800,
        );
        let (_, _, r) = rx.try_recv().unwrap();
        assert_eq!(r.output, Some(10));
        assert!(!r.complete);
        assert!(r.ttft.is_none() && r.decode.is_none());
        assert!(!t.disabled);
    }
}
#[test]
fn websocket_and_sse_use_identical_reasoning_answer_and_buffering_rules() {
    let mut sse = metrics::Observer::default();
    let mut ws = metrics::Observer::default();
    let mut events = vec![(
        100,
        json!({"type":"response.reasoning_summary_text.delta","delta":"summary"}),
    )];
    for n in 0..=10 {
        events.push((
            1000 + n * 100,
            json!({"type":"response.output_text.delta","delta":"answer"}),
        ));
    }
    events.push((2200,json!({"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":400,"output_tokens_details":{"reasoning_tokens":100}}}})));
    for (at, event) in events {
        sse.feed(format!("data: {event}\n\n").as_bytes(), at);
        ws.websocket(&event, at);
    }
    assert_eq!(sse.first, ws.first);
    assert_eq!(sse.decode(2200), ws.decode(2200));
    assert_eq!(ws.decode(2200), Some((300, 1200)));
    assert_eq!(sse.speed_unavailable(2200), ws.speed_unavailable(2200));
}

#[tokio::test]
async fn websocket_refusal_preserves_status_body_and_retry_after_without_inference_or_retry() {
    let (mut t, mut rx) = tracker();
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    Arc::get_mut(&mut t.route).unwrap().registration.upstream =
        format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/responses",
        get(move || {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, Ordering::Relaxed);
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "17")],
                    "upstream-ws-limit",
                )
            }
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let state = Arc::new(Shared {
        plugins: crate::plugins::Runtime::new(crate::plugins::store::root(), 0),
        sources: Arc::new(recording::Sources::default()),
        directory: PathBuf::new(),
        token: "admin".into(),
        port: 0,
        stopping: AtomicBool::new(false),
        routes: RwLock::new(HashMap::from([("route".into(), t.route.clone())])),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        idle_timeout: Duration::from_secs(2),
        records: t.records.clone(),
    });
    let request = axum::http::Request::builder()
        .method("GET")
        .header("authorization", "Bearer rotated-token")
        .header("chatgpt-account-id", "workspace")
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .body(Body::empty())
        .unwrap();
    let response = super::super::forward(
        State(state),
        RoutePath((
            "route".into(),
            "capability/backend-api/codex/responses".into(),
        )),
        request,
    )
    .await;
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["retry-after"], "17");
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap(),
        "upstream-ws-limit"
    );
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    assert!(rx.try_recv().is_err());
    assert_eq!(t.route.active.load(Ordering::Relaxed), 0);
    task.abort();
}
#[tokio::test]
async fn unsupported_frame_bytes_still_reach_the_other_end_and_select_cli_statistics() {
    let (tracker, mut rx) = tracker();
    let tracker = Arc::new(Mutex::new(tracker));
    let (mut peer, io) = tokio::io::duplex(1024);
    let mut observed = Observed {
        io,
        frames: frames::Frames::default(),
        tracker,
        client: false,
    };
    let mut bytes = frame(1, true, b"compressed-payload", false);
    bytes[0] |= 0x40;
    peer.write_all(&bytes).await.unwrap();
    let mut received = vec![0; bytes.len()];
    observed.read_exact(&mut received).await.unwrap();
    assert_eq!(received, bytes);
    assert!(observed.tracker.lock().unwrap().route.source.is_fallback());
    assert!(rx.try_recv().is_err());
    // A subsequent read is still forwarded after observation has been disabled.
    peer.write_all(b"more-opaque-bytes").await.unwrap();
    let mut received = [0; 17];
    observed.read_exact(&mut received).await.unwrap();
    assert_eq!(&received, b"more-opaque-bytes");
}

fn compress(encoder: &mut flate2::Compress, input: &[u8]) -> Vec<u8> {
    let mut output = vec![0; input.len() * 2 + 128];
    let before_in = encoder.total_in();
    let before_out = encoder.total_out();
    encoder
        .compress(input, &mut output, flate2::FlushCompress::Sync)
        .unwrap();
    assert_eq!(encoder.total_in() - before_in, input.len() as u64);
    output.truncate((encoder.total_out() - before_out) as usize);
    assert!(output.ends_with(&[0, 0, 255, 255]));
    output.truncate(output.len() - 4);
    output
}
#[test]
fn deflate_observation_handles_context_takeover_resets_and_uncompressed_messages() {
    for reset in [false, true] {
        for client in [false, true] {
            let extension = if reset {
                "permessage-deflate; client_no_context_takeover; server_no_context_takeover; client_max_window_bits=15; server_max_window_bits=15"
            } else {
                "permessage-deflate"
            };
            let mut decoder = frames::Frames::negotiated(Some(extension), client);
            let mut encoder = flate2::Compress::new(flate2::Compression::fast(), false);
            for _ in 0..4 {
                if reset {
                    encoder = flate2::Compress::new(flate2::Compression::fast(), false);
                }
                let payload = b"context context context repeated repeated repeated";
                let compressed = compress(&mut encoder, payload);
                let middle = compressed.len() / 2;
                let mut raw = frame(1, false, &compressed[..middle], client);
                raw[0] |= 0x40;
                raw.extend(frame(9, true, b"ping", client));
                raw.extend(frame(0, true, &compressed[middle..], client));
                let mut messages = Vec::new();
                for chunk in raw.chunks(2) {
                    assert!(
                        decoder
                            .feed(chunk, client, |m| messages.push(m.to_vec()))
                            .is_none()
                    );
                }
                assert_eq!(messages, [payload]);
                let raw = frame(1, true, b"plain", client);
                assert!(
                    decoder
                        .feed(&raw, client, |m| assert_eq!(m, b"plain"))
                        .is_none()
                );
            }
        }
    }
}
#[test]
fn deflate_expansion_is_bounded_and_invalid_compressed_data_cannot_create_metrics() {
    let mut encoder = flate2::Compress::new(flate2::Compression::fast(), false);
    let compressed = compress(&mut encoder, &vec![b'a'; 1024 * 1024 + 1]);
    let mut raw = frame(1, true, &compressed, false);
    raw[0] |= 0x40;
    assert_eq!(
        frames::Frames::negotiated(Some("permessage-deflate"), false).feed(
            &raw,
            false,
            |_| panic!("oversized expansion")
        ),
        Some("websocket_decompressed_message_limit")
    );
    let mut raw = frame(1, true, &[0xff, 0xff, 0xff], false);
    raw[0] |= 0x40;
    assert_eq!(
        frames::Frames::negotiated(Some("permessage-deflate"), false).feed(
            &raw,
            false,
            |_| panic!("invalid deflate")
        ),
        Some("websocket_invalid_deflate")
    );
}

#[test]
fn multiplexed_requests_use_each_native_thread_identity_instead_of_the_socket_default() {
    let (mut t, mut rx) = tracker();
    for (lane, thread) in [
        ("a", "123e4567-e89b-12d3-a456-426614174001"),
        ("b", "123e4567-e89b-12d3-a456-426614174002"),
    ] {
        client(
            &mut t,
            json!({"type":"response.create","stream_id":lane,"client_metadata":{"thread_id":thread,"session_id":"123e4567-e89b-12d3-a456-426614174000"}}),
        );
        created(&mut t, lane, lane);
        finish(&mut t, lane, lane);
        let (_, _, r) = rx.try_recv().unwrap();
        assert_eq!(r.session, thread);
    }
}

#[tokio::test]
async fn full_sample_queue_cannot_drop_websocket_source_fallback() {
    let (mut t, mut rx) = tracker();
    let sources = Arc::new(recording::Sources::default());
    Arc::get_mut(&mut t.route).unwrap().source = sources.reserve("scope", "run").unwrap();
    // First persistable sample exists, then the bounded sample queue fills up.
    client(&mut t, json!({"type":"response.create"}));
    created(&mut t, "", "first");
    finish(&mut t, "", "first");
    while t.records.capacity() > 0 {
        t.records
            .try_send((
                t.route.source.clone(),
                "openai".into(),
                Record {
                    run: "run".into(),
                    ..Default::default()
                },
            ))
            .ok()
            .unwrap();
    }
    t.fallback("unsupported_client_event");
    assert!(t.route.source.is_fallback());
    // The source selection is independent of both full and closed sample queues.
    rx.close();
    let root = std::env::temp_dir().join(format!("usage-ws-source-{}", uuid::Uuid::new_v4()));
    let path = root.join("performance.sqlite3");
    let service = Arc::new(crate::performance::Service::at(path.clone()));
    drop(t);
    tokio::time::timeout(
        Duration::from_secs(5),
        recording::write(rx, sources, service),
    )
    .await
    .unwrap();
    let db = rusqlite::Connection::open(path).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM perf_gateway_native WHERE scope='scope' AND run='run'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn maximum_message_followed_by_terminal_frame_is_independent_of_tcp_chunking() {
    const LIMIT: usize = 1024 * 1024;
    let mut delta = br#"{"type":"response.output_text.delta","delta":""#.to_vec();
    delta.resize(LIMIT - 2, b'x');
    delta.extend_from_slice(b"\"}");
    assert_eq!(delta.len(), LIMIT);
    let large = frame(1, true, &delta, false);
    let terminal = frame(
        1,
        true,
        br#"{"type":"response.completed","response":{"id":"r","usage":{"output_tokens":200}}}"#,
        false,
    );
    let mut wire = large.clone();
    wire.extend_from_slice(&terminal);
    // Separate packets, a coalesced tail + terminal, one read, and a split header.
    for split in [large.len(), large.len() - 2, 0, 1] {
        let (mut t, mut rx) = tracker();
        client(&mut t, json!({"type":"response.create"}));
        created(&mut t, "", "r");
        let mut parser = frames::Frames::default();
        for chunk in [&wire[..split], &wire[split..]] {
            assert_eq!(parser.feed(chunk, false, |m| t.message(false, m)), None);
        }
        assert!(!t.route.source.is_fallback());
        let (_, _, record) = rx.try_recv().unwrap();
        assert!(record.complete);
        assert_eq!(record.output, Some(200));
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn multiple_maximum_frames_and_fragmented_messages_remain_bounded_per_message() {
    const LIMIT: usize = 1024 * 1024;
    for masked in [false, true] {
        let payload = vec![b'x'; LIMIT];
        let mut wire = frame(1, true, &payload, masked);
        wire.extend(frame(9, true, b"ping", masked));
        wire.extend(frame(1, false, &payload[..LIMIT / 2], masked));
        wire.extend(frame(0, true, &payload[LIMIT / 2..], masked));
        let mut parser = frames::Frames::default();
        let mut count = 0;
        assert_eq!(
            parser.feed(&wire, masked, |m| {
                assert_eq!(m, payload);
                count += 1;
            }),
            None
        );
        assert_eq!(count, 2);
        let mut oversized = frame(1, false, &payload, masked);
        oversized.extend(frame(0, true, b"x", masked));
        assert_eq!(
            parser.feed(&oversized, masked, |_| panic!("oversized message")),
            Some("websocket_message_limit")
        );
    }
}
