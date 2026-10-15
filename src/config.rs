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
    let raw: RawConfig = toml::from_str(&text)
        .map_err(|error| ConfigError(format!("配置不是合法的 TOML: {error}")))?;
    parse_raw(raw)
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

pub fn write_style(path: &Path, style: &str) -> Result<(), ConfigError> {
    ensure_style(style)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ConfigError(format!("无法创建配置目录: {error}")))?;
    }
    if !path.exists() {
        return std::fs::write(path, default_toml(style))
            .map_err(|error| ConfigError(format!("无法写入配置: {error}")));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| ConfigError(format!("无法读取配置: {error}")))?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|error| ConfigError(format!("配置不是合法的 TOML: {error}")))?;
    doc["style"] = toml_edit::value(style);
    std::fs::write(path, doc.to_string())
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
