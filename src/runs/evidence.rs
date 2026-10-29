//! Read-only evidence of which runs used a session. Storage adapters own discovery and
//! schema compatibility; consumers decide what those confirmed facts mean for their query.
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub const MAX_EVIDENCE_SESSIONS: usize = 4096;

/// A backend namespace and its durable store identity, independent of reader allocation.
/// Changing the store or the meaning of its evidence must change this identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionEvidenceIdentity {
    pub backend: String,
    pub store: String,
}

#[derive(Clone, Copy, Debug)]
pub struct SessionEvidenceQuery<'a> {
    pub sessions: &'a [String],
    pub since_ms: i64,
    pub until_ms: i64,
}

/// The confirmed intersection of a run lifetime and its observed session activity.
/// Turn IDs narrow overlapping claims; an empty set means turn evidence is unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionClaim {
    pub run: String,
    pub account: String,
    pub revision: i64,
    pub start: i64,
    pub end: i64,
    pub turns: HashSet<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SessionEvidenceSnapshot {
    pub sessions: HashMap<String, Vec<SessionClaim>>,
}

pub trait SessionEvidenceReader: Send + Sync + std::fmt::Debug {
    /// Return a consistent, bounded snapshot, or an error. Never silently truncate competing
    /// claims: a partial snapshot could turn ambiguous ownership into a false positive.
    /// Missing historical evidence is an empty snapshot, not inferred from a Home or provider.
    fn read(&self, query: SessionEvidenceQuery<'_>) -> Result<SessionEvidenceSnapshot, String>;
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionEvidenceSource {
    identity: SessionEvidenceIdentity,
    #[serde(skip)]
    reader: Arc<dyn SessionEvidenceReader>,
}

impl SessionEvidenceSource {
    pub fn new(identity: SessionEvidenceIdentity, reader: Arc<dyn SessionEvidenceReader>) -> Self {
        Self { identity, reader }
    }

    pub fn identity(&self) -> &SessionEvidenceIdentity {
        &self.identity
    }

    pub fn read(&self, query: SessionEvidenceQuery<'_>) -> Result<SessionEvidenceSnapshot, String> {
        if query.sessions.len() > MAX_EVIDENCE_SESSIONS {
            return Err("运行归属查询范围过大，请缩短时间范围".into());
        }
        if query.since_ms > query.until_ms {
            return Err("运行归属时间范围无效".into());
        }
        self.reader.read(query)
    }
}
