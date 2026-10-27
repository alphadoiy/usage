//! 可选的 TOML 配置。没有配置文件时使用内置默认值。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::view::builtin_styles;

#[derive(Debug)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub style: String,
    pub refresh_seconds: u64,
    pub providers: Vec<String>,
    pub claude_dir: Option<PathBuf>,
    pub codex_dir: Option<PathBuf>,
    pub grok_dir: Option<PathBuf>,
    pub instances: Vec<DataInstance>,
    pub resources: Vec<crate::resources::connections::ConnectionConfig>,
    pub provider_usage: std::collections::BTreeMap<String, crate::resources::QuotaConversion>,
    pub alerts: AlertConfig,
    pub resume: ResumeMode,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            style: "bar".into(),
            refresh_seconds: 60,
            providers: vec!["claude".into(), "codex".into(), "grok".into()],
            claude_dir: None,
            codex_dir: None,
            grok_dir: None,
            instances: Vec::new(),
            resources: Vec::new(),
            provider_usage: Default::default(),
            alerts: AlertConfig::default(),
            resume: ResumeMode::Auto,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataInstance {
    pub id: String,
    pub provider: String,
    pub path: PathBuf,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AlertConfig {
    pub enabled: bool,
    /// Alert when remaining quota falls to this percentage or below.
    pub remaining_percent: u8,
    pub cooldown_seconds: u64,
    pub recovery: bool,
    pub bell: bool,
    pub desktop: bool,
}
impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            remaining_percent: 15,
            cooldown_seconds: 300,
            recovery: true,
            bell: false,
            desktop: false,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResumeMode {
    #[default]
    Auto,
    Ghostty,
    Current,
}
impl ResumeMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::Ghostty => "Ghostty 分屏",
            Self::Current => "当前终端",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Ghostty,
            Self::Ghostty => Self::Current,
            Self::Current => Self::Auto,
        }
    }
}
impl AppConfig {
    /// Built-in IDs identify the primary homes; explicit instances are additional read-only homes.
    pub fn data_instances(&self) -> Vec<DataInstance> {
        let home = home_dir();
        let mut instances = vec![
            DataInstance {
                id: "codex".into(),
                provider: "codex".into(),
                path: self
                    .codex_dir
                    .clone()
                    .unwrap_or_else(|| home.join(".codex")),
            },
            DataInstance {
                id: "claude".into(),
                provider: "claude".into(),
                path: self
                    .claude_dir
                    .clone()
                    .unwrap_or_else(|| home.join(".claude")),
            },
            DataInstance {
                id: "grok".into(),
                provider: "grok".into(),
                path: self.grok_dir.clone().unwrap_or_else(|| home.join(".grok")),
            },
        ];
        instances.extend(self.instances.clone());
        if let Ok(managed) = crate::codex_manager::managed_instances() {
            for instance in managed {
                if !instances.iter().any(|i| {
                    i.id == instance.id
                        || (i.provider == instance.provider
                            && (resolved_data_path(&i.path).starts_with(&instance.path)
                                || instance.path.starts_with(resolved_data_path(&i.path))))
                }) {
                    instances.push(instance);
                }
            }
        }
        for instance in &mut instances {
            instance.path = resolved_data_path(&instance.path);
        }
        instances
    }
}

/// Resolve existing ancestors as well as deleted log paths; ledger entries can
/// outlive their files and retain the same identity through symlink aliases.
pub fn resolved_data_path(path: &Path) -> PathBuf {
    let mut path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    for _ in 0..16 {
        let mut redirected = None;
        for ancestor in path.ancestors() {
            if let Ok(base) = ancestor.canonicalize() {
                let suffix = path.strip_prefix(ancestor).unwrap();
                return if suffix.as_os_str().is_empty() {
                    base
                } else {
                    base.join(suffix)
                };
            }
            if let Ok(target) = std::fs::read_link(ancestor) {
                let target = if target.is_absolute() {
                    target
                } else {
                    ancestor.parent().unwrap_or(Path::new("/")).join(target)
                };
                redirected = Some(target.join(path.strip_prefix(ancestor).unwrap()));
                break;
            }
        }
        match redirected {
            Some(next) => path = next,
            None => break,
        }
    }
    path
}

pub fn default_config_path() -> PathBuf {
    if let Some(override_path) = std::env::var_os("USAGE_CONFIG") {
        let text = override_path.to_string_lossy().trim().to_string();
        if !text.is_empty() {
            return PathBuf::from(text);
        }
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home_dir().join(".config"));
    base.join("usage").join("config.toml")
}

pub fn default_toml(style: &str) -> String {
    format!(
        r#"# 终端用量查询
# style: bar Braille 进度条 / ring 圆环 / statusline 圆点状态栏
style = "{style}"

# 自动刷新间隔（秒）。0 表示只在打开和按 r 时查询
refresh_seconds = 60

# 查询顺序。已内置 claude、codex、grok
providers = ["claude", "codex", "grok"]

[alerts]
# 达到剩余额度阈值时提醒；默认关闭
enabled = false
remaining_percent = 15
cooldown_seconds = 300
recovery = true
bell = false
desktop = false

[sessions]
# auto：Ghostty 中分屏，其它终端在当前终端恢复；也可设 ghostty / current
resume = "auto"

# 额外实例示例（数据只读）
# [[instances]]
# id = "work-codex"
# provider = "codex"
# path = "~/.codex-work"

[paths]
# 留空则使用 ~/.claude、~/.codex、~/.grok
claude = ""
codex = ""
grok = ""

# 独立资源连接；用 usage resources 查询。凭据只填写环境变量名称
# [[resources]]
# id = "relay-token"
# service = "new_api_token"
# base_url = "https://relay.example" # New API 站点根路径，非推理 /v1
# credential_env = "RELAY_USAGE_KEY"
# options = {{}} # 默认保留原始额度；换算金额需显式提供比例与币种
# inference = {{ base_url = "https://relay.example/v1", credential_env = "RELAY_USAGE_KEY" }}
"#
    )
}

pub fn load_config(path: &Path, required: bool) -> Result<AppConfig, ConfigError> {
    if !path.exists() {
        if required {
            return Err(ConfigError(format!("找不到配置文件: {}", path.display())));
        }
        return Ok(AppConfig::default());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| ConfigError(format!("无法读取配置: {error}")))?;
    parse_config(&text)
}

pub(crate) fn parse_config(text: &str) -> Result<AppConfig, ConfigError> {
    let raw: RawConfig = toml::from_str(text)
        .map_err(|error| ConfigError(format!("配置不是合法的 TOML: {error}")))?;
    parse_raw(raw)
}

/// Edit only conversion settings, preserving comments, credentials and unrelated config.
pub(crate) fn save_quota_conversion(
    path: &Path,
    original: &str,
    profile: &str,
    resource: Option<&str>,
    conversion: &crate::resources::QuotaConversion,
) -> Result<(), ConfigError> {
    conversion
        .validate()
        .map_err(|error| ConfigError(error.to_string()))?;
    if uuid::Uuid::parse_str(profile).is_err() {
        return Err(ConfigError("账号 ID 无效".into()));
    }
    // Save through existing symlinks instead of replacing a dotfiles-managed link.
    let resolved = std::fs::canonicalize(path)
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound && !path.is_symlink() {
                std::path::absolute(path)
            } else {
                Err(error)
            }
        })
        .map_err(|error| ConfigError(format!("无法解析配置路径: {error}")))?;
    let path = resolved.as_path();
    let current = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(ConfigError(format!("无法读取配置: {error}"))),
    };
    if current != original {
        return Err(ConfigError(
            "配置已被其他操作更新，请重新打开额度设置".into(),
        ));
    }
    let config = parse_config(original)?;
    let mut doc: toml_edit::DocumentMut = original
        .parse()
        .map_err(|_| ConfigError("配置不是合法的 TOML".into()))?;
    if let Some(id) = resource {
        if !config.resources.iter().any(|r| {
            r.id == id
                && r.profile_ids.iter().any(|p| p == profile)
                && matches!(r.service.as_str(), "new_api_token" | "new_api_account")
        }) {
            return Err(ConfigError("资源不属于此账号或不支持额度换算".into()));
        }
        let row = doc
            .get_mut("resources")
            .and_then(toml_edit::Item::as_array_of_tables_mut)
            .and_then(|rows| {
                rows.iter_mut()
                    .find(|row| row.get("id").and_then(toml_edit::Item::as_str) == Some(id))
            })
            .ok_or_else(|| ConfigError("资源配置结构无法编辑".into()))?;
        if row.get("options").is_none() {
            row.insert("options", toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let options = row
            .get_mut("options")
            .and_then(toml_edit::Item::as_table_like_mut)
            .ok_or_else(|| ConfigError("资源 options 不是表".into()))?;
        for (key, value) in [
            ("quota_per_unit", &conversion.quota_per_unit),
            ("currency", &conversion.currency),
        ] {
            match value {
                Some(value) => {
                    options.insert(key, toml_edit::value(value));
                }
                None => {
                    options.remove(key);
                }
            }
        }
    } else {
        if doc.get("provider_usage").is_none() {
            doc["provider_usage"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let usage = doc
            .get_mut("provider_usage")
            .and_then(toml_edit::Item::as_table_like_mut)
            .ok_or_else(|| ConfigError("provider_usage 不是表".into()))?;
        if let (Some(ratio), Some(currency)) = (&conversion.quota_per_unit, &conversion.currency) {
            let mut options = toml_edit::Table::new();
            options.insert("quota_per_unit", toml_edit::value(ratio));
            options.insert("currency", toml_edit::value(currency));
            usage.insert(profile, toml_edit::Item::Table(options));
        } else {
            usage.remove(profile);
        }
    }
    let text = doc.to_string();
    parse_config(&text)?;
    crate::codex_manager::private_write(path, text.as_bytes())
        .map_err(|error| ConfigError(error.to_string()))
}

pub fn write_default_config(path: &Path, force: bool) -> Result<(), ConfigError> {
    if path.exists() && !force {
        return Err(ConfigError(format!("配置已存在: {}", path.display())));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ConfigError(format!("无法创建配置目录: {error}")))?;
    }
    std::fs::write(path, default_toml("bar"))
        .map_err(|error| ConfigError(format!("无法写入配置: {error}")))
}

pub fn with_overrides(
    mut config: AppConfig,
    style: Option<&str>,
    providers: Option<&str>,
    refresh_seconds: Option<u64>,
) -> Result<AppConfig, ConfigError> {
    if let Some(style) = style {
        ensure_style(style)?;
        config.style = style.to_string();
    }
    if let Some(providers) = providers {
        let mut deduped = Vec::new();
        for item in providers.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            if !deduped.iter().any(|existing: &String| existing == item) {
                deduped.push(item.to_string());
            }
        }
        if deduped.is_empty() {
            return Err(ConfigError("providers 不能为空".into()));
        }
        config.providers = deduped;
    }
    if let Some(refresh_seconds) = refresh_seconds {
        config.refresh_seconds = refresh_seconds;
    }
    Ok(config)
}

fn ensure_style(style: &str) -> Result<(), ConfigError> {
    let styles = builtin_styles();
    if styles.contains(style) {
        Ok(())
    } else {
        Err(ConfigError(format!(
            "style 必须是 {}",
            styles.ids().join(" / ")
        )))
    }
}

fn parse_raw(raw: RawConfig) -> Result<AppConfig, ConfigError> {
    let mut config = AppConfig::default();
    if let Some(style) = raw.style {
        // Older config files used `both` as the default. Migrate it to the
        // single Braille bar view when reading, without rewriting the file.
        let style = if style == "both" { "bar" } else { &style };
        ensure_style(style)?;
        config.style = style.to_string();
    }
    if let Some(refresh) = raw.refresh_seconds {
        config.refresh_seconds = refresh;
    }
    if let Some(providers) = raw.providers {
        if providers.is_empty() || providers.iter().any(|item| item.trim().is_empty()) {
            return Err(ConfigError("providers 必须是非空的字符串列表".into()));
        }
        let mut deduped = Vec::new();
        for item in providers {
            let item = item.trim().to_string();
            if !deduped.contains(&item) {
                deduped.push(item);
            }
        }
        config.providers = deduped;
    }
    if let Some(paths) = raw.paths {
        config.claude_dir = optional_path(paths.claude);
        config.codex_dir = optional_path(paths.codex);
        config.grok_dir = optional_path(paths.grok);
    }
    if let Some(alerts) = raw.alerts {
        if !(1..=100).contains(&alerts.remaining_percent) {
            return Err(ConfigError(
                "alerts.remaining_percent 必须在 1–100 之间".into(),
            ));
        }
        config.alerts = alerts;
    }
    if let Some(sessions) = raw.sessions {
        config.resume = sessions.resume;
    }
    config.resources = raw.resources.unwrap_or_default();
    config.provider_usage = raw.provider_usage.unwrap_or_default();
    for (id, conversion) in &config.provider_usage {
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(ConfigError("provider_usage 的账号 ID 必须为 UUID".into()));
        }
        conversion
            .validate()
            .map_err(|error| ConfigError(error.to_string()))?;
    }
    let mut resource_ids = std::collections::HashSet::new();
    for resource in &config.resources {
        resource
            .validate()
            .map_err(|error| ConfigError(error.to_string()))?;
        if !resource_ids.insert(&resource.id) {
            return Err(ConfigError("资源连接 id 不能重复".into()));
        }
    }
    config.instances = raw.instances.unwrap_or_default();
    let mut ids = vec![
        "codex".to_string(),
        "claude".to_string(),
        "grok".to_string(),
    ];
    for instance in &mut config.instances {
        instance.id = instance.id.trim().into();
        instance.provider = instance.provider.trim().into();
        if instance.id.is_empty()
            || instance.id.chars().any(char::is_control)
            || ids.contains(&instance.id)
        {
            return Err(ConfigError(
                "实例 ID 必须唯一、非空且不含控制字符（codex/claude/grok 为保留 ID）".into(),
            ));
        }
        if !["codex", "claude", "grok"].contains(&instance.provider.as_str()) {
            return Err(ConfigError(format!("未知实例来源：{}", instance.provider)));
        }
        if instance.path.as_os_str().is_empty() {
            return Err(ConfigError("实例目录不能为空".into()));
        }
        instance.path = expand_user(instance.path.clone());
        ids.push(instance.id.clone());
    }
    for resource in &config.resources {
        if let Some(id) = resource
            .inference
            .as_ref()
            .and_then(|i| i.performance_instance.as_ref())
        {
            let instance = config
                .instances
                .iter()
                .find(|instance| &instance.id == id)
                .ok_or_else(|| {
                    ConfigError(
                        "performance_instance 必须引用 [[instances]] 中的显式实例 ID".into(),
                    )
                })?;
            if crate::performance::source(&instance.provider).is_none() {
                return Err(ConfigError("此实例的 Agent 尚不支持性能日志".into()));
            }
        }
    }
    let instances = config.data_instances();
    for (i, instance) in instances.iter().enumerate() {
        if instances[..i].iter().any(|other| {
            other.provider == instance.provider
                && (other.path.starts_with(&instance.path)
                    || instance.path.starts_with(&other.path))
        }) {
            return Err(ConfigError(format!(
                "{} 的实例目录重复或嵌套，会导致重复统计",
                instance.provider
            )));
        }
    }
    Ok(config)
}

fn optional_path(value: Option<String>) -> Option<PathBuf> {
    let text = value?.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(expand_user(PathBuf::from(text)))
    }
}

fn expand_user(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("~/") {
        home_dir().join(rest)
    } else if text == "~" {
        home_dir()
    } else {
        path
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    style: Option<String>,
    refresh_seconds: Option<u64>,
    providers: Option<Vec<String>>,
    paths: Option<RawPaths>,
    instances: Option<Vec<DataInstance>>,
    resources: Option<Vec<crate::resources::connections::ConnectionConfig>>,
    provider_usage: Option<std::collections::BTreeMap<String, crate::resources::QuotaConversion>>,
    alerts: Option<AlertConfig>,
    sessions: Option<RawSessions>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawSessions {
    resume: ResumeMode,
}
impl Default for RawSessions {
    fn default() -> Self {
        Self {
            resume: ResumeMode::Auto,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawPaths {
    claude: Option<String>,
    codex: Option<String>,
    grok: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_conversion_saves_preserve_other_settings_and_reject_stale_or_invalid_edits() {
        let root = std::env::temp_dir().join(format!("usage-conversion-{}", uuid::Uuid::new_v4()));
        let path = root.join("config.toml");
        std::fs::create_dir_all(&root).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let original = "# keep this comment\nstyle='bar'\nrefresh_seconds=90\n";
        std::fs::write(&path, original).unwrap();
        let conversion = crate::resources::QuotaConversion {
            quota_per_unit: Some("500000".into()),
            currency: Some("USD".into()),
        };
        save_quota_conversion(&path, original, &id, None, &conversion).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# keep this comment"));
        assert_eq!(load_config(&path, true).unwrap().refresh_seconds, 90);
        assert_eq!(
            load_config(&path, true).unwrap().provider_usage[&id],
            conversion
        );
        assert!(save_quota_conversion(&path, original, &id, None, &conversion).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
        let invalid = crate::resources::QuotaConversion {
            quota_per_unit: Some("0".into()),
            currency: Some("USD".into()),
        };
        assert!(save_quota_conversion(&path, &saved, &id, None, &invalid).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
        save_quota_conversion(&path, &saved, &id, None, &Default::default()).unwrap();
        assert!(load_config(&path, true).unwrap().provider_usage.is_empty());

        let resource = format!(
            "{original}\n[[resources]]\nid='wallet'\nservice='new_api_account'\nbase_url='https://relay.invalid'\ncredential_env='ACCOUNT_TOKEN'\nprofile_ids=['{id}']\noptions={{user_id=42}}\n"
        );
        std::fs::write(&path, &resource).unwrap();
        save_quota_conversion(&path, &resource, &id, Some("wallet"), &conversion).unwrap();
        let config = load_config(&path, true).unwrap();
        assert_eq!(config.resources[0].options["user_id"], 42);
        assert_eq!(config.resources[0].options["quota_per_unit"], "500000");
        assert_eq!(config.resources[0].credential_env, "ACCOUNT_TOKEN");
        assert!(config.provider_usage.is_empty());
        let saved = std::fs::read_to_string(&path).unwrap();
        save_quota_conversion(&path, &saved, &id, Some("wallet"), &Default::default()).unwrap();
        assert_eq!(
            load_config(&path, true).unwrap().resources[0].options,
            serde_json::json!({"user_id":42})
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_conversion_requires_an_explicit_valid_currency_and_ratio() {
        let id = uuid::Uuid::new_v4();
        for options in [
            "quota_per_unit='500000'",
            "currency='USD'",
            "quota_per_unit='-1'\ncurrency='USD'",
            "quota_per_unit='500000'\ncurrency='usd'",
            "quota_per_unit=true\ncurrency='USD'",
        ] {
            assert!(parse_config(&format!("[provider_usage.'{id}']\n{options}")).is_err());
        }
        assert!(
            parse_config("[provider_usage.wrong]\nquota_per_unit='500000'\ncurrency='USD'")
                .is_err()
        );
        assert!(parse_config("").unwrap().provider_usage.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn quota_conversion_save_preserves_config_symlinks_and_rejects_dangling_links() {
        let root = std::env::temp_dir().join(format!("usage-config-link-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("dotfiles.toml");
        let link = root.join("config.toml");
        std::fs::write(&target, "# managed config\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let conversion = crate::resources::QuotaConversion {
            quota_per_unit: Some("500000".into()),
            currency: Some("USD".into()),
        };
        save_quota_conversion(&link, "# managed config\n", &id, None, &conversion).unwrap();
        assert!(link.is_symlink());
        assert_eq!(
            load_config(&target, true).unwrap().provider_usage[&id],
            conversion
        );
        std::fs::remove_file(&target).unwrap();
        assert!(save_quota_conversion(&link, "", &id, None, &conversion).is_err());
        assert!(link.is_symlink());
        assert!(!target.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolved_existing_file_and_directory_keep_valid_paths() {
        let root = std::env::temp_dir().join(format!("usage-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("log.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        for path in [&file, &root] {
            let resolved = resolved_data_path(path);
            assert_eq!(
                resolved.as_os_str(),
                path.canonicalize().unwrap().as_os_str()
            );
            assert!(resolved.exists());
        }
        assert!(resolved_data_path(&file).is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn old_both_config_uses_single_bar_view() {
        let raw: RawConfig = toml::from_str("style = 'both'").unwrap();
        assert_eq!(parse_raw(raw).unwrap().style, "bar");
        assert!(with_overrides(AppConfig::default(), Some("both"), None, None).is_err());
    }
    #[test]
    fn performance_binding_rejects_missing_or_unsupported_instances() {
        let text = r#"
[[instances]]
id = "work-agent"
provider = "claude"
path = "/nonexistent/usage-performance-claude"
[[resources]]
id = "api"
service = "new_api_token"
base_url = "https://relay.invalid"
credential_env = "RELAY_KEY"
[resources.inference]
base_url = "https://relay.invalid/v1"
credential_env = "RELAY_KEY"
performance_instance = "work-agent"
"#;
        assert!(parse_raw(toml::from_str(text).unwrap()).is_ok());
        assert!(
            parse_raw(
                toml::from_str(&text.replace(
                    "performance_instance = \"work-agent\"",
                    "performance_instance = \"claude\""
                ))
                .unwrap()
            )
            .is_err()
        );
        assert!(
            parse_raw(
                toml::from_str(&text.replace("provider = \"claude\"", "provider = \"grok\""))
                    .unwrap()
            )
            .is_err()
        );
    }
    #[test]
    fn stage_two_config_is_optional_and_validates_instances_and_thresholds() {
        let config = parse_raw(toml::from_str(&default_toml("bar")).unwrap()).unwrap();
        assert!(!config.alerts.enabled);
        assert!(config.instances.is_empty());
        assert_eq!(config.resume, ResumeMode::Auto);
        let text = r#"[[instances]]
id = "work-codex"
provider = "codex"
path = "/tmp/usage-work-codex"
[alerts]
enabled = true
remaining_percent = 20
[sessions]
resume = "current"
"#;
        let config = parse_raw(toml::from_str(text).unwrap()).unwrap();
        assert_eq!(config.instances[0].id, "work-codex");
        assert_eq!(config.resume, ResumeMode::Current);
        assert_eq!(config.alerts.remaining_percent, 20);
        assert!(parse_raw(toml::from_str("[alerts]\nremaining_percent = 0").unwrap()).is_err());
        assert!(parse_raw(toml::from_str(&text.replace("work-codex", "codex")).unwrap()).is_err());
        assert!(
            parse_raw(toml::from_str(&text.replace("/tmp/usage-work-codex", "~/.codex")).unwrap())
                .is_err()
        );
        assert!(toml::from_str::<RawConfig>("[sessions]\nresume = 'unknown'").is_err());
    }
}
