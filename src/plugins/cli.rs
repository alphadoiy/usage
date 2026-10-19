use super::*;
use clap::Subcommand;
#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// 查询签到状态；--confirm 明确提交签到（仅支持具有此能力的账号）。
    Checkin {
        account: String,
        #[arg(long)]
        confirm: bool,
    },

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
        PluginCommand::Checkin { account, confirm } => print(&call(serde_json::json!({"operation":"action","action":"checkin","phase":if confirm {"execute"}else{"preview"},"account":account})).await?)?,

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
            super::service::register(&store, id, entry, runtime, "local".into(), "local".into())?
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
            super::install(package, id, runtime).await?;
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
            let adapter = super::auth::adapter(&plugin, &provider, &descriptors)?;
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
            let p = super::bind(&account, &name, &model)?;
            println!("已绑定 {} · {} · Codex Provider {}", a.name, model, p.id);
        }
    }
    Ok(0)
}
