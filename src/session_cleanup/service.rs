//! Application-level cleanup planning; the UI supplies scope, not storage policy.
use super::{opens, policy};
use crate::{
    background::Cancellation,
    sessions::{Session, SessionKey},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

pub(crate) fn active_sessions(
    root: &Path,
    sessions: &[Session],
) -> Result<HashMap<SessionKey, ()>, String> {
    let Some(manager) = crate::codex_manager::Manager::open_read_only(root.to_path_buf())
        .map_err(|e| e.to_string())?
    else {
        return Ok(HashMap::new());
    };
    Ok(manager
        .session_activity(sessions)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|(_, runtime)| {
            runtime.runs.iter().any(|run| {
                matches!(
                    run.process,
                    crate::runs::RunState::Running | crate::runs::RunState::Unknown
                )
            })
        })
        .map(|(key, _)| (key, ()))
        .collect())
}

pub(crate) fn plan(
    sessions: &[Session],
    opens: &opens::Store,
    projects: &crate::projects::Store,
    run_root: &Path,
    cancel: &Cancellation,
) -> Result<policy::Plan, String> {
    if cancel.is_cancelled() {
        return Err("分析已取消".into());
    }
    let opens = opens.list()?;
    let pinned = projects
        .list()?
        .into_iter()
        .filter(|r| r.pinned)
        .map(|r| r.path.canonicalize().unwrap_or(r.path))
        .collect::<HashSet<_>>();
    let active = active_sessions(run_root, sessions)?;
    let plan = policy::plan_controlled(
        sessions,
        &pinned,
        &active,
        &opens,
        policy::Policy::default(),
        cancel,
    );
    if cancel.is_cancelled() {
        return Err("分析已取消".into());
    }
    Ok(plan)
}
