//! 很小的 HTTP 客户端。测试可以换上按 URL 前缀返回的假实现。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct TransportError(pub String);

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, TransportError>;
}

pub struct ReqwestClient {
    client: reqwest::Client,
}

impl ReqwestClient {
    pub fn new() -> Self {
        Self::build(reqwest::redirect::Policy::default())
    }

    pub fn without_redirects() -> Self {
        Self::build(reqwest::redirect::Policy::none())
    }

    pub fn same_origin_redirects() -> Self {
        Self::build(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many same-origin redirects")
            } else if attempt.previous().first().is_some_and(|first| {
                first.origin() == attempt.url().origin()
                    && attempt.url().username().is_empty()
                    && attempt.url().password().is_none()
            }) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
    }

    fn build(redirects: reqwest::redirect::Policy) -> Self {
        let client = reqwest::Client::builder()
            .redirect(redirects)
            .timeout(Duration::from_secs(15))
            .build()
            .expect("HTTP client configuration is valid");
        Self { client }
    }
}

impl Default for ReqwestClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for ReqwestClient {
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, TransportError> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|error| TransportError(error.to_string()))?;
        let mut builder = self.client.request(method, url);
        for (name, value) in headers {
            builder = builder.header(*name, value);
        }
        if let Some(body) = body {
            builder = builder.body(body);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| TransportError(error.to_string()))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|text| (name.as_str().to_string(), text.to_string()))
            })
            .collect();
        let body = response
            .bytes()
            .await
            .map_err(|error| TransportError(error.to_string()))?
            .to_vec();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

pub fn is_transient_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..600).contains(&status)
}

pub fn body_excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body).replace('\n', " ");
    let text = text.trim();
    text.chars().take(400).collect()
}

#[derive(Default)]
pub struct FakeHttp {
    routes: Mutex<Vec<(String, Result<HttpResponse, TransportError>)>>,
    pub calls: Mutex<Vec<RecordedCall>>,
}

#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl FakeHttp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn route(&self, prefix: impl Into<String>, response: Result<HttpResponse, TransportError>) {
        self.routes
            .lock()
            .expect("fake http")
            .push((prefix.into(), response));
    }
}

#[async_trait]
impl HttpClient for FakeHttp {
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, TransportError> {
        self.calls.lock().expect("fake http").push(RecordedCall {
            method: method.to_string(),
            url: url.to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_string(), value.clone()))
                .collect(),
            body: body.clone(),
        });
        let routes = self.routes.lock().expect("fake http");
        for (prefix, result) in routes.iter() {
            if url.starts_with(prefix) {
                return result.clone();
            }
        }
        Err(TransportError(format!("no fake route for {url}")))
    }
}

#[cfg(test)]
mod redirect_tests {
    use super::*;
    use std::io::{Read, Write};
    fn server(
        count: usize,
        location: Option<String>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            let until = std::time::Instant::now() + Duration::from_secs(3);
            while requests.len() < count && std::time::Instant::now() < until {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut data = Vec::new();
                let mut buffer = [0; 4096];
                while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buffer).unwrap();
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buffer[..n]);
                }
                requests.push(String::from_utf8(data).unwrap());
                if requests.len() == 1
                    && let Some(location) = &location
                {
                    write!(stream,"HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                    )
                    .unwrap();
                }
            }
            requests
        });
        (base, handle)
    }
    #[tokio::test]
    async fn provider_queries_follow_same_origin_canonical_routes_but_stop_before_other_origins() {
        let (base, handle) = server(2, Some("/canonical/api/usage/token".into()));
        let client = ReqwestClient::same_origin_redirects();
        let result = client
            .request(
                "GET",
                &format!("{base}/api/usage/token"),
                &[("Authorization", "Bearer synthetic-key".into())],
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.status, 200);
        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with("GET /canonical/api/usage/token "));
        assert!(requests.iter().all(|r| {
            r.to_ascii_lowercase()
                .contains("authorization: bearer synthetic-key")
        }));
        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        other.set_nonblocking(true).unwrap();
        let (base, handle) = server(
            1,
            Some(format!(
                "http://{}/do-not-send-key",
                other.local_addr().unwrap()
            )),
        );
        let result = client
            .request(
                "GET",
                &base,
                &[("x-api-key", "synthetic-anthropic-key".into())],
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.status, 301);
        assert_eq!(handle.join().unwrap().len(), 1);
        assert!(
            matches!(other.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock)
        );
    }
}
