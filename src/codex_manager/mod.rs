//! Saved BYOK/OAuth accounts, isolated Codex homes, and read-only configuration imports.
pub mod bark;
pub mod ccswitch;
pub mod cli;
pub mod config;
mod effective;
mod launcher;
mod local;
pub use local::{local_resume_shell_command, start_local_codex};
pub mod oauth;
pub mod runtime;
mod secrets;

use crate::logging;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub struct Error(String);
impl Error {
    pub fn message(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        tracing::debug!(event = "io_failed", error_kind = ?e.kind());
        Self(e.to_string())
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(code, _) = &e {
            tracing::debug!(event = "sqlite_failed", sqlite_code = code.extended_code);
        }
        Self(e.to_string())
    }
}

pub fn default_root() -> PathBuf {
    if let Some(path) = std::env::var_os("USAGE_CODEX_DIR").filter(|v| !v.is_empty()) {
        return crate::config::resolved_data_path(Path::new(&path));
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| crate::config::home_dir().join(".local/share"));
    crate::config::resolved_data_path(&base.join("usage/codex"))
}

fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    private_write_impl(path, bytes, true)
}

/// Atomically initialize a file without replacing a concurrent CLI's mutable configuration.
pub(crate) fn private_create(path: &Path, bytes: &[u8]) -> Result<()> {
    private_write_impl(path, bytes, false)
}

fn private_write_impl(path: &Path, bytes: &[u8], overwrite: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("文件缺少父目录"))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if overwrite {
            fs::rename(&temp, path)?;
        } else {
            match fs::hard_link(&temp, path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}

#[derive(Debug, Clone, Serialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub revision: i64,
    pub base_url: String,
    pub model: String,
    pub source: Option<String>,
    pub follow: bool,
    pub source_status: String,
    pub auth_mode: AuthMode,
    pub oauth: Option<oauth::Identity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    Byok,
    Oauth,
}

impl AuthMode {
    pub(crate) fn log_name(self) -> &'static str {
        match self {
            Self::Byok => "byok",
            Self::Oauth => "oauth",
        }
    }
}

pub struct Revision {
    pub config: String,
    secret_ref: String,
}

/// Selected account/revision at launch. A home can contain many runs and conversations.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RunRecord {
    pub id: String,
    pub profile_id: String,
    pub revision: i64,
    pub cwd: PathBuf,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    /// Absent for records written before launch identity capture was introduced.
    pub context: Option<crate::runs::RunContext>,
    pub sessions: Vec<crate::runs::RunSession>,
    #[serde(skip_serializing)]
    pub process_identity: Option<crate::runs::ProcessIdentity>,
}

impl RunRecord {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let capture = row
            .get::<_, Option<String>>(8)?
            .map(|text| {
                serde_json::from_str::<crate::runs::CapturedContext>(&text).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        8,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            })
            .transpose()?;
        Ok(Self {
            id: row.get(0)?,
            profile_id: row.get(1)?,
            revision: row.get(2)?,
            cwd: PathBuf::from(row.get::<_, String>(3)?),
            started_at: row.get(4)?,
            finished_at: row.get(5)?,
            pid: row.get(6)?,
            exit_code: row.get(7)?,
            process_identity: capture.as_ref().and_then(|capture| capture.process.clone()),
            context: capture.map(|capture| capture.context),
            sessions: Vec::new(),
        })
    }
}

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    pub skipped: Vec<(String, String)>,
    pub conflicts: Vec<String>,
    pub missing: Vec<String>,
}
impl SyncReport {
    fn log(&self, dry_run: bool) {
        tracing::info!(
            event = "source_sync_summary",
            dry_run,
            created = self.created.len(),
            updated = self.updated.len(),
            unchanged = self.unchanged.len(),
            skipped = self.skipped.len(),
            conflicts = self.conflicts.len(),
            missing = self.missing.len()
        );
        if !self.skipped.is_empty() || !self.conflicts.is_empty() || !self.missing.is_empty() {
            tracing::warn!(
                event = "source_sync_attention",
                skipped = self.skipped.len(),
                conflicts = self.conflicts.len(),
                missing = self.missing.len()
            );
        }
    }
}
impl std::fmt::Display for SyncReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "新增 {} · 更新 {} · 未变化 {} · 跳过 {} · 冲突 {} · 源已删除 {}",
            self.created.len(),
            self.updated.len(),
            self.unchanged.len(),
            self.skipped.len(),
            self.conflicts.len(),
            self.missing.len()
        )?;
        for name in &self.created {
            writeln!(f, "新增：{name}")?;
        }
        for name in &self.updated {
            writeln!(f, "更新：{name}")?;
        }
        for (name, reason) in &self.skipped {
            writeln!(f, "跳过 {name}：{reason}")?;
        }
        for name in &self.conflicts {
            writeln!(f, "冲突：{name}（保留本地配置，可复制为独立账号配置）")?;
        }
        for name in &self.missing {
            writeln!(f, "源已删除：{name}（保留配置及历史）")?;
        }
        Ok(())
    }
}

struct SourceBinding {
    id: String,
    provider: String,
    name: String,
    revision: i64,
    hash: Option<String>,
    source_revision: Option<i64>,
    follow: bool,
}

#[derive(Serialize, Deserialize)]
struct HomeBinding {
    profile_id: String,
    revision: i64,
}

pub struct Manager {
    root: PathBuf,
    conn: Connection,
}

impl Manager {
    /// Dashboard discovery never creates, migrates, or changes permissions on a store.
    pub fn open_read_only(root: PathBuf) -> Result<Option<Self>> {
        let path = root.join("manager.sqlite3");
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_millis(500))?;
        Ok(Some(Self { root, conn }))
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

    pub fn open_default() -> Result<Self> {
        Self::open(default_root())
    }

    pub fn open(root: PathBuf) -> Result<Self> {
        logging::operation("codex.manager_open", |op| {
            op.step("directory_prepare", || private_dir(&root))?;
            let root = root.canonicalize()?;
            let conn = op.step("database_open", || {
                Connection::open(root.join("manager.sqlite3"))
            })?;
            conn.busy_timeout(Duration::from_secs(5))?;
            op.begin("database_schema");
            conn.execute_batch("PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS profiles (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, revision INTEGER NOT NULL,
                source_path TEXT, source_provider TEXT, source_hash TEXT, source_revision INTEGER,
                follow INTEGER NOT NULL DEFAULT 0, source_status TEXT NOT NULL DEFAULT '', archived INTEGER NOT NULL DEFAULT 0,
                UNIQUE(source_path,source_provider));
            CREATE TABLE IF NOT EXISTS revisions (
                profile_id TEXT NOT NULL REFERENCES profiles(id), revision INTEGER NOT NULL,
                config TEXT NOT NULL, secret_ref TEXT NOT NULL, created_at TEXT NOT NULL,
                PRIMARY KEY(profile_id,revision));
            CREATE TABLE IF NOT EXISTS homes (
                profile_id TEXT NOT NULL, revision INTEGER NOT NULL, path TEXT UNIQUE NOT NULL,
                PRIMARY KEY(profile_id,revision),
                FOREIGN KEY(profile_id,revision) REFERENCES revisions(profile_id,revision));
            CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY, profile_id TEXT NOT NULL, revision INTEGER NOT NULL,
                cwd TEXT NOT NULL, started_at TEXT NOT NULL, finished_at TEXT, pid INTEGER, exit_code INTEGER,
                FOREIGN KEY(profile_id,revision) REFERENCES revisions(profile_id,revision));
            CREATE TABLE IF NOT EXISTS oauth_accounts (
                profile_id TEXT PRIMARY KEY REFERENCES profiles(id),
                identity_key TEXT UNIQUE NOT NULL, identity_json TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS run_contexts (
                run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
                context_json TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS run_events (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                kind TEXT NOT NULL, session_id TEXT, turn_id TEXT, model TEXT,
                observed_at TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS run_events_run ON run_events(run_id, sequence);
            CREATE TABLE IF NOT EXISTS run_sessions (
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                session_id TEXT NOT NULL, cwd TEXT NOT NULL, activity TEXT NOT NULL,
                turn_id TEXT, model TEXT, first_seen TEXT NOT NULL, last_seen TEXT NOT NULL,
                PRIMARY KEY(run_id, session_id));")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(
                    root.join("manager.sqlite3"),
                    fs::Permissions::from_mode(0o600),
                )?;
            }
            Ok(Self { root, conn })
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list(&self) -> Result<Vec<Profile>> {
        list_profiles(&self.conn, false)
    }

    pub fn resolve(&self, selector: &str) -> Result<Profile> {
        let profiles = list_profiles(&self.conn, true)?;
        if let Some(profile) = profiles.iter().find(|p| p.id == selector) {
            return Ok(profile.clone());
        }
        let found: Vec<_> = profiles
            .into_iter()
            .filter(|p| p.name == selector)
            .collect();
        match found.as_slice() {
            [profile] => Ok(profile.clone()),
            [] => Err(Error::message(
                "找不到账号配置；使用 usage codex list 查看名称和 ID",
            )),
            _ => Err(Error::message("名称重复；请使用完整 ID 选择账号配置")),
        }
    }

    pub fn revision(&self, id: &str, revision: i64) -> Result<Revision> {
        self.conn
            .query_row(
                "SELECT config,secret_ref FROM revisions WHERE profile_id=?1 AND revision=?2",
                params![id, revision],
                |r| {
                    Ok(Revision {
                        config: r.get(0)?,
                        secret_ref: r.get(1)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| Error::message("找不到保存的配置版本"))
    }

    pub(crate) fn usage_source(&self, id: &str) -> Result<Option<(PathBuf, String)>> {
        Ok(self.conn.query_row(
            "SELECT source_path,source_provider FROM profiles WHERE id=?1 AND source_path IS NOT NULL AND source_provider IS NOT NULL AND source_revision=revision AND follow=1",
            [id], |row| Ok((PathBuf::from(row.get::<_,String>(0)?),row.get(1)?)),
        ).optional()?)
    }

    /// Prepare a read-only model query using this exact saved account revision.
    pub(crate) fn model_catalog_query(
        &self,
        profile: &Profile,
    ) -> Result<crate::provider_catalog::ModelQuery> {
        if profile.auth_mode == AuthMode::Oauth {
            let home = self.home(&profile.id, profile.revision)?;
            let auth = oauth::read_auth(&home.join("auth.json"))?;
            if profile.oauth.as_ref().is_none_or(|identity| {
                identity.account_id != auth.identity.account_id
                    || identity.user_id != auth.identity.user_id
            }) {
                return Err(Error::message("OAuth 登录文件与账号身份不匹配；请重新登录"));
            }
            return crate::provider_catalog::ModelQuery::codex(
                profile.id.clone(),
                &home,
                auth.access_token,
                auth.identity.account_id,
            )
            .map_err(Error::message);
        }
        let saved = self.revision(&profile.id, profile.revision)?;
        let (base, _) = config::summary(&saved.config)?;
        let key = secrets::get(&self.root, &saved.secret_ref)?;
        crate::provider_catalog::ModelQuery::new(profile.id.clone(), &base, key)
            .map_err(Error::message)
    }

    pub fn add(&mut self, name: &str, text: &str, key: &str) -> Result<Profile> {
        logging::operation("codex.account_add", |op| {
            op.step("name_validate", || validate_name(name))?;
            let config = op.step("config_validate", || config::normalize(text))?;
            let id = uuid::Uuid::new_v4().to_string();
            op.account(&id, 1, "byok");
            op.begin("database_transaction");
            let tx = self.conn.transaction()?;
            let secret_ref = op.step("credential_save", || secrets::put(&self.root, key))?;
            tx.execute(
                "INSERT INTO profiles(id,name,revision) VALUES (?1,?2,1)",
                params![id, name],
            )?;
            tx.execute(
                "INSERT INTO revisions VALUES (?1,1,?2,?3,?4)",
                params![id, config, secret_ref, now()],
            )?;
            op.step("database_commit", || tx.commit())?;
            self.resolve(&id)
        })
    }

    /// Local edits detach from following. expected_revision implements optimistic conflict detection.
    pub fn update(
        &mut self,
        selector: &str,
        expected_revision: i64,
        text: &str,
        key: Option<&str>,
        name: Option<&str>,
    ) -> Result<Profile> {
        logging::operation("codex.account_update", |op| {
            let profile = op.step("account_resolve", || self.resolve(selector))?;
            op.account(&profile.id, profile.revision, profile.auth_mode.log_name());
            op.begin("config_validate");
            if let Some(name) = name {
                validate_name(name)?;
            }
            let config = match profile.auth_mode {
                AuthMode::Byok => config::normalize(text)?,
                AuthMode::Oauth => {
                    if key.is_some() {
                        return Err(Error::message("OAuth 账号不能设置 API Key；请重新登录"));
                    }
                    oauth::normalize_config(text)?
                }
            };
            let old = self.revision(&profile.id, profile.revision)?;
            op.begin("database_transaction");
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let revision: i64 = tx.query_row(
                "SELECT revision FROM profiles WHERE id=?1",
                [&profile.id],
                |r| r.get(0),
            )?;
            if revision != expected_revision {
                tracing::warn!(
                    event = "revision_conflict",
                    expected_revision,
                    current_revision = revision
                );
                return Err(Error::message("配置已被其他进程更新；重新打开后再保存"));
            }
            let secret_ref = match key {
                Some(key) => op.step("credential_save", || secrets::put(&self.root, key))?,
                None => old.secret_ref,
            };
            let next = revision + 1;
            tx.execute(
                "INSERT INTO revisions VALUES (?1,?2,?3,?4,?5)",
                params![profile.id, next, config, secret_ref, now()],
            )?;
            let status = if profile.auth_mode == AuthMode::Oauth {
                "已保存 OAuth 配置"
            } else if profile.source.is_some() {
                "本地修改，已暂停跟随"
            } else {
                "本地配置"
            };
            tx.execute(
                "UPDATE profiles SET name=?1,revision=?2,follow=0,source_status=?4 WHERE id=?3",
                params![name.unwrap_or(&profile.name), next, profile.id, status],
            )?;
            op.step("database_commit", || tx.commit())?;
            self.resolve(&profile.id)
        })
    }

    pub fn copy(&mut self, selector: &str, name: &str) -> Result<Profile> {
        let profile = self.resolve(selector)?;
        if profile.auth_mode == AuthMode::Oauth {
            return Err(Error::message(
                "OAuth 登录不能复制；请添加另一个账号并独立登录",
            ));
        }
        let revision = self.revision(&profile.id, profile.revision)?;
        let key = secrets::get(&self.root, &revision.secret_ref)?;
        self.add(name, &revision.config, &key)
    }

    pub fn remove(&mut self, selector: &str) -> Result<()> {
        let p = self.resolve(selector)?;
        self.conn.execute(
            "UPDATE profiles SET archived=1,follow=0 WHERE id=?1",
            [&p.id],
        )?;
        Ok(())
    }

    pub fn detach(&mut self, selector: &str) -> Result<()> {
        let p = self.resolve(selector)?;
        self.conn.execute("UPDATE profiles SET source_path=NULL,source_provider=NULL,source_hash=NULL,source_revision=NULL,follow=0,source_status='独立配置' WHERE id=?1",[p.id])?;
        Ok(())
    }

    pub fn follow(&mut self, selector: &str, enabled: bool) -> Result<()> {
        let profile = self.resolve(selector)?;
        if enabled && profile.source.is_none() {
            return Err(Error::message("此账号配置没有 CC Switch 来源"));
        }
        let source_revision: Option<i64> = self.conn.query_row(
            "SELECT source_revision FROM profiles WHERE id=?1",
            [&profile.id],
            |r| r.get(0),
        )?;
        if enabled && source_revision != Some(profile.revision) {
            return Err(Error::message(
                "此配置已在本地修改；请保留为独立配置，或导入为新的账号配置",
            ));
        }
        self.conn.execute(
            "UPDATE profiles SET follow=?1,source_status=?2 WHERE id=?3",
            params![
                enabled,
                if enabled {
                    "跟随 CC Switch"
                } else {
                    "已暂停跟随"
                },
                profile.id
            ],
        )?;
        Ok(())
    }

    pub fn import_ccswitch(
        &mut self,
        path: &Path,
        follow: bool,
        selected: &[String],
        dry_run: bool,
    ) -> Result<SyncReport> {
        logging::operation("codex.source_import", |op| {
            let source = op.step("source_read", || ccswitch::read(path))?;
            let report = op.step("source_apply", || {
                self.apply_source(source, follow, selected, dry_run, false)
            })?;
            report.log(dry_run);
            Ok(report)
        })
    }

    pub fn sync(&mut self, dry_run: bool) -> Result<SyncReport> {
        logging::operation("codex.source_sync", |op| {
            op.begin("source_bindings_read");
            let sources: Vec<String> = self.conn.prepare("SELECT DISTINCT source_path FROM profiles WHERE follow=1 AND source_path IS NOT NULL")?
            .query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
            let mut report = SyncReport::default();
            for path in sources {
                match op.step("source_read", || ccswitch::read(Path::new(&path))) {
                    Ok(source) => {
                        let one = op.step("source_apply", || {
                            self.apply_source(source, true, &[], dry_run, true)
                        })?;
                        report.created.extend(one.created);
                        report.updated.extend(one.updated);
                        report.unchanged.extend(one.unchanged);
                        report.skipped.extend(one.skipped);
                        report.conflicts.extend(one.conflicts);
                        report.missing.extend(one.missing);
                    }
                    Err(error) => report.skipped.push((path, error.to_string())),
                }
            }
            report.log(dry_run);
            Ok(report)
        })
    }

    fn apply_source(
        &mut self,
        source: ccswitch::ReadResult,
        follow: bool,
        selected: &[String],
        dry_run: bool,
        only_followed: bool,
    ) -> Result<SyncReport> {
        let path = source.path.to_string_lossy().to_string();
        let mut report = SyncReport {
            skipped: source.skipped,
            ..Default::default()
        };
        if selected.iter().any(|id| !source.provider_ids.contains(id)) {
            return Err(Error::message("选中的 CC Switch 供应商 ID 不存在"));
        }
        // Lock before reading bindings so simultaneous launches cannot apply the same revision twice.
        let tx = self.conn.transaction_with_behavior(if dry_run {
            TransactionBehavior::Deferred
        } else {
            TransactionBehavior::Immediate
        })?;
        let existing: Vec<SourceBinding> = tx
            .prepare("SELECT id,source_provider,name,revision,source_hash,source_revision,follow FROM profiles WHERE source_path=?1")?
            .query_map([&path], |r| Ok(SourceBinding {
                id: r.get(0)?, provider: r.get(1)?, name: r.get(2)?, revision: r.get(3)?,
                hash: r.get(4)?, source_revision: r.get(5)?, follow: r.get(6)?,
            }))?.collect::<std::result::Result<_, _>>()?;
        for candidate in source.candidates {
            if !selected.is_empty() && !selected.contains(&candidate.provider_id) {
                continue;
            }
            let old = existing
                .iter()
                .find(|e| e.provider == candidate.provider_id);
            if only_followed && !old.is_some_and(|e| e.follow) {
                continue;
            }
            validate_name(&candidate.name)?;
            let digest = source_digest(&candidate.name, &candidate.config, &candidate.key);
            if let Some(old) = old {
                if old.hash.as_deref() == Some(&digest) {
                    let reference: String = tx.query_row(
                        "SELECT secret_ref FROM revisions WHERE profile_id=?1 AND revision=?2",
                        params![old.id, old.revision],
                        |r| r.get(0),
                    )?;
                    let migrate = reference.starts_with("keychain:")
                        && old.source_revision == Some(old.revision);
                    if migrate {
                        report.updated.push(candidate.name.clone());
                    } else {
                        report.unchanged.push(candidate.name.clone());
                    }
                    if !dry_run {
                        if migrate {
                            // Matching source digest verifies the same key; no Keychain access is needed.
                            let file_ref = secrets::put(&self.root, &candidate.key)?;
                            tx.execute("UPDATE revisions SET secret_ref=?1 WHERE profile_id=?2 AND secret_ref=?3",
                                params![file_ref, old.id, reference])?;
                        }
                        tx.execute(
                            "UPDATE profiles SET follow=?1,source_status=?2,archived=0 WHERE id=?3",
                            params![
                                follow && old.source_revision == Some(old.revision),
                                if old.source_revision == Some(old.revision) {
                                    "源配置未变化"
                                } else {
                                    "本地修改，已暂停跟随"
                                },
                                old.id
                            ],
                        )?;
                    }
                    continue;
                }
                if old.source_revision != Some(old.revision) {
                    report.conflicts.push(old.name.clone());
                    continue;
                }
                report.updated.push(candidate.name.clone());
            } else {
                report.created.push(candidate.name.clone());
            }
            if dry_run {
                continue;
            }
            let id = old
                .map(|e| e.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let revision = old.map(|e| e.revision + 1).unwrap_or(1);
            let secret_ref = secrets::put(&self.root, &candidate.key)?;
            if old.is_none() {
                tx.execute("INSERT INTO profiles(id,name,revision,source_path,source_provider,source_hash,source_revision,follow,source_status)
                    VALUES (?1,?2,?3,?4,?5,?6,?3,?7,'已导入')", params![id,candidate.name,revision,path,candidate.provider_id,digest,follow])?;
            } else {
                tx.execute("UPDATE profiles SET name=?1,revision=?2,source_hash=?3,source_revision=?2,follow=?4,source_status='已同步',archived=0 WHERE id=?5",
                    params![candidate.name,revision,digest,follow,id])?;
            }
            tx.execute(
                "INSERT INTO revisions VALUES (?1,?2,?3,?4,?5)",
                params![id, revision, candidate.config, secret_ref, now()],
            )?;
        }
        for old in &existing {
            if only_followed && old.follow && !source.provider_ids.contains(&old.provider) {
                report.missing.push(old.name.clone());
                if !dry_run {
                    tx.execute(
                        "UPDATE profiles SET source_status='源已删除，保留快照' WHERE id=?1",
                        [&old.id],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(report)
    }

    pub fn home(&self, id: &str, revision: i64) -> Result<PathBuf> {
        uuid::Uuid::parse_str(id).map_err(|_| Error::message("无效的账号配置 ID"))?;
        if revision < 1 {
            return Err(Error::message("无效的配置版本"));
        }
        let revision = if self.resolve(id)?.auth_mode == AuthMode::Oauth {
            1
        } else {
            revision
        };
        Ok(self
            .root
            .join("homes")
            .join(id)
            .join(format!("r{revision}")))
    }

    /// BYOK revisions own persistent Homes; OAuth versions share one account Home and native profiles.
    pub fn materialize(&self, id: &str, revision: i64) -> Result<PathBuf> {
        logging::operation("codex.home_materialize", |op| {
            op.account(id, revision, "byok");
            if op.step("account_resolve", || self.resolve(id))?.auth_mode == AuthMode::Oauth {
                op.account(id, revision, "oauth");
                return self.materialize_oauth(id, revision);
            }
            let saved = op.step("revision_read", || self.revision(id, revision))?;
            let home = self.home(id, revision)?;
            let tx = self.conn.unchecked_transaction()?;
            op.step("home_prepare", || private_dir(&home))?;
            let config_path = home.join("config.toml");
            op.step("config_initialize", || {
                private_create(&config_path, saved.config.as_bytes())
            })?;
            let runtime = op.step("runtime_config_read", || fs::read_to_string(&config_path))?;
            op.begin("runtime_config_validate");
            if !config::matches_runtime_config(&runtime, &saved.config) {
                tracing::error!(event = "config_mismatch", reason = "managed_settings_changed",
                changed_fields = ?config::runtime_differences(&runtime, &saved.config));
                return Err(Error::message(
                    "实例配置被手动修改；请导入为新版本后再启动，原文件已保留",
                ));
            }
            let binding = serde_json::to_vec(&HomeBinding {
                profile_id: id.into(),
                revision,
            })
            .map_err(|_| Error::message("无法保存实例绑定"))?;
            op.step("binding_write", || {
                private_write(&home.join("usage-binding.json"), &binding)
            })?;
            tx.execute(
                "INSERT OR IGNORE INTO homes VALUES (?1,?2,?3)",
                params![id, revision, home.to_string_lossy()],
            )?;
            op.step("database_commit", || tx.commit())?;
            Ok(home)
        })
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

    #[allow(clippy::too_many_arguments)]
    pub fn start_recorded_with_config(
        &mut self,
        selector: &str,
        cwd: &Path,
        revision: Option<i64>,
        resume: Option<&str>,
        program: &Path,
        args: &[String],
        usage_config: Option<&Path>,
    ) -> Result<crate::agents::ProcessHandle> {
        logging::operation("codex.run_start", |op| {
            // Sync completes before selecting a revision. Failures keep the last saved snapshot usable.
            let report = op.step("source_sync", || self.sync(false))?;
            if !report.skipped.is_empty()
                || !report.conflicts.is_empty()
                || !report.missing.is_empty()
            {
                eprintln!("{report}");
            }
            let profile = op.step("account_resolve", || self.resolve(selector))?;
            let revision = revision.unwrap_or(profile.revision);
            op.account(&profile.id, revision, profile.auth_mode.log_name());
            tracing::info!(event = "run_selected", resume = resume.is_some());
            let _oauth_lease = if profile.auth_mode == AuthMode::Oauth {
                Some(op.step("oauth_run_lease", || self.oauth_run_lease(&profile.id))?)
            } else {
                None
            };
            let home = op.step("home_materialize", || {
                self.materialize(&profile.id, revision)
            })?;
            let saved = op.step("revision_read", || self.revision(&profile.id, revision))?;
            let key = if profile.auth_mode == AuthMode::Byok {
                Some(op.step("credential_read", || {
                    secrets::get(&self.root, &saved.secret_ref)
                })?)
            } else {
                let auth = op.step("oauth_auth_read", || {
                    oauth::read_auth(&home.join("auth.json"))
                })?;
                if profile.oauth.as_ref().is_none_or(|o| {
                    o.account_id != auth.identity.account_id || o.user_id != auth.identity.user_id
                }) {
                    tracing::error!(event = "oauth_identity_mismatch");
                    return Err(Error::message("OAuth 登录文件与账号身份不匹配；请重新登录"));
                }
                None
            };
            op.begin("cwd_validate");
            let cwd = cwd
                .canonicalize()
                .map_err(|_| Error::message("项目目录不存在"))?;
            if !cwd.is_dir() {
                return Err(Error::message("项目路径不是目录"));
            }
            if let Some(id) = resume {
                uuid::Uuid::parse_str(id).map_err(|_| Error::message("无效的会话 ID"))?;
            }
            if profile.auth_mode == AuthMode::Oauth {
                op.step("arguments_validate", || oauth::validate_run_args(args))?;
                op.step("cli_version_detect", || oauth::verify_cli_version(program))?;
            } else {
                op.step("arguments_validate", || {
                    launcher::validate_run_args(args, profile.auth_mode)
                })?;
            }
            op.begin("cli_capability_detect");
            let mut help = Command::new(program);
            clear_auth_env(&mut help);
            help.arg("--help");
            let help = crate::credentials::output_with_timeout(help, Duration::from_secs(10))
            .ok_or_else(|| {
                Error::message(
                    "无法运行 Codex CLI 或能力检测超时；请先安装 codex 或使用 --binary 指定路径",
                )
            })?;
            if !help.status.success() {
                return Err(Error::message("Codex CLI 能力检测失败"));
            }
            let mut command = Command::new(program);
            clear_auth_env(&mut command);
            command.current_dir(&cwd).env("CODEX_HOME", &home);
            if let Some(key) = &key {
                command.env(config::KEY_ENV, key);
            }
            if profile.auth_mode == AuthMode::Oauth {
                if !String::from_utf8_lossy(&help.stdout).contains("--profile") {
                    return Err(Error::message(
                        "OAuth 配置版本需要支持独立 profile 文件的新版 Codex CLI；请升级 codex",
                    ));
                }
                command.args(["--profile", &format!("usage-r{revision}")]);
                command.args([
                    "-c",
                    "cli_auth_credentials_store=\"file\"",
                    "-c",
                    "forced_login_method=\"chatgpt\"",
                    "-c",
                    "model_provider=\"openai\"",
                ]);
                if let Some(identity) = &profile.oauth {
                    command.args([
                        "-c",
                        &format!(
                            "forced_chatgpt_workspace_id={}",
                            toml_edit::value(&identity.account_id)
                        ),
                    ]);
                }
            }
            if String::from_utf8_lossy(&help.stdout).contains("--no-daemon") {
                command.arg("--no-daemon");
            }
            let run_id = op.id().to_owned();
            let mut capture = op.step("run_identity_capture", || {
                let environment = |name: &str| std::env::var(name).ok();
                let effective = effective::resolve(&saved.config, profile.auth_mode, &cwd, resume.is_some(), args, key.as_deref(), &environment);
                if let (Some(env), Some(key)) = (&effective.credential_env, &effective.key) {
                    command.env(env, key);
                }
                let path = usage_config.map(Path::to_path_buf).unwrap_or_else(crate::config::default_config_path);
                let config = crate::config::load_config(&path, usage_config.is_some());
                let resources = std::cell::OnceCell::new();
                let captured = crate::runs::CapturedContext::capture(
                    &run_id, &profile.id, effective.identity, effective.key.as_deref(),
                    config.as_ref().map(|c| c.resources.as_slice()).map_err(|_| "启动时资源配置读取失败"),
                    &environment,
                    &|config| resources.get_or_init(|| crate::resources::connections::ResourceService::from_environment(crate::resources::builtin_registry())).prepare(config).ok().map(|reader| reader.scope()),
                );
                tracing::info!(event="run_identity_captured", state=?captured.context.identity.state,
                    reason=captured.context.identity.reason.as_deref(),
                    resource_count=captured.context.resources.len(),
                    matched_resources=captured.context.resources.iter().filter(|l| l.state == crate::runs::LinkState::Matched).count(),
                    resources_error=captured.context.resources_error.as_deref());
                for link in &captured.context.resources {
                    tracing::info!(event="run_resource_link", connection_id=link.binding.account,
                        service=link.service, revision=link.binding.revision, state=?link.state);
                }
                Ok::<_, Error>(captured)
            })?;
            op.step("notification_inject", || {
                bark::inject(
                    &mut command,
                    &self.root,
                    &home,
                    &profile.name,
                    &run_id,
                    &saved.config,
                )
            })?;
            op.step("runtime_events_inject", || {
                runtime::inject(
                    &mut command,
                    &self.root,
                    &home,
                    &run_id,
                    &saved.config,
                    String::from_utf8_lossy(&help.stdout)
                        .contains("--dangerously-bypass-hook-trust"),
                )
            })?;
            if let Some(id) = resume {
                command.args(["resume", id]);
            }
            command.args(args);
            #[cfg(unix)]
            if let Some(lease) = &_oauth_lease {
                launcher::inherit_run_lease(&mut command, lease)?;
            }
            let mut child = op.step("child_spawn", || command.spawn())?;
            tracing::info!(event = "child_spawned", child_pid = child.id());
            op.begin("run_record");
            let recorded = (|| -> Result<RunRecord> {
                capture.process = crate::runs::ProcessIdentity::capture(child.id());
                let capture = serde_json::to_string(&capture)
                    .map_err(|_| Error::message("运行身份无法序列化"))?;
                let tx = self.conn.transaction()?;
                tx.execute("INSERT INTO runs(id,profile_id,revision,cwd,started_at,pid) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![run_id,profile.id,revision,cwd.to_string_lossy(),now(),child.id()])?;
                tx.execute(
                    "INSERT INTO run_contexts(run_id,context_json) VALUES (?1,?2)",
                    params![run_id, capture],
                )?;
                runtime::record_started(&tx, &run_id)?;
                tx.commit()?;
                self.run_record(&run_id)?
                    .ok_or_else(|| Error::message("启动记录不存在"))
            })();
            let run = match recorded {
                Ok(run) => run,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
            let root = self.root.clone();
            let span = op.span();
            let finish = Box::new(move |status: ExitStatus| {
                // Keep the OAuth lease owned by this process handle until exit.
                let _lease = &_oauth_lease;
                span.in_scope(|| {
                    runtime::finish_run(&root, &home, &run_id, profile.auth_mode, status)
                })
                .map_err(|error| error.to_string())
            });
            Ok(crate::agents::ProcessHandle::new(
                child,
                Some(run),
                Some(finish),
            ))
        })
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

    pub async fn test_connection(&self, selector: &str) -> Result<u16> {
        let mut op = logging::Operation::new("codex.connection_test");
        let span = op.span();
        let result = logging::instrument(span, async {
            let profile = op.step("account_resolve", || self.resolve(selector))?;
            op.account(&profile.id, profile.revision, profile.auth_mode.log_name());
            if profile.auth_mode == AuthMode::Oauth {
                return Err(Error::message(
                    "Responses API 测试仅用于 BYOK；OAuth 请打开 Codex 验证登录",
                ));
            }
            let saved = op.step("revision_read", || {
                self.revision(&profile.id, profile.revision)
            })?;
            let key = op.step("credential_read", || {
                secrets::get(&self.root, &saved.secret_ref)
            })?;
            let url = format!("{}/responses", profile.base_url.trim_end_matches('/'));
            op.begin("http_request");
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| Error::message("无法初始化 HTTP 客户端"))?;
            let response = client
                .post(url)
                .bearer_auth(key)
                .json(&serde_json::json!({
                    "model": profile.model, "input": "Reply with OK.",
                    "max_output_tokens": 64, "stream": false
                }))
                .send()
                .await
                .map_err(|_| Error::message("Responses 测试请求失败（未记录请求内容或密钥）"))?;
            let status = response.status().as_u16();
            if (200..300).contains(&status) {
                tracing::info!(event = "http_response", status);
            } else {
                tracing::warn!(event = "http_response", status);
            }
            op.begin("http_response_validate");
            if (200..300).contains(&status) {
                let body: serde_json::Value = response
                    .json()
                    .await
                    .map_err(|_| Error::message("上游返回成功状态，但响应不是有效 JSON"))?;
                if body.get("object").and_then(serde_json::Value::as_str) != Some("response")
                    || !body.get("output").is_some_and(serde_json::Value::is_array)
                    || body.get("error").is_some_and(|e| !e.is_null())
                {
                    return Err(Error::message(
                        "上游响应不符合 Responses API 或模型执行失败",
                    ));
                }
            }
            Ok(status)
        })
        .await;
        op.finish(result.as_ref().is_ok_and(|s| (200..300).contains(s)));
        result
    }

    pub fn export(&self, selector: &str, target: &Path) -> Result<()> {
        let p = self.resolve(selector)?;
        let r = self.revision(&p.id, p.revision)?;
        // Output only Codex configuration with env_key; source blobs/auth are never exported.
        private_write(target, r.config.as_bytes())
    }

    pub fn instance_binding(&self, home: &Path) -> Result<Option<(String, i64)>> {
        if !home.starts_with(self.root.join("homes")) {
            return Ok(None);
        }
        let bytes = fs::read(home.join("usage-binding.json"))?;
        let b: HomeBinding =
            serde_json::from_slice(&bytes).map_err(|_| Error::message("实例绑定损坏"))?;
        if self.home(&b.profile_id, b.revision)? != home {
            return Err(Error::message("实例绑定与目录不匹配"));
        }
        let profile = self.resolve(&b.profile_id)?;
        let revision = if profile.auth_mode == AuthMode::Oauth {
            profile.revision
        } else {
            b.revision
        };
        Ok(Some((b.profile_id, revision)))
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.chars().any(char::is_control) {
        Err(Error::message("名称必须非空且不含控制字符"))
    } else {
        Ok(())
    }
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
fn source_digest(name: &str, config: &str, key: &str) -> String {
    let mut hash = Sha256::new();
    for part in [name, config, key] {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}
const CLEARED_AUTH_ENV: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "CODEX_HOME",
    "CODEX_API_KEY",
    "CODEX_AUTH_TOKEN",
    "CODEX_ACCESS_TOKEN",
    "CODEX_API_KEY_FILE",
    "CODEX_WIF_PROVIDER",
    "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
    "CODEX_REVOKE_TOKEN_URL_OVERRIDE",
    "CODEX_APP_SERVER_LOGIN_CLIENT_ID",
    config::KEY_ENV,
    bark::CONTEXT_ENV,
];
pub(crate) fn clear_auth_env(command: &mut Command) {
    for key in CLEARED_AUTH_ENV {
        command.env_remove(key);
    }
}
fn list_profiles(conn: &Connection, archived: bool) -> Result<Vec<Profile>> {
    struct SavedRow {
        id: String,
        name: String,
        revision: i64,
        config: String,
        source: Option<String>,
        follow: bool,
        status: String,
        secret_ref: String,
    }
    let raw: Vec<SavedRow> = conn.prepare(
        "SELECT p.id,p.name,p.revision,r.config,p.source_path,p.follow,p.source_status,r.secret_ref FROM profiles p
        JOIN revisions r ON p.id=r.profile_id AND p.revision=r.revision WHERE (?1 OR p.archived=0) ORDER BY p.name,p.id")?
        .query_map([archived], |r| Ok(SavedRow {
            id: r.get(0)?, name: r.get(1)?, revision: r.get(2)?, config: r.get(3)?,
            source: r.get(4)?, follow: r.get(5)?, status: r.get(6)?,
            secret_ref: r.get(7)?,
        }))?.collect::<std::result::Result<_, _>>()?;
    raw.into_iter()
        .map(|r| {
            let auth_mode = if r.secret_ref == "oauth" {
                AuthMode::Oauth
            } else {
                AuthMode::Byok
            };
            let (base_url, model, oauth) = if auth_mode == AuthMode::Oauth {
                let doc = config::parse(&r.config)?;
                let identity: String = conn.query_row(
                    "SELECT identity_json FROM oauth_accounts WHERE profile_id=?1",
                    [&r.id],
                    |row| row.get(0),
                )?;
                (
                    "ChatGPT 官方".into(),
                    doc.get("model")
                        .and_then(toml_edit::Item::as_str)
                        .unwrap_or("默认模型")
                        .into(),
                    Some(
                        serde_json::from_str(&identity)
                            .map_err(|_| Error::message("OAuth 账号信息损坏"))?,
                    ),
                )
            } else {
                let (url, model) = config::summary(&r.config)?;
                (url, model, None)
            };
            Ok(Profile {
                id: r.id,
                name: r.name,
                revision: r.revision,
                base_url,
                model,
                source: r.source,
                follow: r.follow,
                source_status: r.status,
                auth_mode,
                oauth,
            })
        })
        .collect()
}

pub fn saved_profiles() -> Result<Vec<Profile>> {
    let path = default_root().join("manager.sqlite3");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_secs(3))?;
    list_profiles(&conn, false)
}

pub fn managed_instances() -> Result<Vec<crate::config::DataInstance>> {
    let path = default_root().join("manager.sqlite3");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let rows: Vec<(String, i64, String)> = conn
        .prepare("SELECT profile_id,revision,path FROM homes ORDER BY profile_id,revision")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(id, revision, path)| crate::config::DataInstance {
            id: format!("managed-{id}-r{revision}"),
            provider: "codex".into(),
            path: path.into(),
        })
        .collect())
}

/// Resume resolves this Home's account; OAuth uses its current config, BYOK its original revision.
pub fn binding_for_home(home: &Path) -> Result<Option<(String, i64)>> {
    let root = default_root().join("homes");
    if !home.starts_with(&root) {
        return Ok(None);
    }
    let manager = Manager::open_default()?;
    manager.instance_binding(home)
}

pub fn with_usage_config(command: String, config: Option<&Path>) -> Result<String> {
    match config {
        Some(path) => {
            let path = std::path::absolute(path)?;
            Ok(format!(
                "env USAGE_CONFIG={} {command}",
                shell_quote(&path.to_string_lossy())
            ))
        }
        None => Ok(command),
    }
}

pub fn launch_shell_command(
    selector: &str,
    cwd: &Path,
    revision: Option<i64>,
    session: Option<&str>,
) -> Result<String> {
    let executable = std::env::current_exe()?;
    let mut args = vec![
        executable.to_string_lossy().to_string(),
        "codex".into(),
        "run".into(),
        selector.into(),
        "--cwd".into(),
        cwd.to_string_lossy().to_string(),
    ];
    if let Some(rev) = revision {
        args.extend(["--revision".into(), rev.to_string()]);
    }
    if let Some(id) = session {
        args.extend(["--resume".into(), id.into()]);
    }
    Ok(usage_shell_command(&args))
}

fn usage_shell_command(args: &[String]) -> String {
    #[cfg(not(target_os = "windows"))]
    {
        let mut command = format!(
            "env USAGE_CODEX_DIR={} ",
            shell_quote(&default_root().to_string_lossy())
        );
        command.push_str(
            &args
                .iter()
                .map(|s| shell_quote(s))
                .collect::<Vec<_>>()
                .join(" "),
        );
        command
    }
    #[cfg(target_os = "windows")]
    {
        format!(
            "$env:USAGE_CODEX_DIR={}; & {}",
            shell_quote(&default_root().to_string_lossy()),
            args.iter()
                .map(|s| shell_quote(s))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }
}
#[cfg(not(target_os = "windows"))]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
#[cfg(target_os = "windows")]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests;
