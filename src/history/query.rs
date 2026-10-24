//! SQLite-backed, scoped queries over the append-only history ledger.
//!
//! The file cursor table owns log parsing. Queries resolve the configured instance
//! at read time, so changing a data-instance mapping does not rewrite the ledger.
use super::{History, HistoryQuery, HistoryReport, Record, SessionUsage, Tokens};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{Connection, OpenFlags, functions::FunctionFlags, params};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub(super) struct HistoryDatabase {
    pub path: PathBuf,
    pub scope: Vec<(String, String)>,
}

// File rows may overlap after a fork or a log move. Merge by the same
// (instance, source, record id) key and sequence order as the in-memory path.
const CANONICAL: &str = r#"
WITH raw AS MATERIALIZED (
    SELECT h.sequence, s.instance,
           json_extract(h.data,'$.source') source,
           json_extract(h.data,'$.id') id,
           json_extract(h.data,'$.time') time,
           COALESCE(json_extract(h.data,'$.session'),'') session,
           COALESCE(json_extract(h.data,'$.project'),'') project,
           COALESCE(json_extract(h.data,'$.model'),'') model,
           COALESCE(json_extract(h.data,'$.tokens.input'),0) input,
           COALESCE(json_extract(h.data,'$.tokens.output'),0) output,
           COALESCE(json_extract(h.data,'$.tokens.cached'),0) cached,
           COALESCE(json_extract(h.data,'$.tokens.calls'),0) calls
    FROM history_records h JOIN temp_history_scope s ON s.path=h.path
    WHERE json_valid(h.data)
), grouped AS (
    SELECT instance,source,id,MIN(time) time,
           MAX(input) input,MAX(output) output,MAX(cached) cached,MAX(calls) calls,
           MIN(sequence) first_sequence,
           MIN(CASE WHEN project<>'' THEN sequence END) project_sequence,
           MIN(CASE WHEN model<>'' THEN sequence END) model_sequence
    FROM raw GROUP BY instance,source,id
), canonical AS (
    SELECT g.instance,g.source,g.id,g.time,g.input,g.output,g.cached,g.calls,
           first.session,COALESCE(project.project,'') project,
           COALESCE(model.model,'') model
    FROM grouped g
    JOIN raw first ON first.sequence=g.first_sequence
    LEFT JOIN raw project ON project.sequence=g.project_sequence
    LEFT JOIN raw model ON model.sequence=g.model_sequence
), filtered AS (
    SELECT *,date(time,'localtime') day FROM canonical c
    WHERE (input>0 OR output>0)
      AND EXISTS (SELECT 1 FROM json_each(?1) WHERE value=c.source)
      AND (?2 IS NULL OR date(time,'localtime')>=?2)
      AND (?3 IS NULL OR date(time,'localtime')<=?3)
      AND (?4 IS NULL OR history_contains(project,?4))
      AND (?5 IS NULL OR history_contains(model,?5))
      AND (?6 IS NULL OR instance=?6)
)
"#;

impl HistoryDatabase {
    fn connection(&self) -> Result<Connection, String> {
        let db = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| e.to_string())?;
        db.busy_timeout(std::time::Duration::from_secs(20))
            .map_err(|e| e.to_string())?;
        db.create_scalar_function(
            "history_contains",
            2,
            FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let haystack: String = ctx.get(0)?;
                let needle: String = ctx.get(1)?;
                Ok(haystack.to_lowercase().contains(&needle.to_lowercase()))
            },
        )
        .map_err(|e| e.to_string())?;
        db.execute_batch(
            "PRAGMA temp_store=FILE; CREATE TEMP TABLE temp_history_scope(path TEXT PRIMARY KEY,instance TEXT NOT NULL)",
        )
        .map_err(|e| e.to_string())?;
        {
            let mut insert = db
                .prepare("INSERT INTO temp_history_scope(path,instance) VALUES(?1,?2)")
                .map_err(|e| e.to_string())?;
            for (path, instance) in &self.scope {
                insert
                    .execute(params![path, instance])
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(db)
    }

    pub(super) fn report(
        &self,
        snapshot: &History,
        year: i32,
        sources: &[String],
        query: &HistoryQuery,
    ) -> Result<HistoryReport, String> {
        let db = self.connection()?;
        let from = query
            .from
            .or_else(|| NaiveDate::from_ymd_opt(year, 1, 1))
            .ok_or_else(|| "无效的历史年份".to_string())?;
        let to = query
            .to
            .or_else(|| NaiveDate::from_ymd_opt(year, 12, 31))
            .ok_or_else(|| "无效的历史年份".to_string())?;
        let sources = serde_json::to_string(sources).map_err(|e| e.to_string())?;
        let sql = format!(
            "{CANONICAL} SELECT day,source,instance,SUM(input),SUM(output),SUM(cached),SUM(calls) \
             FROM filtered GROUP BY day,source,instance"
        );
        let mut stmt = db.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(
                params![
                    sources,
                    from.to_string(),
                    to.to_string(),
                    query.project,
                    query.model,
                    query.instance
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        Tokens {
                            input: r.get::<_, i64>(3)?.max(0) as u64,
                            output: r.get::<_, i64>(4)?.max(0) as u64,
                            cached: r.get::<_, i64>(5)?.max(0) as u64,
                            calls: r.get::<_, i64>(6)?.max(0) as u64,
                        },
                    ))
                },
            )
            .map_err(|e| e.to_string())?;
        let mut daily = BTreeMap::<NaiveDate, BTreeMap<String, Tokens>>::new();
        let mut by_instance = BTreeMap::<String, Tokens>::new();
        for row in rows {
            let (date, source, instance, tokens) = row.map_err(|e| e.to_string())?;
            let date = NaiveDate::parse_from_str(&date, "%Y-%m-%d").map_err(|e| e.to_string())?;
            daily
                .entry(date)
                .or_default()
                .entry(source)
                .or_default()
                .add(tokens);
            by_instance.entry(instance).or_default().add(tokens);
        }
        let temporary = History {
            days: daily,
            warnings: snapshot.warnings.clone(),
            synced_at: snapshot.synced_at,
            scanned_files: snapshot.scanned_files,
            read_bytes: snapshot.read_bytes,
            ..History::default()
        };
        let mut report = temporary.report(
            year,
            &serde_json::from_str::<Vec<String>>(&sources).map_err(|e| e.to_string())?,
        );
        report.by_instance = by_instance;
        report.from = query.from;
        report.to = query.to;
        if query.from.is_some() || query.to.is_some() {
            report.daily.clear();
            report.monthly.clear();
            report.totals = Tokens::default();
            let mut date = from;
            while date <= to {
                let tokens = temporary.day(date, &report.providers);
                report.daily.insert(date.to_string(), tokens);
                report
                    .monthly
                    .entry(date.format("%Y-%m").to_string())
                    .or_default()
                    .add(tokens);
                report.totals.add(tokens);
                let Some(next) = date.succ_opt() else { break };
                date = next;
            }
        }
        Ok(report)
    }

    pub(super) fn session_usage(
        &self,
        sources: &[String],
        query: &HistoryQuery,
    ) -> Result<HashMap<crate::sessions::SessionKey, SessionUsage>, String> {
        let db = self.connection()?;
        let from = query.from.map(|date| date.to_string());
        let to = query.to.map(|date| date.to_string());
        let sql = format!(
            "{CANONICAL} SELECT instance,source,session,model,SUM(input),SUM(output),SUM(cached),SUM(calls) \
             FROM filtered GROUP BY instance,source,session,model"
        );
        let sources = serde_json::to_string(sources).map_err(|e| e.to_string())?;
        let mut stmt = db.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(
                params![
                    sources,
                    from,
                    to,
                    query.project,
                    query.model,
                    query.instance
                ],
                |r| {
                    Ok((
                        crate::sessions::SessionKey {
                            instance: r.get(0)?,
                            provider: r.get(1)?,
                            id: r.get(2)?,
                        },
                        r.get::<_, String>(3)?,
                        Tokens {
                            input: r.get::<_, i64>(4)?.max(0) as u64,
                            output: r.get::<_, i64>(5)?.max(0) as u64,
                            cached: r.get::<_, i64>(6)?.max(0) as u64,
                            calls: r.get::<_, i64>(7)?.max(0) as u64,
                        },
                    ))
                },
            )
            .map_err(|e| e.to_string())?;
        let mut sessions = HashMap::<crate::sessions::SessionKey, SessionUsage>::new();
        for row in rows {
            let (key, model, tokens) = row.map_err(|e| e.to_string())?;
            let usage = sessions.entry(key).or_default();
            usage.tokens.add(tokens);
            if !model.is_empty() && !usage.models.contains(&model) {
                usage.models.push(model);
            }
        }
        for usage in sessions.values_mut() {
            usage.models.sort();
        }
        Ok(sessions)
    }

    pub(super) fn distinct(&self, sources: &[String], field: &str) -> Result<Vec<String>, String> {
        let db = self.connection()?;
        let field = match field {
            "project" | "model" | "instance" => field,
            _ => return Err("无效的历史维度".into()),
        };
        let sql = format!(
            "{CANONICAL} SELECT DISTINCT {field} FROM filtered WHERE {field}<>'' ORDER BY {field}"
        );
        let sources = serde_json::to_string(sources).map_err(|e| e.to_string())?;
        let mut stmt = db.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(
                params![
                    sources,
                    Option::<String>::None,
                    Option::<String>::None,
                    Option::<String>::None,
                    Option::<String>::None,
                    Option::<String>::None
                ],
                |r| r.get::<_, String>(0),
            )
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub(super) fn records(&self, query: &HistoryQuery) -> Result<Vec<Record>, String> {
        let db = self.connection()?;
        let sql = format!(
            "{CANONICAL} SELECT source,instance,id,time,input,output,cached,calls,session,project,model \
             FROM filtered ORDER BY time,id"
        );
        let sources = serde_json::to_string(&super::SOURCES).map_err(|e| e.to_string())?;
        let mut stmt = db.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(
                params![
                    sources,
                    query.from.map(|d| d.to_string()),
                    query.to.map(|d| d.to_string()),
                    query.project,
                    query.model,
                    query.instance
                ],
                |r| {
                    let timestamp: String = r.get(3)?;
                    let time = DateTime::parse_from_rfc3339(&timestamp)
                        .map_err(|_| {
                            rusqlite::Error::InvalidColumnType(
                                3,
                                "time".into(),
                                rusqlite::types::Type::Text,
                            )
                        })?
                        .with_timezone(&Utc);
                    Ok(Record {
                        source: r.get(0)?,
                        instance: r.get(1)?,
                        id: r.get(2)?,
                        time,
                        tokens: Tokens {
                            input: r.get::<_, i64>(4)?.max(0) as u64,
                            output: r.get::<_, i64>(5)?.max(0) as u64,
                            cached: r.get::<_, i64>(6)?.max(0) as u64,
                            calls: r.get::<_, i64>(7)?.max(0) as u64,
                        },
                        session: r.get(8)?,
                        project: r.get(9)?,
                        model: r.get(10)?,
                    })
                },
            )
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
}
