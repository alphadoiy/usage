use super::*;
use clap::Subcommand;
use std::process::Command;
#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// 安装固定版本的 npm Provider 插件，安装脚本不执行
    Install {
        package: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        runtime: Option<PathBuf>,
    },
    /// 注册本地插件入口，供开发和兼容性验证
    Add {
        id: String,
        entry: PathBuf,
        #[arg(long)]
        runtime: Option<PathBuf>,
    },
    /// 列出已安装插件，不加载代码或请求网络
    List,
    /// 发现插件支持的上游、登录方式和能力
    Providers {
        plugin: String,
    },
    /// 查看插件账号（不输出凭据）
    Accounts,
    /// 登录一个插件上游；--input key=value 可传登录表单参数
    Login {
        plugin: String,
        provider: String,
        #[arg(long, default_value = "0")]
        method: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "input")]
        inputs: Vec<String>,
        #[arg(long)]
        no_browser: bool,
        #[arg(long)]
        key_stdin: bool,
    },
    Models {
        account: String,
    },
    Usage {
        account: String,
    },
    /// 绑定为一个托管 Codex Provider；从首页项目或 codex run 启动
    Bind {
        account: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        model: String,
    },
    Enable {
        plugin: String,
    },
    Disable {
        plugin: String,
    },
}
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
fn register(
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
fn print(value: &impl Serialize) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|_| "插件输出编码失败")?
    );
    Ok(())
}
pub async fn dispatch(command: PluginCommand) -> Result<i32> {
    let store = store::Store::new(store::root());
    let enabled = matches!(&command, PluginCommand::Enable { .. });
    match command {
        PluginCommand::List => print(&store.plugins()?)?,
        PluginCommand::Accounts => print(&store.accounts()?)?,
        PluginCommand::Providers { plugin } => {
            print(&call(serde_json::json!({"operation":"providers","plugin":plugin})).await?)?
        }
        PluginCommand::Models { account } => {
            print(&call(serde_json::json!({"operation":"models","account":account})).await?)?
        }
        PluginCommand::Usage { account } => {
            print(&call(serde_json::json!({"operation":"usage","account":account})).await?)?
        }
        PluginCommand::Add { id, entry, runtime } => {
            register(&store, id, entry, runtime, "local".into(), "local".into())?
        }
        PluginCommand::Enable { plugin } | PluginCommand::Disable { plugin } => {
            let mut p = store
                .plugins()?
                .into_iter()
                .find(|p| p.id == plugin)
                .ok_or("插件不存在")?;
            p.enabled = enabled;
            store.register(p)?;
        }
        PluginCommand::Install {
            package,
            id,
            runtime,
        } => {
            tokio::task::spawn_blocking(move||->Result<()> {
                let (name,version)=package.rsplit_once('@').filter(|(n,v)|!n.is_empty()&&semver::Version::parse(v).is_ok()).ok_or("请指定固定 npm 版本，例如 @magpie-community/opencode-workbuddy-auth@0.1.24")?;
                if name.split('/').any(|s|s=="..")||!name.bytes().all(|b|b.is_ascii_alphanumeric()||b"@/._-".contains(&b)){return Err("无效 npm 包名".into());}
                let candidate=store.root.join("packages").join(uuid::Uuid::new_v4().to_string());
                fs::create_dir_all(&candidate).map_err(|_|"无法创建插件安装目录")?;
                let mut c=Command::new("npm");c.args(["install","--ignore-scripts","--no-audit","--no-fund","--prefix"]).arg(&candidate).arg(&package);
                let out=crate::credentials::output_with_timeout(c,std::time::Duration::from_secs(120)).ok_or("npm 安装超时或无法执行")?;
                if !out.status.success(){return Err("npm 安装失败；请检查包版本和网络".into());}
                let base=candidate.join("node_modules").join(name);
                let metadata:Value=store::read(&base.join("package.json"))?;
                if metadata["name"]!=name||metadata["version"]!=version{return Err("安装结果与固定包版本不符".into());}
                let exports=&metadata["exports"];
                let entry=exports.get("./server").and_then(|v|v.as_str().or_else(||v["import"].as_str()))
                    .or_else(||metadata["main"].as_str()).or_else(||exports.as_str())
                    .or_else(||exports.get(".").and_then(|v|v.as_str().or_else(||v["import"].as_str()))).unwrap_or("index.js");
                register(&store,id,base.join(entry),runtime,name.into(),version.into())
            }).await.map_err(|_|"插件安装任务中断")??;
        }
        PluginCommand::Login {
            plugin,
            provider,
            method,
            name,
            inputs,
            no_browser,
            key_stdin,
        } => {
            let mut values = serde_json::Map::new();
            for input in inputs {
                let (k, v) = input
                    .split_once('=')
                    .ok_or("登录参数格式为 --input key=value")?;
                values.insert(k.into(), Value::String(v.into()));
            }
            if key_stdin {
                use std::io::Read;
                let mut key = String::new();
                std::io::stdin()
                    .take(65537)
                    .read_to_string(&mut key)
                    .map_err(|_| "读取插件 API Key 失败")?;
                if key.len() > 65536 || key.trim().is_empty() {
                    return Err("插件 API Key 无效或过长".into());
                }
                values.insert("key".into(), Value::String(key.trim().into()));
            }
            let descriptors =
                call(serde_json::json!({"operation":"providers","plugin":plugin})).await?;
            let mut registry = crate::auth::builtin_registry();
            super::auth::register(&mut registry, &plugin, &descriptors)?;
            let service = format!("plugin:{plugin}:{provider}");
            let adapter = registry
                .create(
                    &service,
                    crate::auth::AuthContext {
                        root: store.root,
                        account: None,
                    },
                )
                .map_err(|e| e.to_string())?;
            let account = crate::auth::run_in_terminal(
                adapter.as_ref(),
                crate::auth::AuthRequest {
                    method,
                    name,
                    config: Some(
                        serde_json::json!({"inputs":values,"open_browser":!no_browser}).to_string(),
                    ),
                    program: PathBuf::new(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            println!("已登录：{} · {}", account.name, account.id);
        }
        PluginCommand::Bind {
            account,
            name,
            model,
        } => {
            let a = store.account(&account)?;
            store.plugin(&a.plugin)?;
            let text =
                crate::codex_manager::config::new_config(&account_endpoint(&account), &model, None)
                    .map_err(|e| e.to_string())?;
            let p = crate::codex_manager::Manager::open_default()
                .and_then(|mut m| {
                    m.add(
                        &name,
                        &text,
                        &store
                            .inference_key(&account)
                            .map_err(crate::codex_manager::Error::message)?,
                    )
                })
                .map_err(|e| e.to_string())?;
            println!("已绑定 {} · {} · Codex Provider {}", a.name, model, p.id);
        }
    }
    Ok(0)
}
