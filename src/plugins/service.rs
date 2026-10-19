//! Shared plugin actions used by CLI and interactive provider management.
use super::*;
use std::process::Command;

fn find_runtime(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let path = explicit.unwrap_or_else(|| {
        if std::process::Command::new("bun")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            PathBuf::from("bun")
        } else {
            PathBuf::from("node")
        }
    });
    let mut command = Command::new(&path);
    command.arg("--version");
    let probe = crate::credentials::output_with_timeout(command, std::time::Duration::from_secs(5));
    if !probe.is_some_and(|o| o.status.success()) {
        return Err("需要 Bun 或 Node 22+；通过 --runtime 指定可执行文件".into());
    }
    Ok(path)
}
pub(super) fn register(
    store: &store::Store,
    id: String,
    entry: PathBuf,
    runtime: Option<PathBuf>,
    package: String,
    version: String,
) -> Result<()> {
    let entry = entry.canonicalize().map_err(|_| "插件入口不存在")?;
    if !entry.is_file() {
        return Err("插件入口不是文件".into());
    }
    store.register(store::Plugin {
        id,
        entry,
        runtime: find_runtime(runtime)?,
        package,
        version,
        enabled: true,
        revision: 1,
    })
}

pub async fn install(package: String, id: String, runtime: Option<PathBuf>) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let store = store::Store::new(store::root());
        let (name, version) = package
            .rsplit_once('@')
            .filter(|(n, v)| !n.is_empty() && semver::Version::parse(v).is_ok())
            .ok_or("请指定固定 npm 版本，例如 @magpie-community/opencode-workbuddy-auth@0.1.24")?;
        if name.split('/').any(|s| s == "..")
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"@/._-".contains(&b))
        {
            return Err("无效 npm 包名".into());
        }
        let candidate = store
            .root
            .join("packages")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&candidate).map_err(|_| "无法创建插件安装目录")?;
        let mut command = Command::new("npm");
        command
            .args([
                "install",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--prefix",
            ])
            .arg(&candidate)
            .arg(&package);
        let output =
            crate::credentials::output_with_timeout(command, std::time::Duration::from_secs(120))
                .ok_or("npm 安装超时或无法执行")?;
        if !output.status.success() {
            return Err("npm 安装失败；请检查包版本和网络".into());
        }
        let base = candidate.join("node_modules").join(name);
        let metadata: Value = store::read(&base.join("package.json"))?;
        if metadata["name"] != name || metadata["version"] != version {
            return Err("安装结果与固定包版本不符".into());
        }
        let exports = &metadata["exports"];
        let entry = exports
            .get("./server")
            .and_then(|v| v.as_str().or_else(|| v["import"].as_str()))
            .or_else(|| metadata["main"].as_str())
            .or_else(|| exports.as_str())
            .or_else(|| {
                exports
                    .get(".")
                    .and_then(|v| v.as_str().or_else(|| v["import"].as_str()))
            })
            .unwrap_or("index.js");
        register(
            &store,
            id,
            base.join(entry),
            runtime,
            name.into(),
            version.into(),
        )
    })
    .await
    .map_err(|_| "插件安装任务中断")?
}

pub fn bind(account: &str, name: &str, model: &str) -> Result<crate::codex_manager::Profile> {
    let store = store::Store::new(store::root());
    let a = store.account(account)?;
    store.plugin(&a.plugin)?;
    let text = crate::codex_manager::config::new_config(&account_endpoint(account), model, None)
        .map_err(|e| e.to_string())?;
    crate::codex_manager::Manager::open_default()
        .and_then(|mut manager| {
            manager.add(
                name,
                &text,
                &store
                    .inference_key(account)
                    .map_err(crate::codex_manager::Error::message)?,
            )
        })
        .map_err(|e| e.to_string())
}

/// Resolve a first-class service to its host implementation without replacing unrelated installs.
pub async fn ensure_preset(
    preset: &'static crate::provider_catalog::presets::Preset,
) -> Result<store::Plugin> {
    let store = store::Store::new(store::root());
    let installed = store.plugins()?;
    if let Some(mut plugin) = installed
        .iter()
        .filter(|p| p.package == preset.package)
        .min_by_key(|p| (!p.enabled, p.id != preset.id))
        .cloned()
    {
        if !plugin.enabled {
            plugin.enabled = true;
            store.register(plugin.clone())?;
        }
        return store.plugin(&plugin.id);
    }
    let id = if installed.iter().any(|p| p.id == preset.id) {
        format!("{}-{}", preset.id, uuid::Uuid::new_v4().simple())
    } else {
        preset.id.into()
    };
    install(
        format!("{}@{}", preset.package, preset.version),
        id.clone(),
        None,
    )
    .await?;
    store.plugin(&id)
}
