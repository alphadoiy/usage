use super::{Error, Result};
use toml_edit::{DocumentMut, Item, Table, value};

pub const KEY_ENV: &str = "USAGE_CODEX_API_KEY";

/// Saved revisions are immutable; the CLI may persist project trust in the running Home.
/// Ignore only valid trust entries, not arbitrary project or authentication overrides.
pub(super) fn matches_runtime_config(runtime: &str, saved: &str) -> bool {
    fn managed_settings(text: &str) -> Option<toml::Value> {
        let mut doc: toml::Value = toml::from_str(text).ok()?;
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
        Some(doc)
    }
    match (managed_settings(runtime), managed_settings(saved)) {
        (Some(runtime), Some(saved)) => runtime == saved,
        _ => false,
    }
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
