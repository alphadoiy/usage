//! Read-only adapter. Never call CC Switch's constructor/migrations or copy a live WAL database.
use super::{Error, Result, config};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::{Path, PathBuf};
use toml_edit::Item;

pub struct Candidate {
    pub provider_id: String,
    pub name: String,
    pub config: String,
    pub key: String,
}
pub struct ReadResult {
    pub path: PathBuf,
    pub candidates: Vec<Candidate>,
    pub skipped: Vec<(String, String)>,
    pub provider_ids: Vec<String>,
}

pub fn default_path() -> PathBuf {
    crate::config::home_dir().join(".cc-switch/cc-switch.db")
}

pub fn read(path: &Path) -> Result<ReadResult> {
    read_with_hook(path, || Ok(()))
}

/// The hook permits deterministic concurrent-commit tests; normal reads use a no-op.
pub(super) fn read_with_hook(
    path: &Path,
    after_common: impl FnOnce() -> Result<()>,
) -> Result<ReadResult> {
    let path = if path.is_dir() {
        path.join("cc-switch.db")
    } else {
        path.to_path_buf()
    };
    let path = path
        .canonicalize()
        .map_err(|_| Error::message("找不到 CC Switch 数据库；可使用 --source 指定自定义目录"))?;
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    // A single read transaction includes committed WAL contents and gives all rows one snapshot.
    conn.execute_batch("BEGIN; PRAGMA query_only = ON;")?;
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(providers)")?
        .query_map([], |r| r.get(1))?
        .collect::<std::result::Result<_, _>>()?;
    for required in ["id", "app_type", "name", "settings_config"] {
        if !columns.iter().any(|c| c == required) {
            return Err(Error::message(
                "不支持的 CC Switch 数据库结构；源数据库未修改",
            ));
        }
    }
    let common: Option<String> = if conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='settings'",
        [],
        |r| r.get::<_, i64>(0),
    )? > 0
    {
        use rusqlite::OptionalExtension;
        conn.query_row(
            "SELECT value FROM settings WHERE key='common_config_codex'",
            [],
            |r| r.get(0),
        )
        .optional()?
    } else {
        None
    };
    after_common()?;
    let meta = if columns.iter().any(|c| c == "meta") {
        "meta"
    } else {
        "'{}'"
    };
    let sql = format!(
        "SELECT id, name, settings_config, {meta} FROM providers WHERE app_type='codex' ORDER BY id"
    );
    let rows: Vec<(String, String, String, String)> = conn
        .prepare(&sql)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let mut result = ReadResult {
        path,
        candidates: Vec::new(),
        skipped: Vec::new(),
        provider_ids: Vec::new(),
    };
    for (id, name, raw, meta) in rows {
        result.provider_ids.push(id.clone());
        if name.trim().is_empty() || name.chars().any(char::is_control) {
            result.skipped.push((id, "供应商名称无效".into()));
            continue;
        }
        match convert(&raw, &meta, common.as_deref()) {
            Ok((text, key)) => result.candidates.push(Candidate {
                provider_id: id,
                name,
                config: text,
                key,
            }),
            Err(e) => {
                let reason = match e.to_string().as_str() {
                    "没有可导入的 API Key（可能是官方登录或环境变量认证）" => {
                        "missing_api_key"
                    }
                    "此供应商绑定订阅账号，首版仅导入 BYOK" => {
                        "managed_oauth_account"
                    }
                    "此供应商依赖 CC Switch 协议转换路由，不能直接启动" => {
                        "protocol_conversion_required"
                    }
                    "此配置是 CC Switch 代理接管配置，需要原始供应商连接信息" => {
                        "proxy_placeholder"
                    }
                    _ => "invalid_provider_config",
                };
                tracing::warn!(event = "source_provider_skipped", reason);
                result.skipped.push((name, e.to_string()));
            }
        }
    }
    Ok(result)
}

fn convert(raw: &str, meta: &str, common: Option<&str>) -> Result<(String, String)> {
    let settings: Value =
        serde_json::from_str(raw).map_err(|_| Error::message("供应商 JSON 无法解析"))?;
    let meta: Value =
        serde_json::from_str(meta).map_err(|_| Error::message("供应商元信息无法解析"))?;
    let format = meta
        .get("apiFormat")
        .or_else(|| settings.get("apiFormat"))
        .or_else(|| settings.get("api_format"));
    if format
        .and_then(Value::as_str)
        .is_some_and(|v| v != "openai_responses" && v != "responses")
        || settings.get("codexRouting").is_some()
    {
        return Err(Error::message(
            "此供应商依赖 CC Switch 协议转换路由，不能直接启动",
        ));
    }
    if meta.pointer("/authBinding/source").and_then(Value::as_str) == Some("managed_account") {
        return Err(Error::message("此供应商绑定订阅账号，首版仅导入 BYOK"));
    }
    let text = settings
        .get("config")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::message("缺少 Codex TOML 配置"))?;
    let doc = config::parse(text)?;
    let selected = doc
        .get("model_provider")
        .and_then(Item::as_str)
        .unwrap_or("openai");
    let table = doc.get("model_providers").and_then(|x| x.get(selected));
    let key = settings
        .pointer("/auth/OPENAI_API_KEY")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            table
                .and_then(|x| x.get("experimental_bearer_token"))
                .and_then(Item::as_str)
        })
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::message("没有可导入的 API Key（可能是官方登录或环境变量认证）"))?;
    if ["cc-switch-official", "ccswitch", "cc_switch"].contains(&selected)
        || ["cc-switch-proxy", "PROXY_MANAGED"].contains(&key)
    {
        return Err(Error::message(
            "此配置是 CC Switch 代理接管配置，需要原始供应商连接信息",
        ));
    }
    if key.len() > 65536 || key.chars().any(char::is_control) {
        return Err(Error::message("供应商 API Key 格式无效"));
    }
    let text = if meta.get("commonConfigEnabled").and_then(Value::as_bool) == Some(true) {
        if let Some(common) = common {
            config::merge(text, common)?
        } else {
            text.into()
        }
    } else {
        text.into()
    };
    Ok((config::normalize(&text)?, key.into()))
}

// Only built-in template connection parameters are read; JavaScript stays in CC Switch.
#[derive(serde::Deserialize)]
pub(crate) struct UsageSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub code: String,
    #[serde(rename = "templateType")]
    pub template_type: Option<String>,
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    #[serde(rename = "accessToken")]
    pub access_token: Option<String>,
    #[serde(rename = "userId")]
    pub user_id: Option<String>,
}

pub(crate) fn read_usage(
    path: &Path,
    id: &str,
    expected_base: &str,
    expected_key: &str,
) -> Result<Option<UsageSettings>> {
    use rusqlite::OptionalExtension;
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| Error::message("无法只读打开 CC Switch 用量配置"))?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    conn.execute_batch("BEGIN; PRAGMA query_only=ON;")?;
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(providers)")?
        .query_map([], |r| r.get(1))?
        .collect::<std::result::Result<_, _>>()?;
    if !columns.iter().any(|c| c == "meta") {
        return Ok(None);
    }
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT settings_config,meta FROM providers WHERE app_type='codex' AND id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((raw, meta)) = row else {
        return Err(Error::message("CC Switch 来源 Provider 已删除"));
    };
    let has_settings = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='settings'",
        [],
        |r| r.get::<_, i64>(0),
    )? > 0;
    let common: Option<String> = if has_settings {
        conn.query_row(
            "SELECT value FROM settings WHERE key='common_config_codex'",
            [],
            |r| r.get(0),
        )
        .optional()?
    } else {
        None
    };
    let (config, key) = convert(&raw, &meta, common.as_deref())
        .map_err(|_| Error::message("CC Switch 来源配置已不可用，请重新导入核对"))?;
    let (base, _) = config::summary(&config)?;
    if crate::resources::api_endpoint(&base) != crate::resources::api_endpoint(expected_base)
        || key != expected_key
    {
        return Err(Error::message(
            "CC Switch 来源推理身份已变化，请先同步或重新导入",
        ));
    }
    let meta: Value =
        serde_json::from_str(&meta).map_err(|_| Error::message("CC Switch 用量元信息无效"))?;
    let Some(value) = meta
        .get("usage_script")
        .or_else(|| meta.get("usageScript"))
        .filter(|v| !v.is_null())
    else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|_| Error::message("CC Switch 用量查询参数格式无效"))
}
