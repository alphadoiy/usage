//! Codex-native catalog for a plugin account's advertised models.
use std::collections::HashSet;

use serde_json::{Value, json};

const INSTRUCTIONS: &str = "You are Codex, a coding assistant. Follow the user's instructions and use the available tools to complete the task.";
const EFFORTS: [&str; 7] = ["none", "low", "medium", "high", "xhigh", "max", "ultra"];

pub(super) fn plugin_models(value: &Value) -> Result<Value, &'static str> {
    let source = value.as_array().ok_or("Invalid plugin model list")?;
    if source.is_empty() || source.len() > 5000 {
        return Err("Plugin model list is empty or too large");
    }
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for entry in source {
        let Some(slug) = entry.get("id").and_then(Value::as_str) else {
            return Err("Plugin model is missing an id");
        };
        if slug.is_empty() || slug.len() > 256 || slug.chars().any(char::is_control) {
            return Err("Plugin model id is invalid");
        }
        if !seen.insert(slug) {
            continue;
        }
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(slug);
        let offered = entry.get("reasoning").and_then(Value::as_object);
        let efforts: Vec<&str> = EFFORTS
            .into_iter()
            .filter(|effort| offered.is_some_and(|levels| levels.contains_key(*effort)))
            .collect();
        let efforts = if efforts.is_empty() {
            vec!["none"]
        } else {
            efforts
        };
        let default = if efforts.contains(&"medium") {
            "medium"
        } else {
            efforts[0]
        };
        let levels: Vec<Value> = efforts
            .into_iter()
            .map(|effort| json!({"effort":effort,"description":effort}))
            .collect();
        let images = entry.get("images").and_then(Value::as_bool) == Some(true);
        let modalities = if images {
            vec!["text", "image"]
        } else {
            vec!["text"]
        };
        let mut model = json!({
            "slug": slug,
            "display_name": name,
            "description": format!("{} (plugin account)", name),
            "default_reasoning_level": default,
            "supported_reasoning_levels": levels,
            "shell_type": "unified_exec",
            "visibility": "list",
            "supported_in_api": true,
            "priority": models.len(),
            "base_instructions": INSTRUCTIONS,
            "support_verbosity": false,
            "truncation_policy": {"mode":"tokens","limit":10000},
            "experimental_supported_tools": [],
            "input_modalities": modalities,
            "service_tiers": [],
            "additional_speed_tiers": [],
            "upgrade": null,
        });
        if let Some(context) = entry.get("context").and_then(Value::as_u64)
            && context > 0
        {
            model["context_window"] = json!(context);
        }
        models.push(model);
    }
    let catalog = json!({"models":models});
    if serde_json::to_vec(&catalog)
        .map_err(|_| "Invalid plugin model catalog")?
        .len()
        > 1024 * 1024
    {
        return Err("Plugin model catalog exceeds Codex size limit");
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_contains_only_plugin_models_with_codex_fields() {
        let catalog = plugin_models(&json!([
            {"id":"model-a","name":"Model A","context":128000,"images":true,
             "reasoning":{"low":{},"medium":{},"high":{}}},
            {"id":"model-a","name":"Duplicate"},
            {"id":"model-b","name":"Model B","images":false}
        ]))
        .unwrap();
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["slug"], "model-a");
        assert_eq!(models[0]["default_reasoning_level"], "medium");
        assert_eq!(models[0]["input_modalities"], json!(["text", "image"]));
        assert_eq!(models[0]["context_window"], 128000);
        assert_eq!(models[1]["slug"], "model-b");
        assert_eq!(models[1]["default_reasoning_level"], "none");
        assert!(models[1].get("context_window").is_none());
    }

    #[test]
    fn malformed_or_empty_catalogs_fail_instead_of_advertising_defaults() {
        for value in [
            json!([]),
            json!({}),
            json!([{"name":"missing"}]),
            json!([{"id":"bad\nname"}]),
        ] {
            assert!(plugin_models(&value).is_err());
        }
    }
}
