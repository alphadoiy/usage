use super::*;

#[test]
fn nominated_hop_headers_are_removed_and_end_to_end_headers_survive() {
    let mut headers = axum::http::HeaderMap::new();
    for (name, value) in [
        ("connection", "keep-alive, x-hop"),
        ("x-hop", "private"),
        ("proxy-authorization", "secret"),
        ("authorization", "secret"),
        ("content-length", "10"),
        ("content-encoding", "gzip"),
        ("x-request-id", "upstream-id"),
        ("session_id", "session"),
    ] {
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    let clean = server::clean_headers(&headers);
    for name in [
        "connection",
        "x-hop",
        "proxy-authorization",
        "authorization",
        "content-length",
    ] {
        assert!(!clean.contains_key(name));
    }
    assert_eq!(clean["content-encoding"], "gzip");
    assert_eq!(clean["x-request-id"], "upstream-id");
    assert_eq!(clean["session_id"], "session");
}

#[test]
fn chat_usage_tokens_are_authoritative_and_completion_fixes_the_generation_window() {
    let mut o = metrics::Observer::new("asked".into());
    let send = |o: &mut metrics::Observer, v: serde_json::Value, ms| {
        o.feed(format!("data: {v}\n\n").as_bytes(), ms)
    };
    send(
        &mut o,
        serde_json::json!({"choices":[{"delta":{"role":"assistant"}}]}),
        10,
    );
    assert!(o.first.is_none());
    for ms in [100, 200, 300, 400, 500] {
        send(
            &mut o,
            serde_json::json!({"model":"served","choices":[{"delta":{"content":"abcd"}}]}),
            ms,
        );
    }
    send(
        &mut o,
        serde_json::json!({"usage":{"prompt_tokens":100,"completion_tokens":200,"prompt_tokens_details":{"cached_tokens":30}}}),
        600,
    );
    o.feed(b"data: [DONE]\n\n", 600);
    assert_eq!(o.input, Some(100));
    assert_eq!(o.cache_read, Some(30));
    assert_eq!(
        o.decode(20_000),
        Some((200, 500)),
        "EOF or client backpressure must not extend generation time after completion"
    );
}
