use super::{Error, Result};
use toml_edit::{DocumentMut, Item, Table, value};

pub const KEY_ENV: &str = "USAGE_CODEX_API_KEY";
/// Known `model_reasoning_effort` values, weakest first, matching magpie's effort rank.
/// A model may accept only some, and others pass through.
pub const REASONING_EFFORTS: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// Saved revisions are immutable; the CLI persists model choices, trust and onboarding in its Home.
/// Ignore only known, correctly typed runtime state, not arbitrary settings or auth overrides.
fn managed_settings(text: &str) -> Option<toml::Value> {
    let mut doc: toml::Value = toml::from_str(text).ok()?;
    // /model persists model and effort choices independently of the provider route.
    // Validate the model's shape without pinning it to a catalog that may be stale.
    if let Some(model) = doc.get("model") {
        let model = model.as_str()?;
        if model.trim().is_empty() || model.chars().any(char::is_control) {
            return None;
        }
        doc.as_table_mut()?.remove("model");
    }
    if doc
        .get("model_reasoning_effort")
        .and_then(toml::Value::as_str)
        .is_some_and(|effort| {
            !effort.is_empty()
                && effort.len() <= 32
                && effort.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'
                        || byte == b'-'
                })
        })
    {
        doc.as_table_mut()?.remove("model_reasoning_effort");
    }
    if let Some(projects) = doc.get_mut("projects") {
        let projects = projects.as_table_mut()?;
        for (_, project) in projects.iter_mut() {
            let project = project.as_table_mut()?;
            if let Some(trust) = project.get("trust_level") {
                if !matches!(trust.as_str(), Some("trusted" | "untrusted")) {
                    return None;
                }
                project.remove("trust_level");
            }
        }
        projects.retain(|_, project| project.as_table().is_none_or(|t| !t.is_empty()));
        if projects.is_empty() {
            doc.as_table_mut()?.remove("projects");
        }
    }
    for (section, fields) in [
        ("notice", &["model_migrations"][..]),
        (
            "tui",
            &["screen_reader_detection_done", "model_availability_nux"][..],
        ),
    ] {
        if let Some(table) = doc.get_mut(section) {
            let table = table.as_table_mut()?;
            for &field in fields {
                if let Some(state) = table.get(field) {
                    let valid = match field {
                        "model_migrations" => state
                            .as_table()
                            .is_some_and(|t| t.values().all(|v| v.as_str().is_some())),
                        "screen_reader_detection_done" => state.as_bool().is_some(),
                        "model_availability_nux" => state
                            .as_table()
                            .is_some_and(|t| t.values().all(|v| v.as_integer().is_some())),
                        _ => false,
                    };
                    if !valid {
                        return None;
                    }
                    table.remove(field);
                }
            }
            if table.is_empty() {
                doc.as_table_mut()?.remove(section);
            }
        }
    }
    // /hooks saves reviewed hashes and enablement in the Home. These are runtime
    // metadata; hook definitions themselves must still match the saved revision.
    if let Some(hooks) = doc.get_mut("hooks") {
        let hooks = hooks.as_table_mut()?;
        if let Some(state) = hooks.get("state") {
            for entry in state.as_table()?.values() {
                for (key, value) in entry.as_table()? {
                    let valid = match key.as_str() {
                        "enabled" => value.as_bool().is_some(),
                        "trusted_hash" => value.as_str().is_some_and(|hash| {
                            hash.strip_prefix("sha256:").is_some_and(|hash| {
                                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
                            })
                        }),
                        _ => false,
                    };
                    if !valid {
                        return None;
                    }
                }
            }
            hooks.remove("state");
        }
        if hooks.is_empty() {
            doc.as_table_mut()?.remove("hooks");
        }
    }
    Some(doc)
}

pub(super) fn matches_runtime_config(runtime: &str, saved: &str) -> bool {
    runtime_differences(runtime, saved).is_empty()
}

/// Only schema field names are emitted. User-selected table keys and all values stay private.
pub(super) fn runtime_differences(runtime: &str, saved: &str) -> Vec<String> {
    match (managed_settings(runtime), managed_settings(saved)) {
        (Some(runtime), Some(saved)) => configuration_differences(&runtime, &saved),
        (None, _) => vec!["<invalid_runtime_config>".into()],
        (_, None) => vec!["<invalid_saved_config>".into()],
    }
}

pub(super) fn configuration_differences(runtime: &toml::Value, saved: &toml::Value) -> Vec<String> {
    fn compare(
        a: Option<&toml::Value>,
        b: Option<&toml::Value>,
        path: &str,
        dynamic: bool,
        result: &mut Vec<String>,
        depth: usize,
    ) {
        if a == b || result.len() >= 16 {
            return;
        }
        if depth < 8
            && let (Some(toml::Value::Table(a)), Some(toml::Value::Table(b))) = (a, b)
        {
            let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for key in keys {
                let safe = if dynamic {
                    "<entry>"
                } else {
                    match key.as_str() {
                        "model"
                        | "model_provider"
                        | "model_providers"
                        | "model_reasoning_effort"
                        | "base_url"
                        | "name"
                        | "env_key"
                        | "wire_api"
                        | "requires_openai_auth"
                        | "http_headers"
                        | "experimental_bearer_token"
                        | "projects"
                        | "trust_level"
                        | "notice"
                        | "tui"
                        | "features"
                        | "mcp_servers"
                        | "notifications"
                        | "status_line"
                        | "cli_auth_credentials_store"
                        | "forced_login_method"
                        | "forced_chatgpt_workspace_id"
                        | "sandbox_mode"
                        | "approval_policy"
                        | "profiles" => key.as_str(),
                        _ => "<unknown>",
                    }
                };
                let next = if path.is_empty() {
                    safe.to_owned()
                } else {
                    format!("{path}.{safe}")
                };
                compare(
                    a.get(key),
                    b.get(key),
                    &next,
                    matches!(
                        safe,
                        "model_providers"
                            | "projects"
                            | "mcp_servers"
                            | "http_headers"
                            | "profiles"
                    ),
                    result,
                    depth + 1,
                );
            }
        } else if !result.iter().any(|p| p == path) {
            result.push(path.into());
        }
    }
    let mut result = Vec::new();
    compare(Some(runtime), Some(saved), "", false, &mut result, 0);
    result
}

pub fn parse(text: &str) -> Result<DocumentMut> {
    text.parse().map_err(|_| {
        Error::message("Codex TOML 无法解析；请检查配置语法（错误内容已隐藏以保护密钥）")
    })
}

pub fn new_config(base_url: &str, model: &str, effort: Option<&str>) -> Result<String> {
    let mut doc = DocumentMut::new();
    doc["model"] = value(model);
    doc["model_provider"] = value("usage_byok");
    doc["model_providers"]["usage_byok"]["name"] = value("BYOK");
    doc["model_providers"]["usage_byok"]["base_url"] = value(base_url);
    if let Some(effort) = effort {
        doc["model_reasoning_effort"] = value(effort);
    }
    normalize(&doc.to_string())
}

/// Preserve comments and unowned settings, but move active-provider auth to child-process env.
pub fn normalize(text: &str) -> Result<String> {
    let mut doc = parse(text)?;
    let model = doc.get("model").and_then(Item::as_str).unwrap_or_default();
    if model.trim().is_empty() || model.chars().any(char::is_control) {
        return Err(Error::message("配置缺少有效的 model"));
    }
    let selected = doc
        .get("model_provider")
        .and_then(Item::as_str)
        .unwrap_or("openai")
        .to_string();
    if doc
        .get("model_providers")
        .is_some_and(|v| v.as_table_like().is_none())
    {
        return Err(Error::message("model_providers 必须是 TOML 表"));
    }
    let id = if selected == "openai" {
        let url = doc
            .get("model_providers")
            .and_then(|x| x.get("openai"))
            .and_then(|x| x.get("base_url"))
            .and_then(Item::as_str)
            .or_else(|| doc.get("openai_base_url").and_then(Item::as_str))
            .unwrap_or("https://api.openai.com/v1")
            .to_string();
        let mut table = doc
            .get("model_providers")
            .and_then(|x| x.get("openai"))
            .cloned()
            .unwrap_or_else(|| Item::Table(Table::new()));
        if table.as_table_like().is_none() {
            return Err(Error::message("OpenAI 供应商配置必须是 TOML 表"));
        }
        table["base_url"] = value(url);
        doc["model_providers"]["usage_byok"] = table;
        if let Some(t) = doc
            .get_mut("model_providers")
            .and_then(Item::as_table_like_mut)
        {
            t.remove("openai");
        }
        doc.remove("openai_base_url");
        doc["model_provider"] = value("usage_byok");
        "usage_byok".to_string()
    } else {
        selected
    };
    let table = doc
        .get_mut("model_providers")
        .and_then(|x| x.get_mut(&id))
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| Error::message("找不到选中的 model_providers 配置"))?;
    let url = table
        .get("base_url")
        .and_then(Item::as_str)
        .unwrap_or_default();
    validate_url(url)?;
    if table
        .get("wire_api")
        .and_then(Item::as_str)
        .is_some_and(|v| v != "responses")
    {
        return Err(Error::message(
            "首版仅支持 Responses API；此配置需要接口转换代理",
        ));
    }
    if table
        .get("http_headers")
        .and_then(Item::as_table_like)
        .is_some_and(|h| {
            h.iter().any(|(k, _)| {
                ["authorization", "x-api-key", "api-key"].contains(&k.to_ascii_lowercase().as_str())
            })
        })
    {
        return Err(Error::message(
            "配置含认证请求头；请把密钥移入账号的 API Key 字段",
        ));
    }
    table.remove("experimental_bearer_token");
    table.remove("auth");
    table.insert("env_key", value(KEY_ENV));
    table.insert("requires_openai_auth", value(false));
    table.insert("wire_api", value("responses"));
    if table.get("name").is_none() {
        table.insert("name", value("BYOK"));
    }
    // Never persist credentials belonging to inactive provider tables either.
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        for (_, item) in providers.iter_mut() {
            if let Some(t) = item.as_table_like_mut() {
                t.remove("experimental_bearer_token");
                t.remove("auth");
                if t.get("http_headers")
                    .and_then(Item::as_table_like)
                    .is_some_and(|h| {
                        h.iter().any(|(k, _)| {
                            ["authorization", "x-api-key", "api-key"]
                                .contains(&k.to_ascii_lowercase().as_str())
                        })
                    })
                {
                    return Err(Error::message("其他供应商表含认证请求头；请先移除明文密钥"));
                }
            }
        }
    }
    Ok(doc.to_string())
}

pub fn validate_url(url: &str) -> Result<()> {
    if url.chars().any(char::is_control) {
        return Err(Error::message("Base URL 不能包含控制字符"));
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| Error::message("无效的 Base URL"))?;
    if !["http", "https"].contains(&parsed.scheme())
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(Error::message(
            "Base URL 必须是 HTTP(S) 地址，不能包含凭据、查询参数或 fragment",
        ));
    }
    Ok(())
}

pub fn summary(text: &str) -> Result<(String, String)> {
    let doc = parse(text)?;
    let id = doc
        .get("model_provider")
        .and_then(Item::as_str)
        .unwrap_or_default();
    let base = doc
        .get("model_providers")
        .and_then(|v| v.get(id))
        .and_then(|v| v.get("base_url"))
        .and_then(Item::as_str)
        .ok_or_else(|| Error::message("配置缺少供应商 Base URL"))?;
    let model = doc
        .get("model")
        .and_then(Item::as_str)
        .ok_or_else(|| Error::message("配置缺少 model"))?;
    Ok((base.into(), model.into()))
}

pub fn patch(
    text: &str,
    base: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<String> {
    let mut doc = parse(&normalize(text)?)?;
    let id = doc["model_provider"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if let Some(base) = base {
        doc["model_providers"][&id]["base_url"] = value(base);
    }
    if let Some(model) = model {
        doc["model"] = value(model);
    }
    if let Some(effort) = effort {
        doc["model_reasoning_effort"] = value(effort);
    }
    normalize(&doc.to_string())
}

/// Legacy CC Switch common snippets are recursive overlays, not string concatenation.
pub fn merge(base: &str, overlay: &str) -> Result<String> {
    fn merge_table(dst: &mut dyn toml_edit::TableLike, src: &dyn toml_edit::TableLike) {
        for (key, item) in src.iter() {
            if let Some(d) = dst.get_mut(key).and_then(Item::as_table_like_mut)
                && let Some(s) = item.as_table_like()
            {
                merge_table(d, s);
            } else {
                dst.insert(key, item.clone());
            }
        }
    }
    let mut doc = parse(base)?;
    let other = parse(overlay)?;
    merge_table(doc.as_table_mut(), other.as_table());
    Ok(doc.to_string())
}
