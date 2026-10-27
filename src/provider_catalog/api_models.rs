//! Read a relay's live model list using Magpie's path, auth and pagination conventions.
use std::collections::BTreeSet;
use std::time::Duration;

use reqwest::Url;
use serde::Deserialize;

use crate::http::{HttpClient, HttpResponse};

pub(super) struct Query {
    pub(super) base: Url,
    urls: Vec<Url>,
    anthropic: bool,
    auth: Auth,
}

#[derive(Clone, Copy)]
enum Auth {
    Both,
    ApiKey,
    Bearer,
}

impl Query {
    pub(super) fn new(base: &str) -> Result<Self, &'static str> {
        let base = crate::http::validate_api_base_url(base).map_err(|error| match error {
            crate::http::BaseUrlError::Invalid => "模型接口地址无效",
            crate::http::BaseUrlError::Policy => {
                "模型接口地址需为无凭据和查询参数的 HTTPS 地址（本机回环可用 HTTP）"
            }
        })?;
        let official_anthropic = base.host_str() == Some("api.anthropic.com");
        let path = base.path().trim_end_matches('/');
        let anthropic = official_anthropic || path.ends_with("/anthropic");
        let versioned = path.split('/').any(
            |part| matches!(part.as_bytes(), [b'v' | b'V', digit, ..] if digit.is_ascii_digit()),
        );
        let mut paths = Vec::new();
        // Known official roots don't need a speculative request to /models.
        if official_anthropic && path.is_empty() {
            paths.push("/v1/models".to_owned());
        } else {
            paths.push(format!("{path}/models"));
            if anthropic || !versioned {
                paths.push(format!("{path}/v1/models"));
                let mut root = path;
                for suffix in [
                    "/anthropic",
                    "/apps/anthropic",
                    "/api/anthropic",
                    "/v1",
                    "/api",
                    "/api/v1",
                ] {
                    root = root.strip_suffix(suffix).unwrap_or(root);
                }
                paths.push(format!("{root}/v1/models"));
                paths.push(format!("{root}/models"));
            }
        }
        let mut urls = Vec::new();
        for path in paths {
            let mut url = base.clone();
            url.set_path(&path);
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
        Ok(Self {
            base,
            urls,
            anthropic,
            auth: if official_anthropic {
                Auth::ApiKey
            } else {
                Auth::Both
            },
        })
    }

    pub(super) async fn fetch(
        &self,
        http: &dyn HttpClient,
        key: &str,
    ) -> Result<Vec<String>, &'static str> {
        let mut error = None;
        for url in &self.urls {
            let result =
                tokio::time::timeout(Duration::from_secs(8), self.fetch_url(http, url, key))
                    .await
                    .unwrap_or(Err("模型列表查询超时"));
            match result {
                Ok(models) => return Ok(models),
                Err(next) => {
                    // A later missing route must not hide an auth or malformed-page error.
                    if error.is_none()
                        || matches!(
                            error,
                            Some("模型列表网络请求失败" | "此端点未提供模型列表接口")
                        )
                    {
                        error = Some(next);
                    }
                }
            }
        }
        Err(error.unwrap_or("此端点未提供模型列表接口"))
    }

    async fn fetch_url(
        &self,
        http: &dyn HttpClient,
        first: &Url,
        key: &str,
    ) -> Result<Vec<String>, &'static str> {
        let mut url = first.clone();
        let mut auth = self.auth;
        let mut models = BTreeSet::new();
        let mut cursors = BTreeSet::new();
        let mut count = 0;
        for _ in 0..20 {
            let response = self.request(http, &url, key, auth).await?;
            let response = if matches!(auth, Auth::Both) && both_auth_refused(&response) {
                let mut response = response;
                for single in [Auth::ApiKey, Auth::Bearer] {
                    response = self.request(http, &url, key, single).await?;
                    if !matches!(response.status, 401 | 403) {
                        auth = single;
                        break;
                    }
                }
                response
            } else {
                response
            };
            match response.status {
                200 => {}
                401 | 403 => return Err("模型列表认证失败或权限不足"),
                404 | 405 | 501 => return Err("此端点未提供模型列表接口"),
                429 => return Err("模型列表查询受到限流"),
                _ => return Err("模型列表接口暂不可用"),
            }
            let page: Page =
                serde_json::from_slice(&response.body).map_err(|_| "模型列表响应格式无效")?;
            let rows = match (page.data, page.models) {
                (Some(rows), other) if rows.is_empty() => other.unwrap_or(rows),
                (Some(rows), _) | (None, Some(rows)) => rows,
                (None, None) => return Err("模型列表响应格式无效"),
            };
            count += rows.len();
            if count > 5000 {
                return Err("模型列表超过展示上限");
            }
            for row in rows {
                let id = row
                    .id
                    .filter(|id| !id.is_empty())
                    .or(row.name)
                    .unwrap_or_default();
                if id.is_empty() {
                    continue;
                }
                if id.len() > 256 || id.chars().any(char::is_control) {
                    return Err("模型列表包含无效的模型名称");
                }
                let output = row
                    .modalities
                    .as_ref()
                    .and_then(|m| m.output.as_deref())
                    .unwrap_or_default();
                if text_model(&id, output, row.kind.as_deref().unwrap_or_default()) {
                    models.insert(id);
                }
            }
            if !page.has_more {
                return if models.is_empty() {
                    Err("模型列表为空，可手动输入模型 ID")
                } else {
                    Ok(models.into_iter().collect())
                };
            }
            let cursor = page
                .last_id
                .filter(|id| !id.is_empty())
                .ok_or("模型列表分页缺少游标")?;
            if !cursors.insert(cursor.clone()) {
                return Err("模型列表分页游标重复");
            }
            url = first.clone();
            url.query_pairs_mut()
                .append_pair("after_id", &cursor)
                .append_pair("limit", "1000");
        }
        Err("模型列表分页超过上限")
    }

    async fn request(
        &self,
        http: &dyn HttpClient,
        url: &Url,
        key: &str,
        auth: Auth,
    ) -> Result<HttpResponse, &'static str> {
        let mut headers = vec![
            ("Accept", "application/json".into()),
            ("User-Agent", format!("usage/{}", env!("CARGO_PKG_VERSION"))),
        ];
        if !matches!(auth, Auth::ApiKey) {
            headers.push(("Authorization", format!("Bearer {key}")));
        }
        if !matches!(auth, Auth::Bearer) {
            headers.push(("x-api-key", key.into()));
        }
        if self.anthropic {
            headers.push(("anthropic-version", "2023-06-01".into()));
        }
        let response = http
            .request("GET", url.as_str(), &headers, None)
            .await
            .map_err(|_| "模型列表网络请求失败")?;
        if response.body.len() > 2 * 1024 * 1024 {
            return Err("模型列表响应过大");
        }
        Ok(response)
    }
}

#[derive(Deserialize)]
struct Page {
    data: Option<Vec<Row>>,
    models: Option<Vec<Row>>,
    #[serde(default)]
    has_more: bool,
    last_id: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Row {
    id: Option<String>,
    name: Option<String>,
    kind: Option<String>,
    modalities: Option<Modalities>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Modalities {
    output: Option<Vec<String>>,
}

fn text_model(id: &str, output: &[String], kind: &str) -> bool {
    if matches!(kind, "image" | "video")
        || (!output.is_empty() && !output.iter().any(|v| v == "text"))
    {
        return false;
    }
    let id = id.to_ascii_lowercase();
    ![
        "embed",
        "-tts",
        "image",
        "audio",
        "-live",
        "robotics",
        "computer-use",
        "deep-research",
        "transcribe",
        "realtime",
        "moderation",
        "whisper",
        "dall-e",
        "sora",
    ]
    .iter()
    .any(|part| id.contains(part))
}

fn both_auth_refused(response: &HttpResponse) -> bool {
    if response.status != 401 {
        return false;
    }
    let body = String::from_utf8_lossy(&response.body).to_lowercase();
    body.contains("x-api-key")
        && body.contains("authorization")
        && [
            "同时",
            "并存",
            "共存",
            "只能使用一个",
            "只能选择一个",
            "只能有一个",
            "cannot both",
            "can't both",
            "can not both",
            "cannot be both",
            "not both",
            "together",
            "at the same time",
            "simultaneous",
            "mutually exclusive",
            "only one of",
            "only one auth",
            "only one header",
            "both present",
            "both be present",
            "both provided",
            "both be provided",
        ]
        .iter()
        .any(|part| body.contains(part))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{FakeHttp, TransportError};

    fn reply(status: u16, body: &str) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            headers: Default::default(),
            body: body.as_bytes().into(),
        })
    }

    #[test]
    fn candidate_paths_match_magpie_and_preserve_versioned_bases_and_origins() {
        for (base, paths) in [
            ("https://relay.invalid", vec!["/models", "/v1/models"]),
            (
                "https://relay.invalid/api",
                vec!["/api/models", "/api/v1/models", "/v1/models", "/models"],
            ),
            (
                "https://relay.invalid/api/paas/v4",
                vec!["/api/paas/v4/models"],
            ),
            (
                "https://relay.invalid/v1beta/openai",
                vec!["/v1beta/openai/models"],
            ),
            (
                "https://relay.invalid/anthropic",
                vec![
                    "/anthropic/models",
                    "/anthropic/v1/models",
                    "/v1/models",
                    "/models",
                ],
            ),
        ] {
            let query = Query::new(base).unwrap();
            assert_eq!(
                query.urls.iter().map(|url| url.path()).collect::<Vec<_>>(),
                paths
            );
            assert!(
                query
                    .urls
                    .iter()
                    .all(|url| url.origin() == query.base.origin())
            );
        }
        for base in [
            "https://u:secret@relay.invalid/v1",
            "https://relay.invalid/v1?key=secret",
            "http://relay.invalid/v1",
        ] {
            assert!(Query::new(base).is_err());
        }
    }

    #[tokio::test]
    async fn empty_first_list_falls_back_to_v1_and_filters_non_chat_models() {
        let http = FakeHttp::new();
        http.route(
            "https://relay.invalid/api/models",
            reply(200, r#"{"data":[]}"#),
        );
        http.route("https://relay.invalid/api/v1/models", reply(200, r#"{"models":[{"id":"model-a"},{"id":null,"name":"model-b","kind":null,"modalities":null},{"id":"model-a"},{"id":"text-embedding-3"},{"id":"gpt-image-2"},{"id":"voice","modalities":{"output":["audio"]}},{"id":"movie","kind":"video"}]}"#));
        assert_eq!(
            Query::new("https://relay.invalid/api")
                .unwrap()
                .fetch(&http, "fixture-key")
                .await
                .unwrap(),
            ["model-a", "model-b"]
        );
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        for call in calls.iter() {
            assert_eq!(call.method, "GET");
            assert!(call.body.is_none());
            assert!(
                call.headers
                    .contains(&("Authorization".into(), "Bearer fixture-key".into()))
            );
            assert!(
                call.headers
                    .contains(&("x-api-key".into(), "fixture-key".into()))
            );
            assert!(
                !call
                    .headers
                    .iter()
                    .any(|(key, _)| key == "anthropic-version")
            );
        }
    }

    #[tokio::test]
    async fn custom_anthropic_base_uses_version_header_and_same_origin_fallbacks() {
        let http = FakeHttp::new();
        http.route(
            "https://relay.invalid/v1/models",
            reply(200, r#"{"data":[{"id":"claude-model"}]}"#),
        );
        assert_eq!(
            Query::new("https://relay.invalid/anthropic")
                .unwrap()
                .fetch(&http, "fixture-key")
                .await
                .unwrap(),
            ["claude-model"]
        );
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        for call in calls.iter() {
            assert!(call.url.starts_with("https://relay.invalid/"));
            for (name, value) in [
                ("Authorization", "Bearer fixture-key"),
                ("x-api-key", "fixture-key"),
                ("anthropic-version", "2023-06-01"),
            ] {
                assert!(call.headers.contains(&(name.into(), value.into())));
            }
        }
    }

    #[tokio::test]
    async fn failed_pagination_discards_partial_list_before_trying_another_route() {
        let http = FakeHttp::new();
        http.route(
            "https://relay.invalid/api/models?after_id=first",
            reply(503, "private-upstream-error"),
        );
        http.route(
            "https://relay.invalid/api/models",
            reply(
                200,
                r#"{"data":[{"id":"partial"}],"has_more":true,"last_id":"first"}"#,
            ),
        );
        http.route(
            "https://relay.invalid/api/v1/models",
            reply(200, r#"{"data":[{"id":"complete"}]}"#),
        );
        assert_eq!(
            Query::new("https://relay.invalid/api")
                .unwrap()
                .fetch(&http, "fixture-key")
                .await
                .unwrap(),
            ["complete"]
        );
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls[1].url.contains("after_id=first&limit=1000"));
    }

    #[tokio::test]
    async fn incomplete_pages_and_generic_auth_failures_never_return_partial_models_or_secrets() {
        for (status, body, expected) in [
            (401, "fixture-key is invalid", "认证失败"),
            (
                200,
                r#"{"data":[{"id":"partial"}],"has_more":true}"#,
                "缺少游标",
            ),
            (
                200,
                r#"{"data":[{"id":"partial"}],"has_more":true,"last_id":"same"}"#,
                "游标重复",
            ),
            (200, r#"{"data":[],"models":[]}"#, "模型列表为空"),
            (200, r#"{"unexpected":"fixture-key"}"#, "格式无效"),
        ] {
            let http = FakeHttp::new();
            http.route("https://relay.invalid/v1/models", reply(status, body));
            let error = Query::new("https://relay.invalid/v1")
                .unwrap()
                .fetch(&http, "fixture-key")
                .await
                .unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("fixture-key"));
            if status == 401 {
                assert_eq!(http.calls.lock().unwrap().len(), 1);
            }
        }
    }

    struct SingleAuthHttp(std::sync::Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl HttpClient for SingleAuthHttp {
        async fn request(
            &self,
            _: &str,
            url: &str,
            headers: &[(&str, String)],
            _: Option<Vec<u8>>,
        ) -> Result<HttpResponse, TransportError> {
            let bearer = headers.iter().any(|(key, _)| *key == "Authorization");
            let api_key = headers.iter().any(|(key, _)| *key == "x-api-key");
            self.0.lock().unwrap().push(format!("{bearer}:{api_key}"));
            match (bearer, api_key, url.contains("after_id")) {
                (true, true, _) => {
                    reply(401, "x-api-key and Authorization cannot both be provided")
                }
                (false, true, _) => reply(403, "use bearer"),
                (true, false, false) => reply(
                    200,
                    r#"{"data":[{"id":"first"}],"has_more":true,"last_id":"first"}"#,
                ),
                (true, false, true) => reply(200, r#"{"data":[{"id":"last"}]}"#),
                _ => panic!("invalid fixture authentication"),
            }
        }
    }

    #[tokio::test]
    async fn explicit_dual_auth_rejection_learns_one_header_for_remaining_pages() {
        let http = SingleAuthHttp(Default::default());
        assert_eq!(
            Query::new("https://relay.invalid/v1")
                .unwrap()
                .fetch(&http, "fixture-key")
                .await
                .unwrap(),
            ["first", "last"]
        );
        assert_eq!(
            *http.0.lock().unwrap(),
            ["true:true", "false:true", "true:false", "true:false"]
        );
    }

    #[test]
    fn dual_auth_retry_requires_the_specific_refusal_and_both_header_names() {
        assert!(both_auth_refused(
            &reply(
                401,
                "鉴权 header，x-api-key 和 Authorization 不可以同时存在"
            )
            .unwrap()
        ));
        for (status, body) in [
            (403, "x-api-key and authorization cannot both be provided"),
            (401, "invalid authorization"),
            (401, "x-api-key and authorization invalid"),
        ] {
            assert!(!both_auth_refused(&reply(status, body).unwrap()));
        }
    }
}
