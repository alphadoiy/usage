//! A shared directory proves storage, not which account made a request. Only confirmed
//! run/session lifetimes (and turn IDs when available) can assign its CLI observations.
use super::Binding;
use crate::runs::{
    MAX_EVIDENCE_SESSIONS, SessionClaim, SessionEvidenceQuery, SessionEvidenceSnapshot,
};
use rusqlite::{Connection, functions::FunctionFlags, params};

/// 1 = this account/version, -1 = another confirmed owner, 0 = unconfirmed/ambiguous.
fn classify(
    evidence: &SessionEvidenceSnapshot,
    session: &str,
    turn: Option<&str>,
    at: i64,
    binding: &super::ManagedBinding,
) -> i32 {
    let Some(claims) = evidence.sessions.get(session) else {
        return 0;
    };
    let in_time = |claim: &&SessionClaim| claim.start <= at && at <= claim.end;
    let exact = turn.is_some_and(|turn| {
        claims
            .iter()
            .filter(in_time)
            .any(|claim| claim.turns.contains(turn))
    });
    let mut candidates = claims.iter().filter(in_time).filter(|claim| {
        !exact || claim.turns.is_empty() || turn.is_some_and(|turn| claim.turns.contains(turn))
    });
    let Some(owner) = candidates.next() else {
        return 0;
    };
    if candidates.any(|other| other.run != owner.run) {
        return 0;
    }
    if owner.account == binding.account && owner.revision == binding.revision {
        1
    } else {
        -1
    }
}

/// Register a bounded, immutable ownership snapshot on this query connection. Discovery reads
/// indexed session IDs and asks the injected run store for evidence; it never scans rollouts.
pub(super) fn install(
    db: &Connection,
    binding: &Binding,
    since: i64,
    now: i64,
) -> Result<(), String> {
    let Some(managed) = binding.shared_account() else {
        return Ok(());
    };
    let sessions: Vec<String> = db
        .prepare(
            "SELECT DISTINCT session FROM perf_calls WHERE scope=?1 AND at>=?2 AND at<=?3 LIMIT ?4",
        )
        .and_then(|mut query| {
            query
                .query_map(
                    params![binding.log_scope(), since, now, MAX_EVIDENCE_SESSIONS + 1],
                    |row| row.get(0),
                )?
                .collect()
        })
        .map_err(|_| "无法读取性能会话范围")?;
    let evidence = managed.evidence.read(SessionEvidenceQuery {
        sessions: &sessions,
        since_ms: since,
        until_ms: now,
    })?;
    let managed = managed.clone();
    db.create_scalar_function(
        "usage_cli_owner",
        3,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |ctx| {
            let session: String = ctx.get(0)?;
            let turn: Option<String> = ctx.get(1)?;
            Ok(classify(
                &evidence,
                &session,
                turn.as_deref(),
                ctx.get(2)?,
                &managed,
            ))
        },
    )
    .map_err(|_| "无法准备性能归属查询".into())
}
