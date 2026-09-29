//! Claude Code 风格的轻量状态栏。标准输入由宿主提供；额度仍来自 provider。

use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinSet;

use crate::config::home_dir;
use crate::labels::fmt_percent;
use crate::model::{CredentialStatus, QuotaTier, UsageSnapshot, snapshot_ok, snapshot_transient};
use crate::providers::{UsageProvider, query_with_timeout};
use crate::theme;
use crate::timeutil::{compact_duration, parse_time};

const QUERY_TIMEOUT: Duration = Duration::from_secs(4);
const STALE_MAX_AGE: i64 = 600;
const BAR_WIDTH: usize = 8;

#[derive(Debug, Default)]
pub struct StatusInput {
    pub model: Option<String>,
    pub context_percent: Option<f64>,
    pub directory: Option<String>,
    pub branch: Option<String>,
    pub session_started: Option<DateTime<Utc>>,
    pub rate_tiers: Vec<QuotaTier>,
}

impl StatusInput {
    pub fn from_json(value: &Value) -> Self {
        let model = value
            .pointer("/model/display_name")
            .and_then(Value::as_str)
            .map(clean);
        let context = value.get("context_window");
        let size = context
            .and_then(|item| item.get("context_window_size"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let usage = context.and_then(|item| item.get("current_usage"));
        let used: f64 = [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ]
        .iter()
        .map(|key| {
            usage
                .and_then(|item| item.get(*key))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        })
        .sum();
        let context_percent = context
            .and_then(|item| item.get("used_percentage"))
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(0.0, 100.0))
            .or_else(|| (size > 0.0).then_some((used * 100.0 / size).clamp(0.0, 100.0)));
        let cwd = value
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty());
        let directory = cwd
            .and_then(|text| Path::new(text).file_name())
            .map(|name| clean(&name.to_string_lossy()));
        let branch = cwd.and_then(git_branch);
        let session_started = value
            .pointer("/session/start_time")
            .and_then(Value::as_str)
            .and_then(parse_time);

        let mut rate_tiers = Vec::new();
        for (key, name) in [
            ("five_hour", "five_hour"),
            ("seven_day", "seven_day"),
            ("spend_limit", "spend_limit"),
        ] {
            let Some(window) = value.pointer(&format!("/rate_limits/{key}")) else {
                continue;
            };
            let Some(percent) = window.get("used_percentage").and_then(Value::as_f64) else {
                continue;
            };
            if !percent.is_finite() {
                continue;
            }
            rate_tiers.push(QuotaTier {
                name: name.into(),
                utilization: percent.clamp(0.0, 100.0),
                resets_at: window.get("resets_at").and_then(parse_reset),
            });
        }
        Self {
            model,
            context_percent,
            directory,
            branch,
            session_started,
            rate_tiers,
        }
    }
}

/// Reading a tty would hang a manual `usage statusline` invocation.
pub fn read_stdin() -> StatusInput {
    if io::stdin().is_terminal() {
        return StatusInput::default();
    }
    let mut input = String::new();
    if io::stdin()
        .take(1_048_576)
        .read_to_string(&mut input)
        .is_err()
    {
        return StatusInput::default();
    }
    serde_json::from_str::<Value>(&input)
        .ok()
        .map(|value| StatusInput::from_json(&value))
        .unwrap_or_default()
}

fn parse_reset(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(text) = value.as_str() {
        return parse_time(text);
    }
    let epoch = value.as_i64()?;
    if epoch > 1_000_000_000_000 {
        DateTime::from_timestamp_millis(epoch)
    } else {
        DateTime::from_timestamp(epoch, 0)
    }
}

fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(80)
        .collect()
}

fn git_branch(cwd: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", cwd, "symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| clean(String::from_utf8_lossy(&output.stdout).trim()))
}

pub fn cache_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cache"));
    base.join("usage").join("statusline.json")
}

#[derive(Default, Serialize, Deserialize)]
struct CacheFile {
    entries: HashMap<String, CacheEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CacheEntry {
    saved_at: DateTime<Utc>,
    snapshot: UsageSnapshot,
}

fn read_cache(path: &Path) -> CacheFile {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_cache(path: &Path, cache: &CacheFile) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!("statusline-{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(cache).map_err(io::Error::other)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
    }
    #[cfg(not(unix))]
    fs::write(&temporary, &bytes)?;
    fs::rename(temporary, path)
}

fn within_age(saved_at: DateTime<Utc>, now: DateTime<Utc>, max_age: i64) -> bool {
    let age = now.signed_duration_since(saved_at).num_seconds();
    age >= 0 && age < max_age
}

/// Reuse fresh results, ask remaining providers concurrently, and keep a
/// recent successful result when a request times out or the network fails.
pub async fn collect(
    providers: &[Arc<dyn UsageProvider>],
    input: &StatusInput,
    path: &Path,
    cache_seconds: u64,
    now: DateTime<Utc>,
) -> Vec<UsageSnapshot> {
    let mut cache = read_cache(path);
    let mut results = vec![None; providers.len()];
    let mut jobs = JoinSet::new();
    let mut changed = false;

    for (index, provider) in providers.iter().enumerate() {
        let id = provider.id();
        if id == "claude" && !input.rate_tiers.is_empty() {
            let snapshot = snapshot_ok(
                id,
                provider.display_name(),
                input.rate_tiers.clone(),
                None,
                now,
            );
            let should_cache = cache.entries.get(id).is_none_or(|entry| {
                !within_age(
                    entry.saved_at,
                    now,
                    cache_seconds.min(i64::MAX as u64) as i64,
                )
            });
            if should_cache {
                cache.entries.insert(
                    id.into(),
                    CacheEntry {
                        saved_at: now,
                        snapshot: snapshot.clone(),
                    },
                );
                changed = true;
            }
            results[index] = Some(snapshot);
            continue;
        }
        if let Some(entry) = cache.entries.get(id)
            && within_age(
                entry.saved_at,
                now,
                cache_seconds.min(i64::MAX as u64) as i64,
            )
        {
            results[index] = Some(entry.snapshot.clone());
            continue;
        }
        let provider = Arc::clone(provider);
        jobs.spawn(async move {
            let snapshot = query_with_timeout(provider.as_ref(), QUERY_TIMEOUT).await;
            (index, snapshot)
        });
    }

    while let Some(Ok((index, snapshot))) = jobs.join_next().await {
        let id = providers[index].id();
        let usable = if snapshot.transient {
            cache.entries.get(id).and_then(|entry| {
                (entry.snapshot.success && within_age(entry.saved_at, now, STALE_MAX_AGE))
                    .then(|| entry.snapshot.clone())
            })
        } else {
            cache.entries.insert(
                id.into(),
                CacheEntry {
                    saved_at: now,
                    snapshot: snapshot.clone(),
                },
            );
            changed = true;
            None
        };
        results[index] = Some(usable.unwrap_or(snapshot));
    }
    if changed {
        let _ = write_cache(path, &cache);
    }
    results
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            item.unwrap_or_else(|| {
                snapshot_transient(
                    providers[index].id(),
                    providers[index].display_name(),
                    "用量查询中断",
                )
            })
        })
        .collect()
}

pub fn render(
    input: &StatusInput,
    snapshots: &[UsageSnapshot],
    now: DateTime<Utc>,
    color: bool,
) -> String {
    let mut lines = Vec::new();
    let mut header = Vec::new();
    if let Some(model) = &input.model {
        header.push(paint(model, theme::accent("claude"), color));
    }
    if let Some(percent) = input.context_percent {
        header.push(format!(
            "ctx {}",
            paint(
                &fmt_percent(percent),
                theme::meter("claude", percent),
                color
            )
        ));
    }
    if let Some(directory) = &input.directory {
        let mut project = directory.clone();
        if let Some(branch) = &input.branch {
            project.push_str(&format!(" ({branch})"));
        }
        header.push(project);
    }
    if let Some(started) = input.session_started {
        let elapsed = now.signed_duration_since(started).num_seconds();
        if elapsed >= 0 {
            header.push(format!("session {}", compact_duration(elapsed)));
        }
    }
    if !header.is_empty() {
        lines.push(header.join(" │ "));
    }
    for snapshot in snapshots {
        let name = snapshot.display_name.as_str();
        if !snapshot.success {
            let state = match snapshot.credential_status {
                CredentialStatus::NotFound => "未登录",
                CredentialStatus::Expired => "登录过期",
                CredentialStatus::ParseError => "凭据无效",
                CredentialStatus::Valid if snapshot.transient => "暂不可用",
                CredentialStatus::Valid => "查询失败",
            };
            lines.push(format!(
                "{} │ {state}",
                paint(name, theme::accent(&snapshot.tool), color)
            ));
            continue;
        }
        if snapshot.tiers.is_empty() {
            lines.push(format!(
                "{} │ 暂无用量窗口",
                paint(name, theme::accent(&snapshot.tool), color)
            ));
        }
        for tier in &snapshot.tiers {
            let percent = tier.utilization.clamp(0.0, 100.0);
            let filled = ((percent / 100.0) * BAR_WIDTH as f64).round() as usize;
            let filled = filled.min(BAR_WIDTH);
            let bar = format!(
                "{}{}",
                paint(
                    &"● ".repeat(filled),
                    theme::meter(&snapshot.tool, percent),
                    color
                ),
                paint(&"○ ".repeat(BAR_WIDTH - filled), (110, 110, 115), color)
            );
            let mut line = format!(
                "{} {:<4} {} {}",
                paint(name, theme::accent(&snapshot.tool), color),
                short_tier(&tier.name),
                bar,
                paint(
                    &fmt_percent(percent),
                    theme::meter(&snapshot.tool, percent),
                    color
                ),
            );
            if let Some(reset) = tier.resets_at {
                let remaining = reset.signed_duration_since(now).num_seconds();
                let value = if remaining > 0 {
                    compact_duration(remaining)
                } else {
                    "soon".into()
                };
                line.push_str(&format!(" ⟳ {value}"));
            }
            lines.push(line);
        }
        if let Some(extra) = snapshot
            .extra_usage
            .as_ref()
            .filter(|extra| extra.is_enabled)
            && let Some(used) = extra.used_credits
        {
            let currency = if extra
                .currency
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("USD"))
            {
                "$"
            } else {
                ""
            };
            let total = extra
                .monthly_limit
                .map(|limit| format!("/{currency}{limit:.2}"))
                .unwrap_or_default();
            lines.push(format!(
                "{} extra {currency}{used:.2}{total}",
                paint(name, theme::accent(&snapshot.tool), color),
            ));
        }
    }
    lines.join("\n")
}

fn short_tier(name: &str) -> &str {
    match name {
        "five_hour" => "5h",
        "seven_day" | "weekly_limit" => "7d",
        "monthly" | "30_day" => "30d",
        "credits" => "额度",
        "spend_limit" => "spend",
        _ => name,
    }
}

fn paint(value: &str, rgb: theme::Rgb, enabled: bool) -> String {
    if !enabled || value.is_empty() {
        return value.to_string();
    }
    format!("\x1b[38;2;{};{};{}m{value}\x1b[0m", rgb.0, rgb.1, rgb.2)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    const NOW: &str = "2026-09-29T12:00:00Z";

    struct CountingProvider(Arc<AtomicUsize>);

    #[async_trait]
    impl UsageProvider for CountingProvider {
        fn id(&self) -> &'static str {
            "claude"
        }
        fn display_name(&self) -> &'static str {
            "Claude Code"
        }
        async fn query(&self) -> UsageSnapshot {
            self.0.fetch_add(1, Ordering::SeqCst);
            snapshot_ok(
                "claude",
                "Claude Code",
                vec![QuotaTier {
                    name: "five_hour".into(),
                    utilization: 42.0,
                    resets_at: None,
                }],
                None,
                Utc::now(),
            )
        }
    }

    #[test]
    fn parses_host_context_and_rate_limits() {
        let input = StatusInput::from_json(&json!({
            "model": {"display_name": "Sonnet"},
            "context_window": {"context_window_size": 200000, "current_usage": {"input_tokens": 10000, "cache_creation_input_tokens": 5000, "cache_read_input_tokens": 5000}},
            "rate_limits": {"five_hour": {"used_percentage": 57.4, "resets_at": 1790686800}, "seven_day": {"used_percentage": 24}},
            "session": {"start_time": "2026-09-29T11:00:00Z"}
        }));
        assert_eq!(input.model.as_deref(), Some("Sonnet"));
        assert_eq!(input.context_percent, Some(10.0));
        assert_eq!(input.rate_tiers.len(), 2);
        assert_eq!(input.rate_tiers[0].utilization, 57.4);
        assert!(input.rate_tiers[0].resets_at.is_some());
        let now = Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap();
        let snapshot = snapshot_ok("claude", "Claude Code", input.rate_tiers.clone(), None, now);
        let rendered = render(&input, &[snapshot], now, false);
        assert!(rendered.contains("Sonnet │ ctx 10%"), "{rendered}");
        assert!(
            rendered.contains("5h") && rendered.contains("57.4%"),
            "{rendered}"
        );
        assert!(
            rendered.contains("●") && rendered.contains("○"),
            "{rendered}"
        );
        assert!(!rendered.contains('\x1b'));
    }

    #[test]
    fn official_context_percentage_takes_precedence() {
        let input = StatusInput::from_json(&json!({
            "context_window": {"used_percentage": 36, "context_window_size": 200000, "current_usage": {"input_tokens": 10000}},
            "rate_limits": {"spend_limit": {"used_percentage": 62.8, "resets_at": "2026-10-01T00:00:00Z"}}
        }));
        assert_eq!(input.context_percent, Some(36.0));
        assert_eq!(input.rate_tiers[0].name, "spend_limit");
        assert!(input.rate_tiers[0].resets_at.is_some());
    }

    #[tokio::test]
    async fn stdin_rates_skip_query_and_cache_avoids_repeat_query() {
        let now = parse_time(NOW).unwrap();
        let path = std::env::temp_dir().join(format!(
            "usage-statusline-{}-{}.json",
            std::process::id(),
            now.timestamp_nanos_opt().unwrap()
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn UsageProvider> = Arc::new(CountingProvider(Arc::clone(&calls)));
        let providers = vec![provider];
        let input = StatusInput {
            rate_tiers: vec![QuotaTier {
                name: "five_hour".into(),
                utilization: 57.0,
                resets_at: None,
            }],
            ..Default::default()
        };
        let first = collect(&providers, &input, &path, 60, now).await;
        assert_eq!(first[0].tiers[0].utilization, 57.0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let second = collect(&providers, &StatusInput::default(), &path, 60, now).await;
        assert_eq!(second[0].tiers[0].utilization, 57.0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let _ = fs::remove_file(path);
    }
}
