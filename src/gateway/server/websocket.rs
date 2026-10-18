//! Observe bounded RFC 6455 messages while forwarding the original bytes unchanged.
//! Unsupported encodings or ambiguous attribution fall back to CLI statistics for the run.
use super::*;
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

mod frames;
#[cfg(test)]
mod tests;

const MAX_PENDING: usize = 64;
struct Active(Arc<Route>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

struct Tracker {
    route: Arc<Route>,
    records: mpsc::Sender<recording::Sample>,
    session: String,
    connection: String,
    pending: HashMap<String, VecDeque<Capture>>,
    finished: VecDeque<String>,
    disabled: bool,
}
impl Tracker {
    fn fallback(&mut self, reason: &'static str) {
        if self.disabled {
            return;
        }
        self.disabled = true;
        tracing::warn!(
            event = "gateway_websocket_observation_fallback",
            run_id = self.route.registration.run,
            connection_id = self.connection,
            reason,
            statistics = "native_cli"
        );
        for queue in self.pending.values_mut() {
            for sample in queue.iter_mut() {
                sample.inference = false;
            }
        }
        self.pending.clear();
        self.route.source.fallback();
    }
    fn message(&mut self, client: bool, bytes: &[u8]) {
        if self.disabled {
            return;
        }
        let Ok(event) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            self.fallback("invalid_json_message");
            return;
        };
        let Some(kind) = event.get("type").and_then(serde_json::Value::as_str) else {
            self.fallback("unknown_message_type");
            return;
        };
        let lane = match event.get("stream_id") {
            None => "",
            Some(value) => match value.as_str().filter(|s| {
                !s.is_empty()
                    && s.len() <= 256
                    && s.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
            }) {
                Some(s) => s,
                None => {
                    self.fallback("invalid_stream_id");
                    return;
                }
            },
        };
        if client {
            if kind == "response.cancel" {
                return;
            }
            if kind != "response.create" {
                self.fallback("unsupported_client_event");
                return;
            }
            if self.pending.values().map(VecDeque::len).sum::<usize>() >= MAX_PENDING {
                self.fallback("pending_request_limit");
                return;
            }
            let model = event
                .get("model")
                .and_then(serde_json::Value::as_str)
                .filter(|s| s.len() <= 256 && !s.chars().any(char::is_control))
                .unwrap_or_default();
            let mut sample = Capture::new(
                self.route.clone(),
                self.records.clone(),
                metadata_session(&event).unwrap_or_else(|| self.session.clone()),
                model.into(),
                "websocket",
            );
            sample.connection_id = Some(self.connection.clone());
            sample.status = 200;
            sample.inference =
                event.get("generate").and_then(serde_json::Value::as_bool) != Some(false);
            tracing::info!(
                event = "gateway_request_started",
                transport = "websocket",
                request_id = sample.id,
                run_id = self.route.registration.run,
                connection_id = self.connection,
                model = sample.requested,
                session_id = sample.session,
                generation = sample.inference
            );
            self.pending
                .entry(lane.into())
                .or_default()
                .push_back(sample);
            return;
        }
        if !kind.starts_with("response.") && kind != "error" {
            return;
        }
        let response_id = event
            .pointer("/response/id")
            .or_else(|| event.get("response_id"))
            .and_then(serde_json::Value::as_str);
        if response_id.is_some_and(|id| self.finished.iter().any(|old| old == id)) {
            return;
        }
        let Some(queue) = self.pending.get_mut(lane).filter(|q| !q.is_empty()) else {
            self.fallback("unmatched_server_event");
            return;
        };
        // An error without a response ID cannot disambiguate queued requests on a lane.
        if kind == "error" && queue.len() != 1 {
            self.fallback("ambiguous_error_event");
            return;
        }
        let sample = queue.front_mut().expect("nonempty lane");
        if kind == "response.created" {
            let Some(id) = response_id.filter(|s| !s.is_empty() && s.len() <= 256) else {
                self.fallback("missing_response_id");
                return;
            };
            if sample
                .observer
                .response_id
                .as_deref()
                .is_some_and(|old| old != id)
            {
                self.fallback("unmatched_response_created");
                return;
            }
        } else if kind != "error"
            && (sample.observer.response_id.is_none()
                || response_id.is_some_and(|id| sample.observer.response_id.as_deref() != Some(id)))
        {
            self.fallback("unmatched_response_id");
            return;
        }
        let ms = sample.start.elapsed().as_millis().min(i64::MAX as u128) as i64;
        sample.observer.websocket(&event, ms);
        let terminal = match kind {
            "response.completed" => Some("finished"),
            "response.failed" => Some("upstream_ws_failed"),
            "response.incomplete" => Some("upstream_ws_incomplete"),
            "response.cancelled" => Some("upstream_ws_cancelled"),
            "error" => Some("upstream_ws_error"),
            _ => None,
        };
        if let Some(outcome) = terminal {
            let mut sample = queue.pop_front().expect("nonempty lane");
            sample.outcome = outcome;
            if matches!(kind, "error" | "response.failed") {
                sample.status = event
                    .get("status")
                    .or_else(|| event.pointer("/error/status"))
                    .and_then(serde_json::Value::as_u64)
                    .filter(|v| (400..600).contains(v))
                    .unwrap_or(502) as u16;
            }
            if let Some(id) = sample.observer.response_id.as_ref() {
                if self.finished.len() == 128 {
                    self.finished.pop_front();
                }
                self.finished.push_back(id.clone());
            }
            if queue.is_empty() {
                self.pending.remove(lane);
            }
            drop(sample); // Persist immediately at the terminal event, not at socket EOF.
        }
    }
}
impl Drop for Tracker {
    fn drop(&mut self) {
        for queue in self.pending.values_mut() {
            for sample in queue {
                sample.outcome = "websocket_disconnected";
            }
        }
    }
}

struct Observed<T> {
    io: T,
    frames: frames::Frames,
    tracker: Arc<Mutex<Tracker>>,
    client: bool,
}
impl<T: AsyncRead + Unpin> AsyncRead for Observed<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.io).poll_read(cx, buf);
        if buf.filled().len() > before {
            let mut tracker = this.tracker.lock().unwrap_or_else(|p| p.into_inner());
            if !tracker.disabled {
                let reason = this
                    .frames
                    .feed(&buf.filled()[before..], this.client, |message| {
                        tracker.message(this.client, message)
                    });
                if let Some(reason) = reason {
                    tracker.fallback(reason);
                }
            }
        }
        result
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for Observed<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
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
    headers.insert(
        "connection",
        axum::http::HeaderValue::from_static("Upgrade"),
    );
    headers.insert("upgrade", axum::http::HeaderValue::from_static("websocket"));
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
    headers.insert(
        "connection",
        axum::http::HeaderValue::from_static("Upgrade"),
    );
    headers.insert("upgrade", axum::http::HeaderValue::from_static("websocket"));
    let extensions = headers
        .get("sec-websocket-extensions")
        .and_then(|h| h.to_str().ok());
    let client_frames = frames::Frames::negotiated(extensions, true);
    let server_frames = frames::Frames::negotiated(extensions, false);
    let deflate = extensions.is_some_and(|s| s.split(';').next() == Some("permessage-deflate"));
    let upstream = match response.upgrade().await {
        Ok(io) => io,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "Upstream WebSocket upgrade failed"),
    };
    let downstream = hyper::upgrade::on(&mut request);
    let session = header_session(request.headers());
    let connection = uuid::Uuid::new_v4().to_string();
    let tracker = Arc::new(Mutex::new(Tracker {
        route: route.clone(),
        records: state.records.clone(),
        session,
        connection: connection.clone(),
        pending: HashMap::new(),
        finished: VecDeque::new(),
        disabled: false,
    }));
    tracing::info!(
        event = "gateway_websocket_opened",
        run_id = route.registration.run,
        connection_id = connection,
        permessage_deflate = deflate,
        statistics = "gateway_observation"
    );
    tokio::spawn(async move {
        if let Ok(downstream) = downstream.await {
            let mut downstream = Observed {
                io: hyper_util::rt::TokioIo::new(downstream),
                frames: client_frames,
                tracker: tracker.clone(),
                client: true,
            };
            let mut upstream = Observed {
                io: upstream,
                frames: server_frames,
                tracker: tracker.clone(),
                client: false,
            };
            let result = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
            tracing::info!(
                event = "gateway_websocket_closed",
                run_id = route.registration.run,
                connection_id = connection,
                clean_transport_close = result.is_ok()
            );
        }
        drop(tracker);
        drop(active);
    });
    let mut result = Response::new(Body::empty());
    *result.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    *result.headers_mut() = headers;
    result
}
