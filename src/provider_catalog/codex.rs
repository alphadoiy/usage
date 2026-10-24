//! Account-specific ChatGPT model discovery, following Magpie's Codex catalog protocol.
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{HttpClient, ModelQuery, Protocol, model_response};

const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
// Protocol negotiation floor from Magpie; model IDs are always returned by the account.
const MIN_VERSION: Version = Version([0, 159, 0]);
type VersionCache = Option<(Instant, Option<Version>)>;
static CLI_VERSION: tokio::sync::Mutex<VersionCache> = tokio::sync::Mutex::const_new(None);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Version([u64; 3]);
impl Version {
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('-').next()?.trim_start_matches('v').split('.');
        let result = Self([
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        ]);
        parts.next().is_none().then_some(result)
    }
}
impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0[0], self.0[1], self.0[2])
    }
}

pub(super) struct AccountModels {
    pub account_id: String,
    pub cached_version: Version,
}

impl ModelQuery {
    pub(crate) fn codex(
        id: String,
        home: &Path,
        key: String,
        account_id: String,
    ) -> Result<Self, &'static str> {
        if key.is_empty()
            || key.chars().any(char::is_control)
            || account_id.is_empty()
            || account_id.chars().any(char::is_control)
        {
            return Err("OAuth 模型查询凭据无效");
        }
        Ok(Self {
            id,
            url: MODELS_URL.parse().expect("fixed HTTPS model endpoint"),
            key,
            protocol: Protocol::Codex(AccountModels {
                account_id,
                cached_version: cached_version(home).unwrap_or(MIN_VERSION).max(MIN_VERSION),
            }),
        })
    }
}

fn cached_version(home: &Path) -> Option<Version> {
    #[derive(Deserialize)]
    struct Cache {
        client_version: String,
    }
    let mut bytes = Vec::new();
    std::fs::File::open(home.join("models_cache.json"))
        .ok()?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 4 * 1024 * 1024 {
        return None;
    }
    let cache: Cache = serde_json::from_slice(&bytes).ok()?;
    Version::parse(&cache.client_version)
}

async fn installed_version() -> Option<Version> {
    let mut cache = CLI_VERSION.lock().await;
    if let Some((at, version)) = *cache
        && at.elapsed() < Duration::from_secs(600)
    {
        return version;
    }
    // Off the UI thread; both successful and failed probes are cached, with bounded output/time.
    let version = crate::background::blocking(|| {
        let mut command = std::process::Command::new("codex");
        crate::codex_manager::clear_auth_env(&mut command);
        command.arg("--version").stdin(std::process::Stdio::null());
        let output = crate::credentials::output_with_timeout(command, Duration::from_secs(2))?;
        if !output.status.success() {
            return None;
        }
        std::str::from_utf8(&output.stdout)
            .ok()?
            .split_whitespace()
            .find_map(Version::parse)
    })
    .await
    .ok()
    .flatten();
    *cache = Some((Instant::now(), version));
    version
}

impl AccountModels {
    pub async fn fetch(
        &self,
        http: &dyn HttpClient,
        key: &str,
    ) -> Result<Vec<String>, &'static str> {
        let version = installed_version()
            .await
            .unwrap_or(MIN_VERSION)
            .max(self.cached_version)
            .to_string();
        let mut url: reqwest::Url = MODELS_URL.parse().expect("fixed HTTPS model endpoint");
        url.query_pairs_mut()
            .append_pair("client_version", &version);
        let headers = vec![
            ("Authorization", format!("Bearer {key}")),
            ("ChatGPT-Account-Id", self.account_id.clone()),
            ("originator", "codex_cli_rs".into()),
            ("OpenAI-Beta", "responses=experimental".into()),
            ("version", version.clone()),
            (
                "User-Agent",
                format!(
                    "codex_cli_rs/{version} ({}; {}) usage/{}",
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    env!("CARGO_PKG_VERSION")
                ),
            ),
        ];
        let bytes = model_response(http, &url, &headers, 4 * 1024 * 1024).await?;
        parse_models(&bytes)
    }
}

fn parse_models(bytes: &[u8]) -> Result<Vec<String>, &'static str> {
    #[derive(Deserialize)]
    struct Model {
        slug: String,
        #[serde(default)]
        visibility: String,
        #[serde(default)]
        priority: i64,
    }
    #[derive(Deserialize)]
    struct Response {
        models: Vec<Model>,
    }
    let mut response: Response =
        serde_json::from_slice(bytes).map_err(|_| "模型列表响应格式无效")?;
    if response.models.len() > 5000 {
        return Err("模型列表超过展示上限");
    }
    response.models.sort_by_key(|model| model.priority);
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for model in response.models {
        if model.visibility == "hide" {
            continue;
        }
        if model.slug.is_empty()
            || model.slug.len() > 256
            || model.slug.chars().any(char::is_control)
        {
            return Err("模型列表包含无效的模型名称");
        }
        if seen.insert(model.slug.clone()) {
            result.push(model.slug);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_models_keep_priority_hide_internal_models_and_deduplicate() {
        assert_eq!(
            parse_models(
                br#"{"models":[
            {"slug":"second","priority":5}, {"slug":"internal","visibility":"hide","priority":0},
            {"slug":"first","priority":1}, {"slug":"second","priority":9}
        ]}"#
            )
            .unwrap(),
            vec!["first", "second"]
        );
        for body in [
            r#"{"data":[{"id":"api-model"}]}"#,
            r#"{"models":[{"slug":"bad\nmodel"}]}"#,
        ] {
            assert!(parse_models(body.as_bytes()).is_err());
        }
        assert!(parse_models(br#"{"models":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn version_comparison_and_account_cache_keys_are_isolated() {
        assert!(Version::parse("0.160.0").unwrap() > Version::parse("0.99.9").unwrap());
        assert!(Version::parse("0.159.3-alpha.1").is_some());
        assert!(Version::parse("0.159.3/extra").is_none());
        let home = Path::new("/nonexistent/usage-codex-models");
        let a =
            ModelQuery::codex("a".into(), home, "same-token".into(), "workspace-a".into()).unwrap();
        let b =
            ModelQuery::codex("b".into(), home, "same-token".into(), "workspace-b".into()).unwrap();
        assert_ne!(a.cache_key(), b.cache_key());
    }
}
