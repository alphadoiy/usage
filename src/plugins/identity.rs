//! Stable account identity is distinct from display names and rotating credentials.
use super::Value;
use sha2::{Digest, Sha256};

pub(super) fn key(auth: &Value) -> Option<String> {
    let uid = match &auth["uid"] {
        Value::String(value) if !value.trim().is_empty() => value.trim().to_owned(),
        Value::Number(value) => value.to_string(),
        _ => {
            // Unknown plugins without a UID can only reuse identical credential payloads.
            // accountId is often a nickname (including WorkBuddy and Trae), not an identity.
            if !["key", "access"]
                .iter()
                .any(|field| auth[field].as_str().is_some_and(|s| !s.is_empty()))
            {
                return None;
            }
            return Some(format!(
                "credential:{:x}",
                Sha256::digest(serde_json::to_vec(auth).ok()?)
            ));
        }
    };
    let identity = serde_json::json!({
        "uid": uid,
        "type": auth["type"],
        "enterprise": auth["enterpriseId"],
        "tenant": auth["tenantId"],
        "account": auth["account"],
        "region": auth["region"],
    });
    Some(format!(
        "account:{:x}",
        Sha256::digest(serde_json::to_vec(&identity).ok()?)
    ))
}
