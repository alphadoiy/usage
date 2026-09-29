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
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
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
