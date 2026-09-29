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
        }
    }
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

[paths]
# 留空则使用 ~/.claude、~/.codex、~/.grok
claude = ""
codex = ""
grok = ""
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
    fn old_both_config_uses_single_bar_view() {
        let raw: RawConfig = toml::from_str("style = 'both'").unwrap();
        assert_eq!(parse_raw(raw).unwrap().style, "bar");
        assert!(with_overrides(AppConfig::default(), Some("both"), None, None).is_err());
    }
}
