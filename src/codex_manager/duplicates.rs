//! Add-time duplicate checks under the same SQLite write transaction as the insert.
use super::{Error, Result, secrets};
use rusqlite::Connection;
use std::path::Path;

fn settings(text: &str) -> Option<toml::Value> {
    let mut value: toml::Value = toml::from_str(text).ok()?;
    let provider = value.get("model_provider")?.as_str()?.to_owned();
    // Equivalent endpoint spelling must not create a second configuration.
    if let Some(base) = value
        .get_mut("model_providers")?
        .get_mut(&provider)?
        .get_mut("base_url")
    {
        let url = reqwest::Url::parse(base.as_str()?).ok()?;
        if url.query().is_none() && url.fragment().is_none() {
            *base = toml::Value::String(url.as_str().trim_end_matches('/').into());
        }
    }
    Some(value)
}

pub(super) fn reject(conn: &Connection, root: &Path, text: &str, key: &str) -> Result<()> {
    let Some(candidate) = settings(text) else {
        return Ok(());
    };
    let mut query = conn.prepare(
        "SELECT p.id,p.name,r.config,r.secret_ref FROM profiles p
         JOIN revisions r ON r.profile_id=p.id AND r.revision=p.revision
         WHERE p.archived=0 AND r.secret_ref<>'oauth' ORDER BY p.rowid",
    )?;
    let rows = query.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (id, name, config, reference) = row?;
        if settings(&config).as_ref() == Some(&candidate)
            && secrets::get(root, &reference).is_ok_and(|stored| stored == key)
        {
            tracing::info!(event="provider_duplicate_rejected", account_id=%id);
            return Err(Error::message(format!(
                "Provider 配置已存在：{name}；请选择已有 Provider，如需独立副本请使用“复制”"
            )));
        }
    }
    Ok(())
}
