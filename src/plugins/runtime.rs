use super::*;
use axum::{
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
};

pub(crate) struct Host {
    port: u16,
    token: String,
    signature: String,
    _child: Mutex<Child>,
}
type HostSlots = HashMap<String, Arc<Mutex<Option<Arc<Host>>>>>;
type Capabilities = HashMap<String, (String, std::sync::Weak<Host>)>;
type AccountLocks = Mutex<HashMap<String, Arc<Mutex<()>>>>;
const LOGIN_TTL: Duration = Duration::from_secs(600);
#[derive(Clone)]
pub(crate) struct Runtime {
    store: store::Store,
    hosts: Arc<Mutex<HostSlots>>,
    capabilities: Arc<Mutex<Capabilities>>,
    accounts: Arc<AccountLocks>,
    action_locks: Arc<AccountLocks>,
    client: reqwest::Client,
    pub active: Arc<AtomicUsize>,
    broker: String,
    logins: Arc<Mutex<HashMap<String, Login>>>,
}
struct Login {
    plugin: String,
    provider: String,
    name: Option<String>,
    auth: Option<Value>,
    created: std::time::Instant,
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
fn failure(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({"error":{"type":"usage_plugin","message":message}})),
    )
        .into_response()
}
/// Only host-defined codes may enter logs; arbitrary plugin error prose is excluded.
fn safe_error_code(value: &Value) -> &'static str {
    const CODES: &[&str] = &[
        "account_scope_required",
        "credential_broker_failed",
        "credential_provider_mismatch",
        "unsupported_client_api",
        "duplicate_provider",
        "no_supported_provider_exports",
        "provider_not_found",
        "login_method_not_found",
        "invalid_login_input",
        "invalid_login_callback",
        "login_session_expired",
        "usage_unsupported",
        "operation_unsupported",
        "sdk_protocol_unsupported",
        "invalid_upstream",
        "request_too_large",
        "protocol",
        "cancelled",
        "hook",
    ];
    CODES
        .iter()
        .copied()
        .find(|code| value["error"]["code"].as_str() == Some(*code))
        .unwrap_or("hook")
}
impl Runtime {
    pub fn new(root: PathBuf, port: u16) -> Self {
        Self {
            store: store::Store::new(root),
            hosts: Default::default(),
            capabilities: Default::default(),
            accounts: Default::default(),
            action_locks: Default::default(),
            active: Default::default(),
            logins: Default::default(),
            broker: format!("http://127.0.0.1:{port}/_usage/plugins/credentials"),
            client: control_client().expect("local plugin client").clone(),
        }
    }
    /// Pending browser/code authorization owns a lease even between control requests.
    pub async fn has_work(&self, now: std::time::Instant) -> bool {
        let mut logins = self.logins.lock().await;
        logins.retain(|_, login| now.saturating_duration_since(login.created) < LOGIN_TTL);
        !logins.is_empty() || self.active.load(Ordering::Acquire) > 0
    }
    fn activity(&self) -> Active {
        self.active.fetch_add(1, Ordering::AcqRel);
        Active(self.active.clone())
    }
    async fn lock_account(&self, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        Self::lock_for(&self.accounts, id).await
    }
    async fn lock_action(&self, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        Self::lock_for(&self.action_locks, id).await
    }
    async fn lock_for(locks: &AccountLocks, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        // Release the map before waiting: one account's queue must not lock out others.
        let account = locks.lock().await.entry(id.into()).or_default().clone();
        account.lock_owned().await
    }
    async fn host(&self, id: &str) -> Result<Arc<Host>> {
        let plugin = self.store.plugin(id)?;
        let signature = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&plugin).map_err(|_| "插件配置无效")?)
        );
        let slot = self
            .hosts
            .lock()
            .await
            .entry(id.into())
            .or_default()
            .clone();
        let mut loaded = slot.lock().await;
        if let Some(host) = loaded.as_ref()
            && host.signature == signature
            && host
                ._child
                .lock()
                .await
                .try_wait()
                .map_err(|_| "插件进程状态读取失败")?
                .is_none()
        {
            return Ok(host.clone());
        }
        let dir = self.store.root.join("host").join(format!(
            "{:x}",
            Sha256::digest(format!(
                "{HOST}{PROTOCOL}{INFERENCE}{ACTIONS}{WORKBUDDY_ACTIONS}{TRAE_ACTIONS}{ACTION_CAPABILITIES}{ACTION_SUPPORT}"
            ))
        ));
        fs::create_dir_all(&dir).map_err(|_| "无法创建插件宿主目录")?;
        crate::codex_manager::private_write(&dir.join("host.mjs"), HOST.as_bytes())
            .map_err(|_| "无法保存插件宿主")?;
        crate::codex_manager::private_write(&dir.join("protocol.mjs"), PROTOCOL.as_bytes())
            .map_err(|_| "无法保存协议适配器")?;
        for (name, source) in [
            ("inference.mjs", INFERENCE),
            ("actions.mjs", ACTIONS),
            ("action_support.mjs", ACTION_SUPPORT),
            ("action_capabilities.json", ACTION_CAPABILITIES),
            ("workbuddy_actions.mjs", WORKBUDDY_ACTIONS),
            ("trae_actions.mjs", TRAE_ACTIONS),
        ] {
            crate::codex_manager::private_write(&dir.join(name), source.as_bytes())
                .map_err(|_| "无法保存插件账号操作适配器")?;
        }
        let token = uuid::Uuid::new_v4().to_string();
        let mut command = Command::new(&plugin.runtime);
        crate::codex_manager::clear_auth_env(command.as_std_mut());
        for key in [
            "NODE_OPTIONS",
            "NODE_PATH",
            "USAGE_CODEX_RUN_CONTEXT",
            "USAGE_CODEX_NOTIFY_CONTEXT",
        ] {
            command.env_remove(key);
        }
        command
            .arg(dir.join("host.mjs"))
            .arg(&plugin.entry)
            .arg(format!("{}/{}", self.broker, id))
            .arg(&token)
            .arg(&plugin.package)
            .current_dir(plugin.entry.parent().ok_or("插件入口无效")?)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|_| "无法启动插件运行时，请检查 Bun/Node 路径")?;
        let mut ready = String::new();
        let mut reader = BufReader::new(child.stdout.take().ok_or("插件输出不可用")?.take(4097));
        tokio::time::timeout(Duration::from_secs(15), reader.read_line(&mut ready))
            .await
            .map_err(|_| "插件加载超时")?
            .map_err(|_| "插件加载失败")?;
        if ready.len() > 4096 {
            return Err("插件启动响应过大".into());
        }
        let ready: Value = serde_json::from_str(&ready)
            .map_err(|_| "插件入口或宿主契约不兼容，请检查所需运行时")?;
        let port = ready["port"]
            .as_u64()
            .filter(|p| *p > 0 && *p <= 65535)
            .ok_or("插件端口无效")? as u16;
        let host = Arc::new(Host {
            port,
            token,
            signature,
            _child: Mutex::new(child),
        });
        *loaded = Some(host.clone());
        let mut capabilities = self.capabilities.lock().await;
        capabilities.retain(|_, (_, h)| h.strong_count() > 0);
        capabilities.insert(host.token.clone(), (id.into(), Arc::downgrade(&host)));
        tracing::info!(event="plugin_host_started",plugin_id=id,package=plugin.package,version=plugin.version,runtime=%plugin.runtime.display());
        Ok(host)
    }
    pub async fn credentials(&self, plugin: &str, token: &str, x: Value) -> Response {
        if !self
            .capabilities
            .lock()
            .await
            .get(token)
            .is_some_and(|(p, h)| p == plugin && h.strong_count() > 0)
        {
            return failure(StatusCode::UNAUTHORIZED, "Invalid plugin capability");
        }
        if x["operation"] == "diagnostic" {
            // The host is executable plugin code: accept typed, bounded fields only.
            let uuid = |key: &str| {
                x["context"][key]
                    .as_str()
                    .filter(|v| uuid::Uuid::parse_str(v).is_ok())
            };
            let pick = |key: &str, allowed: &[&str]| {
                x[key]
                    .as_str()
                    .filter(|v| allowed.contains(v))
                    .unwrap_or("unknown")
                    .to_owned()
            };
            tracing::info!(
                event = "plugin_inference_diagnostic",
                plugin_id = plugin,
                run_id = uuid("run"),
                request_id = uuid("request"),
                session_id = uuid("session"),
                account_id = x["account"]
                    .as_str()
                    .filter(|v| uuid::Uuid::parse_str(v).is_ok()),
                upstream_request_id =
                    x["upstream_request_id"].as_str().filter(|v| v.len() <= 128
                        && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')),
                vendor_code = x["vendor_code"]
                    .as_str()
                    .filter(|v| v.len() <= 10 && v.bytes().all(|b| b.is_ascii_digit())),
                stage = pick(
                    "stage",
                    &[
                        "upstream_response",
                        "upstream_stream",
                        "retry",
                        "response",
                        "converted",
                        "delivery"
                    ]
                ),
                outcome = pick(
                    "outcome",
                    &[
                        "complete",
                        "eof_without_terminal",
                        "interrupted",
                        "cancelled",
                        "retrying",
                        "returned",
                        "completed",
                        "incomplete",
                        "failed"
                    ]
                ),
                category = pick(
                    "category",
                    &["rate", "quota", "context", "upstream", "request"]
                ),
                reason = pick(
                    "reason",
                    &[
                        "missing_finish",
                        "missing_terminal",
                        "upstream_error",
                        "size_limit",
                        "invalid_tool_call",
                        "other"
                    ]
                ),
                finish_reason = pick(
                    "finish",
                    &[
                        "stop",
                        "tool_calls",
                        "length",
                        "content_filter",
                        "function_call"
                    ]
                ),
                http_status = x["status"].as_u64().filter(|v| (100..=599).contains(v)),
                attempt = x["attempt"].as_u64().filter(|v| *v <= 4),
                wait_ms = x["wait_ms"].as_u64().filter(|v| *v <= 8000),
                duration_ms = x["duration_ms"].as_u64(),
                terminal = x["terminal"].as_bool(),
                events = x["events"].as_u64(),
                tool_calls = x["tool_calls"].as_u64(),
                recovered_calls = x["recovered_calls"].as_u64(),
                text_chars = x["text_chars"].as_u64(),
                reasoning_chars = x["reasoning_chars"].as_u64(),
            );
            return axum::Json(serde_json::json!({})).into_response();
        }
        let store = self.store.clone();
        let plugin = plugin.to_owned();
        let result = tokio::task::spawn_blocking(move || -> Result<Value> {
            let id = x["account"].as_str().ok_or("缺少插件账号")?;
            match x["operation"].as_str() {
                Some("get") => store.credential(&plugin, id),
                Some("set") => store
                    .update(
                        &plugin,
                        id,
                        x["generation"].as_u64().ok_or("缺少凭据版本")?,
                        x["auth"].clone(),
                    )
                    .map(|g| serde_json::json!({"generation":g})),
                _ => Err("未知凭据操作".into()),
            }
        })
        .await;
        match result {
            Ok(Ok(v)) => axum::Json(v).into_response(),
            _ => failure(
                StatusCode::CONFLICT,
                "Credential scope or generation conflict",
            ),
        }
    }
    // Only credential preparation is serialized. Slow metadata and response bodies
    // must not hold the account's refresh coordinator.
    async fn prepare(&self, host: &Host, account: &str, provider: &str) -> Result<()> {
        let _lock = self.lock_account(account).await;
        let response = self
            .client
            .post(format!("http://127.0.0.1:{}/call", host.port))
            .bearer_auth(&host.token)
            .json(&serde_json::json!({"operation":"prepare","account":account,"provider":provider}))
            .timeout(Duration::from_secs(35))
            .send()
            .await
            .map_err(|_| "插件凭据准备失败或超时")?;
        if !response.status().is_success() {
            return Err("插件凭据准备失败".into());
        }
        response.bytes().await.map_err(|_| "插件凭据准备响应中断")?;
        Ok(())
    }
    pub async fn call(&self, mut x: Value) -> Result<Value> {
        let _activity = self.activity();
        if x["_root"]
            .as_str()
            .is_some_and(|p| Path::new(p) != self.store.root)
        {
            return Err("插件运行时目录已变化，请等待网关退出后重试".into());
        }
        let op = x["operation"].as_str().unwrap_or_default();
        if matches!(op, "login.commit" | "login.cancel") {
            let session = x["session"].as_str().ok_or("缺少登录会话")?;
            let mut logins = self.logins.lock().await;
            let login = logins.get(session).ok_or("登录会话已取消或过期")?;
            if Some(login.plugin.as_str()) != x["plugin"].as_str()
                || Some(login.provider.as_str()) != x["provider"].as_str()
            {
                return Err("登录会话作用域不匹配".into());
            }
            if op == "login.cancel" {
                let plugin = login.plugin.clone();
                let provider = login.provider.clone();
                logins.remove(session);
                drop(logins);
                if let Ok(host) = self.host(&plugin).await {
                    let _=self.client.post(format!("http://127.0.0.1:{}/call",host.port)).bearer_auth(&host.token).json(&serde_json::json!({"operation":"login.cancel","provider":provider,"session":session})).timeout(Duration::from_secs(3)).send().await;
                }
                return Ok(serde_json::json!({"cancelled":true}));
            }
            if login.created.elapsed() >= LOGIN_TTL {
                logins.remove(session);
                return Err("登录会话已过期".into());
            }
            let auth = login.auth.as_ref().ok_or("登录尚未完成")?;
            let name = login
                .name
                .as_deref()
                .or_else(|| auth["accountId"].as_str())
                .unwrap_or("Plugin account");
            let account =
                self.store
                    .create_account(&login.plugin, &login.provider, name, auth.clone())?;
            logins.remove(session);
            return serde_json::to_value(account).map_err(|_| "无法编码插件账号".into());
        }
        let account = x["account"].as_str().map(str::to_owned);
        if let Some(id) = &account {
            let a = self.store.account(id)?;
            x["plugin"] = Value::String(a.plugin);
            x["provider"] = Value::String(a.provider);
        }
        let plugin = x["plugin"].as_str().ok_or("缺少插件 ID")?.to_owned();
        let operation = x["operation"].as_str().ok_or("缺少插件操作")?.to_owned();
        let host = self.host(&plugin).await?;
        if let Some(id) = &account {
            self.prepare(&host, id, x["provider"].as_str().ok_or("缺少服务 ID")?)
                .await?;
        }
        // Serialize mutating actions across every CLI/TUI sharing this gateway/account.
        let _action_lock = if operation == "action" && x["phase"] == "execute" {
            let id = account.as_ref().ok_or("账号操作需要绑定账号")?;
            Some(self.lock_action(id).await)
        } else {
            None
        };
        let response = self
            .client
            .post(format!("http://127.0.0.1:{}/call", host.port))
            .bearer_auth(&host.token)
            .json(&x)
            .timeout(Duration::from_secs(if operation == "login.finish" {
                310
            } else if operation == "usage" {
                45
            } else {
                25
            }))
            .send()
            .await
            .map_err(|_| "插件调用中断或超时")?;
        let status = response.status();
        let mut bytes = Vec::new();
        {
            use futures_util::StreamExt;
            let mut chunks = response.bytes_stream();
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.map_err(|_| "插件控制响应中断")?;
                if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                    return Err("插件控制响应过大".into());
                }
                bytes.extend_from_slice(&chunk);
            }
        }
        if !status.is_success() {
            let detail = serde_json::from_slice::<Value>(&bytes).unwrap_or_default();
            let kind = match detail["error"]["kind"].as_str() {
                Some("protocol") => "protocol",
                Some("cancelled") => "cancelled",
                _ => "hook",
            };
            tracing::warn!(
                event = "plugin_hook_failed",
                plugin_id = plugin,
                account_id = account,
                hook = operation,
                http_status = status.as_u16(),
                error_kind = kind,
                error_code = safe_error_code(&detail),
                backend = x["provider"].as_str()
            );
            return Err("插件 hook 执行失败；请确认运行时、登录及网络配置".into());
        }
        let mut result: Value = serde_json::from_slice(&bytes).map_err(|_| "插件响应无效")?;
        if operation == "action" {
            let outcome = result["outcome"]
                .as_str()
                .filter(|v| {
                    [
                        "ready",
                        "claimed",
                        "done",
                        "ineligible",
                        "inactive",
                        "captcha",
                        "own-app",
                        "expired",
                        "failed",
                    ]
                    .contains(v)
                })
                .unwrap_or("unknown");
            let reason = result["reason"].as_str().filter(|v| {
                [
                    "unsupported",
                    "invalid_response",
                    "request_failed",
                    "timeout",
                ]
                .contains(v)
            });
            tracing::info!(
                event = "plugin_account_action_result",
                plugin_id = plugin,
                account_id = account,
                backend = x["provider"].as_str(),
                action = x["action"].as_str().filter(|v| *v == "checkin"),
                phase = x["phase"]
                    .as_str()
                    .filter(|v| ["preview", "execute"].contains(v)),
                outcome,
                reason
            );
        }
        if operation == "providers" {
            let definition = self.store.plugin(&plugin)?;
            store::save(
                &self
                    .store
                    .root
                    .join("descriptors")
                    .join(format!("{plugin}.json")),
                &serde_json::json!({"revision":definition.revision,"providers":result}),
            )?;
        }
        if operation == "login.begin" && result["session"].as_str().is_some() {
            let mut logins = self.logins.lock().await;
            logins.retain(|_, l| l.created.elapsed() < LOGIN_TTL);
            if logins.len() >= 32 {
                return Err("登录任务过多".into());
            }
            logins.insert(
                result["session"].as_str().unwrap().into(),
                Login {
                    plugin: plugin.clone(),
                    provider: x["provider"].as_str().ok_or("缺少服务 ID")?.into(),
                    name: x["name"].as_str().map(str::to_owned),
                    auth: None,
                    created: std::time::Instant::now(),
                },
            );
        }
        if operation == "login.finish" || (operation == "login.begin" && result["complete"] == true)
        {
            if operation == "login.begin" {
                result = result["result"].take();
            }
            if result["type"] != "success" {
                let failure = auth::LoginFailure::from_result(&result);
                tracing::warn!(
                    event = "plugin_auth_failed",
                    plugin_id = plugin,
                    backend = x["provider"].as_str(),
                    hook = operation,
                    method_id = x["method"].as_str(),
                    error_code = failure.code()
                );
                return Ok(serde_json::json!({"error":{"type":"plugin_auth","code":failure}}));
            }
            let auth = if result.get("refresh").is_some() {
                result["type"] = "oauth".into();
                result
            } else if result["key"].as_str().is_some_and(|k| !k.is_empty()) {
                serde_json::json!({"type":"api","key":result["key"],"metadata":result["metadata"]})
            } else {
                return Err("插件未返回有效凭据".into());
            };
            let mut logins = self.logins.lock().await;
            let session = if operation == "login.finish" {
                let id = x["session"].as_str().ok_or("缺少登录会话")?;
                let login = logins.get_mut(id).ok_or("登录已取消，不保存凭据")?;
                if login.plugin != plugin || Some(login.provider.as_str()) != x["provider"].as_str()
                {
                    return Err("登录会话作用域不匹配".into());
                }
                login.auth = Some(auth);
                id.to_owned()
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                logins.insert(
                    id.clone(),
                    Login {
                        plugin,
                        provider: x["provider"].as_str().ok_or("缺少服务 ID")?.into(),
                        name: x["name"].as_str().map(str::to_owned),
                        auth: Some(auth),
                        created: std::time::Instant::now(),
                    },
                );
                id
            };
            return Ok(serde_json::json!({"session":session,"ready":true}));
        }
        Ok(result)
    }
    pub async fn inference(
        &self,
        account: &str,
        protocol: &str,
        body: Value,
        context: Value,
    ) -> Response {
        let activity = self.activity();
        let result=async {
            let a=self.store.account(account)?;
            let host=self.host(&a.plugin).await?;
            self.prepare(&host,account,&a.provider).await?;
            let res=self.client.post(format!("http://127.0.0.1:{}/fetch",host.port)).bearer_auth(&host.token)
                .json(&serde_json::json!({"account":account,"provider":a.provider,"protocol":protocol,"body":body,"context":context}))
                .timeout(Duration::from_secs(900)).send().await.map_err(|_|"插件请求中断或超时")?;
            Ok::<_,String>((res,host))
        }.await;
        match result {
            Err(reason) => {
                tracing::warn!(
                    event = "plugin_inference_failed",
                    account_id = account,
                    protocol,
                    run_id = context["run"].as_str(),
                    request_id = context["request"].as_str(),
                    reason
                );
                failure(
                    StatusCode::BAD_GATEWAY,
                    "Plugin inference failed; see Usage logs",
                )
            }
            Ok((res, host)) => {
                use futures_util::StreamExt;
                let status = res.status();
                tracing::info!(
                    event = "plugin_inference_response",
                    account_id = account,
                    protocol,
                    run_id = context["run"].as_str(),
                    request_id = context["request"].as_str(),
                    http_status = status.as_u16()
                );
                let headers = res.headers().clone();
                let stream = async_stream::stream! {let _host=host;let _activity=activity;let mut stream=res.bytes_stream();while let Some(bytes)=stream.next().await{yield bytes;}};
                let mut response = Response::new(Body::from_stream(stream));
                *response.status_mut() = status;
                for (name, value) in headers.iter() {
                    if !matches!(
                        name.as_str(),
                        "connection" | "transfer-encoding" | "content-length" | "content-encoding"
                    ) {
                        response.headers_mut().insert(name, value.clone());
                    }
                }
                response
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn queued_account_work_does_not_block_another_account() {
        let runtime = Runtime::new(PathBuf::new(), 0);
        for actions in [false, true] {
            let runtime = &runtime;
            let lock = |id| async move {
                if actions {
                    runtime.lock_action(id).await
                } else {
                    runtime.lock_account(id).await
                }
            };
            let held = lock("a").await;
            let queued = lock("a");
            tokio::pin!(queued);
            // Poll A's second request until it is actually queued, without timing assumptions.
            assert!(futures_util::poll!(queued.as_mut()).is_pending());
            let other = tokio::time::timeout(Duration::from_millis(200), lock("b")).await;
            drop(held);
            queued.await;
            assert!(
                other.is_ok(),
                "account B was blocked by account A's queue; actions={actions}"
            );
        }
    }
    #[test]
    fn log_codes_accept_known_failures_and_exclude_arbitrary_plugin_prose() {
        assert_eq!(
            safe_error_code(&serde_json::json!({"error":{"code":"unsupported_client_api"}})),
            "unsupported_client_api"
        );
        assert_eq!(
            safe_error_code(
                &serde_json::json!({"error":{"code":"synthetic-access-token","message":"synthetic-secret"}})
            ),
            "hook"
        );
        assert_eq!(safe_error_code(&serde_json::json!({"error":{}})), "hook");
    }
}
