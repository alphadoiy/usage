//! Unified performance queries over explicitly bound CLI logs and gateway observations.
mod loader;
mod parser;
mod store;
#[cfg(test)]
mod tests;

pub use loader::{LoadRequest, LoadTask, LoadUpdate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
pub use store::{Batch, Service};

#[derive(Clone, Debug)]
pub struct Binding {
    pub provider: String,
    pub source: RequestSource,
    pub agent: String,
    pub home: PathBuf,
    pub revision: String,
    pub expected_upstream: Option<String>,
    /// Whether this binding may merge gateway observations with CLI logs.
    pub gateway_observations: bool,
}
impl Binding {
    /// UI cache identity; unlike the database scope, this performs no filesystem work.
    pub fn cache_key(&self) -> String {
        let value = serde_json::to_vec(&(
            &self.provider,
            self.source.id,
            &self.agent,
            &self.home,
            &self.revision,
            &self.expected_upstream,
            self.gateway_observations,
        ))
        .expect("binding serializes");
        format!("{:x}", Sha256::digest(value))
    }
    pub fn scope(&self) -> String {
        let home = self
            .home
            .canonicalize()
            .unwrap_or_else(|_| self.home.clone());
        let mut hash = Sha256::new();
        hash.update(self.source.id.as_bytes());
        hash.update([0]);
        hash.update(self.agent.as_bytes());
        hash.update([0]);
        hash.update(home.as_os_str().as_encoded_bytes());
        format!("{:x}", hash.finalize())
    }
}

/// A source owns its format and checkpoint; factories perform no I/O.
#[derive(Clone, Copy)]
pub struct RequestSource {
    pub id: &'static str,
    pub directories: &'static [&'static str],
    pub parser: fn(serde_json::Value) -> Box<dyn RequestParser>,
}
pub trait RequestParser: Send {
    fn feed(&mut self, line: &[u8]) -> Result<Vec<Observation>, String>;
    fn checkpoint(&self) -> serde_json::Value;
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Observation {
    pub id: String,
    pub session: String,
    pub turn: Option<String>,
    pub at: i64,
    pub model: String,
    pub upstream: Option<String>,
    pub output: u64,
    #[serde(default)]
    pub output_unknown: bool,
    pub reasoning: u64,
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    /// Native CLI timing, rather than inferred timestamps. Still not proxy observation.
    pub native_timing: bool,
    #[serde(default)]
    pub gateway: bool,
    /// Computed by the shared SQL policy when reading indexed rows, not stored in parser state.
    #[serde(skip)]
    pub generated: Option<RateSample>,
    #[serde(skip)]
    pub lower_bound: Option<RateSample>,
}
impl Observation {
    fn valid(&self) -> bool {
        !self.id.is_empty()
            && self.id.len() <= 256
            && self.session.len() <= 256
            && self.model.len() <= 256
            && !self.model.chars().any(char::is_control)
            && self
                .upstream
                .as_ref()
                .is_none_or(|s| s.len() <= 256 && !s.chars().any(char::is_control))
            && self.at > 0
            && self.output <= 1_000_000_000
            && self.reasoning <= self.output
    }
    pub fn speed(&self) -> Option<(u64, i64)> {
        self.generated.map(|r| (r.tokens, r.ms))
    }
    pub fn floor(&self) -> Option<(u64, i64)> {
        self.lower_bound.map(|r| (r.tokens, r.ms))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateSample {
    pub tokens: u64,
    pub ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Period {
    Day,
    Week,
    Month,
}
impl Period {
    pub fn days(self) -> i64 {
        match self {
            Self::Day => 1,
            Self::Week => 7,
            Self::Month => 30,
        }
    }
    pub fn next(self, reverse: bool) -> Self {
        match (self, reverse) {
            (Self::Day, false) | (Self::Month, true) => Self::Week,
            (Self::Week, false) | (Self::Day, true) => Self::Month,
            _ => Self::Day,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct ModelStats {
    pub model: String,
    pub calls: u64,
    pub mean_ttft_ms: Option<f64>,
    pub timed_samples: u64,
    pub speed_tokens: u64,
    pub speed_ms: i64,
    pub speed_samples: u64,
    pub floor_tokens: u64,
    pub floor_ms: i64,
    pub floor_samples: u64,
}
impl ModelStats {
    pub fn speed(&self) -> Option<f64> {
        (self.speed_ms > 0).then(|| self.speed_tokens as f64 * 1000.0 / self.speed_ms as f64)
    }
    pub fn floor(&self) -> Option<f64> {
        (self.floor_ms > 0).then(|| self.floor_tokens as f64 * 1000.0 / self.floor_ms as f64)
    }
}
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub calls: u64,
    pub mean_ttft_ms: Option<f64>,
    pub timed_samples: u64,
    pub speed_tokens: u64,
    pub speed_ms: i64,
    pub speed_samples: u64,
    pub floor_tokens: u64,
    pub floor_ms: i64,
    pub floor_samples: u64,
}
impl Summary {
    pub fn speed(&self) -> Option<f64> {
        (self.speed_ms > 0).then(|| self.speed_tokens as f64 * 1000.0 / self.speed_ms as f64)
    }
    pub fn floor(&self) -> Option<f64> {
        (self.floor_ms > 0).then(|| self.floor_tokens as f64 * 1000.0 / self.floor_ms as f64)
    }
}
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub summary: Summary,
    pub models: Vec<ModelStats>,
    pub models_truncated: bool,
    pub unmatched: u64,
    pub recent: Vec<Observation>,
    pub read_bytes: u64,
    pub files_checked: u64,
    pub skipped_lines: u64,
    pub partial: bool,
    pub native_transport: bool,
}

pub(crate) fn local_binding(id: &str, agent: &str, home: &Path, source: RequestSource) -> Binding {
    Binding {
        provider: id.into(),
        source,
        agent: agent.into(),
        home: home.into(),
        revision: String::new(),
        expected_upstream: None,
        gateway_observations: true,
    }
}
pub(crate) fn cancelled(flag: &AtomicBool) -> bool {
    flag.load(std::sync::atomic::Ordering::Relaxed)
}
pub(crate) fn source(agent: &str) -> Option<RequestSource> {
    match agent {
        "codex" => Some(RequestSource {
            id: "codex_log_v1",
            directories: &["sessions", "archived_sessions"],
            parser: parser::codex,
        }),
        "claude" => Some(RequestSource {
            id: "claude_log_v1",
            directories: &["projects"],
            parser: parser::claude,
        }),
        _ => None,
    }
}

impl std::fmt::Debug for RequestSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestSource")
            .field("id", &self.id)
            .finish()
    }
}
