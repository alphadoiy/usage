//! Public npm metadata only; never installs code or touches subscription credentials.
use super::*;
use crate::http::HttpClient;
use semver::Version;
use std::{io::Read, time::Duration};

const REGISTRY: &str = "https://registry.npmjs.org/lmfpd/latest";
const SUCCESS_TTL: i64 = 6 * 3600;
const FAILURE_TTL: i64 = 3600;
const MAX_BYTES: usize = 64 * 1024;

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    attempted_at: i64,
    succeeded_at: Option<i64>,
    latest: Option<String>,
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_BYTES).then_some(())?;
    serde_json::from_slice(&bytes).ok()
}
fn cache_path(root: &Path) -> PathBuf {
    root.join("fingerprint/update.json")
}
pub(super) fn default_fpd(root: &Path) -> PathBuf {
    root.join("fingerprint/tools/node_modules/.bin/fpd")
}
pub(super) fn installed(fpd: &Path) -> Option<Version> {
    // npm's .bin entry is a symlink to lmfpd/bin/fpd.js. Do not execute
    // arbitrary wrappers just to discover a version, or guess their package.
    let canonical = fpd.canonicalize().ok()?;
    let package: Value = read_json(&canonical.parent()?.parent()?.join("package.json"))?;
    if package["name"] != "lmfpd" {
        return None;
    }
    Version::parse(package["version"].as_str()?).ok()
}
pub(super) fn notice(root: &Path, fpd: Option<&Path>) -> Option<String> {
    let current = installed(fpd.unwrap_or(&default_fpd(root)))?;
    let cache: Cache = read_json(&cache_path(root))?;
    let latest = Version::parse(cache.latest.as_deref()?).ok()?;
    // SemVer build metadata is not a precedence change.
    latest.cmp_precedence(&current).is_gt().then(|| {
        format!(
            "lmfpd 有更新：{current} → {latest}（npm 上次成功检查：{}）",
            cache
                .succeeded_at
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| t
                    .with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M")
                    .to_string())
                .unwrap_or_else(|| "未知".into())
        )
    })
}
pub(super) async fn check(root: &Path, http: &dyn HttpClient, now: i64) {
    let previous: Option<Cache> = read_json(&cache_path(root));
    let mut cache = previous.unwrap_or_default();
    let ttl = if cache.succeeded_at == Some(cache.attempted_at) {
        SUCCESS_TTL
    } else {
        FAILURE_TTL
    };
    if cache.attempted_at > 0 && now >= cache.attempted_at && now - cache.attempted_at < ttl {
        return;
    }
    let response = tokio::time::timeout(
        Duration::from_secs(8),
        http.request("GET", REGISTRY, &[], None),
    )
    .await;
    let latest = match response {
        Ok(Ok(response)) if response.status == 200 && response.body.len() <= MAX_BYTES => {
            serde_json::from_slice::<Value>(&response.body)
                .ok()
                .and_then(|value| {
                    if value["name"] != "lmfpd" {
                        return None;
                    }
                    Version::parse(value["version"].as_str()?).ok()
                })
        }
        _ => None,
    };
    cache.attempted_at = now;
    if let Some(version) = latest {
        tracing::debug!(event="fingerprint_update_checked", latest=%version);
        cache.latest = Some(version.to_string());
        cache.succeeded_at = Some(now);
    } else {
        tracing::debug!(
            event = "fingerprint_update_check_failed",
            reason = "registry_unavailable_or_invalid"
        );
    }
    // A failed check retains the last successful result and backs off for an hour.
    if store::save(&cache_path(root), &cache).is_err() {
        tracing::warn!(event = "fingerprint_update_cache_failed");
    }
}

#[cfg(test)]
mod tests;
