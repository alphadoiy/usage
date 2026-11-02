use super::*;
use serde_json::json;

pub(super) fn codex_models(models: &[Model]) -> Result<Value> {
    let entries: Vec<_> = models.iter().filter_map(|model| {
        let inputs = input_modalities(model);
        (!inputs.is_empty()).then_some((model, inputs))
    }).enumerate().map(|(index,(m,inputs))| {
        let efforts: Vec<_> = crate::codex_manager::config::REASONING_EFFORTS.into_iter()
            .filter(|e|m.reasoning_levels.iter().any(|v|v==e) || m.supported_reasoning_levels.iter().any(|v|v["effort"].as_str()==Some(*e)))
            .map(|e|json!({"effort":e,"description":e})).collect();
        let default = efforts.iter().find(|v|v["effort"]=="medium").or(efforts.first()).map(|v|v["effort"].clone());
        let name = if m.display_name.is_empty() { &m.id } else { &m.display_name };
        let mut entry = json!({
            "slug":m.id,"display_name":name,"description":format!("{} · {}",m.category(),name),
            "default_reasoning_level":default,"supported_reasoning_levels":efforts,
            "shell_type":"unified_exec","visibility":"list","supported_in_api":true,"priority":index,
            "base_instructions":"You are Codex, a coding assistant. Follow the user's instructions and use the available tools to complete the task.",
            "support_verbosity":false,"truncation_policy":{"mode":"tokens","limit":10000},
            "experimental_supported_tools":[],"input_modalities":inputs,
            "service_tiers":[],"additional_speed_tiers":[],"upgrade":null
        });
        if let Some(context) = m.context_window.filter(|v|*v>0) { entry["context_window"]=json!(context); }
        entry
    }).collect();
    let value = json!({"models":entries});
    if serde_json::to_vec(&value)
        .map_err(|_| "模型目录编码失败")?
        .len()
        > 1024 * 1024
    {
        return Err("Magpie 模型目录超过 Codex 大小限制".into());
    }
    Ok(value)
}

/// Codex 0.166's catalog schema accepts only these input capabilities. Keep
/// upstream metadata intact; project it only at the native catalog boundary.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum InputModality {
    Text,
    Image,
    Audio,
}

fn input_modalities(model: &Model) -> Vec<InputModality> {
    let values = match model.modalities.get("input") {
        None | Some(Value::Null) => return vec![InputModality::Text],
        Some(Value::Array(values)) => values,
        Some(_) => return Vec::new(),
    };
    let mut supported = Vec::new();
    for value in values {
        let modality = match value.as_str() {
            Some("text") => InputModality::Text,
            Some("image") => InputModality::Image,
            Some("audio") => InputModality::Audio,
            _ => continue,
        };
        if !supported.contains(&modality) {
            supported.push(modality);
        }
    }
    supported
}
