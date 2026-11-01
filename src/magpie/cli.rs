use super::*;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum MagpieCommand {
    /// 检查连接与 Magpie 版本
    Status,
    /// 列出自动路由、自动供应来源及具体模型
    Models,
    /// 配置唯一的内置 Magpie；保留历史版本
    Configure {
        #[arg(long)]
        model: String,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        account: Option<String>,
    },
    /// 使用 --replace 原地迁移旧插件预设；不允许新增 Magpie Provider
    Bind {
        #[arg(long)]
        name: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        replace: String,
    },
    /// 查询共享账号额度；由 Magpie 访问供应商
    Quotas,
    /// 查询 Magpie 整体用量汇总，可能包含其他客户端和本地会话
    Summary {
        #[arg(long, default_value="today", value_parser=["today","week","month","all"])]
        period: String,
    },
    /// 读取 Magpie 请求观测，包含实际模型、账号、耗时和 Token
    Requests {
        #[arg(long, default_value="today", value_parser=["today","week","month","all"])]
        period: String,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t=50, value_parser=clap::value_parser!(u16).range(1..=500))]
        limit: u16,
        #[arg(long)]
        session: Option<String>,
    },
    /// 查询会话当前实际路由与命中规则
    Route { session: String },
}
pub async fn dispatch(
    address: Option<String>,
    key_env: Option<String>,
    command: MagpieCommand,
) -> Result<()> {
    let use_saved = address.is_none() && key_env.is_none();
    let key = match key_env {
        Some(name) => std::env::var(name).map_err(|_| "未设置 Magpie 网关密钥环境变量")?,
        None => LOCAL_KEY.into(),
    };
    let connection = if use_saved {
        let manager = Manager::open_read_only(crate::codex_manager::default_root())
            .map_err(|e| e.to_string())?;
        builtin_connection(manager.as_ref())?
    } else {
        Connection::new(&address.unwrap_or_else(discover_address), &key)?
    };
    let http = http();
    let value = match command {
        MagpieCommand::Configure {
            model,
            effort,
            account,
        } => {
            connection.hello(http.as_ref()).await?;
            if !connection
                .models(http.as_ref())
                .await?
                .iter()
                .any(|m| m.id == model)
            {
                return Err("Magpie 目录中没有此模型或路由组".into());
            }
            let profile = crate::background::blocking(move || {
                let mut manager = Manager::open_default().map_err(|e| e.to_string())?;
                connection.configure(&mut manager, &model, effort.as_deref(), account.as_deref())
            })
            .await
            .map_err(str::to_owned)??;
            serde_json::json!({"id":profile.id,"name":BUILTIN_NAME,"model":profile.model,"revision":profile.revision})
        }
        MagpieCommand::Status => {
            serde_json::to_value(connection.hello(http.as_ref()).await?).unwrap()
        }
        MagpieCommand::Models => {
            serde_json::to_value(connection.models(http.as_ref()).await?).unwrap()
        }
        MagpieCommand::Bind {
            name,
            model,
            effort,
            account,
            replace,
        } => {
            connection.hello(http.as_ref()).await?;
            if !connection
                .models(http.as_ref())
                .await?
                .iter()
                .any(|m| m.id == model)
            {
                return Err("Magpie 目录中没有此模型或路由组".into());
            }
            let profile = crate::background::blocking(move || {
                let mut manager = Manager::open_default().map_err(|e| e.to_string())?;
                connection.bind(
                    &mut manager,
                    &name,
                    &model,
                    effort.as_deref(),
                    account.as_deref(),
                    Some(&replace),
                )
            })
            .await
            .map_err(str::to_owned)??;
            serde_json::json!({"id":profile.id,"name":profile.name,"model":profile.model,"revision":profile.revision})
        }
        MagpieCommand::Quotas => {
            connection
                .get(http.as_ref(), "v1/magpie/quotas", &[])
                .await?
        }
        MagpieCommand::Summary { period } => {
            connection
                .get(http.as_ref(), "v1/magpie/usage", &[("period", &period)])
                .await?
        }
        MagpieCommand::Requests {
            period,
            offset,
            limit,
            session,
        } => {
            let offset = offset.to_string();
            let limit = limit.to_string();
            let mut params = vec![
                ("period", period.as_str()),
                ("offset", offset.as_str()),
                ("limit", limit.as_str()),
                ("via", "through"),
            ];
            if let Some(session) = &session {
                params.push(("q", session));
            }
            let mut value = connection
                .get(http.as_ref(), "v1/magpie/usage/requests", &params)
                .await?;
            if let Some(session) = &session {
                if let Some(rows) = value["rows"].as_array_mut() {
                    rows.retain(|r| r["session"].as_str() == Some(session));
                }
                // Server-side q is a broad search, so its aggregates are not session totals.
                value = serde_json::json!({"session":session,"rows":value["rows"],"offset":offset,"scope":"session_matches_in_page"});
            }
            value
        }
        MagpieCommand::Route { session } => {
            connection
                .get(http.as_ref(), "v1/magpie/route", &[("session", &session)])
                .await?
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|_| "无法输出 Magpie 数据")?
    );
    Ok(())
}
