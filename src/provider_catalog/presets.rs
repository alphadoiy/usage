//! Product identities and their implementation bindings; UI presents the service, not the host.
use super::ProviderKind;

pub struct Variant {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub desktop_auth_file: Option<&'static str>,
}
pub enum SignInSource {
    Browser,
    Desktop,
    Plugin,
}
pub struct SignIn {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub source: SignInSource,
}
pub struct SignInOption {
    pub name: &'static str,
    pub description: String,
    pub available: bool,
}
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub package: &'static str,
    pub version: &'static str,
    pub kind: ProviderKind,
    pub variants: &'static [Variant],
    pub sign_in: &'static [SignIn],
}
pub const WORKBUDDY: Preset = Preset {
    id: "workbuddy",
    name: "WorkBuddy",
    package: "@magpie-community/opencode-workbuddy-auth",
    version: "0.1.24",
    kind: ProviderKind::SubscriptionService,
    variants: &[
        Variant {
            id: "workbuddy",
            name: "国内版",
            description: "使用中国大陆 WorkBuddy 账号",
            desktop_auth_file: Some("workbuddy-desktop.info"),
        },
        Variant {
            id: "workbuddy-ai",
            name: "国际版",
            description: "使用 WorkBuddy AI 国际账号",
            desktop_auth_file: Some("workbuddy-desktop-ai.info"),
        },
    ],
    sign_in: &[
        SignIn {
            id: "0",
            name: "浏览器登录（推荐）",
            description: "在浏览器完成授权，适用于新版客户端",
            source: SignInSource::Browser,
        },
        SignIn {
            id: "1",
            name: "读取桌面登录",
            description: "仅支持未加密的桌面凭据",
            source: SignInSource::Desktop,
        },
    ],
};
pub const TRAE: Preset = Preset {
    id: "trae",
    name: "Trae",
    package: "@magpie-community/opencode-trae-auth",
    version: "0.2.17",
    kind: ProviderKind::SubscriptionService,
    variants: &[
        Variant {
            id: "trae-cn",
            name: "国内版",
            description: "使用 trae.cn 账号；支持 Credits 额度与每日签到",
            desktop_auth_file: None,
        },
        Variant {
            id: "trae-global",
            name: "国际版",
            description: "使用 trae.ai 账号；查询美元额度，不提供每日签到",
            desktop_auth_file: None,
        },
    ],
    sign_in: &[SignIn {
        id: "0",
        name: "浏览器登录",
        description: "在对应地区网站授权，本机回调自动完成登录",
        source: SignInSource::Browser,
    }],
};
pub const ZCODE: Preset = Preset {
    id: "zcode",
    name: "ZCode",
    package: "@magpie-community/opencode-zcode-auth",
    version: "0.1.27",
    kind: ProviderKind::SubscriptionService,
    variants: &[Variant {
        id: "zcode",
        name: "GLM Coding Plan",
        description: "支持 Z.ai、智谱、团队订阅及 ZCode 赠送计划",
        desktop_auth_file: None,
    }],
    sign_in: &[
        SignIn {
            id: "0",
            name: "Z.ai 浏览器登录",
            description: "在浏览器登录 Z.ai，自动完成授权",
            source: SignInSource::Browser,
        },
        SignIn {
            id: "1",
            name: "智谱浏览器登录",
            description: "在浏览器登录 BigModel，自动完成授权",
            source: SignInSource::Browser,
        },
        SignIn {
            id: "2",
            name: "团队订阅登录",
            description: "选择 Z.ai 或智谱，使用团队 GLM Coding Plan",
            source: SignInSource::Browser,
        },
        SignIn {
            id: "3",
            name: "读取 ZCode 应用登录",
            description: "由插件读取本机 ZCode 已登录账号",
            source: SignInSource::Plugin,
        },
        SignIn {
            id: "4",
            name: "GLM Coding Plan API Key",
            description: "选择密钥所属网站；赠送计划需要 ZCode 登录",
            source: SignInSource::Plugin,
        },
    ],
};
pub static PRESETS: &[Preset] = &[WORKBUDDY, TRAE, ZCODE];

pub fn for_package(package: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.package == package)
}
impl Preset {
    pub fn sign_in_option(
        &self,
        provider: &str,
        method: &str,
        home: &std::path::Path,
    ) -> Option<SignInOption> {
        let spec = self.sign_in.iter().find(|m| m.id == method)?;
        let reason = match spec.source {
            SignInSource::Browser | SignInSource::Plugin => None,
            SignInSource::Desktop => {
                let file = self
                    .variants
                    .iter()
                    .find(|v| v.id == provider)?
                    .desktop_auth_file?;
                desktop_unavailable(home, file)
            }
        };
        Some(SignInOption {
            name: spec.name,
            available: reason.is_none(),
            description: reason
                .map(|r| format!("{} · {r}", spec.description))
                .unwrap_or_else(|| spec.description.into()),
        })
    }
    pub fn variant_name(&self, provider: &str) -> Option<&'static str> {
        self.variants
            .iter()
            .find(|v| v.id == provider)
            .map(|v| v.name)
    }
    pub fn account_name(&self, provider: &str, name: &str) -> String {
        match self.variant_name(provider) {
            Some(variant) => format!("{} {} · {name}", self.name, variant),
            None => format!("{} · {name}", self.name),
        }
    }
}
fn desktop_unavailable(home: &std::path::Path, file: &str) -> Option<&'static str> {
    use std::io::Read;
    let path = desktop_path(home, file);
    let mut bytes = Vec::new();
    let Some(value) = (|| {
        std::fs::File::open(path)
            .ok()?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > 64 * 1024 {
            return None;
        }
        serde_json::from_slice::<serde_json::Value>(&bytes).ok()
    })() else {
        return Some("本机未发现可用的桌面登录");
    };
    if value["auth"]["accessToken"]["$wbEncrypted"] == 1
        || value["auth"]["refreshToken"]["$wbEncrypted"] == 1
    {
        Some("本机凭据已加密，请使用浏览器登录")
    } else if value["auth"]["accessToken"]
        .as_str()
        .is_some_and(|s| !s.is_empty())
        && value["account"]["uid"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    {
        None
    } else {
        Some("本机未发现可用的桌面登录")
    }
}

fn desktop_path(home: &std::path::Path, file: &str) -> std::path::PathBuf {
    let base = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else if cfg!(windows) {
        home.join("AppData/Local")
    } else {
        home.join(".local/share")
    };
    base.join("CodeBuddyExtension/Data/Public/auth").join(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn login_availability_checks_the_selected_region_without_exposing_credentials() {
        let home =
            std::env::temp_dir().join(format!("usage-workbuddy-menu-{}", uuid::Uuid::new_v4()));
        let option = |region, method| WORKBUDDY.sign_in_option(region, method, &home).unwrap();
        assert!(option("workbuddy", "0").available);
        assert!(!option("workbuddy", "1").available);
        let path = desktop_path(&home, "workbuddy-desktop.info");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for (payload, available, reason) in [
            (
                r#"{"auth":{"accessToken":{"$wbEncrypted":1}},"account":{"uid":"test"}}"#,
                false,
                "已加密",
            ),
            (
                r#"{"auth":{"accessToken":"synthetic-secret","refreshToken":{"$wbEncrypted":1}},"account":{"uid":"test"}}"#,
                false,
                "已加密",
            ),
            (
                r#"{"auth":{"accessToken":"synthetic-secret"},"account":{"uid":"test"}}"#,
                true,
                "仅支持未加密",
            ),
            ("{}", false, "未发现"),
            ("invalid", false, "未发现"),
        ] {
            std::fs::write(&path, payload).unwrap();
            let result = option("workbuddy", "1");
            assert_eq!(result.available, available);
            assert!(result.description.contains(reason));
            assert!(!result.description.contains("synthetic-secret"));
            assert!(!option("workbuddy-ai", "1").available);
            assert!(option("workbuddy", "0").available);
        }
        std::fs::write(&path, vec![b' '; 64 * 1024 + 1]).unwrap();
        assert!(!option("workbuddy", "1").available);
        assert!(
            WORKBUDDY
                .sign_in_option("workbuddy", "unknown", &home)
                .is_none()
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn service_identity_is_independent_of_the_installed_plugin_id() {
        let preset = for_package("@magpie-community/opencode-workbuddy-auth").unwrap();
        assert_eq!(preset.name, "WorkBuddy");
        assert_eq!(
            preset.account_name("workbuddy-ai", "Alice"),
            "WorkBuddy 国际版 · Alice"
        );
        assert!(for_package("unrelated-package").is_none());
    }

    #[test]
    fn trae_regions_have_independent_browser_signin_and_service_identity() {
        let preset = for_package("@magpie-community/opencode-trae-auth").unwrap();
        assert_eq!(preset.id, "trae");
        assert_eq!(preset.version, "0.2.17");
        for (id, name) in [("trae-cn", "国内版"), ("trae-global", "国际版")] {
            assert_eq!(
                preset.account_name(id, "Alice"),
                format!("Trae {name} · Alice")
            );
            let method = preset
                .sign_in_option(id, "0", std::path::Path::new("/unused"))
                .unwrap();
            assert!(method.available);
            assert!(method.description.contains("回调"));
            assert!(
                preset
                    .sign_in_option(id, "1", std::path::Path::new("/unused"))
                    .is_none()
            );
        }
    }

    #[test]
    fn zcode_preserves_all_original_login_methods_and_subscription_identity() {
        let preset = for_package("@magpie-community/opencode-zcode-auth").unwrap();
        assert_eq!(preset.id, "zcode");
        assert_eq!(preset.version, "0.1.27");
        assert_eq!(
            preset.account_name("zcode", "Alice"),
            "ZCode GLM Coding Plan · Alice"
        );
        for id in ["0", "1", "2", "3", "4"] {
            assert!(
                preset
                    .sign_in_option("zcode", id, std::path::Path::new("/unused"))
                    .unwrap()
                    .available
            );
        }
        assert!(preset.variant_name("trae-cn").is_none());
    }
}
