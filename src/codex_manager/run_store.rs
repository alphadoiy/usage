//! Persisted runs: launch records with their captured identity and confirmed sessions, kept
//! in the account store but separate from account configuration.
mod evidence;

use std::path::Path;
use std::process::ExitStatus;

use rusqlite::OptionalExtension;

use super::{Error, Manager, Result, RunRecord, runtime};

impl Manager {
    /// A read-only capability can outlive this connection without exposing account SQL.
    pub(crate) fn session_evidence_source(&self) -> crate::runs::SessionEvidenceSource {
        evidence::source(&self.root)
    }

    /// Keep all pages bounded; unfinished records precede completed history.
    pub fn recent_runs(&self, limit: usize) -> Result<Vec<RunRecord>> {
        let has_contexts: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='run_contexts')",
            [],
            |row| row.get(0),
        )?;
        let (column, join) = if has_contexts {
            (
                "c.context_json",
                "LEFT JOIN run_contexts c ON c.run_id=r.id",
            )
        } else {
            ("NULL", "")
        };
        let sql = format!("SELECT r.id,r.profile_id,r.revision,r.cwd,r.started_at,r.finished_at,r.pid,r.exit_code,{column} FROM runs r {join}
            ORDER BY r.finished_at IS NOT NULL,r.started_at DESC,r.id DESC LIMIT ?1");
        let mut query = self.conn.prepare(&sql)?;
        let mut runs = query
            .query_map([limit.min(1000) as i64], RunRecord::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        runtime::load_sessions(&self.conn, &mut runs)?;
        Ok(runs)
    }

    pub fn run(
        &mut self,
        selector: &str,
        cwd: &Path,
        revision: Option<i64>,
        resume: Option<&str>,
        program: &Path,
        args: &[String],
    ) -> Result<ExitStatus> {
        self.run_recorded(selector, cwd, revision, resume, program, args)
            .map(|(status, _)| status)
    }

    pub fn run_recorded(
        &mut self,
        selector: &str,
        cwd: &Path,
        revision: Option<i64>,
        resume: Option<&str>,
        program: &Path,
        args: &[String],
    ) -> Result<(ExitStatus, RunRecord)> {
        self.run_recorded_with_config(selector, cwd, revision, resume, program, args, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_recorded_with_config(
        &mut self,
        selector: &str,
        cwd: &Path,
        revision: Option<i64>,
        resume: Option<&str>,
        program: &Path,
        args: &[String],
        usage_config: Option<&Path>,
    ) -> Result<(ExitStatus, RunRecord)> {
        use crate::agents::RunningAgent;
        let mut handle = self.start_recorded_with_config(
            selector,
            cwd,
            revision,
            resume,
            program,
            args,
            usage_config,
        )?;
        let outcome = handle.wait().map_err(Error::message)?;
        Ok((
            outcome.status,
            outcome
                .run
                .ok_or_else(|| Error::message("启动记录不存在"))?,
        ))
    }

    /// Read persisted runs without guessing liveness from an unfinished record.
    pub fn runs(&self, profile_id: Option<&str>) -> Result<Vec<RunRecord>> {
        let mut query = self.conn.prepare(
            "SELECT r.id,r.profile_id,r.revision,r.cwd,r.started_at,r.finished_at,r.pid,r.exit_code,c.context_json FROM runs r
             LEFT JOIN run_contexts c ON c.run_id=r.id
             WHERE (?1 IS NULL OR r.profile_id=?1) ORDER BY r.started_at DESC,r.id DESC",
        )?;
        let mut records = query
            .query_map([profile_id], RunRecord::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        runtime::load_sessions(&self.conn, &mut records)?;
        Ok(records)
    }

    pub fn run_record(&self, id: &str) -> Result<Option<RunRecord>> {
        let record = self.conn.query_row(
            "SELECT r.id,r.profile_id,r.revision,r.cwd,r.started_at,r.finished_at,r.pid,r.exit_code,c.context_json FROM runs r
             LEFT JOIN run_contexts c ON c.run_id=r.id WHERE r.id=?1",
            [id], RunRecord::from_row,
        ).optional()?;
        let mut records: Vec<_> = record.into_iter().collect();
        runtime::load_sessions(&self.conn, &mut records)?;
        Ok(records.pop())
    }

    pub fn resources_for_run(
        &self,
        id: &str,
        configs: &[crate::resources::connections::ConnectionConfig],
        ids: &[String],
    ) -> Result<crate::runs::PinnedResources> {
        self.resources_for_run_with(id, configs, ids, &|name| std::env::var(name).ok())
    }

    pub(crate) fn resources_for_run_with(
        &self,
        id: &str,
        configs: &[crate::resources::connections::ConnectionConfig],
        ids: &[String],
        credentials: &dyn Fn(&str) -> Option<String>,
    ) -> Result<crate::runs::PinnedResources> {
        let (profile_id, text): (String, Option<String>) = self
            .conn
            .query_row(
                "SELECT r.profile_id,c.context_json FROM runs r
                 LEFT JOIN run_contexts c ON c.run_id=r.id WHERE r.id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| Error::message("找不到运行记录"))?;
        let captured: crate::runs::CapturedContext = serde_json::from_str(
            &text.ok_or_else(|| Error::message("旧运行未记录实际配置，不能推断资源关联"))?,
        )
        .map_err(|_| Error::message("运行身份记录无法解析"))?;
        captured
            .select(id, &profile_id, configs, ids, credentials)
            .map_err(Error::message)
    }
}
