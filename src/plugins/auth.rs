use super::*;
use crate::auth::*;
use std::{sync::Arc, time::Duration};

/// Expected failures cross the local API as codes, never arbitrary plugin prose.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LoginFailure {
    DesktopEncrypted,
    DesktopNotSignedIn,
    TimedOut,
    #[serde(other)]
    Failed,
}
impl LoginFailure {
    pub fn from_result(result: &Value) -> Self {
        let message = result["error"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if message.contains("desktop") && message.contains("encrypted") {
            Self::DesktopEncrypted
        } else if message.contains("desktop isn't signed in") {
            Self::DesktopNotSignedIn
        } else if message.contains("sign-in timed out") {
            Self::TimedOut
        } else {
            Self::Failed
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Self::DesktopEncrypted => {
                "桌面登录凭据已加密，插件无法导入；请选择 account (browser) 浏览器登录。"
            }
            Self::DesktopNotSignedIn => {
                "桌面端尚未登录；请先登录桌面端，或选择 account (browser) 浏览器登录。"
            }
            Self::TimedOut => "浏览器授权已超时，请重新发起登录。",
            Self::Failed => "插件登录失败，请更换登录方式或重试。",
        }
    }
    pub fn code(self) -> &'static str {
        match self {
            Self::DesktopEncrypted => "desktop_encrypted",
            Self::DesktopNotSignedIn => "desktop_not_signed_in",
            Self::TimedOut => "timed_out",
            Self::Failed => "failed",
        }
    }
}

fn authorization_url(start: &Value) -> Result<Option<reqwest::Url>> {
    let method = start["method"].as_str().ok_or("插件未返回授权方式")?;
    if !matches!(method, "auto" | "code") {
        return Err("插件授权方式不受支持".into());
    }
    let url = match start.get("url") {
        None | Some(Value::Null) => "",
        Some(Value::String(url)) => url.trim(),
        _ => return Err("插件登录地址格式无效".into()),
    };
    if url.is_empty() {
        // Local/desktop automatic callbacks do not open a browser. WorkBuddy's
        // desktop method explicitly returns an empty URL under this contract.
        return if method == "auto" {
            Ok(None)
        } else {
            Err("验证码登录缺少登录地址".into())
        };
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| "插件登录地址无效")?;
    if parsed.scheme() != "https"
        && !(parsed.scheme() == "http" && parsed.host_str() == Some("localhost"))
    {
        return Err("插件登录地址不安全".into());
    }
    Ok(Some(parsed))
}

struct Adapter {
    context: AuthContext,
    plugin: String,
    provider: String,
    service: String,
    methods: Vec<AuthMethod>,
}
pub fn adapter(plugin: &str, provider: &str, descriptors: &Value) -> Result<Arc<dyn AuthAdapter>> {
    // Only this plugin's factory is needed; the built-in backends are not.
    let mut registry = crate::auth::AuthRegistry::new();
    register(&mut registry, plugin, descriptors)?;
    registry
        .create(
            &format!("plugin:{plugin}:{provider}"),
            AuthContext {
                root: store::root(),
                account: None,
            },
        )
        .map_err(|e| e.to_string())
}
impl AuthAdapter for Adapter {
    fn service(&self) -> &str {
        &self.service
    }
    fn context(&self) -> &AuthContext {
        &self.context
    }
    fn methods(&self) -> &[AuthMethod] {
        &self.methods
    }
    fn start(&self, request: AuthRequest) -> std::result::Result<AuthTask, AuthError> {
        if self.context.account.is_some() {
            return Err(AuthError::Failed(
                "插件登录创建独立账号；现有账号凭据不会被覆盖".into(),
            ));
        }
        if !self.methods.iter().any(|m| m.id == request.method) {
            return Err(AuthError::UnsupportedMethod);
        }
        if self.context.root != store::root() {
            return Err(AuthError::Failed("插件认证目录与运行时不一致".into()));
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| AuthError::Failed("插件登录需要应用运行时".into()))?;
        let plugin = self.plugin.clone();
        let provider = self.provider.clone();
        let inputs = request
            .config
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .map_err(|_| AuthError::Failed("插件登录参数无效".into()))?
            .unwrap_or_else(|| serde_json::json!({}));
        AuthTask::spawn(move |control| {
            handle
                .block_on(login(
                    plugin,
                    provider,
                    request.method,
                    request.name,
                    inputs["inputs"].clone(),
                    inputs["open_browser"].as_bool().unwrap_or(true),
                    &control,
                ))
                .map_err(AuthError::Failed)
        })
    }
}
/// Register backend types from an already discovered capability snapshot. Factories do no I/O.
pub fn register(registry: &mut AuthRegistry, plugin: &str, descriptors: &Value) -> Result<()> {
    for descriptor in descriptors.as_array().ok_or("插件能力描述无效")? {
        let provider = descriptor["id"]
            .as_str()
            .ok_or("插件未声明上游 ID")?
            .to_owned();
        let service = format!("plugin:{plugin}:{provider}");
        let methods = descriptor["methods"]
            .as_array()
            .ok_or("插件未声明登录方法")?
            .iter()
            .map(|m| {
                Ok(AuthMethod {
                    id: m["id"]
                        .as_str()
                        .ok_or("登录方法 ID 无效")?
                        .to_owned()
                        .into(),
                    label: m["label"]
                        .as_str()
                        .ok_or("登录方法名称无效")?
                        .to_owned()
                        .into(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let plugin = plugin.to_owned();
        let id = service.clone();
        registry
            .register(&service, move |context| {
                Ok(Arc::new(Adapter {
                    context,
                    plugin: plugin.clone(),
                    provider: provider.clone(),
                    service: id.clone(),
                    methods: methods.clone(),
                }) as Arc<dyn AuthAdapter>)
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(super) async fn login(
    plugin: String,
    provider: String,
    method: String,
    name: Option<String>,
    inputs: Value,
    open_browser: bool,
    control: &AuthControl,
) -> Result<AuthAccount> {
    let mut base = serde_json::json!({"plugin":plugin,"provider":provider,"method":method,"name":name,"inputs":inputs});
    base["operation"] = "login.begin".into();
    control.checkpoint().map_err(|e| e.to_string())?;
    let start = call(base.clone()).await?;
    let session = start["session"].clone();
    let mut finish = base.clone();
    finish["session"] = session.clone();
    let operation = async {
        if start["ready"] != true {
            let url = authorization_url(&start).inspect_err(|_| {
                tracing::warn!(
                    event = "plugin_login_rejected",
                    plugin_id = plugin,
                    backend = provider,
                    method_id = method,
                    reason = "invalid_authorization_response"
                );
            })?;
            tracing::info!(
                event = "plugin_login_started",
                plugin_id = plugin,
                backend = provider,
                method_id = method,
                flow = if url.is_none() {
                    "local"
                } else if start["method"] == "code" {
                    "code"
                } else {
                    "browser"
                }
            );
            println!(
                "{}",
                start["instructions"]
                    .as_str()
                    .unwrap_or("请在浏览器完成登录")
            );
            if let Some(url) = &url {
                #[cfg(target_os = "macos")]
                {
                    if open_browser {
                        let _ = std::process::Command::new("open")
                            .arg(url.as_str())
                            .status();
                    } else {
                        println!("{url}");
                    }
                }
                #[cfg(not(target_os = "macos"))]
                println!("{url}");
            }
            control.waiting().map_err(|e| e.to_string())?;
            finish["operation"] = "login.finish".into();
            if start["method"] == "code" {
                use tokio::io::AsyncBufReadExt;
                eprintln!("输入授权验证码：");
                let mut code = String::new();
                tokio::io::BufReader::new(tokio::io::stdin())
                    .read_line(&mut code)
                    .await
                    .map_err(|_| "读取验证码失败")?;
                finish["code"] = code.trim().into();
            }
            call(finish.clone()).await?;
        }
        control.begin_save().map_err(|e| e.to_string())?;
        finish["operation"] = "login.commit".into();
        let account = call(finish.clone()).await?;
        Ok::<_, String>(AuthAccount {
            id: account["id"].as_str().ok_or("插件账号无效")?.into(),
            name: account["name"].as_str().unwrap_or("Plugin account").into(),
        })
    };
    tokio::select! {
        result=operation=>{
            if result.is_err(){base["operation"]="login.cancel".into();base["session"]=session.clone();let _=call(base).await;}
            result
        },
        _=async {loop {tokio::time::sleep(Duration::from_millis(50)).await;if control.checkpoint().is_err(){break;}}}=>{
            base["operation"]="login.cancel".into();base["session"]=session;let _=call(base).await;Err("插件登录已取消".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn only_automatic_callbacks_can_skip_the_browser_url() {
        for value in [json!({"method":"auto","url":""}), json!({"method":"auto"})] {
            assert_eq!(authorization_url(&value).unwrap(), None);
        }
        assert!(authorization_url(&json!({"method":"code","url":""})).is_err());
        assert!(authorization_url(&json!({"method":"unknown","url":""})).is_err());
        for method in ["auto", "code"] {
            for url in [
                "invalid-url",
                "file:///tmp/login",
                "javascript:alert(1)",
                "http://example.test/login",
            ] {
                assert!(authorization_url(&json!({"method":method,"url":url})).is_err());
            }
            for url in ["https://example.test/login", "http://localhost:1234/login"] {
                assert!(
                    authorization_url(&json!({"method":method,"url":url}))
                        .unwrap()
                        .is_some()
                );
            }
        }
    }
}
