//! User context is inherited independently of the account's routing and credentials.
use fs2::FileExt;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use super::{Error, Result, config, private_create, private_write};

#[cfg(test)]
mod tests;

pub(super) fn base_home(root: &Path) -> PathBuf {
    if let Some(home) = std::env::var_os("USAGE_CODEX_BASE_HOME").filter(|s| !s.is_empty()) {
        return crate::config::resolved_data_path(Path::new(&home));
    }
    #[cfg(test)]
    return root.parent().unwrap().join("user-codex");
    #[cfg(not(test))]
    {
        let home = std::env::var_os("CODEX_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .map(|home| {
                crate::codex_native::SessionStore::resolve(&home)
                    .root()
                    .to_path_buf()
            })
            .filter(|p| {
                !p.join("usage-binding.json").exists()
                    && !crate::config::resolved_data_path(p).starts_with(root.join("homes"))
            })
            .unwrap_or_else(|| crate::config::home_dir().join(".codex"));
        crate::config::resolved_data_path(&home)
    }
}

fn read_config(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) if text.len() <= 1024 * 1024 => Ok(text),
        Ok(_) => Err(Error::message("Codex 配置文件过大")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn preferences(text: &str, base: &Path) -> Result<String> {
    let mut doc = config::parse(text)?;
    // Legacy default profiles are part of an ordinary `codex` invocation too.
    if let Some(profile) = doc.get("profile").and_then(toml_edit::Item::as_str)
        && let Some(settings) = doc.get("profiles").and_then(|p| p.get(profile))
    {
        let mut overlay = toml_edit::DocumentMut::new();
        if let Some(table) = settings.as_table_like() {
            for (key, value) in table.iter() {
                overlay[key] = value.clone();
            }
        }
        doc = config::parse(&config::merge(&doc.to_string(), &overlay.to_string())?)?;
    }
    // A global provider or selected profile must never replace the chosen account.
    fn strip_account(table: &mut dyn toml_edit::TableLike) {
        for key in [
            "model_provider",
            "model_providers",
            "openai_base_url",
            "chatgpt_base_url",
            "profile",
            "cli_auth_credentials_store",
            "forced_login_method",
            "forced_chatgpt_workspace_id",
            "experimental_bearer_token",
            "api_key",
            "OPENAI_API_KEY",
            "auth",
            "tokens",
            "sqlite_home",
        ] {
            table.remove(key);
        }
        for section in ["profiles", "projects"] {
            if let Some(entries) = table
                .get_mut(section)
                .and_then(toml_edit::Item::as_table_like_mut)
            {
                for (_, entry) in entries.iter_mut() {
                    if let Some(entry) = entry.as_table_like_mut() {
                        strip_account(entry);
                    }
                }
            }
        }
    }
    strip_account(doc.as_table_mut());
    fn resolve_path(item: &mut toml_edit::Item, base: &Path) {
        if let Some(value) = item.as_value_mut() {
            resolve_path_value(value, base);
        }
    }
    fn resolve_path_value(value: &mut toml_edit::Value, base: &Path) {
        if let Some(path) = value.as_str() {
            let path = match path.strip_prefix('~') {
                Some("") => crate::config::home_dir(),
                Some(rest) if rest.starts_with('/') => {
                    crate::config::home_dir().join(rest.trim_start_matches('/'))
                }
                _ => base.join(path),
            };
            *value = path.to_string_lossy().as_ref().into();
        }
    }
    fn resolve_path_array(item: &mut toml_edit::Item, base: &Path) {
        if let Some(paths) = item.as_array_mut() {
            for path in paths.iter_mut() {
                resolve_path_value(path, base);
            }
        }
    }
    fn resolve_paths(table: &mut dyn toml_edit::TableLike, base: &Path) {
        // ConfigToml/ConfigProfile's native AbsolutePathBuf fields (Codex 0.165).
        // Keep this list explicit: arbitrary strings, commands, and permission selectors
        // have different semantics and must not be treated as configuration-relative paths.
        for key in [
            "model_instructions_file",
            "model_catalog_json",
            "experimental_compact_prompt_file",
            "log_dir",
            "js_repl_node_path",
        ] {
            if let Some(item) = table.get_mut(key) {
                resolve_path(item, base);
            }
        }
        if let Some(item) = table.get_mut("js_repl_node_module_dirs") {
            resolve_path_array(item, base);
        }
        if let Some(item) = table
            .get_mut("sandbox_workspace_write")
            .and_then(|sandbox| sandbox.get_mut("writable_roots"))
        {
            resolve_path_array(item, base);
        }
        for (section, field) in [("agents", "config_file"), ("mcp_servers", "cwd")] {
            if let Some(entries) = table
                .get_mut(section)
                .and_then(toml_edit::Item::as_table_like_mut)
            {
                for (_, entry) in entries.iter_mut() {
                    if let Some(item) = entry.as_table_like_mut().and_then(|t| t.get_mut(field)) {
                        resolve_path(item, base);
                    }
                }
            }
        }
        if let Some(skills) = table.get_mut("skills").and_then(|s| s.get_mut("config")) {
            // Both [[skills.config]] and config = [{ path = ... }] are valid TOML.
            if let Some(skills) = skills.as_array_of_tables_mut() {
                for skill in skills.iter_mut() {
                    if let Some(path) = skill.get_mut("path") {
                        resolve_path(path, base);
                    }
                }
            } else if let Some(skills) = skills.as_array_mut() {
                for skill in skills.iter_mut() {
                    if let Some(path) = skill.as_inline_table_mut().and_then(|s| s.get_mut("path"))
                    {
                        resolve_path_value(path, base);
                    }
                }
            }
        }
        if let Some(otel) = table
            .get_mut("otel")
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            for exporter in ["exporter", "trace_exporter", "metrics_exporter"] {
                for transport in ["otlp-http", "otlp-grpc"] {
                    if let Some(tls) = otel
                        .get_mut(exporter)
                        .and_then(|e| e.get_mut(transport))
                        .and_then(|e| e.get_mut("tls"))
                        .and_then(toml_edit::Item::as_table_like_mut)
                    {
                        for field in ["ca-certificate", "client-certificate", "client-private-key"]
                        {
                            if let Some(path) = tls.get_mut(field) {
                                resolve_path(path, base);
                            }
                        }
                    }
                }
            }
        }
        if let Some(profiles) = table
            .get_mut("profiles")
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            for (_, profile) in profiles.iter_mut() {
                if let Some(profile) = profile.as_table_like_mut() {
                    resolve_paths(profile, base);
                }
            }
        }
    }
    resolve_paths(doc.as_table_mut(), base);
    Ok(doc.to_string())
}

/// Reapply the native CLI's edits (including deletions) over freshly inherited defaults.
fn rebase(old: &toml::Value, runtime: &toml::Value, next: &mut toml::Value) {
    if old == runtime {
        return;
    }
    if let (Some(old), Some(runtime), Some(next)) =
        (old.as_table(), runtime.as_table(), next.as_table_mut())
    {
        for key in old.keys() {
            if !runtime.contains_key(key) {
                next.remove(key);
            }
        }
        for (key, value) in runtime {
            match (old.get(key), next.get_mut(key)) {
                (Some(previous), Some(next)) => rebase(previous, value, next),
                (previous, _) if previous != Some(value) => {
                    next.insert(key.clone(), value.clone());
                }
                _ => {}
            }
        }
    } else {
        *next = runtime.clone();
    }
}

pub(super) fn prepare_config(root: &Path, home: &Path, path: &Path, saved: &str) -> Result<()> {
    // Multiple launches of the same account must update the inherited baseline together.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.join(".usage-context.lock"))?;
    lock.lock_exclusive()?;
    let base = base_home(root);
    if base.starts_with(home) {
        return Err(Error::message("全局 Codex Home 不能指向当前托管实例"));
    }
    let inherited = preferences(&read_config(&base.join("config.toml"))?, &base)?;
    let desired = config::merge(&inherited, saved)?;
    let snapshot = path.with_extension("usage-defaults.toml");
    if path.exists() {
        let runtime = read_config(path)?;
        if !config::matches_runtime_config(&runtime, saved) {
            let fields = config::runtime_differences(&runtime, saved);
            tracing::error!(event = "config_mismatch", reason = "account_settings_changed", changed_fields = ?fields);
            return Err(Error::message(format!(
                "账号路由或认证配置已改变（{}）；请通过 Usage 编辑保存新版本，原文件已保留",
                fields.join("、")
            )));
        }
        let previous = if snapshot.exists() {
            read_config(&snapshot)?
        } else {
            saved.to_string()
        };
        let parse = |text: &str| {
            toml::from_str::<toml::Value>(text).map_err(|_| Error::message("Codex 配置无法解析"))
        };
        let mut next = parse(&desired)?;
        rebase(&parse(&previous)?, &parse(&runtime)?, &mut next);
        if next != parse(&runtime)? {
            private_write(
                path,
                toml::to_string(&next)
                    .map_err(|_| Error::message("无法保存用户配置"))?
                    .as_bytes(),
            )?;
        }
    } else {
        private_create(path, desired.as_bytes())?;
    }
    private_write(&snapshot, desired.as_bytes())?;
    inherit_files(&base, home)
}

fn inherit_files(base: &Path, home: &Path) -> Result<()> {
    for name in [
        "AGENTS.md",
        "AGENTS.override.md",
        "hooks.json",
        ".credentials.json",
    ] {
        link_missing(&base.join(name), &home.join(name))?;
    }
    for name in [
        "skills", "rules", "agents", "prompts", "memories", "plugins", "hooks",
    ] {
        let source = base.join(name);
        if !source.is_dir() {
            continue;
        }
        let destination = home.join(name);
        if name != "skills" && fs::symlink_metadata(&destination).is_err() {
            link_missing(&source, &destination)?;
            continue;
        }
        fs::create_dir_all(&destination)?;
        for entry in fs::read_dir(&source)? {
            let entry = entry?;
            // Codex installs version-specific bundled skills itself.
            if name == "skills" && entry.file_name() == ".system" {
                continue;
            }
            link_missing(&entry.path(), &destination.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn link_missing(source: &Path, destination: &Path) -> Result<()> {
    if !source.exists() || fs::symlink_metadata(destination).is_ok() {
        return Ok(());
    }
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(source, destination);
    #[cfg(windows)]
    let result = if source.is_dir() {
        std::os::windows::fs::symlink_dir(source, destination)
    } else {
        std::os::windows::fs::symlink_file(source, destination)
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
}
