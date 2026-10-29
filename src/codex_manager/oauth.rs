//! One mutable credential Home per OAuth account; configuration revisions use native profiles.
//! Authentication and refresh are delegated to the installed official Codex CLI.
mod recovery;
use recovery::{SaveFiles, recover_home, recovery_needed};
#[cfg(test)]
use recovery::{pending_journals, saved_names};

use super::{
    AuthMode, Error, HomeBinding, Manager, Profile, Result, clear_auth_env, now, private_dir,
    private_write, validate_name,
};
use crate::logging;
use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use fs2::FileExt;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};
use toml_edit::{Item, value};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub email: String,
    pub account_id: String,
    pub user_id: String,
    pub organization_id: Option<String>,
    pub plan: Option<String>,
}
impl Identity {
    fn key(&self) -> String {
        json!([self.account_id, self.user_id]).to_string()
    }
    pub fn label(&self) -> String {
        format!(
            "{} · {}",
            self.email,
            self.plan.as_deref().unwrap_or("ChatGPT")
        )
    }
}

pub struct Auth {
    pub identity: Identity,
    bytes: Vec<u8>,
    pub(super) access_token: String,
}
fn jwt_claims(token: &str) -> Value {
    token
        .split('.')
        .nth(1)
        .and_then(|v| {
            URL_SAFE_NO_PAD
                .decode(v)
                .or_else(|_| URL_SAFE.decode(v))
                .ok()
        })
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or(Value::Null)
}
fn string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}
fn safe_identity(value: &str) -> Result<()> {
    if value.len() > 1024 || value.chars().any(char::is_control) {
        return Err(Error::message("OAuth 账号标识无效"));
    }
    Ok(())
}
/// JWT claims are display/identity hints, not a signature validation or a login check.
/// Only the official CLI can determine whether the imported grant is valid remotely.
fn parse_auth(bytes: &[u8]) -> Result<Auth> {
    let doc: Value = serde_json::from_slice(bytes)
        .map_err(|_| Error::message("登录文件不是有效 JSON（解析内容已隐藏）"))?;
    if doc.get("requires_reauth").and_then(Value::as_bool) == Some(true) {
        return Err(Error::message("来源账号需要重新登录，不能导入失效凭据"));
    }
    if string(&doc, "OPENAI_API_KEY").is_some()
        || string(&doc, "openai_api_key").is_some()
        || string(&doc, "auth_mode")
            .is_some_and(|v| !["chatgpt", "oauth"].contains(&v.to_lowercase().as_str()))
    {
        return Err(Error::message(
            "此文件不是 ChatGPT OAuth 登录；API Key 请使用 BYOK 添加",
        ));
    }
    let tokens = doc
        .get("tokens")
        .ok_or_else(|| Error::message("登录文件缺少 OAuth tokens"))?;
    let required = |key: &str| -> Result<String> {
        let token = string(tokens, key)
            .ok_or_else(|| Error::message(format!("OAuth 登录缺少 {key}；请重新登录")))?;
        if token.len() > 128 * 1024 || token.chars().any(char::is_control) {
            return Err(Error::message("OAuth token 格式无效"));
        }
        Ok(token)
    };
    let id_token = required("id_token")?;
    let access_token = required("access_token")?;
    let refresh_token = required("refresh_token")?;
    let id_claims = jwt_claims(&id_token);
    let access_claims = jwt_claims(&access_token);
    let id_auth = &id_claims["https://api.openai.com/auth"];
    let access_auth = &access_claims["https://api.openai.com/auth"];
    let identity = Identity {
        account_id: string(tokens, "account_id")
            .or_else(|| string(access_auth, "chatgpt_account_id"))
            .or_else(|| string(id_auth, "chatgpt_account_id"))
            .or_else(|| string(&doc, "account_id"))
            .ok_or_else(|| Error::message("登录文件缺少 ChatGPT account_id"))?,
        user_id: string(id_auth, "chatgpt_user_id")
            .or_else(|| string(access_auth, "chatgpt_user_id"))
            .or_else(|| string(&id_claims, "sub"))
            .or_else(|| string(&doc, "user_id"))
            .ok_or_else(|| Error::message("登录文件缺少用户标识"))?,
        email: string(&id_claims, "email")
            .or_else(|| string(&access_claims["https://api.openai.com/profile"], "email"))
            .or_else(|| string(&doc, "email"))
            .unwrap_or_else(|| "未提供邮箱".into()),
        organization_id: string(access_auth, "organization_id")
            .or_else(|| string(id_auth, "organization_id"))
            .or_else(|| string(&doc, "organization_id")),
        plan: string(access_auth, "chatgpt_plan_type")
            .or_else(|| string(id_auth, "chatgpt_plan_type"))
            .or_else(|| string(&doc, "plan_type")),
    };
    for text in [&identity.account_id, &identity.user_id, &identity.email]
        .into_iter()
        .chain(identity.organization_id.iter())
        .chain(identity.plan.iter())
    {
        safe_identity(text)?;
    }
    let last_refresh = doc
        .get("last_refresh")
        .and_then(Value::as_str)
        .filter(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
        .map(str::to_owned)
        .or_else(|| {
            doc.get("token_updated_at")
                .and_then(Value::as_i64)
                .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
                .map(|d| d.to_rfc3339())
        })
        .unwrap_or_else(now);
    let bytes = serde_json::to_vec_pretty(&json!({"auth_mode":"chatgpt", "OPENAI_API_KEY":null,
        "tokens":{"id_token":id_token,"access_token":access_token,"refresh_token":refresh_token,"account_id":identity.account_id},
        "last_refresh":last_refresh})).map_err(|_| Error::message("无法保存登录凭据"))?;
    Ok(Auth {
        identity,
        bytes,
        access_token,
    })
}

pub fn read_auth(path: &Path) -> Result<Auth> {
    logging::operation("codex.oauth_auth_read", |op| {
        op.begin("auth_file_read");
        let file = File::open(path).map_err(|_| {
            Error::message(
                "无法读取 OAuth 登录文件；请选择 auth.json / Cockpit 单账号文件，或重新登录",
            )
        })?;
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(Error::message("登录文件过大"));
        }
        op.step("auth_validate", || parse_auth(&bytes))
    })
}

pub fn normalize_config(text: &str) -> Result<String> {
    let mut doc = super::config::parse(text)?;
    if doc
        .get("model_provider")
        .and_then(Item::as_str)
        .is_some_and(|s| s != "openai")
    {
        return Err(Error::message(
            "OAuth 账号仅支持官方 openai 提供商；自定义 API 请使用 BYOK",
        ));
    }
    // These routes/auth settings cannot accompany an official account snapshot.
    for key in [
        "model_providers",
        "openai_base_url",
        "chatgpt_base_url",
        "profiles",
        "forced_chatgpt_workspace_id",
        "experimental_bearer_token",
        "api_key",
        "OPENAI_API_KEY",
        "auth",
        "tokens",
    ] {
        doc.remove(key);
    }
    for key in ["model", "model_reasoning_effort"] {
        if let Some(item) = doc.get(key) {
            let text = item
                .as_str()
                .ok_or_else(|| Error::message("模型和推理强度必须是字符串"))?;
            if text.trim().is_empty() || text.chars().any(char::is_control) {
                return Err(Error::message("模型或推理强度无效"));
            }
        }
    }
    doc["model_provider"] = value("openai");
    doc["cli_auth_credentials_store"] = value("file");
    doc["forced_login_method"] = value("chatgpt");
    Ok(doc.to_string())
}

pub fn patch_config(text: &str, model: Option<&str>, effort: Option<&str>) -> Result<String> {
    let mut doc = super::config::parse(&normalize_config(text)?)?;
    if let Some(model) = model {
        if model.is_empty() {
            doc.remove("model");
        } else {
            doc["model"] = value(model);
        }
    }
    if let Some(effort) = effort {
        if effort.is_empty() {
            doc.remove("model_reasoning_effort");
        } else {
            doc["model_reasoning_effort"] = value(effort);
        }
    }
    normalize_config(&doc.to_string())
}

pub(crate) fn verify_cli_version(program: &Path) -> Result<()> {
    let mut command = Command::new(program);
    clear_auth_env(&mut command);
    command.arg("--version");
    let output =
        crate::credentials::output_with_timeout(command, std::time::Duration::from_secs(10))
            .ok_or_else(|| {
                Error::message("无法检测 Codex CLI 版本；OAuth 需要 Codex CLI 0.134.0 或更新版本")
            })?;
    let version = String::from_utf8_lossy(&output.stdout);
    let supported = output.status.success() && version.split_whitespace().any(|part| {
        let mut parts = part.trim_start_matches('v').split('.');
        matches!((parts.next().and_then(|p| p.parse::<u64>().ok()),parts.next().and_then(|p| p.parse::<u64>().ok())),
            (Some(major),Some(minor)) if major > 0 || minor >= 134)
    });
    if !supported {
        return Err(Error::message(
            "OAuth 配置版本需要 Codex CLI 0.134.0 或更新版本；请升级 codex",
        ));
    }
    Ok(())
}

pub(crate) fn validate_run_args(args: &[String]) -> Result<()> {
    super::launcher::validate_run_args(args, AuthMode::Oauth)
}

pub(crate) fn restrict_auth_permissions(home: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = home.join("auth.json");
        if path.exists() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

struct Stage(PathBuf);
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Manager {
    pub(super) fn recover_oauth_saves(&self) -> Result<()> {
        let homes = self.root.join("homes");
        let entries = match fs::read_dir(&homes) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if uuid::Uuid::parse_str(&id).is_err() {
                continue;
            }
            let home = entry.path().join("r1");
            let needed = match recovery_needed(&home) {
                Ok(needed) => needed,
                Err(_) => {
                    tracing::error!(event = "oauth_recovery_blocked", profile_id = %id, stage = "journal_discover");
                    continue;
                }
            };
            if !needed {
                continue;
            }
            // A damaged account stays fenced at its credential lease. Other accounts
            // and provider inventory remain available; startup never uses its credentials.
            match acquire_lease(&self.root, &id, true) {
                Ok(_lease) => {
                    if recover_home(&self.conn, &home).is_err() {
                        tracing::error!(event = "oauth_recovery_blocked", profile_id = %id, stage = "account_recover");
                    }
                }
                Err(_) => {
                    tracing::warn!(event = "oauth_recovery_deferred", profile_id = %id, stage = "lease_acquire")
                }
            }
        }
        Ok(())
    }
}

/// Own the login subprocess and, for task-based login, its private process group.
struct LoginChild {
    child: std::process::Child,
    #[cfg(unix)]
    grouped: bool,
}
impl LoginChild {
    fn stop(&mut self) {
        #[cfg(unix)]
        if self.grouped {
            // The child is unreaped and was spawned into this new process group.
            unsafe {
                libc::killpg(self.child.id() as i32, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for LoginChild {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            self.stop();
        }
    }
}

fn acquire_lease(root: &Path, id: &str, exclusive: bool) -> Result<File> {
    logging::operation("codex.oauth_lease", |op| {
        uuid::Uuid::parse_str(id).map_err(|_| Error::message("无效的账号 ID"))?;
        op.account(id, 0, "oauth");
        tracing::debug!(event = "lease_requested", exclusive);
        op.begin("lease_file_prepare");
        let dir = root.join("locks");
        private_dir(&dir)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(dir.join(format!("{id}.lock")))?;
        op.begin("lease_acquire");
        let result = if exclusive {
            FileExt::try_lock_exclusive(&file)
        } else {
            FileExt::try_lock_shared(&file)
        };
        result.map_err(|error| {
        tracing::warn!(event = "lease_failed", exclusive, busy = error.kind() == std::io::ErrorKind::WouldBlock, error_kind = ?error.kind());
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Error::from(error);
        }
        Error::message(if exclusive {
            "此 OAuth 账号正在运行或更新登录；关闭该账号的所有实例后才能替换登录"
        } else {
            "此 OAuth 账号正在更新登录；完成后可继续打开 Codex"
        })
    })?;
        tracing::debug!(event = "lease_acquired", exclusive);
        Ok(file)
    })
}

impl Manager {
    /// Credential replacement excludes all runs. Refresh remains owned by Codex itself.
    pub(crate) fn oauth_lease(&self, id: &str) -> Result<File> {
        let lease = acquire_lease(&self.root, id, true)?;
        recover_home(&self.conn, &self.root.join("homes").join(id).join("r1"))?;
        Ok(lease)
    }

    /// Runs share the same account Home and may coexist; only login replacement is blocked.
    pub(crate) fn oauth_run_lease(&self, id: &str) -> Result<File> {
        let home = self.root.join("homes").join(id).join("r1");
        if recovery_needed(&home)? {
            let _lease = acquire_lease(&self.root, id, true)?;
            recover_home(&self.conn, &home)?;
        }
        let lease = acquire_lease(&self.root, id, false)?;
        if recovery_needed(&home)? {
            return Err(Error::message("OAuth 登录需要恢复，请重试"));
        }
        Ok(lease)
    }

    pub fn import_oauth(
        &mut self,
        name: Option<&str>,
        text: Option<&str>,
        source: &Path,
        account: Option<&str>,
    ) -> Result<Profile> {
        logging::operation("codex.oauth_import", |op| {
            op.begin("source_validate");
            let source = source
                .canonicalize()
                .map_err(|_| Error::message("无法读取 OAuth 登录文件"))?;
            if source.starts_with(self.root.join("homes"))
                || source.starts_with(self.root.join("login-staging"))
            {
                return Err(Error::message(
                    "托管账号不能作为外部导入来源；请使用重新登录功能",
                ));
            }
            let auth = op.step("auth_read", || read_auth(&source))?;
            op.step("account_save", || {
                self.save_oauth(name, text, auth, account, None)
            })
        })
    }

    fn save_oauth(
        &mut self,
        name: Option<&str>,
        text: Option<&str>,
        auth: Auth,
        account: Option<&str>,
        reserved_lease: Option<File>,
    ) -> Result<Profile> {
        logging::operation("codex.oauth_save", |op| {
            op.begin("config_validate");
            if let Some(name) = name {
                validate_name(name)?;
            }
            let text = text.map(normalize_config).transpose()?;
            // SQLite serializes identity deduplication, before choosing the account lease.
            op.begin("database_transaction");
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<String> = tx
                .query_row(
                    "SELECT profile_id FROM oauth_accounts WHERE identity_key=?1",
                    [auth.identity.key()],
                    |r| r.get(0),
                )
                .optional()?;
            let requested = if let Some(selector) = account {
                // Resolve using this transaction's connection; never accept another account's login.
                let matches: Vec<_> = super::list_profiles(&tx, true)?
                    .into_iter()
                    .filter(|p| p.id == selector || p.name == selector)
                    .collect();
                if matches.len() != 1 {
                    return Err(Error::message(
                        "重新登录目标不存在或名称重复；请使用账号 ID",
                    ));
                }
                let p = &matches[0];
                if p.auth_mode != AuthMode::Oauth
                    || p.oauth.as_ref().map(Identity::key) != Some(auth.identity.key())
                {
                    tracing::error!(
                        event = "oauth_identity_mismatch",
                        reason = "replacement_identity_changed"
                    );
                    return Err(Error::message(
                        "登录身份与所选 OAuth 账号不一致；原登录已保留，请作为新账号添加",
                    ));
                }
                Some(p.id.clone())
            } else {
                None
            };
            let id = requested
                .or(existing)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            op.account(&id, 0, "oauth");
            let _lease = match reserved_lease {
                Some(file) => file,
                None => acquire_lease(&self.root, &id, true)?,
            };
            recover_home(&tx, &self.root.join("homes").join(&id).join("r1"))?;
            let old: Option<(String, i64)> = tx
                .query_row(
                    "SELECT name,revision FROM profiles WHERE id=?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let chosen_name = name.unwrap_or_else(|| {
                old.as_ref()
                    .map(|v| v.0.as_str())
                    .unwrap_or(&auth.identity.email)
            });
            validate_name(chosen_name)?;
            if let Some((_, revision)) = &old {
                let next = revision + i64::from(text.is_some());
                if let Some(text) = &text {
                    tx.execute(
                        "INSERT INTO revisions VALUES (?1,?2,?3,'oauth',?4)",
                        params![id, next, text, now()],
                    )?;
                }
                tx.execute("UPDATE profiles SET name=?1,revision=?2,archived=0,source_status='已保存 OAuth 登录' WHERE id=?3", params![chosen_name,next,id])?;
            } else {
                tx.execute("INSERT INTO profiles(id,name,revision,source_status) VALUES (?1,?2,1,'已保存 OAuth 登录')", params![id,chosen_name])?;
                tx.execute(
                    "INSERT INTO revisions VALUES (?1,1,?2,'oauth',?3)",
                    params![id, text.unwrap_or(normalize_config("")?), now()],
                )?;
            }
            tx.execute("INSERT INTO oauth_accounts VALUES (?1,?2,?3) ON CONFLICT(profile_id) DO UPDATE SET identity_key=excluded.identity_key,identity_json=excluded.identity_json",
            params![id,auth.identity.key(),serde_json::to_string(&auth.identity).map_err(|_| Error::message("无法保存账号信息"))?])?;
            // Resolve the return value before committing. No fallible lookup or
            // materialization may report failure after the new login is committed.
            let p = super::list_profiles(&tx, true)?
                .into_iter()
                .find(|profile| profile.id == id)
                .ok_or_else(|| Error::message("OAuth 账号保存记录不存在"))?;
            let saved: String = tx.query_row(
                "SELECT config FROM revisions WHERE profile_id=?1 AND revision=?2",
                params![id, p.revision],
                |row| row.get(0),
            )?;
            let home = self.root.join("homes").join(&id).join("r1");
            private_dir(&home)?;
            let files = SaveFiles::capture(&home, p.revision)?;
            #[cfg(test)]
            save_checkpoint("captured");
            tx.execute(
                "INSERT INTO oauth_save_commits(operation) VALUES(?1)",
                [files.operation()],
            )?;
            let result = (|| -> Result<()> {
                op.step("home_materialize", || {
                    materialize_home(&tx, &home, &id, p.revision, &saved)
                })?;
                op.step("auth_write", || {
                    private_write(&home.join("auth.json"), &auth.bytes)
                })?;
                #[cfg(test)]
                save_checkpoint("files_written");
                op.step("database_commit", || tx.commit())?;
                #[cfg(test)]
                save_checkpoint("committed");
                Ok(())
            })();
            if let Err(error) = result {
                if let Err(recovery) = files.rollback() {
                    tracing::error!(event = "oauth_save_restore_failed");
                    return Err(Error::message(format!("{error}；{recovery}")));
                }
                return Err(error);
            }
            let operation = files.operation().to_string();
            if files.commit() {
                let _ = self.conn.execute(
                    "DELETE FROM oauth_save_commits WHERE operation=?1",
                    [&operation],
                );
            }
            op.account(&id, p.revision, "oauth");
            Ok(p)
        })
    }

    pub fn login_oauth(
        &mut self,
        name: Option<&str>,
        text: Option<&str>,
        device_auth: bool,
        program: &Path,
        account: Option<&str>,
    ) -> Result<Profile> {
        self.login_oauth_inner(name, text, device_auth, program, account, None)
    }

    pub(crate) fn login_oauth_controlled(
        &mut self,
        name: Option<&str>,
        text: Option<&str>,
        device_auth: bool,
        program: &Path,
        account: Option<&str>,
        control: &crate::auth::AuthControl,
    ) -> Result<Profile> {
        self.login_oauth_inner(name, text, device_auth, program, account, Some(control))
    }

    fn login_oauth_inner(
        &mut self,
        name: Option<&str>,
        text: Option<&str>,
        device_auth: bool,
        program: &Path,
        account: Option<&str>,
        control: Option<&crate::auth::AuthControl>,
    ) -> Result<Profile> {
        logging::operation("codex.oauth_login", |op| {
            op.begin("input_validate");
            if let Some(name) = name {
                validate_name(name)?;
            }
            let config = normalize_config(text.unwrap_or(""))?;
            op.step("cli_version_detect", || verify_cli_version(program))?;
            if let Some(control) = control {
                control
                    .checkpoint()
                    .map_err(|error| Error::message(error.to_string()))?;
            }
            // Check busy state before asking the user to sign in; reserve this account throughout login.
            let target = account.map(|s| self.resolve(s)).transpose()?;
            if target
                .as_ref()
                .is_some_and(|p| p.auth_mode != AuthMode::Oauth)
            {
                return Err(Error::message(
                    "BYOK 账号不能通过 OAuth 重新登录；请添加新账号",
                ));
            }
            if let Some(p) = &target {
                op.account(&p.id, p.revision, "oauth");
            }
            op.begin("lease_acquire");
            let lease = target
                .as_ref()
                .map(|p| self.oauth_lease(&p.id))
                .transpose()?;
            op.begin("login_staging_prepare");
            let stages = self.root.join("login-staging");
            private_dir(&stages)?;
            let stage = Stage(stages.join(uuid::Uuid::new_v4().to_string()));
            private_dir(&stage.0)?;
            private_write(&stage.0.join("config.toml"), config.as_bytes())?;
            let mut cmd = Command::new(program);
            clear_auth_env(&mut cmd);
            cmd.env("CODEX_HOME", &stage.0).args(["login"]);
            if device_auth {
                cmd.arg("--device-auth");
            }
            #[cfg(unix)]
            let grouped = {
                use std::io::IsTerminal;
                // An interactive child must remain in the foreground group to read the TTY.
                control.is_some() && !std::io::stdin().is_terminal()
            };
            #[cfg(unix)]
            if grouped {
                use std::os::unix::process::CommandExt;
                cmd.process_group(0);
            }
            op.begin("native_login");
            if let Some(control) = control {
                control
                    .waiting()
                    .map_err(|error| Error::message(error.to_string()))?;
            }
            let mut child = LoginChild {
                child: cmd.spawn().map_err(|_| {
                    Error::message("无法启动 Codex 登录；请安装 codex 或使用 --binary 指定路径")
                })?,
                #[cfg(unix)]
                grouped,
            };
            let status = loop {
                if control.is_some_and(|control| control.checkpoint().is_err()) {
                    child.stop();
                    tracing::info!(event = "login_cancelled");
                    return Err(Error::message("登录已取消"));
                }
                if let Some(status) = child.child.try_wait()? {
                    break status;
                }
                std::thread::sleep(std::time::Duration::from_millis(40));
            };
            tracing::info!(
                event = "login_child_exited",
                successful = status.success(),
                exit_code = status.code()
            );
            if !status.success() {
                #[cfg(unix)]
                if let Some(control) = control {
                    use std::os::unix::process::ExitStatusExt;
                    if status.signal() == Some(libc::SIGINT) {
                        control.cancel();
                    }
                }
                return Err(Error::message("Codex 登录未完成；已有账号及登录保持原样"));
            }
            let auth = op.step("auth_read", || read_auth(&stage.0.join("auth.json")))?;
            if let Some(control) = control {
                control
                    .begin_save()
                    .map_err(|error| Error::message(error.to_string()))?;
            }
            op.step("account_save", || {
                self.save_oauth(name, text, auth, account, lease)
            })
        })
    }

    pub(crate) fn materialize_oauth(&self, id: &str, revision: i64) -> Result<PathBuf> {
        let _lease = self.oauth_run_lease(id)?;
        let saved = self.revision(id, revision)?;
        let home = self.home(id, revision)?;
        materialize_home(&self.conn, &home, id, revision, &saved.config)?;
        super::context::prepare_config(
            &self.root,
            &home,
            &home.join(format!("usage-r{revision}.config.toml")),
            &saved.config,
        )?;
        Ok(home)
    }
}

fn materialize_home(
    conn: &rusqlite::Connection,
    home: &Path,
    id: &str,
    revision: i64,
    saved: &str,
) -> Result<()> {
    logging::operation("codex.oauth_home_materialize", |op| {
        op.account(id, revision, "oauth");
        op.step("home_prepare", || private_dir(home))?;
        let profile = home.join(format!("usage-r{revision}.config.toml"));
        op.begin("runtime_config_validate");
        if profile.exists() {
            let runtime = fs::read_to_string(&profile)?;
            if !super::config::matches_runtime_config(&runtime, saved) {
                tracing::error!(event = "config_mismatch", reason = "oauth_profile_changed",
                        changed_fields = ?super::config::runtime_differences(&runtime, saved));
                return Err(Error::message(
                    "OAuth 配置版本被手动修改；请通过编辑保存新版本",
                ));
            }
        } else {
            op.step("profile_write", || {
                private_write(&profile, saved.as_bytes())
            })?;
        }
        op.begin("binding_write");
        // Codex may legitimately add project trust entries here; never overwrite its mutable base.
        super::private_create(&home.join("config.toml"), normalize_config("")?.as_bytes())?;
        private_write(
            &home.join("usage-binding.json"),
            &serde_json::to_vec(&HomeBinding {
                profile_id: id.into(),
                revision: 1,
            })
            .map_err(|_| Error::message("无法保存账号绑定"))?,
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO homes VALUES (?1,1,?2)",
            params![id, home.to_string_lossy()],
        )?;
        Ok(())
    })
}

#[cfg(test)]
fn save_checkpoint(phase: &str) {
    if std::env::var("USAGE_TEST_OAUTH_CRASH_PHASE").as_deref() == Ok(phase) {
        // Only the dedicated test subprocess sets this; skip every Rust destructor.
        std::process::exit(86);
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::codex_manager::tests::{Fixture, oauth_source};

    #[test]
    fn crash_child() {
        let Ok(root) = std::env::var("USAGE_TEST_OAUTH_CRASH_ROOT") else {
            return;
        };
        let mut manager = Manager::open(root.into()).unwrap();
        if std::env::var("USAGE_TEST_OAUTH_CRASH_PHASE").as_deref() == Ok("restoring") {
            panic!("recovery did not reach checkpoint");
        }
        let source = PathBuf::from(std::env::var("USAGE_TEST_OAUTH_CRASH_SOURCE").unwrap());
        let account = std::env::var("USAGE_TEST_OAUTH_CRASH_ACCOUNT").ok();
        manager
            .import_oauth(
                Some("Replacement"),
                Some("model='replacement-model'"),
                &source,
                account.as_deref(),
            )
            .unwrap();
        panic!("save did not reach checkpoint");
    }
    fn crash(root: &Path, source: &Path, account: Option<&str>, phase: &str) {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "codex_manager::oauth::recovery_tests::crash_child",
                "--nocapture",
            ])
            .env("USAGE_TEST_OAUTH_CRASH_ROOT", root)
            .env("USAGE_TEST_OAUTH_CRASH_SOURCE", source)
            .env("USAGE_TEST_OAUTH_CRASH_PHASE", phase)
            .env_remove("USAGE_TEST_OAUTH_CRASH_ACCOUNT")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(account) = account {
            command.env("USAGE_TEST_OAUTH_CRASH_ACCOUNT", account);
        }
        assert_eq!(command.status().unwrap().code(), Some(86), "{phase}");
    }
    #[test]
    fn interrupted_saves_and_interrupted_recovery_restore_one_consistent_revision() {
        for phase in [
            "capturing",
            "captured",
            "files_written",
            "committed",
            "cleaning",
        ] {
            let fixture = Fixture::new();
            let mut manager = fixture.manager();
            let source = oauth_source(&fixture, "crash", "original-refresh");
            let profile = manager
                .import_oauth(Some("Original"), None, &source, None)
                .unwrap();
            let home = manager.home(&profile.id, 1).unwrap();
            let old: Vec<_> = saved_names(1)
                .iter()
                .map(|name| (name.clone(), fs::read(home.join(name)).unwrap()))
                .collect();
            let source = oauth_source(&fixture, "crash", "replacement-refresh");
            crash(manager.root(), &source, Some(&profile.id), phase);
            if phase == "files_written" {
                // The first backup is restored, then recovery itself is interrupted.
                crash(manager.root(), &source, Some(&profile.id), "restoring");
            }
            // Exercise an already-open manager as well as startup recovery.
            let lease = manager.oauth_run_lease(&profile.id).unwrap();
            drop(lease);
            let recovered = Manager::open(manager.root().to_path_buf()).unwrap();
            let current = recovered.resolve(&profile.id).unwrap();
            if matches!(phase, "committed" | "cleaning") {
                assert_eq!(current.name, "Replacement");
                assert_eq!(current.revision, 2);
                assert!(
                    String::from_utf8(fs::read(home.join("auth.json")).unwrap())
                        .unwrap()
                        .contains("replacement-refresh")
                );
                assert!(home.join("usage-r2.config.toml").is_file());
            } else {
                assert_eq!(current.name, "Original");
                assert_eq!(current.revision, 1);
                for (name, bytes) in &old {
                    assert_eq!(
                        &fs::read(home.join(name)).unwrap(),
                        bytes,
                        "{phase}: {name}"
                    );
                }
                assert!(!home.join("usage-r2.config.toml").exists());
            }
            assert!(pending_journals(&home).unwrap().is_empty());
            assert_eq!(
                recovered
                    .conn
                    .query_row("SELECT COUNT(*) FROM oauth_save_commits", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }
    #[test]
    fn interrupted_new_account_removes_uncommitted_files_and_retains_committed_login() {
        for phase in ["capturing", "files_written", "committed"] {
            let fixture = Fixture::new();
            let manager = fixture.manager();
            let source = oauth_source(&fixture, "new-crash", "new-refresh");
            crash(manager.root(), &source, None, phase);
            let recovered = Manager::open(manager.root().to_path_buf()).unwrap();
            assert_eq!(
                recovered.list().unwrap().len(),
                usize::from(phase == "committed")
            );
            for entry in fs::read_dir(manager.root().join("homes")).unwrap() {
                let home = entry.unwrap().path().join("r1");
                assert_eq!(home.join("auth.json").exists(), phase == "committed");
                assert!(pending_journals(&home).unwrap().is_empty());
            }
        }
    }
    #[test]
    fn corrupt_backup_fails_closed_without_partially_restoring_files() {
        let fixture = Fixture::new();
        let mut manager = fixture.manager();
        let source = oauth_source(&fixture, "corrupt", "original");
        let profile = manager.import_oauth(None, None, &source, None).unwrap();
        let home = manager.home(&profile.id, 1).unwrap();
        let source = oauth_source(&fixture, "corrupt", "replacement");
        crash(manager.root(), &source, Some(&profile.id), "files_written");
        let journal = pending_journals(&home).unwrap().remove(0);
        fs::write(journal.join("config.toml"), "corrupted").unwrap();
        let auth = fs::read(home.join("auth.json")).unwrap();
        assert!(manager.oauth_run_lease(&profile.id).is_err());
        let mut reopened = Manager::open(manager.root().to_path_buf()).unwrap();
        assert!(reopened.oauth_run_lease(&profile.id).is_err());
        assert!(reopened.model_catalog_query(&profile).is_err());
        assert!(reopened.materialize(&profile.id, profile.revision).is_err());
        let healthy_source = oauth_source(&fixture, "healthy", "healthy-refresh");
        let healthy = reopened
            .import_oauth(None, None, &healthy_source, None)
            .unwrap();
        assert!(reopened.oauth_run_lease(&healthy.id).is_ok());
        assert_eq!(reopened.list().unwrap().len(), 2);
        assert_eq!(fs::read(home.join("auth.json")).unwrap(), auth);
        assert!(journal.exists());
    }
    #[test]
    fn legacy_recovery_without_a_commit_manifest_is_preserved() {
        let fixture = Fixture::new();
        let mut manager = fixture.manager();
        let source = oauth_source(&fixture, "legacy-recovery", "original");
        let profile = manager.import_oauth(None, None, &source, None).unwrap();
        let home = manager.home(&profile.id, 1).unwrap();
        let journal = home.join(format!(".usage-oauth-recovery-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&journal).unwrap();
        fs::write(journal.join("auth.json"), "legacy-backup").unwrap();
        assert!(manager.oauth_run_lease(&profile.id).is_err());
        assert_eq!(
            fs::read_to_string(journal.join("auth.json")).unwrap(),
            "legacy-backup"
        );
    }
    #[test]
    fn an_active_save_is_not_recovered_and_readers_wait_for_its_lease() {
        let fixture = Fixture::new();
        let mut manager = fixture.manager();
        let source = oauth_source(&fixture, "active-save", "original");
        let profile = manager.import_oauth(None, None, &source, None).unwrap();
        let home = manager.home(&profile.id, 1).unwrap();
        let lease = manager.oauth_lease(&profile.id).unwrap();
        let journal = SaveFiles::capture(&home, 1).unwrap();
        let reader = Manager::open(manager.root().to_path_buf()).unwrap();
        assert_eq!(pending_journals(&home).unwrap().len(), 1);
        assert!(reader.oauth_run_lease(&profile.id).is_err());
        journal.rollback().unwrap();
        drop(lease);
        assert!(reader.oauth_run_lease(&profile.id).is_ok());
        assert!(pending_journals(&home).unwrap().is_empty());
    }
    #[test]
    fn recovery_diagnostics_identify_failed_stage_without_logging_credential_contents() {
        // Isolate tracing callsite state from other tests' scoped subscribers.
        if std::env::var_os("USAGE_TEST_RECOVERY_LOG_CAPTURE").is_none() {
            let output = Command::new(std::env::current_exe().unwrap()).args(["--exact", "codex_manager::oauth::recovery_tests::recovery_diagnostics_identify_failed_stage_without_logging_credential_contents", "--nocapture"])
                .env("USAGE_TEST_RECOVERY_LOG_CAPTURE", "1").output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use tracing_subscriber::prelude::*;
        let fixture = Fixture::new();
        let mut manager = fixture.manager();
        let source = oauth_source(&fixture, "log-fixture", "private-original-refresh");
        let profile = manager.import_oauth(None, None, &source, None).unwrap();
        let home = manager.home(&profile.id, 1).unwrap();
        let source = oauth_source(&fixture, "log-fixture", "private-new-refresh");
        crash(manager.root(), &source, Some(&profile.id), "files_written");
        let journal = pending_journals(&home).unwrap().remove(0);
        fs::write(journal.join("config.toml"), "corrupted-private-config").unwrap();
        let log = fixture.path.join("diagnostics.jsonl");
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::sync::Mutex::new(File::create(&log).unwrap())),
        );
        tracing::subscriber::with_default(subscriber, || {
            assert!(manager.oauth_run_lease(&profile.id).is_err());
        });
        let text = fs::read_to_string(log).unwrap();
        assert!(text.contains("codex.oauth_recovery"));
        assert!(text.contains("backup_validate"));
        assert!(text.contains("operation_failed"));
        assert!(text.contains(&profile.id));
        for private in [
            "private-original-refresh",
            "private-new-refresh",
            "corrupted-private-config",
            "fixture@example.test",
            "log-fixture",
        ] {
            assert!(!text.contains(private), "diagnostics included {private}");
        }
        assert!(journal.exists());
    }
}
