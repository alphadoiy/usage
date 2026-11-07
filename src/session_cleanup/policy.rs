//! Explainable candidates for one-click cleanup. Planning is read-only; deletion stays in Store.
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::evidence::{self, Evidence};
use crate::background::Cancellation;

use crate::sessions::{Session, SessionKey};

pub const DEFAULT_STALE_DAYS: u32 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Empty,
    Stale,
}

impl Reason {
    pub fn label(self, stale_days: u32) -> String {
        match self {
            Self::Empty => "空会话".into(),
            Self::Stale => format!("{stale_days} 天无打开或活动"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub session: Session,
    pub reason: Reason,
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub candidates: Vec<Candidate>,
    pub stale_days: u32,
    pub skipped_pinned: usize,
    pub skipped_active: usize,
    pub errors: Vec<String>,
}

impl Plan {
    pub fn count(&self, reason: Reason) -> usize {
        self.candidates
            .iter()
            .filter(|candidate| candidate.reason == reason)
            .count()
    }
}

#[derive(Clone, Copy)]
pub struct Policy {
    pub now_ms: i64,
    pub stale_days: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            now_ms: chrono::Utc::now().timestamp_millis(),
            stale_days: DEFAULT_STALE_DAYS,
        }
    }
}

/// Staleness uses the later of Usage's launch time and the Agent's activity. Historical
/// sessions predate the open store, so Agent activity is used as the fallback.
pub fn plan(
    sessions: &[Session],
    pinned: &HashSet<PathBuf>,
    active: &HashMap<SessionKey, ()>,
    opens: &HashMap<SessionKey, i64>,
    policy: Policy,
) -> Plan {
    plan_controlled(
        sessions,
        pinned,
        active,
        opens,
        policy,
        &Cancellation::new(),
    )
}

pub fn plan_controlled(
    sessions: &[Session],
    pinned: &HashSet<PathBuf>,
    active: &HashMap<SessionKey, ()>,
    opens: &HashMap<SessionKey, i64>,
    policy: Policy,
    cancel: &Cancellation,
) -> Plan {
    let mut result = Plan {
        stale_days: policy.stale_days,
        ..Plan::default()
    };
    let stale_ms = i64::from(policy.stale_days) * 24 * 60 * 60 * 1000;
    let stale_before = policy.now_ms.saturating_sub(stale_ms);
    let mut eligible = Vec::new();
    for session in sessions {
        let cwd = session
            .cwd
            .canonicalize()
            .unwrap_or_else(|_| session.cwd.clone());
        if pinned.contains(&cwd) || pinned.contains(&session.cwd) {
            result.skipped_pinned += 1;
            continue;
        }
        if active.contains_key(&session.key()) {
            result.skipped_active += 1;
            continue;
        }
        eligible.push(session.clone());
    }
    let evidence = evidence::collect(&eligible, cancel);
    for session in eligible {
        if cancel.is_cancelled() {
            break;
        }
        let last_activity = opens
            .get(&session.key())
            .copied()
            .unwrap_or(i64::MIN)
            .max(session.recency_at_ms);
        let empty = match evidence.get(&session.key()) {
            Some(Evidence::Empty) => true,
            Some(Evidence::NonEmpty) => false,
            Some(Evidence::Unknown(error)) => {
                result
                    .errors
                    .push(format!("{} · {}：{error}", session.title, session.instance));
                continue;
            }
            None => {
                result
                    .errors
                    .push(format!("{}：未找到完整会话证据", session.title));
                continue;
            }
        };
        let stale = last_activity < stale_before;
        if empty {
            result.candidates.push(Candidate {
                session: session.clone(),
                reason: Reason::Empty,
            });
        } else if stale {
            result.candidates.push(Candidate {
                session: session.clone(),
                reason: Reason::Stale,
            });
        }
    }
    result
        .candidates
        .sort_by(|a, b| a.session.id.cmp(&b.session.id));
    result
}
