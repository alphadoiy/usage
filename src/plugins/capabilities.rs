//! Built-in host adapters and discovered native hooks share one capability projection.
use super::{ACTION_CAPABILITIES, Value};
use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Builtin {
    package: String,
    provider: String,
    checkin_status: bool,
    day_offset_seconds: i32,
}
pub(crate) struct Checkin {
    pub automatic_status: bool,
    pub day_offset_seconds: i32,
}
pub(crate) fn checkin(
    package: &str,
    provider: &str,
    descriptor: Option<&Value>,
) -> Option<Checkin> {
    static BUILTINS: OnceLock<Vec<Builtin>> = OnceLock::new();
    let builtin = BUILTINS
        .get_or_init(|| {
            serde_json::from_str(ACTION_CAPABILITIES).expect("built-in action capabilities")
        })
        .iter()
        .find(|b| b.package == package && b.provider == provider);
    let native = descriptor
        .and_then(|d| d["providers"].as_array())
        .and_then(|providers| providers.iter().find(|p| p["id"] == provider))
        .map(|p| &p["capabilities"]);
    if builtin.is_none() && !native.is_some_and(|c| c["checkin"] == true) {
        return None;
    }
    Some(Checkin {
        automatic_status: builtin.is_some_and(|b| b.checkin_status)
            || native.is_some_and(|c| c["checkin_status"] == true),
        day_offset_seconds: builtin.map_or(0, |b| b.day_offset_seconds),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readonly_status_is_explicit_and_scoped_to_the_exported_provider() {
        let package = "@magpie-community/opencode-workbuddy-auth";
        let builtin = checkin(package, "workbuddy", None).unwrap();
        assert!(builtin.automatic_status);
        assert_eq!(builtin.day_offset_seconds, 8 * 3600);
        assert!(checkin(package, "workbuddy-ai", None).is_none());
        assert!(checkin("unknown", "workbuddy", None).is_none());
        for readonly in [false, true] {
            let descriptor = serde_json::json!({"providers":[
                {"id":"native", "capabilities":{"checkin":true,"checkin_status":readonly}}
            ]});
            let native = checkin("unknown", "native", Some(&descriptor)).unwrap();
            assert_eq!(native.automatic_status, readonly);
            assert_eq!(native.day_offset_seconds, 0);
            assert!(checkin("unknown", "other", Some(&descriptor)).is_none());
        }
    }
}
