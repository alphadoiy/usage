use super::*;
mod recording;
mod websocket;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Path as RoutePath, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::{RwLock, mpsc};

struct Route {
    registration: Registration,
    source: Arc<recording::Source>,
    active: AtomicUsize,
}
struct Shared {
    directory: PathBuf,
    token: String,
    port: u16,
    stopping: AtomicBool,
    routes: RwLock<HashMap<String, Arc<Route>>>,
    client: reqwest::Client,
    idle_timeout: Duration,
    records: mpsc::Sender<recording::Sample>,
    sources: Arc<recording::Sources>,
    plugins: crate::plugins::Runtime,
}
/// An empty service can retire only after a full idle interval; live routes reset it.
struct Idle {
    since: Option<Instant>,
}
impl Idle {
    fn new(now: Instant) -> Self {
        Self { since: Some(now) }
    }
    fn should_stop(&mut self, has_routes: bool, now: Instant) -> bool {
        if has_routes {
            self.since = None;
            return false;
        }
        now.saturating_duration_since(*self.since.get_or_insert(now)) >= Duration::from_secs(60)
    }
}
fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok())
        == Some(format!("Bearer {token}").as_str())
}
fn error(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({"error":{"message":message,"type":"usage_gateway"}})),
    )
        .into_response()
}
async fn status(State(state): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.token) {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    if state.stopping.load(Ordering::Acquire) {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Gateway is stopping");
    }
    let routes = state.routes.read().await;
    axum::Json(serde_json::json!({"service":"usage-gateway","pid":std::process::id(),"port":state.port,"routes":routes.len(),"protocol":PROTOCOL_VERSION,"plugin_actions":1,"active_requests":routes.values().map(|r|r.active.load(Ordering::Relaxed)).sum::<usize>()})).into_response()
}
async fn register(
    State(state): State<Arc<Shared>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !authorized(&headers, &state.token) {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    if state.stopping.load(Ordering::Acquire) {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Gateway is stopping");
    }
    if state.routes.read().await.len() >= 256 {
        return error(StatusCode::TOO_MANY_REQUESTS, "Too many active routes");
    }
    let Ok(registration) = serde_json::from_slice::<Registration>(&body) else {
        return error(StatusCode::BAD_REQUEST, "Invalid registration");
    };
    if uuid::Uuid::parse_str(&registration.run).is_err()
        || uuid::Uuid::parse_str(&registration.token).is_err()
        || registration.scope.len() != 64
        || !registration.scope.bytes().all(|b| b.is_ascii_hexdigit())
        || registration.lease != state.directory.join(format!("{}.lease", registration.run))
        || crate::http::validate_api_base_url(&registration.upstream).is_err()
        || !registration.valid_auth()
    {
        return error(StatusCode::BAD_REQUEST, "Invalid route");
    }
    let Some(source) = state
        .sources
        .reserve(&registration.scope, &registration.run)
    else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Performance source backlog",
        );
    };
    let run = registration.run.clone();
    let scope = registration.scope.clone();
    let owner = state.directory.to_string_lossy().into_owned();
    let registered = tokio::task::spawn_blocking(move || {
        crate::performance::Service::from_environment().register_gateway_run(&scope, &run, &owner)
    })
    .await;
    if !matches!(registered, Ok(Ok(()))) {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Cannot register performance source",
        );
    }
    let id = uuid::Uuid::new_v4().to_string();
    tracing::info!(
        event = "gateway_route_registered",
        run_id = registration.run,
        provider_id = registration.provider,
        model_provider = registration.model_provider,
        websocket_observation = registration.observe_websocket,
        upstream_host = reqwest::Url::parse(&registration.upstream)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
    );
    let mut routes = state.routes.write().await;
    if state.stopping.load(Ordering::Acquire) {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Gateway is stopping");
    }
    routes.insert(
        id.clone(),
        Arc::new(Route {
            source,
            registration,
            active: AtomicUsize::new(0),
        }),
    );
    axum::Json(id).into_response()
}
/// Never forward hop-by-hop headers, including names nominated by Connection.
pub(super) fn clean_headers(headers: &HeaderMap) -> HeaderMap {
    let nominated: HashSet<String> = headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    headers
        .iter()
        .filter(|(name, _)| {
            !nominated.contains(name.as_str())
                && !matches!(
                    name.as_str(),
                    "host"
                        | "authorization"
                        | "proxy-authorization"
                        | "proxy-authenticate"
                        | "connection"
                        | "keep-alive"
                        | "transfer-encoding"
                        | "te"
                        | "trailer"
                        | "upgrade"
                        | "content-length"
                )
        })
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect()
}
struct Capture {
    transport: &'static str,
    connection_id: Option<String>,
    route: Arc<Route>,
    records: mpsc::Sender<recording::Sample>,
    start: Instant,
    at: i64,
    id: String,
    session: String,
    requested: String,
    observer: metrics::Observer,
    status: u16,
    outcome: &'static str,
    inference: bool,
}
pub(super) fn metadata_session(body: &serde_json::Value) -> Option<String> {
    body.pointer("/client_metadata/thread_id").map(|id| {
        id.as_str()
            .filter(|s| uuid::Uuid::parse_str(s).is_ok())
            .unwrap_or_default()
            .to_owned()
    })
}
pub(super) fn header_session(headers: &HeaderMap) -> String {
    // Current Codex's `session-id` is cache affinity, not necessarily thread identity.
    ["thread-id", "thread_id", "session_id", "x-session-id"]
        .iter()
        .find_map(|name| headers.get(*name))
        .and_then(|id| id.to_str().ok())
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .unwrap_or_default()
        .to_owned()
}
impl Capture {
    fn new(
        route: Arc<Route>,
        records: mpsc::Sender<recording::Sample>,
        session: String,
        requested: String,
        transport: &'static str,
    ) -> Self {
        route.active.fetch_add(1, Ordering::Relaxed);
        Self {
            route,
            records,
            start: Instant::now(),
            at: chrono::Utc::now().timestamp_millis(),
            id: uuid::Uuid::new_v4().to_string(),
            session,
            observer: metrics::Observer::new(requested.clone()),
            requested,
            status: 0,
            outcome: "cancelled",
            inference: true,
            transport,
            connection_id: None,
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.route.active.fetch_sub(1, Ordering::Relaxed);
        if !self.inference {
            return;
        }
        let duration = self.start.elapsed().as_millis().min(i64::MAX as u128) as i64;
        let successful = (200..300).contains(&self.status) && self.observer.complete;
        let record = Record {
            transport: self.transport.into(),
            connection_id: self.connection_id.clone(),
            id: self.id.clone(),
            run: self.route.registration.run.clone(),
            session: self.session.clone(),
            at: self.at,
            model: self.observer.model.clone(),
            requested_model: self.requested.clone(),
            response_id: self.observer.response_id.clone(),
            input: self.observer.input,
            cache_read: self.observer.cache_read,
            first_text: self.observer.first_text,
            first_reasoning: self.observer.first_reasoning,
            flow: self.observer.flow(),
            completed_ms: self.observer.completed_at,
            speed_unavailable: if (200..300).contains(&self.status) {
                self.observer.speed_unavailable(duration)
            } else {
                Some(
                    if self.outcome == "finished" {
                        "upstream_http_error"
                    } else {
                        self.outcome
                    }
                    .into(),
                )
            },
            output: self.observer.output,
            reasoning: self.observer.reasoning,
            duration,
            ttft: if successful {
                self.observer.first
            } else {
                None
            },
            decode: if successful {
                self.observer.decode(duration)
            } else {
                None
            },
            status: self.status,
            complete: successful,
            native_transport: false,
            outcome: if successful && self.outcome == "cancelled" {
                "complete_client_closed".into()
            } else {
                self.outcome.into()
            },
        };
        tracing::info!(
            event = "gateway_request_finished",
            transport = record.transport,
            connection_id = record.connection_id,
            request_id = record.id,
            run_id = record.run,
            provider_id = self.route.registration.provider,
            model = record.model,
            requested_model = record.requested_model,
            session_id = record.session,
            status = record.status,
            outcome = record.outcome,
            complete = record.complete,
            duration_ms = record.duration,
            ttft_ms = record.ttft,
            input_tokens = record.input,
            cache_read_tokens = record.cache_read,
            first_text_ms = record.first_text,
            first_reasoning_ms = record.first_reasoning,
            flow_ms = record.flow,
            speed_unavailable = record.speed_unavailable,
            output_tokens = record.output,
            reasoning_tokens = record.reasoning,
            speed_tokens = record.decode.map(|s| s.0),
            speed_ms = record.decode.map(|s| s.1)
        );
        if self
            .records
            .try_send((
                self.route.source.clone(),
                self.route.registration.model_provider.clone(),
                record,
            ))
            .is_err()
        {
            self.route.source.fallback();
            tracing::error!(
                event = "gateway_record_dropped",
                request_id = self.id,
                run_id = self.route.registration.run,
                reason = "writer_queue_full_or_closed"
            );
        }
    }
}
async fn forward(
    State(state): State<Arc<Shared>>,
    RoutePath((id, path)): RoutePath<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    let Some(route) = state.routes.read().await.get(&id).cloned() else {
        return error(StatusCode::NOT_FOUND, "Route expired");
    };
    let Some((path, authorization)) = route.registration.request_auth(&path, request.headers())
    else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let method = request.method().clone();
    let upgrading = request
        .headers()
        .get("upgrade")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| s.eq_ignore_ascii_case("websocket"));
    let inference = method == Method::POST && matches!(path, "responses" | "chat/completions");
    if upgrading && !(method == Method::GET && path == "responses") {
        return error(StatusCode::NOT_FOUND, "Unsupported WebSocket path");
    }
    if !(inference || (upgrading && method == Method::GET && path == "responses"))
        && !(method == Method::GET && path == "models")
        && !(method == Method::POST && path == "responses/compact")
    {
        return error(StatusCode::NOT_FOUND, "Unsupported upstream path");
    }
    let query = request
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let upstream = format!("{}/{}{}", route.registration.upstream, path, query);
    let mut headers = clean_headers(request.headers());
    headers.insert("authorization", authorization);
    if upgrading {
        if route.registration.observe_websocket {
            return websocket::forward(state, route, request, upstream, headers).await;
        }
        tracing::info!(
            event = "gateway_transport_fallback",
            run_id = route.registration.run,
            from = "websocket",
            to = "sse",
            reason = "sse_default"
        );
        return error(StatusCode::UPGRADE_REQUIRED, "Use HTTP/SSE for Responses");
    }
    let session = header_session(request.headers());
    let body = match to_bytes(request.into_body(), 32 * 1024 * 1024).await {
        Ok(body) => body,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "Request exceeds 32 MiB"),
    };
    let request_json = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let session = request_json
        .as_ref()
        .and_then(metadata_session)
        .unwrap_or(session);
    let model = request_json
        .as_ref()
        .and_then(|v| {
            v.get("model")
                .and_then(|m| m.as_str())
                .filter(|m| m.len() <= 256 && !m.chars().any(char::is_control))
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let mut capture = Capture::new(route.clone(), state.records.clone(), session, model, "http");
    capture.inference = inference;
    tracing::info!(
        event = "gateway_request_started",
        transport = capture.transport,
        request_id = capture.id,
        run_id = route.registration.run,
        provider_id = route.registration.provider,
        model = capture.requested,
        session_id = capture.session,
        endpoint = path
    );
    let response = if let RouteAuth::Plugin { account } = &route.registration.auth {
        if path == "models" {
            match state
                .plugins
                .call(serde_json::json!({"operation":"models","account":account}))
                .await
            {
                Ok(models) => {
                    axum::Json(serde_json::json!({"object":"list","data":models})).into_response()
                }
                Err(_) => error(StatusCode::BAD_GATEWAY, "Plugin model discovery failed"),
            }
        } else if inference {
            state.plugins.inference(account,if path=="responses"{"responses"}else{"chat"},request_json.clone().unwrap_or_default(),serde_json::json!({"session":capture.session,"run":route.registration.run,"request":capture.id})).await
        } else {
            error(
                StatusCode::NOT_IMPLEMENTED,
                "Plugin backend does not support this endpoint",
            )
        }
    } else {
        match tokio::time::timeout(
            state.idle_timeout,
            state
                .client
                .request(method, upstream)
                .headers(headers)
                .body(body)
                .send(),
        )
        .await
        {
            Ok(Ok(response)) => {
                let status = response.status();
                let headers = response.headers().clone();
                let mut result = Response::new(Body::from_stream(response.bytes_stream()));
                *result.status_mut() = status;
                *result.headers_mut() = headers;
                result
            }
            Ok(Err(_)) => {
                capture.outcome = "upstream_error";
                capture.status = 502;
                return error(StatusCode::BAD_GATEWAY, "Upstream connection failed");
            }
            Err(_) => {
                capture.outcome = "upstream_timeout";
                capture.status = 504;
                return error(StatusCode::GATEWAY_TIMEOUT, "Upstream response timed out");
            }
        }
    };
    capture.status = response.status().as_u16();
    let status = response.status();
    let mut headers = clean_headers(response.headers());
    let streaming = response
        .headers()
        .get("content-type")
        .and_then(|s| s.to_str().ok())
        .is_some_and(|s| s.starts_with("text/event-stream"));
    headers.remove("content-length");
    let mut upstream = response.into_body().into_data_stream();
    let stream = async_stream::stream! {
        let mut json = Vec::new();
        let mut large = false;
        loop {
            let next = tokio::time::timeout(state.idle_timeout, upstream.next()).await;
            match next {
                Ok(Some(Ok(bytes))) => {
                    if inference {
                        if streaming { capture.observer.feed(&bytes, capture.start.elapsed().as_millis() as i64); }
                        else if !large {
                            if json.len() + bytes.len() <= 1024 * 1024 { json.extend_from_slice(&bytes); }
                            else { json.clear(); large=true; }
                        }
                    }
                    yield Ok::<_, std::io::Error>(bytes);
                }
                Ok(None) => {
                    if inference && !streaming && !large { capture.observer.json(&json); }
                    capture.outcome="finished";
                    break;
                }
                Err(_) => {
                    capture.outcome="stream_timeout";
                    yield Err(std::io::Error::other("upstream stream timed out"));
                    break;
                }
                Ok(Some(Err(_))) => {
                    capture.outcome="stream_error";
                    yield Err(std::io::Error::other("upstream stream interrupted"));
                    break;
                }
            }
        }
        drop(capture);
    };
    let mut result = Response::new(Body::from_stream(stream));
    *result.status_mut() = status;
    *result.headers_mut() = headers;
    result
}

async fn plugin_call(
    State(state): State<Arc<Shared>>,
    headers: HeaderMap,
    axum::Json(value): axum::Json<serde_json::Value>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    if state.stopping.load(Ordering::Acquire) {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Gateway is stopping");
    }
    match state.plugins.call(value).await {
        Ok(value) => axum::Json(value).into_response(),
        Err(reason) => {
            tracing::warn!(event = "plugin_call_failed", reason);
            error(
                StatusCode::BAD_GATEWAY,
                "Plugin operation failed; see Usage logs",
            )
        }
    }
}
async fn plugin_credentials(
    State(state): State<Arc<Shared>>,
    RoutePath(plugin): RoutePath<String>,
    headers: HeaderMap,
    axum::Json(value): axum::Json<serde_json::Value>,
) -> Response {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    state.plugins.credentials(&plugin, token, value).await
}

pub(super) async fn serve(directory: PathBuf) -> Result<(), String> {
    private_dir(&directory)?;
    let lock = private_file(&directory.join("service.lock"))?;
    lock.try_lock_exclusive().map_err(|_| "观测网关已运行")?;
    let owner = directory.to_string_lossy().into_owned();
    tokio::task::spawn_blocking(move || {
        crate::performance::Service::from_environment().recover_gateway_runs(&owner)
    })
    .await
    .map_err(|_| "无法恢复网关索引")??;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "无法监听本地网关端口")?;
    let port = listener
        .local_addr()
        .map_err(|_| "无法读取网关端口")?
        .port();
    let token = uuid::Uuid::new_v4().to_string();
    let (records, receiver) = mpsc::channel::<recording::Sample>(256);
    let sources = Arc::new(recording::Sources::default());
    let writer = tokio::spawn(recording::write(
        receiver,
        sources.clone(),
        Arc::new(crate::performance::Service::from_environment()),
    ));
    let state = Arc::new(Shared {
        plugins: crate::plugins::Runtime::new(crate::plugins::store::root(), port),
        directory: directory.clone(),
        token: token.clone(),
        port,
        stopping: AtomicBool::new(false),
        routes: RwLock::new(HashMap::new()),
        records,
        sources,
        idle_timeout: Duration::from_secs(120),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| "无法创建上游连接")?,
    });
    let router = Router::new()
        .route("/_usage/status", get(status))
        .route("/_usage/routes", post(register))
        .route("/_usage/plugins/call", post(plugin_call))
        .route(
            "/_usage/plugins/credentials/{plugin}",
            post(plugin_credentials),
        )
        .route("/r/{id}/{*path}", any(forward))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(state.clone());
    let path = directory.join("state.json");
    let temp = directory.join("state.tmp");
    {
        use std::io::Write;
        let mut file = private_file(&temp)?;
        file.set_len(0).map_err(|_| "无法更新网关状态")?;
        file.write_all(
            &serde_json::to_vec(&StateFile { port, token }).map_err(|_| "无法编码网关状态")?,
        )
        .map_err(|_| "无法写入网关状态")?;
        fs::rename(temp, &path).map_err(|_| "无法发布网关状态")?;
    }
    tracing::info!(event = "gateway_started", port, pid = std::process::id());
    let monitor = state.clone();
    let shutdown = async move {
        let mut idle = Idle::new(Instant::now());
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let mut routes = monitor.routes.write().await;
            let mut expired = Vec::new();
            routes.retain(|_, route| {
                if route.active.load(Ordering::Relaxed) > 0 {
                    return true;
                }
                let path = &route.registration.lease;
                if let Ok(file) = private_file(path) {
                    if file.try_lock_exclusive().is_err() {
                        return true;
                    }
                    let _ = fs::remove_file(path);
                }
                expired.push(route.registration.run.clone());
                tracing::info!(
                    event = "gateway_route_expired",
                    run_id = route.registration.run
                );
                false
            });
            let stopping = idle.should_stop(
                !routes.is_empty() || monitor.plugins.has_work(Instant::now()).await,
                Instant::now(),
            );
            if stopping {
                monitor.stopping.store(true, Ordering::Release);
            }
            drop(routes);
            for run in expired {
                let result = tokio::task::spawn_blocking(move || {
                    crate::performance::Service::from_environment().finish_gateway_run(&run)
                })
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    tracing::warn!(event = "gateway_run_finish_failed");
                }
            }
            if stopping {
                break;
            }
        }
    };
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|_| "网关服务异常退出".to_owned());
    let _ = fs::remove_file(path);
    drop(state);
    let _ = writer.await;
    tracing::info!(event = "gateway_stopped");
    drop(lock);
    result
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn idle_exit_waits_sixty_seconds_after_last_live_route() {
        let start = Instant::now();
        let mut idle = Idle::new(start);
        assert!(!idle.should_stop(false, start + Duration::from_secs(59)));
        assert!(idle.should_stop(false, start + Duration::from_secs(60)));
        assert!(!idle.should_stop(true, start + Duration::from_secs(120)));
        assert!(!idle.should_stop(false, start + Duration::from_secs(121)));
        assert!(!idle.should_stop(false, start + Duration::from_secs(180)));
        assert!(idle.should_stop(false, start + Duration::from_secs(181)));
    }

    async fn fixture(
        mode: &'static str,
    ) -> (
        Arc<Shared>,
        mpsc::Receiver<recording::Sample>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let app=Router::new().route("/responses",post(move || {
            let counted=counted.clone();
            async move {
                counted.fetch_add(1,Ordering::Relaxed);
                match mode {
                    "limit" => (StatusCode::TOO_MANY_REQUESTS,[("retry-after","17")],"vendor-limit-body").into_response(),
                    "headers" => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        "too late".into_response()
                    }
                    _ => {
                        let stream=async_stream::stream! {
                            yield Ok::<_,std::io::Error>(bytes::Bytes::from_static(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n"));
                            std::future::pending::<()>().await;
                        };
                        ([("content-type","text/event-stream")],Body::from_stream(stream)).into_response()
                    }
                }
            }
        }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let registration = Registration {
            observe_websocket: false,
            run: "run".into(),
            scope: "scope".into(),
            provider: "provider".into(),
            upstream,
            model_provider: "custom".into(),
            auth: RouteAuth::ApiKey {
                key: "synthetic-upstream".into(),
            },
            token: "local".into(),
            lease: PathBuf::new(),
        };
        let (records, receiver) = mpsc::channel(8);
        let sources = Arc::new(recording::Sources::default());
        let state = Arc::new(Shared {
            plugins: crate::plugins::Runtime::new(crate::plugins::store::root(), 0),
            sources: sources.clone(),
            directory: PathBuf::new(),
            token: "admin".into(),
            port: 0,
            stopping: AtomicBool::new(false),
            routes: RwLock::new(HashMap::from([(
                "route".into(),
                Arc::new(Route {
                    source: sources
                        .reserve(&registration.scope, &registration.run)
                        .unwrap(),
                    registration,
                    active: AtomicUsize::new(0),
                }),
            )])),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            idle_timeout: Duration::from_millis(80),
            records,
        });
        (state, receiver, hits, task)
    }
    fn request() -> axum::extract::Request {
        axum::http::Request::builder()
            .method("POST")
            .header("authorization", "Bearer local")
            .body(Body::from(r#"{"model":"m"}"#))
            .unwrap()
    }
    #[tokio::test]
    async fn websocket_upgrade_returns_426_without_contacting_upstream_or_recording_a_request() {
        let (state, mut records, hits, task) = fixture("limit").await;
        let request = axum::http::Request::builder()
            .method("GET")
            .header("authorization", "Bearer local")
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .body(Body::empty())
            .unwrap();
        let response = forward(
            State(state),
            RoutePath(("route".into(), "responses".into())),
            request,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
        assert_eq!(hits.load(Ordering::Relaxed), 0);
        assert!(records.try_recv().is_err());
        task.abort();
    }
    #[tokio::test]
    async fn rate_limit_is_forwarded_unchanged_without_gateway_retries() {
        let (state, mut records, hits, task) = fixture("limit").await;
        let response = forward(
            State(state),
            RoutePath(("route".into(), "responses".into())),
            request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["retry-after"], "17");
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            "vendor-limit-body"
        );
        let (_, _, record) = records.recv().await.unwrap();
        assert!(!record.complete && record.decode.is_none());
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        task.abort();
    }
    #[tokio::test]
    async fn stalled_response_headers_return_504_and_do_not_retry() {
        let (state, mut records, hits, task) = fixture("headers").await;
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            forward(
                State(state),
                RoutePath(("route".into(), "responses".into())),
                request(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let (_, _, record) = records.recv().await.unwrap();
        assert_eq!(record.outcome, "upstream_timeout");
        assert!(!record.complete && record.decode.is_none());
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        task.abort();
    }
    #[tokio::test]
    async fn stalled_stream_ends_without_a_successful_speed_sample() {
        let (state, mut records, hits, task) = fixture("stream").await;
        let response = forward(
            State(state),
            RoutePath(("route".into(), "responses".into())),
            request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), to_bytes(response.into_body(), 1024))
                .await
                .unwrap()
                .is_err()
        );
        let (_, _, record) = records.recv().await.unwrap();
        assert_eq!(record.outcome, "stream_timeout");
        assert!(!record.complete && record.decode.is_none());
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        task.abort();
    }
}

#[cfg(test)]
mod plugin_login_tests {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_manual_login_prevents_idle_shutdown() {
        let root = std::env::temp_dir().join(format!("usage-login-idle-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let entry = root.join("fixture.mjs");
        fs::write(
            &entry,
            include_str!("../plugins/fixture.mjs").replace("method: \"auto\"", "method: \"code\""),
        )
        .unwrap();
        let store = crate::plugins::store::Store::new(root.join("plugins"));
        store
            .register(crate::plugins::store::Plugin {
                id: "fixture".into(),
                package: "local".into(),
                version: "1".into(),
                entry,
                runtime: "node".into(),
                enabled: true,
                revision: 1,
            })
            .unwrap();
        let runtime = crate::plugins::Runtime::new(store.root.clone(), 0);
        let begin=runtime.call(serde_json::json!({"operation":"login.begin","plugin":"fixture","provider":"fixture","method":"0","inputs":{"uid":"pending"}})).await.unwrap();
        assert_eq!(begin["method"], "code");
        let start = Instant::now();
        let mut idle = Idle::new(start);
        let has_work = runtime.has_work(start + Duration::from_secs(61)).await;
        let stops = idle.should_stop(has_work, start + Duration::from_secs(61));
        runtime.call(serde_json::json!({"operation":"login.cancel","plugin":"fixture","provider":"fixture","session":begin["session"]})).await.unwrap();
        assert!(
            !runtime.has_work(Instant::now()).await,
            "cancel releases the login lease"
        );
        runtime.call(serde_json::json!({"operation":"login.begin","plugin":"fixture","provider":"fixture","method":"0","inputs":{}})).await.unwrap();
        assert!(
            !runtime
                .has_work(Instant::now() + Duration::from_secs(601))
                .await,
            "expired login cannot keep the gateway running"
        );
        drop(runtime);
        let _ = fs::remove_dir_all(root);
        assert!(
            !stops,
            "a valid pending manual login must keep its callback host alive during the 10 minute login window"
        );
    }
}
