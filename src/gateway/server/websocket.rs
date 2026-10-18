//! Raw WebSocket tunnel: preserves masking, extensions, framing and caller authentication.
//! Until per-response WS timing is supported, its run uses native CLI statistics.
use super::*;

struct Active(Arc<Route>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(super) async fn forward(
    state: Arc<Shared>,
    route: Arc<Route>,
    mut request: axum::extract::Request,
    upstream: String,
    mut headers: HeaderMap,
) -> Response {
    route.active.fetch_add(1, Ordering::Relaxed);
    let active = Active(route.clone());
    let record = Record {
        id: uuid::Uuid::new_v4().to_string(),
        run: route.registration.run.clone(),
        native_transport: true,
        ..Default::default()
    };
    if state
        .records
        .try_send((
            route.registration.scope.clone(),
            route.registration.model_provider.clone(),
            record,
        ))
        .is_err()
    {
        tracing::warn!(
            event = "gateway_record_dropped",
            run_id = route.registration.run,
            reason = "native_transport_queue_full"
        );
    }
    // The upstream performs the actual handshake; no frame or extension is rewritten.
    headers.insert("connection", HeaderValue::from_static("Upgrade"));
    headers.insert("upgrade", HeaderValue::from_static("websocket"));
    let response = match tokio::time::timeout(
        state.idle_timeout,
        state
            .client
            .get(upstream)
            .version(reqwest::Version::HTTP_11)
            .headers(headers)
            .send(),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "Upstream WebSocket connection failed",
            );
        }
        Err(_) => {
            return error(
                StatusCode::GATEWAY_TIMEOUT,
                "Upstream WebSocket handshake timed out",
            );
        }
    };
    let status = response.status();
    let mut headers = clean_headers(response.headers());
    if status != StatusCode::SWITCHING_PROTOCOLS {
        let mut upstream = response.bytes_stream();
        let stream = async_stream::stream! {
            loop {
                match tokio::time::timeout(state.idle_timeout,upstream.next()).await {
                    Ok(Some(bytes))=>yield bytes.map_err(|_|std::io::Error::other("upstream WebSocket refusal interrupted")),
                    Ok(None)=>break,
                    Err(_)=>{yield Err(std::io::Error::other("upstream WebSocket refusal timed out"));break;}
                }
            }
        };
        let mut result = Response::new(Body::from_stream(stream));
        *result.status_mut() = status;
        *result.headers_mut() = headers;
        return result;
    }
    headers.insert("connection", HeaderValue::from_static("Upgrade"));
    headers.insert("upgrade", HeaderValue::from_static("websocket"));
    let upstream = match response.upgrade().await {
        Ok(upstream) => upstream,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "Upstream WebSocket upgrade failed"),
    };
    let downstream = hyper::upgrade::on(&mut request);
    tracing::info!(
        event = "gateway_websocket_opened",
        run_id = route.registration.run,
        provider_id = route.registration.provider,
        statistics = "native_cli"
    );
    tokio::spawn(async move {
        if let Ok(downstream) = downstream.await {
            let mut downstream = hyper_util::rt::TokioIo::new(downstream);
            let mut upstream = upstream;
            let result = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
            tracing::info!(
                event = "gateway_websocket_closed",
                run_id = route.registration.run,
                clean_transport_close = result.is_ok()
            );
        }
        drop(active);
    });
    let mut result = Response::new(Body::empty());
    *result.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    *result.headers_mut() = headers;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{RwLock, mpsc};

    #[tokio::test]
    async fn native_websocket_preserves_handshake_authentication_and_raw_frames() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/responses",
            get(|mut request: axum::extract::Request| async move {
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
                    let mut frame = [0; 8];
                    io.read_exact(&mut frame).await.unwrap();
                    assert_eq!(frame, [0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2]);
                    io.write_all(&[0x81, 3, b'o', b'k', b'!']).await.unwrap();
                });
                Response::builder()
                    .status(101)
                    .header("connection", "Upgrade")
                    .header("upgrade", "websocket")
                    .header("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
                    .body(Body::empty())
                    .unwrap()
            }),
        );
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (records, mut receiver) = mpsc::channel(8);
        let state = Arc::new(Shared {
            directory: PathBuf::new(),
            token: "admin".into(),
            port: 0,
            stopping: AtomicBool::new(false),
            routes: RwLock::new(HashMap::from([(
                "route".into(),
                Arc::new(Route {
                    active: AtomicUsize::new(0),
                    registration: Registration {
                        run: "run".into(),
                        scope: "scope".into(),
                        provider: "provider".into(),
                        upstream,
                        model_provider: "openai".into(),
                        auth: RouteAuth::NativeOauth {
                            account_id: "workspace".into(),
                        },
                        token: "capability".into(),
                        lease: PathBuf::new(),
                    },
                }),
            )])),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            idle_timeout: Duration::from_secs(2),
            records,
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
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 101);
        assert_eq!(
            response.headers()["sec-websocket-accept"],
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        let mut io = response.upgrade().await.unwrap();
        io.write_all(&[0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2])
            .await
            .unwrap();
        let mut reply = [0; 5];
        tokio::time::timeout(Duration::from_secs(2), io.read_exact(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply, [0x81, 3, b'o', b'k', b'!']);
        let (_, _, record) = receiver.recv().await.unwrap();
        assert!(record.native_transport && record.decode.is_none());
        drop(io);
        gateway_task.abort();
        upstream_task.abort();
    }
}
