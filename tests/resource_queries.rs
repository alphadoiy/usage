use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use usage::http::{FakeHttp, HttpResponse, TransportError};
use usage::resources::connections::{ConnectionConfig, ResourceService, render_text, select};
use usage::resources::{ResourceMetric, ResourceScope, ResourceState};

fn config(service: &str, options: Value) -> ConnectionConfig {
    ConnectionConfig {
        id: "work".into(),
        label: Some("工作账号".into()),
        service: service.into(),
        base_url: "https://relay.example/panel/".into(),
        credential_env: "FIXTURE_KEY".into(),
        revision: String::new(),
        options,
        profile_ids: Vec::new(),
        inference: None,
    }
}
fn response(status: u16, body: &str) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse {
        status,
        headers: Default::default(),
        body: body.as_bytes().to_vec(),
    })
}
fn service(http: Arc<FakeHttp>) -> ResourceService {
    ResourceService::new(
        usage::resources::builtin_registry(),
        http,
        Arc::new(|reference| {
            assert_eq!(reference, "env:FIXTURE_KEY");
            Ok("synthetic-private-credential".into())
        }),
        Arc::new(|| {
            DateTime::parse_from_rfc3339("2026-10-15T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        }),
    )
}
fn token(unlimited: bool) -> String {
    format!(
        r#"{{"code":true,"data":{{"object":"token_usage","total_used":12,"total_available":9007199254740993,"unlimited_quota":{unlimited}}}}}"#
    )
}

#[tokio::test]
async fn queries_isolate_two_connections_and_keep_large_raw_quotas_exact() {
    let http = Arc::new(FakeHttp::new());
    http.route(
        "https://relay.example/panel/api/usage/token",
        response(200, &token(false)),
    );
    http.route(
        "https://other.example/api/usage/token",
        response(200, &token(true)),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let service = ResourceService::new(
        usage::resources::builtin_registry(),
        http.clone(),
        Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok("private-value".into())
        }),
        Arc::new(Utc::now),
    );
    let first = config("new_api_token", json!({}));
    let mut second = first.clone();
    second.id = "second".into();
    second.base_url = "https://other.example".into();
    service.prepare(&first).unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "factory resolved credentials"
    );
    assert!(http.calls.lock().unwrap().is_empty());
    let reports = service.query(&[first, second]).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(reports[0].snapshot.binding.account, "work");
    assert_eq!(reports[1].snapshot.binding.account, "second");
    assert_eq!(reports[0].snapshot.scope, ResourceScope::Token);
    let encoded = serde_json::to_value(&reports).unwrap();
    assert_eq!(
        encoded[0]["snapshot"]["metrics"][0]["remaining"],
        "9007199254740993"
    );
    assert_eq!(
        encoded[1]["snapshot"]["metrics"][0]["remaining"],
        "9007199254740993"
    );
    assert!(
        encoded[1]["snapshot"]["metrics"][0]["unlimited"]
            .as_bool()
            .unwrap()
    );
    let text = serde_json::to_string(&reports).unwrap();
    assert!(!text.contains("private-value"));
    assert!(!text.contains("FIXTURE_KEY"));
    assert!(!text.contains("relay.example"));
    assert!(render_text(&reports).contains("剩余 9007199254740993 · 令牌不限额"));
}

#[tokio::test]
async fn account_balance_requires_user_header_and_explicit_currency_conversion() {
    let http = Arc::new(FakeHttp::new());
    http.route(
        "https://relay.example/panel/api/user/self",
        response(
            200,
            r#"{"success":true,"data":{"id":42,"quota":1172839,"used_quota":10}}"#,
        ),
    );
    let service = service(http.clone());
    let raw = config("new_api_account", json!({"user_id":42}));
    let reports = service.query(std::slice::from_ref(&raw)).await.unwrap();
    assert_eq!(reports[0].snapshot.scope, ResourceScope::Account);
    assert_eq!(
        reports[0].snapshot.metrics.len(),
        1,
        "invented a currency conversion"
    );
    let mut converted = raw.clone();
    converted.options = json!({"user_id":42,"quota_per_unit":"500000","currency":"CNY"});
    assert_ne!(raw.binding(), converted.binding());
    let reports = service.query(&[converted]).await.unwrap();
    let ResourceMetric::Balance { remaining } = &reports[0].snapshot.metrics[1] else {
        panic!("balance missing")
    };
    assert_eq!(remaining.amount, "2.345678");
    assert_eq!(remaining.currency, "CNY");
    let calls = http.calls.lock().unwrap();
    for call in &*calls {
        assert_eq!(call.method, "GET");
        assert!(call.body.is_none());
        assert!(call.headers.contains(&("New-Api-User".into(), "42".into())));
        assert!(call.headers.contains(&(
            "Authorization".into(),
            "Bearer synthetic-private-credential".into()
        )));
        assert!(!call.url.contains("credential"));
    }
}

#[tokio::test]
async fn refuses_mismatched_accounts_expired_tokens_and_invented_balances() {
    let cases = [
        ("new_api_account", json!({"user_id":42}), r#"{"success":true,"data":{"id":43,"quota":20,"used_quota":0}}"#.to_owned(), ResourceState::AuthenticationFailed),
        ("new_api_token", json!({}), r#"{"code":true,"data":{"object":"token_usage","expires_at":1,"total_available":10,"total_used":1,"unlimited_quota":false}}"#.into(), ResourceState::AuthenticationFailed),
        ("new_api_token", json!({}), r#"{"success":false,"message":"synthetic-private-credential"}"#.into(), ResourceState::Failure),
        ("new_api_token", json!({}), r#"{"data":{"total_available":100}}"#.into(), ResourceState::Failure),
        ("new_api_token", json!({}), r#"{"code":true,"data":{"object":"token_usage","total_used":0,"unlimited_quota":false}}"#.into(), ResourceState::Failure),
        ("new_api_token", json!({}), "<html>login required</html>".into(), ResourceState::Failure),
    ];
    for (kind, opts, body, expected) in cases {
        let http = Arc::new(FakeHttp::new());
        http.route("https://relay.example", response(200, &body));
        let reports = service(http).query(&[config(kind, opts)]).await.unwrap();
        assert_eq!(reports[0].snapshot.state, expected);
        assert!(reports[0].snapshot.metrics.is_empty());
        assert!(
            !serde_json::to_string(&reports)
                .unwrap()
                .contains("synthetic-private-credential")
        );
    }
}

#[tokio::test]
async fn unlimited_token_preserves_returned_balance_and_explicit_ccswitch_conversion() {
    for (available, amount) in [
        ("2500000", "5"),
        ("0", "0"),
        ("-100000", "-0.2"),
        ("9007199254740993", "18014398509.481986"),
    ] {
        let http = Arc::new(FakeHttp::new());
        let body = format!(
            r#"{{"code":true,"data":{{"object":"token_usage","total_used":1000000,"total_available":{available},"unlimited_quota":true}}}}"#
        );
        http.route("https://relay.example", response(200, &body));
        let reports = service(http)
            .query(&[config(
                "new_api_token",
                json!({"quota_per_unit":"500000","currency":"USD"}),
            )])
            .await
            .unwrap();
        assert_eq!(reports[0].snapshot.state, ResourceState::Ready);
        assert_eq!(reports[0].snapshot.scope, ResourceScope::Token);
        assert_eq!(
            reports[0].snapshot.metrics,
            vec![
                ResourceMetric::Allowance {
                    remaining: Some(available.into()),
                    used: Some("1000000".into()),
                    unit: "new_api_quota".into(),
                    unlimited: true,
                },
                ResourceMetric::Balance {
                    remaining: usage::resources::Money {
                        amount: amount.into(),
                        currency: "USD".into(),
                    },
                },
            ]
        );
        let text = render_text(&reports);
        assert!(text.contains(&format!("余额 {amount} USD（接口返回）")));
        assert!(text.contains(&format!("剩余 {available} · 令牌不限额")));
    }
}

#[tokio::test]
async fn maps_auth_transient_configuration_and_missing_credentials_without_echoing_secrets() {
    for (status, expected) in [
        (401, ResourceState::AuthenticationFailed),
        (403, ResourceState::AuthenticationFailed),
        (404, ResourceState::NeedsConfiguration),
        (429, ResourceState::TransientFailure),
        (503, ResourceState::TransientFailure),
        (400, ResourceState::Failure),
    ] {
        let http = Arc::new(FakeHttp::new());
        http.route(
            "https://relay.example",
            response(status, "synthetic-private-credential"),
        );
        let reports = service(http)
            .query(&[config("openai_costs", json!({}))])
            .await
            .unwrap();
        assert_eq!(reports[0].snapshot.state, expected);
        assert!(reports[0].snapshot.metrics.is_empty());
        assert!(
            !serde_json::to_string(&reports)
                .unwrap()
                .contains("synthetic-private-credential")
        );
    }
    let http = Arc::new(FakeHttp::new());
    let missing = ResourceService::new(
        usage::resources::builtin_registry(),
        http.clone(),
        Arc::new(|_| Err("secret-error-do-not-echo".into())),
        Arc::new(Utc::now),
    );
    let reports = missing
        .query(&[config("new_api_token", json!({}))])
        .await
        .unwrap();
    assert_eq!(reports[0].snapshot.state, ResourceState::NeedsConfiguration);
    assert!(http.calls.lock().unwrap().is_empty());
    assert!(
        !serde_json::to_string(&reports)
            .unwrap()
            .contains("secret-error")
    );
}

fn costs_page(amounts: &str, more: bool, page: &str) -> String {
    format!(
        r#"{{"object":"page","data":[{{"results":[{amounts}]}}],"has_more":{more},"next_page":{page}}}"#
    )
}
fn amount(value: &str, currency: &str) -> String {
    format!(
        r#"{{"object":"organization.costs.result","amount":{{"value":{value},"currency":"{currency}"}}}}"#
    )
}

#[tokio::test]
async fn costs_follow_encoded_pagination_and_preserve_decimal_currency_and_scope() {
    let http = Arc::new(FakeHttp::new());
    let mut connection = config("openai_costs", json!({"days":7,"project_id":"proj A&B"}));
    connection.base_url = "https://api.openai.com/v1".into();
    http.route("https://api.openai.com/v1/organization/costs?start_time=1791460800&end_time=1792065600&limit=180&project_ids=proj+A%26B&page=next%2Fpage%3F", response(200,
        &costs_page(&format!("{},{}", amount("0.2", "usd"), amount("1e-8", "eur")), false, "null")));
    http.route(
        "https://api.openai.com/v1/organization/costs",
        response(
            200,
            &costs_page(&amount("0.1", "usd"), true, r#""next/page?""#),
        ),
    );
    let reports = service(http.clone()).query(&[connection]).await.unwrap();
    let snapshot = &reports[0].snapshot;
    assert_eq!(snapshot.state, ResourceState::Ready);
    assert_eq!(
        snapshot.scope,
        ResourceScope::Project {
            id: "proj A&B".into()
        }
    );
    let costs: Vec<_> = snapshot
        .metrics
        .iter()
        .map(|metric| match metric {
            ResourceMetric::Cost {
                used,
                period_start,
                period_end,
            } => {
                assert_eq!((*period_end - *period_start).num_days(), 7);
                (used.currency.as_str(), used.amount.as_str())
            }
            _ => panic!("official costs must not be a balance"),
        })
        .collect();
    assert_eq!(costs, [("EUR", "0.00000001"), ("USD", "0.3")]);
    let calls = http.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].url.contains("page=next%2Fpage%3F"));
}

#[tokio::test]
async fn costs_discard_partial_totals_on_failed_or_repeated_pages() {
    for status in [200, 500] {
        let http = Arc::new(FakeHttp::new());
        http.route(
            "https://relay.example/panel/organization/costs",
            response(
                status,
                &costs_page(&amount("123.45", "usd"), true, r#""same-cursor""#),
            ),
        );
        let reports = service(http.clone())
            .query(&[config("openai_costs", json!({}))])
            .await
            .unwrap();
        assert_ne!(reports[0].snapshot.state, ResourceState::Ready);
        assert!(reports[0].snapshot.metrics.is_empty());
        assert!(http.calls.lock().unwrap().len() <= 2);
    }
}

#[test]
fn connection_validation_and_selection_do_not_resolve_secrets_or_guess_protocols() {
    let http = Arc::new(FakeHttp::new());
    let service = ResourceService::new(
        usage::resources::builtin_registry(),
        http.clone(),
        Arc::new(|_| panic!("unexpected credential lookup")),
        Arc::new(Utc::now),
    );
    for (kind, options) in [
        ("new_api_token", json!({"currency":"USD"})),
        (
            "new_api_token",
            json!({"quota_per_unit":"0","currency":"USD"}),
        ),
        ("new_api_token", json!({"unknown":1})),
        ("new_api_account", json!({})),
        ("new_api_account", json!({"user_id":0})),
        ("openai_costs", json!({"days":181})),
        ("openai_costs", json!({"project_id":"a","api_key_id":"b"})),
        ("unregistered", json!({})),
    ] {
        assert!(service.prepare(&config(kind, options)).is_err());
    }
    for base in [
        "http://example.com",
        "https://user:password@example.com",
        "https://example.com?key=private",
        "file:///private/tmp/key",
    ] {
        let mut connection = config("new_api_token", json!({}));
        connection.base_url = base.into();
        assert!(service.prepare(&connection).is_err());
    }
    let mut first = config("new_api_token", json!({}));
    let profile = "123e4567-e89b-12d3-a456-426614174001";
    first.profile_ids.push(profile.into());
    let mut second = first.clone();
    second.id = "other".into();
    second.profile_ids.clear();
    assert_eq!(
        select(&[first.clone(), second], &[], Some(profile)).unwrap(),
        [first.clone()]
    );
    assert!(select(std::slice::from_ref(&first), &["missing".into()], None).is_err());
    let mut changed = first.clone();
    changed.revision = "rotation-2".into();
    assert_ne!(first.binding(), changed.binding());
    changed = first.clone();
    changed.options = json!({"quota_per_unit":"500000","currency":"USD"});
    assert_ne!(first.binding(), changed.binding());
    changed = first.clone();
    changed.credential_env = "OTHER_KEY".into();
    assert_ne!(first.binding(), changed.binding());
    assert!(http.calls.lock().unwrap().is_empty());
}
