//! One mutable credential Home per OAuth account; configuration revisions use native profiles.
//! Authentication and refresh are delegated to the installed official Codex CLI.
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
    Ok(Auth { identity, bytes })
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
        acquire_lease(&self.root, id, true)
    }

    /// Runs share the same account Home and may coexist; only login replacement is blocked.
    pub(crate) fn oauth_run_lease(&self, id: &str) -> Result<File> {
        acquire_lease(&self.root, id, false)
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
            let home = self.root.join("homes").join(&id).join("r1");
            private_dir(&home)?;
            op.step("auth_write", || {
                private_write(&home.join("auth.json"), &auth.bytes)
            })?;
            op.step("database_commit", || tx.commit())?;
            let p = self.resolve(&id)?;
            op.account(&id, p.revision, "oauth");
            op.step("home_materialize", || {
                self.materialize_oauth(&id, p.revision)
            })?;
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
        logging::operation("codex.oauth_login", |op| {
            op.begin("input_validate");
            if let Some(name) = name {
                validate_name(name)?;
            }
            let config = normalize_config(text.unwrap_or(""))?;
            op.step("cli_version_detect", || verify_cli_version(program))?;
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
            op.begin("native_login");
            let status = cmd.status().map_err(|_| {
                Error::message("无法启动 Codex 登录；请安装 codex 或使用 --binary 指定路径")
            })?;
            tracing::info!(
                event = "login_child_exited",
                successful = status.success(),
                exit_code = status.code()
            );
            if !status.success() {
                return Err(Error::message("Codex 登录未完成；已有账号及登录保持原样"));
            }
            let auth = op.step("auth_read", || read_auth(&stage.0.join("auth.json")))?;
            op.step("account_save", || {
                self.save_oauth(name, text, auth, account, lease)
            })
        })
    }

    pub(crate) fn materialize_oauth(&self, id: &str, revision: i64) -> Result<PathBuf> {
        logging::operation("codex.oauth_home_materialize", |op| {
            op.account(id, revision, "oauth");
            let saved = op.step("revision_read", || self.revision(id, revision))?;
            let home = self.home(id, revision)?;
            op.step("home_prepare", || private_dir(&home))?;
            let profile = home.join(format!("usage-r{revision}.config.toml"));
            op.begin("runtime_config_validate");
            if profile.exists() {
                if fs::read_to_string(&profile)? != saved.config {
                    tracing::error!(event = "config_mismatch", reason = "oauth_profile_changed");
                    return Err(Error::message(
                        "OAuth 配置版本被手动修改；请通过编辑保存新版本",
                    ));
                }
            } else {
                op.step("profile_write", || {
                    private_write(&profile, saved.config.as_bytes())
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
            self.conn.execute(
                "INSERT OR IGNORE INTO homes VALUES (?1,1,?2)",
                params![id, home.to_string_lossy()],
            )?;
            Ok(home)
        })
    }
}
