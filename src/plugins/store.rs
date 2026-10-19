use super::*;
use fs2::FileExt;
use std::io::Read;

#[derive(Clone, Serialize, Deserialize)]
pub struct Plugin {
    pub id: String,
    pub package: String,
    pub version: String,
    pub entry: PathBuf,
    pub runtime: PathBuf,
    pub enabled: bool,
    #[serde(default)]
    pub revision: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub plugin: String,
    pub provider: String,
    pub name: String,
}
#[derive(Serialize, Deserialize)]
struct Credential {
    account: Account,
    generation: u64,
    auth: Value,
    inference_key: String,
}
#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
}
pub fn root() -> PathBuf {
    std::env::var_os("USAGE_PLUGIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| crate::config::home_dir().join(".local/share"))
                .join("usage/plugins")
        })
}
pub(crate) fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|_| "无法读取插件配置")?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取插件配置")?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err("插件配置过大".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "插件配置损坏，原文件已保留".into())
}
pub(super) fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    crate::codex_manager::private_write(
        path,
        &serde_json::to_vec_pretty(value).map_err(|_| "插件配置编码失败")?,
    )
    .map_err(|_| "插件配置保存失败".into())
}
impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    fn uuid(id: &str) -> Result<()> {
        uuid::Uuid::parse_str(id)
            .map(|_| ())
            .map_err(|_| "无效的插件账号 ID".into())
    }
    fn account_path(&self, id: &str) -> Result<PathBuf> {
        Self::uuid(id)?;
        Ok(self.root.join("accounts").join(format!("{id}.json")))
    }
    fn locked(&self) -> Result<fs::File> {
        fs::create_dir_all(&self.root).map_err(|_| "无法创建插件目录")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))
                .map_err(|_| "无法保护插件目录")?;
        }
        let path = self.root.join("store.lock");
        crate::codex_manager::private_create(&path, b"").map_err(|_| "无法创建插件配置锁")?;
        let file = fs::File::open(path).map_err(|_| "无法打开插件配置锁")?;
        file.lock_exclusive().map_err(|_| "无法锁定插件配置")?;
        Ok(file)
    }
    pub fn plugins(&self) -> Result<Vec<Plugin>> {
        let path = self.root.join("plugins.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        read(&path)
    }
    pub fn plugin(&self, id: &str) -> Result<Plugin> {
        self.plugins()?
            .into_iter()
            .find(|p| p.id == id && p.enabled)
            .ok_or_else(|| "插件不存在或已停用".into())
    }
    pub fn register(&self, mut plugin: Plugin) -> Result<()> {
        if plugin.id.is_empty()
            || plugin.id.len() > 80
            || !plugin
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("插件 ID 只能包含字母、数字、下划线或短横线".into());
        }
        let _lock = self.locked()?;
        let mut plugins = self.plugins()?;
        if let Some(p) = plugins.iter_mut().find(|p| p.id == plugin.id) {
            plugin.revision = p.revision.checked_add(1).ok_or("插件配置版本溢出")?;
            *p = plugin;
        } else {
            plugins.push(plugin);
        }
        save(&self.root.join("plugins.json"), &plugins)
    }
    pub fn accounts(&self) -> Result<Vec<Account>> {
        let path = self.root.join("accounts");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut accounts = Vec::new();
        for entry in fs::read_dir(path)
            .map_err(|_| "无法读取插件账号目录")?
            .take(1000)
        {
            let entry = entry.map_err(|_| "无法读取插件账号")?;
            if entry.path().extension().is_some_and(|e| e == "json") {
                match read::<Credential>(&entry.path()) {
                    Ok(c)
                        if entry.path().file_stem().and_then(|s| s.to_str())
                            == Some(c.account.id.as_str()) =>
                    {
                        accounts.push(c.account)
                    }
                    _ => tracing::warn!(
                        event = "plugin_account_read_failed",
                        reason = "invalid_account_file"
                    ),
                }
            }
        }
        accounts.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(accounts)
    }
    pub fn account(&self, id: &str) -> Result<Account> {
        Ok(read::<Credential>(&self.account_path(id)?)?.account)
    }
    pub fn create_account(
        &self,
        plugin: &str,
        provider: &str,
        name: &str,
        auth: Value,
    ) -> Result<Account> {
        self.plugin(plugin)?;
        if provider.is_empty() || provider.len() > 200 || name.chars().any(char::is_control) {
            return Err("插件账号信息无效".into());
        }
        let _lock = self.locked()?;
        if let Some(identity) = super::identity::key(&auth) {
            for existing in self.accounts()? {
                if existing.plugin != plugin || existing.provider != provider {
                    continue;
                }
                let path = self.account_path(&existing.id)?;
                let mut credential: Credential = read(&path)?;
                if super::identity::key(&credential.auth).as_ref() != Some(&identity) {
                    continue;
                }
                credential.generation = credential
                    .generation
                    .checked_add(1)
                    .ok_or("插件凭据版本溢出")?;
                credential.auth = auth;
                save(&path, &credential)?;
                tracing::info!(event="plugin_account_reused", plugin_id=plugin, backend=provider, account_id=%existing.id, generation=credential.generation);
                return Ok(existing);
            }
        }
        let account = Account {
            id: uuid::Uuid::new_v4().to_string(),
            plugin: plugin.into(),
            provider: provider.into(),
            name: name.into(),
        };
        save(
            &self.account_path(&account.id)?,
            &Credential {
                account: account.clone(),
                generation: 1,
                auth,
                inference_key: uuid::Uuid::new_v4().to_string(),
            },
        )?;
        Ok(account)
    }
    pub(super) fn credential(&self, plugin: &str, id: &str) -> Result<Value> {
        let c: Credential = read(&self.account_path(id)?)?;
        if c.account.plugin != plugin || c.account.id != id {
            return Err("插件账号作用域不匹配".into());
        }
        Ok(serde_json::json!({"auth":c.auth,"generation":c.generation}))
    }
    pub(super) fn update(
        &self,
        plugin: &str,
        id: &str,
        generation: u64,
        auth: Value,
    ) -> Result<u64> {
        let _lock = self.locked()?;
        let path = self.account_path(id)?;
        let mut c: Credential = read(&path)?;
        if c.account.plugin != plugin || c.account.id != id || c.generation != generation {
            return Err("插件凭据已变更，请重新读取后再操作".into());
        }
        c.generation = c.generation.checked_add(1).ok_or("插件凭据版本溢出")?;
        c.auth = auth;
        save(&path, &c)?;
        Ok(c.generation)
    }
    pub fn inference_key(&self, id: &str) -> Result<String> {
        Ok(read::<Credential>(&self.account_path(id)?)?.inference_key)
    }
}
