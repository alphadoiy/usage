//! Translate the account store's historical schemas into confirmed session/run evidence.
use crate::runs::{
    SessionClaim, SessionEvidenceIdentity, SessionEvidenceQuery, SessionEvidenceReader,
    SessionEvidenceSnapshot, SessionEvidenceSource,
};
use rusqlite::{Connection, OpenFlags, params_from_iter};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_CLAIMS: usize = 16384;
const MAX_EVENTS: usize = 65536;

#[cfg(test)]
mod tests;

#[derive(Debug)]
struct Reader {
    path: PathBuf,
}

pub(super) fn source(root: &Path) -> SessionEvidenceSource {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    SessionEvidenceSource::new(
        SessionEvidenceIdentity {
            backend: "codex_manager.runs.v1".into(),
            store: format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes())),
        },
        Arc::new(Reader {
            path: root.join("manager.sqlite3"),
        }),
    )
}

impl SessionEvidenceReader for Reader {
    fn read(&self, query: SessionEvidenceQuery<'_>) -> Result<SessionEvidenceSnapshot, String> {
        read(&self.path, query)
    }
}

fn timestamp(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn read(path: &Path, range: SessionEvidenceQuery<'_>) -> Result<SessionEvidenceSnapshot, String> {
    let SessionEvidenceQuery {
        sessions,
        since_ms: since,
        until_ms: now,
    } = range;
    let mut evidence = SessionEvidenceSnapshot::default();
    if sessions.is_empty() || !path.try_exists().map_err(|_| "无法检查运行归属库")? {
        return Ok(evidence);
    }
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| "无法读取运行归属库")?;
    db.busy_timeout(Duration::from_millis(250))
        .map_err(|_| "运行归属库暂不可用")?;
    let began = Instant::now();
    db.progress_handler(
        10_000,
        Some(move || began.elapsed() > Duration::from_millis(250)),
    );
    let tx = db.transaction().map_err(|_| "无法读取运行归属快照")?;
    let has = |table: &str| -> Result<bool, String> {
        tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(|_| "无法读取运行归属结构".into())
    };
    if !has("run_sessions")? || !has("runs")? {
        return Ok(evidence);
    }
    let events = has("run_events")?;
    // UTC-day bounds use the indexes while covering both old second precision and current
    // fractional timestamps. Exact millisecond lifetime bounds are applied to each claim.
    let lower = chrono::DateTime::from_timestamp_millis(since)
        .ok_or("运行归属时间范围无效")?
        .format("%Y-%m-%d")
        .to_string();
    let upper = chrono::DateTime::from_timestamp_millis(now)
        .ok_or("运行归属时间范围无效")?
        .date_naive()
        .succ_opt()
        .ok_or("运行归属时间范围无效")?
        .to_string();
    let mut claim_count = 0;
    let mut event_count = 0;
    for chunk in sessions.chunks(64) {
        let marks = vec!["?"; chunk.len()].join(",");
        let values = || {
            chunk
                .iter()
                .map(String::as_str)
                .chain([lower.as_str(), upper.as_str()])
        };
        let sql = format!("SELECT s.session_id,r.id,r.profile_id,r.revision,r.started_at,r.finished_at,s.first_seen,s.last_seen
            FROM run_sessions s JOIN runs r ON r.id=s.run_id
            WHERE s.session_id IN ({marks}) AND s.last_seen>=? AND s.first_seen<? LIMIT {}", MAX_CLAIMS + 1);
        let mut query = tx.prepare(&sql).map_err(|_| "无法查询会话运行归属")?;
        let mut rows = query
            .query(params_from_iter(values()))
            .map_err(|_| "无法查询会话运行归属")?;
        while let Some(row) = rows.next().map_err(|_| "运行归属查询超时或失败")? {
            claim_count += 1;
            if claim_count > MAX_CLAIMS {
                return Err("运行归属过多，请缩短时间范围".into());
            }
            let session: String = row.get(0).map_err(|_| "会话归属数据无效")?;
            let started: String = row.get(4).map_err(|_| "运行时间无效")?;
            let finished: Option<String> = row.get(5).map_err(|_| "运行时间无效")?;
            let first: String = row.get(6).map_err(|_| "会话时间无效")?;
            let last: String = row.get(7).map_err(|_| "会话时间无效")?;
            let (Some(start), Some(first), Some(last)) =
                (timestamp(&started), timestamp(&first), timestamp(&last))
            else {
                continue;
            };
            let end = match finished.as_deref() {
                Some(value) => match timestamp(value) {
                    Some(at) => at.min(last),
                    None => continue,
                },
                None => last,
            }
            .min(now);
            let start = start.max(first);
            if end < since || start > end {
                continue;
            }
            evidence
                .sessions
                .entry(session)
                .or_default()
                .push(SessionClaim {
                    run: row.get(1).map_err(|_| "运行归属数据无效")?,
                    account: row.get(2).map_err(|_| "账号归属数据无效")?,
                    revision: row.get(3).map_err(|_| "配置版本归属无效")?,
                    start,
                    end,
                    turns: HashSet::new(),
                });
        }
        if !events {
            continue;
        }
        let sql = format!("SELECT session_id,run_id,turn_id FROM run_events
            WHERE session_id IN ({marks}) AND observed_at>=? AND observed_at<? AND turn_id IS NOT NULL LIMIT {}", MAX_EVENTS + 1);
        let mut query = tx.prepare(&sql).map_err(|_| "无法查询轮次归属")?;
        let mut rows = query
            .query(params_from_iter(values()))
            .map_err(|_| "无法查询轮次归属")?;
        while let Some(row) = rows.next().map_err(|_| "轮次归属查询超时或失败")? {
            event_count += 1;
            if event_count > MAX_EVENTS {
                return Err("轮次归属过多，请缩短时间范围".into());
            }
            let session: String = row.get(0).map_err(|_| "轮次归属数据无效")?;
            let run: String = row.get(1).map_err(|_| "轮次归属数据无效")?;
            let turn: String = row.get(2).map_err(|_| "轮次归属数据无效")?;
            if !turn.is_empty()
                && let Some(claims) = evidence.sessions.get_mut(&session)
            {
                for claim in claims.iter_mut().filter(|claim| claim.run == run) {
                    claim.turns.insert(turn.clone());
                }
            }
        }
    }
    Ok(evidence)
}
