//! Usage-owned launch timestamps. Agent activity remains a separate, read-only signal.
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};

use crate::sessions::SessionKey;

#[derive(Clone)]
pub struct Store {
    path: PathBuf,
}

impl Default for Store {
    fn default() -> Self {
        Self::at(crate::paths::data_dir().join("session_opens.sqlite3"))
    }
}

impl Store {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    fn open(&self) -> Result<Connection, String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("无法创建会话打开记录库：{e}"))?;
        }
        let db =
            Connection::open(&self.path).map_err(|e| format!("无法打开会话打开记录库：{e}"))?;
        db.busy_timeout(Duration::from_millis(250))
            .map_err(|e| format!("会话打开记录库暂不可用：{e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("无法设置会话打开记录库权限：{e}"))?;
        }
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_opens(
                provider TEXT NOT NULL,
                instance TEXT NOT NULL,
                id TEXT NOT NULL,
                opened_at INTEGER NOT NULL,
                PRIMARY KEY(provider,instance,id)
            );",
        )
        .map_err(|e| format!("会话打开记录库初始化失败：{e}"))?;
        Ok(db)
    }

    pub fn record(&self, key: &SessionKey, opened_at: i64) -> Result<(), String> {
        let db = self.open()?;
        db.execute(
            "INSERT INTO session_opens(provider,instance,id,opened_at) VALUES(?1,?2,?3,?4)
             ON CONFLICT(provider,instance,id) DO UPDATE SET opened_at=excluded.opened_at
             WHERE excluded.opened_at>session_opens.opened_at",
            params![key.provider, key.instance, key.id, opened_at],
        )
        .map_err(|e| format!("会话打开记录写入失败：{e}"))?;
        Ok(())
    }

    pub fn list(&self) -> Result<HashMap<SessionKey, i64>, String> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let db = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| format!("无法读取会话打开记录库：{e}"))?;
        db.busy_timeout(Duration::from_millis(250))
            .map_err(|e| format!("会话打开记录库暂不可用：{e}"))?;
        let mut statement = db
            .prepare("SELECT provider,instance,id,opened_at FROM session_opens")
            .map_err(|e| format!("会话打开记录读取失败：{e}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    SessionKey {
                        provider: row.get(0)?,
                        instance: row.get(1)?,
                        id: row.get(2)?,
                    },
                    row.get::<_, i64>(3)?,
                ))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .map_err(|e| format!("会话打开记录读取失败：{e}"))?;
        Ok(rows.into_iter().collect())
    }
}
