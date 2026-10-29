//! Per-launch lifecycle events. Hooks remain subject to Codex's normal trust policy.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AuthMode, Error, Manager, Result, RunRecord, now};
use crate::runs::{RunEvent, RunSession};

const CONTEXT_ENV: &str = "USAGE_CODEX_RUN_CONTEXT";
const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "Interrupt",
    "SessionEnd",
];
const MAX_INPUT: u64 = 1024 * 1024;

#[cfg(all(test, unix))]
mod tests;

#[derive(Serialize, Deserialize)]
struct Context {
    root: PathBuf,
    home: PathBuf,
    run_id: String,
}

pub(super) fn inject(
    command: &mut Command,
    root: &Path,
    home: &Path,
    run_id: &str,
    saved: &str,
    supported: bool,
) -> Result<()> {
    command.env_remove(CONTEXT_ENV);
    if !supported {
        tracing::info!(
            event = "run_events_unavailable",
            reason = "cli_capability_missing"
        );
        return Ok(());
    }
    let config: toml::Value =
        toml::from_str(saved).map_err(|_| Error::message("无法读取运行事件配置"))?;
    if config
        .get("features")
        .and_then(|v| v.get("hooks"))
        .and_then(toml::Value::as_bool)
        == Some(false)
    {
        tracing::info!(event = "run_events_unavailable", reason = "hooks_disabled");
        return Ok(());
    }
    let context = Context {
        root: root.into(),
        home: home.into(),
        run_id: run_id.into(),
    };
    command.env(
        CONTEXT_ENV,
        serde_json::to_string(&context).map_err(|_| Error::message("无法创建运行事件上下文"))?,
    );
    let callback = format!(
        "{} codex event",
        super::shell_quote(&std::env::current_exe()?.to_string_lossy())
    );
    // Codex loads hooks from every config layer and appends them (codex-rs hooks discovery), so
    // the account's own hooks stay active from its config file. Repeating them in this session
    // layer would register them twice; only Usage's callback is added here.
    let groups = toml::Value::Array(vec![super::launcher::command_hook(&callback, 3, false)?]);
    for event in EVENTS {
        command.args(["-c", &format!("hooks.{event}={groups}")]);
    }
    tracing::info!(event = "run_events_injected", requires_hook_trust = true);
    Ok(())
}

/// Advisory callback: no decisions, prompts, transcripts or tool arguments are persisted.
pub fn callback() {
    let result = (|| -> Result<()> {
        let Some(context) = std::env::var(CONTEXT_ENV).ok() else {
            return Ok(());
        };
        let context: Context =
            serde_json::from_str(&context).map_err(|_| Error::message("运行事件上下文无效"))?;
        let mut payload = String::new();
        std::io::stdin()
            .take(MAX_INPUT + 1)
            .read_to_string(&mut payload)?;
        if payload.len() as u64 > MAX_INPUT {
            return Err(Error::message("运行事件过大"));
        }
        let data: Value =
            serde_json::from_str(&payload).map_err(|_| Error::message("运行事件格式无效"))?;
        receive(&context, &data)
    })();
    if result.is_err() {
        tracing::warn!(event = "run_event_rejected");
    }
    println!("{{}}");
}

fn text(value: Option<&Value>) -> Result<Option<&str>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value))
            if !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control) =>
        {
            Ok(Some(value))
        }
        _ => Err(Error::message("运行事件标识无效")),
    }
}

fn receive(context: &Context, data: &Value) -> Result<()> {
    crate::logging::operation("codex.run_event", |op| {
        op.link_run(&context.run_id);
        let result = receive_inner(context, data);
        if let Err(error) = &result {
            tracing::warn!(event="run_event_rejected", reason=%error);
        }
        result
    })
}

fn receive_inner(context: &Context, data: &Value) -> Result<()> {
    uuid::Uuid::parse_str(&context.run_id).map_err(|_| Error::message("运行 ID 无效"))?;
    let event = data["hook_event_name"]
        .as_str()
        .filter(|event| EVENTS.contains(event))
        .ok_or_else(|| Error::message("不支持的运行事件"))?;
    let session =
        text(data.get("session_id"))?.ok_or_else(|| Error::message("运行事件缺少会话"))?;
    uuid::Uuid::parse_str(session).map_err(|_| Error::message("会话 ID 无效"))?;
    if data.get("agent_id").is_some_and(|v| !v.is_null())
        || data.get("agent_type").is_some_and(|v| !v.is_null())
    {
        return Err(Error::message("忽略子代理事件"));
    }
    let turn = text(data.get("turn_id"))?;
    let model = text(data.get("model"))?;
    tracing::info!(
        event = "run_event_received",
        kind = event,
        session_id = session,
        turn_id = turn,
        model
    );
    let cwd = data["cwd"]
        .as_str()
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .and_then(|path| path.canonicalize().ok())
        .filter(|path| path.is_dir())
        .ok_or_else(|| Error::message("事件项目目录无效"))?;
    let Some(manager) = Manager::open_read_only(context.root.clone())? else {
        return Err(Error::message("运行库不存在"));
    };
    // A fast child can invoke SessionStart before its parent commits the run. Wait up to 2 s,
    // inside the 3 s hook timeout, rather than drop the event that binds the session.
    let mut run = None;
    for attempt in 0..100 {
        run = manager.run_record(&context.run_id)?;
        if run.as_ref().is_some_and(|run| run.pid.is_some()) {
            break;
        }
        if attempt < 99 {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let run = run.ok_or_else(|| Error::message("运行不存在"))?;
    let home = manager
        .home(&run.profile_id, run.revision)?
        .canonicalize()?;
    if home != context.home.canonicalize()?
        || crate::runs::RunState::observe(&run) != crate::runs::RunState::Running
    {
        return Err(Error::message("运行事件来源不匹配或运行已结束"));
    }
    if super::bark::source::classify(Some(&home), session) != super::bark::source::Origin::User {
        return Err(Error::message("未核验为用户会话"));
    }
    let mut conn = Connection::open_with_flags(
        context.root.join("manager.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    conn.busy_timeout(Duration::from_secs(1))?;
    conn.execute_batch("PRAGMA foreign_keys=ON")?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let live: bool = tx.query_row(
        "SELECT finished_at IS NULL FROM runs WHERE id=?1",
        [&run.id],
        |r| r.get(0),
    )?;
    if !live {
        return Err(Error::message("运行已结束"));
    }
    let time = now();
    let activity = match event {
        "UserPromptSubmit" => "working",
        "Interrupt" => "interrupted",
        "SessionEnd" => "ended",
        _ => "idle",
    };
    // Compaction's SessionStart does not make an active turn idle. A late Stop
    // with a different turn ID likewise must not overwrite a newer active turn.
    let preserve = (event == "SessionStart" && data["source"] == "compact") as i32;
    let terminal_turn = matches!(event, "Stop" | "Interrupt");
    let updated = tx.execute("INSERT INTO run_sessions(run_id,session_id,cwd,activity,turn_id,model,first_seen,last_seen)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?7)
        ON CONFLICT(run_id,session_id) DO UPDATE SET
            cwd=excluded.cwd,
            activity=CASE WHEN ?8 THEN run_sessions.activity ELSE excluded.activity END,
            turn_id=CASE WHEN ?8 THEN run_sessions.turn_id ELSE excluded.turn_id END,
            model=COALESCE(excluded.model,run_sessions.model),last_seen=excluded.last_seen
        WHERE NOT ?9 OR run_sessions.turn_id IS NULL OR run_sessions.turn_id=excluded.turn_id",
        params![run.id, session, cwd.to_string_lossy(), activity, turn, model, time, preserve, terminal_turn])?;
    tx.execute("INSERT INTO run_events(run_id,kind,session_id,turn_id,model,observed_at) VALUES(?1,?2,?3,?4,?5,?6)",
        params![run.id,event,session,turn,model,time])?;
    tx.commit()?;
    if event == "SessionStart"
        && data["source"] != "compact"
        && crate::performance::Service::from_environment()
            .link_gateway_session(&run.id, session)
            .is_err()
    {
        tracing::warn!(
            event = "gateway_session_link_failed",
            run_id = run.id,
            session_id = session
        );
    }
    tracing::info!(
        event = "run_event_recorded",
        kind = event,
        session_id = session,
        turn_id = turn,
        model,
        activity_updated = updated > 0
    );
    Ok(())
}

pub(super) fn record_started(conn: &Connection, run: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO run_events(run_id,kind,observed_at) VALUES(?1,'process_started',?2)",
        params![run, now()],
    )?;
    Ok(())
}

pub(super) fn finish_run(
    root: &Path,
    home: &Path,
    run: &str,
    auth: AuthMode,
    status: ExitStatus,
) -> Result<RunRecord> {
    let mut manager = Manager::open(root.into())?;
    let tx = manager.conn.transaction()?;
    let changed = tx.execute(
        "UPDATE runs SET finished_at=?1,exit_code=?2 WHERE id=?3 AND finished_at IS NULL",
        params![now(), status.code(), run],
    )?;
    if changed > 0 {
        tx.execute(
            "INSERT INTO run_events(run_id,kind,observed_at) VALUES(?1,'process_exited',?2)",
            params![run, now()],
        )?;
    }
    tx.commit()?;
    if crate::performance::Service::from_environment()
        .finish_gateway_run(run)
        .is_err()
    {
        tracing::warn!(event = "gateway_run_finish_failed", run_id = run);
    }
    if auth == AuthMode::Oauth {
        super::oauth::restrict_auth_permissions(home)?;
    }
    if status.success() {
        tracing::info!(
            event = "child_exited",
            exit_code = status.code(),
            successful = true
        );
    } else {
        tracing::warn!(
            event = "child_exited",
            exit_code = status.code(),
            successful = false
        );
    }
    manager
        .run_record(run)?
        .ok_or_else(|| Error::message("启动记录不存在"))
}

pub(super) fn load_sessions(conn: &Connection, runs: &mut [RunRecord]) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='run_sessions' AND type='table')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    for chunk in runs.chunks_mut(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut query = conn.prepare(&format!("SELECT run_id,session_id,cwd,activity,turn_id,model,first_seen,last_seen FROM run_sessions WHERE run_id IN ({placeholders}) ORDER BY last_seen DESC,session_id"))?;
        let rows = query
            .query_map(
                rusqlite::params_from_iter(chunk.iter().map(|run| &run.id)),
                |r| {
                    let activity: String = r.get(3)?;
                    let activity =
                        serde_json::from_value(Value::String(activity)).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?;
                    Ok((
                        r.get::<_, String>(0)?,
                        RunSession {
                            id: r.get(1)?,
                            cwd: PathBuf::from(r.get::<_, String>(2)?),
                            activity,
                            turn_id: r.get(4)?,
                            model: r.get(5)?,
                            first_seen: r.get(6)?,
                            last_seen: r.get(7)?,
                        },
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let indices: std::collections::HashMap<_, _> = chunk
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id.clone(), i))
            .collect();
        for (id, session) in rows {
            if let Some(&i) = indices.get(&id) {
                chunk[i].sessions.push(session);
            }
        }
    }
    Ok(())
}

impl Manager {
    /// Bounded read model for visible sessions. Matching requires the same Home
    /// and a confirmed session ID; shared account/project names are insufficient.
    pub fn session_activity(
        &self,
        sessions: &[crate::sessions::Session],
    ) -> Result<std::collections::HashMap<crate::sessions::SessionKey, crate::runs::SessionRuntime>>
    {
        let mut targets: std::collections::HashMap<_, Vec<_>> = std::collections::HashMap::new();
        for session in sessions
            .iter()
            .filter(|session| session.provider == crate::agents::AgentKind::CODEX)
        {
            targets
                .entry((
                    crate::codex_native::SessionStore::resolve(&session.home)
                        .root()
                        .to_path_buf(),
                    session.id.clone(),
                ))
                .or_default()
                .push(session.key());
        }
        let mut result: std::collections::HashMap<_, crate::runs::SessionRuntime> =
            std::collections::HashMap::new();
        if targets.is_empty() {
            return Ok(result);
        }
        let mut homes = std::collections::HashMap::new();
        let mut runs = self.recent_runs(1000)?;
        runs.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        for run in runs {
            if run.sessions.is_empty() {
                continue;
            }
            let home_key = (run.profile_id.clone(), run.revision);
            let home = homes.entry(home_key).or_insert_with(|| {
                self.home(&run.profile_id, run.revision)
                    .ok()
                    .map(|home| {
                        crate::codex_native::SessionStore::resolve(&home)
                            .root()
                            .to_path_buf()
                    })
                    .and_then(|home| home.canonicalize().ok())
            });
            let Some(home) = home else {
                continue;
            };
            let process = crate::runs::RunState::observe(&run);
            for session in run.sessions {
                if let Some(keys) = targets.get(&(home.clone(), session.id.clone())) {
                    for key in keys {
                        result
                            .entry(key.clone())
                            .or_default()
                            .runs
                            .push(crate::runs::SessionRun {
                                run_id: run.id.clone(),
                                process,
                                activity: session.activity,
                                model: session.model.clone(),
                            });
                    }
                }
            }
        }
        Ok(result)
    }

    pub fn run_events(&self, run: &str, limit: usize) -> Result<Vec<RunEvent>> {
        if self.run_record(run)?.is_none() {
            return Err(Error::message("运行不存在"));
        }
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='run_events' AND type='table')",
            [],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut query = self.conn.prepare("SELECT sequence,kind,session_id,turn_id,model,observed_at FROM (SELECT * FROM run_events WHERE run_id=?1 ORDER BY sequence DESC LIMIT ?2) ORDER BY sequence")?;
        Ok(query
            .query_map(params![run, limit.min(1000) as i64], |r| {
                Ok(RunEvent {
                    sequence: r.get(0)?,
                    kind: r.get(1)?,
                    session_id: r.get(2)?,
                    turn_id: r.get(3)?,
                    model: r.get(4)?,
                    observed_at: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
}
