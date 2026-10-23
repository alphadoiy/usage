//! Process-independent, loopback-only HTTP observation for Usage-owned launches.
mod metrics;
mod server;
#[cfg(test)]
mod tests;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(clap::Subcommand)]
pub enum GatewayCommand {
    /// 查看本地观测网关状态；不会启动服务
    Status,
    #[command(hide = true)]
    Serve {
        #[arg(long)]
        directory: PathBuf,
    },
}

#[derive(Serialize, Deserialize)]
struct StateFile {
    port: u16,
    token: String,
}
#[derive(Serialize, Deserialize)]
pub(crate) enum RouteAuth {
    ApiKey { key: String },
    Plugin { account: String },
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Registration {
    #[serde(default)]
    pub observe_websocket: bool,
    pub run: String,
    pub scope: String,
    pub provider: String,
    pub upstream: String,
    pub model_provider: String,
    pub auth: RouteAuth,
    pub token: String,
    pub lease: PathBuf,
}
impl Registration {
    fn valid_auth(&self) -> bool {
        match &self.auth {
            RouteAuth::Plugin { account } => {
                crate::plugins::endpoint_account(&self.upstream).as_deref()
                    == Some(account.as_str())
            }
            RouteAuth::ApiKey { key } => {
                !key.is_empty()
                    && axum::http::HeaderValue::from_str(&format!("Bearer {key}")).is_ok()
            }
        }
    }
    fn request_auth<'a>(
        &self,
        path: &'a str,
        headers: &axum::http::HeaderMap,
    ) -> Option<(&'a str, axum::http::HeaderValue)> {
        match &self.auth {
            RouteAuth::Plugin { .. } => (headers.get("authorization")?.to_str().ok()?
                == format!("Bearer {}", self.token))
            .then(|| {
                (
                    path,
                    axum::http::HeaderValue::from_static("Bearer usage-plugin"),
                )
            }),
            RouteAuth::ApiKey { key } => {
                if headers.get("authorization")?.to_str().ok()? != format!("Bearer {}", self.token)
                {
                    return None;
                }
                Some((
                    path,
                    axum::http::HeaderValue::from_str(&format!("Bearer {key}")).ok()?,
                ))
            }
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Record {
    #[serde(default)]
    pub transport: String,
    #[serde(default)]
    pub connection_id: Option<String>,
    pub id: String,
    pub run: String,
    pub session: String,
    pub at: i64,
    pub model: String,
    pub requested_model: String,
    pub response_id: Option<String>,
    pub input: Option<u64>,
    pub cache_read: Option<u64>,
    pub first_text: Option<i64>,
    pub first_reasoning: Option<i64>,
    pub flow: Option<i64>,
    pub completed_ms: Option<i64>,
    pub speed_unavailable: Option<String>,
    pub output: Option<u64>,
    pub reasoning: u64,
    pub duration: i64,
    pub ttft: Option<i64>,
    pub decode: Option<(u64, i64)>,
    pub status: u16,
    pub complete: bool,
    pub outcome: String,
    #[serde(default)]
    pub native_transport: bool,
}

pub(crate) fn enabled() -> bool {
    // Unit fixtures execute fake CLIs from the test harness. End-to-end tests use the actual binary.
    !cfg!(test) && cfg!(unix) && std::env::var("USAGE_GATEWAY").map_or(true, |v| v != "off")
}

pub(crate) struct Attachment {
    pub base: String,
    pub token: String,
    /// Inherited by the Agent. Closing the TUI cannot release a live Agent's lease.
    pub lease: File,
}

fn directory(root: &Path) -> PathBuf {
    root.join("gateway")
}
fn private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|_| "无法创建网关运行目录")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| "无法设置网关目录权限")?;
    }
    Ok(())
}
fn private_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|_| "无法打开网关运行文件")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| "无法设置网关文件权限")?;
    }
    Ok(file)
}
const PROTOCOL_VERSION: u32 = 9;
const STARTUP_BUDGET: Duration = Duration::from_millis(1500);
const CONTROL_TIMEOUT: Duration = Duration::from_millis(150);

fn control_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(CONTROL_TIMEOUT)
        .build()
        .map_err(|_| "无法创建网关控制连接".into())
}
fn read_state(path: &Path) -> Option<StateFile> {
    use std::io::Read;
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(4096)
        .read_to_end(&mut bytes)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}
fn url(state: &StateFile, path: &str) -> String {
    format!("http://127.0.0.1:{}{path}", state.port)
}
struct ReadyGateway {
    state: StateFile,
    status: serde_json::Value,
}
fn health(client: &reqwest::blocking::Client, state: &StateFile) -> Option<serde_json::Value> {
    let status: serde_json::Value = client
        .get(url(state, "/_usage/status"))
        .bearer_auth(&state.token)
        .send()
        .ok()
        .filter(|r| r.status().is_success())
        .and_then(|r| r.json().ok())?;
    (status["service"] == "usage-gateway" && status["protocol"] == PROTOCOL_VERSION)
        .then_some(status)
}
fn ready(client: &reqwest::blocking::Client, path: &Path) -> Option<ReadyGateway> {
    let state = read_state(path)?;
    let status = health(client, &state)?;
    Some(ReadyGateway { state, status })
}
fn ensure(
    directory: &Path,
    client: &reqwest::blocking::Client,
    deadline: Instant,
) -> Result<ReadyGateway, String> {
    let path = directory.join("state.json");
    if let Some(gateway) = ready(client, &path) {
        return Ok(gateway);
    }
    let service = private_file(&directory.join("service.lock"))?;
    service
        .try_lock_exclusive()
        .map_err(|_| "观测网关忙或暂不可用")?;
    drop(service);
    if Instant::now() >= deadline {
        return Err("观测网关准备超时".into());
    }
    let mut command =
        Command::new(std::env::current_exe().map_err(|_| "无法定位 Usage 可执行文件")?);
    command
        .args(["gateway", "serve", "--directory"])
        .arg(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and only affects the forked child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = command.spawn().map_err(|_| "无法启动观测网关")?;
    loop {
        if let Some(gateway) = ready(client, &path) {
            // Reap only after natural exit, including when this launcher is killed.
            let _ = std::thread::Builder::new()
                .name("usage-gateway-reap".into())
                .spawn(move || {
                    let _ = child.wait();
                });
            return Ok(gateway);
        }
        if child.try_wait().map_err(|_| "无法检查网关进程")?.is_some() {
            return Err("观测网关启动失败，请查看 Usage 日志".into());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("观测网关启动超时".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn attach(
    root: &Path,
    run: &str,
    binding: &crate::performance::Binding,
    upstream: &str,
    model_provider: &str,
    auth: RouteAuth,
) -> Result<Attachment, String> {
    let directory = directory(root);
    let mut registration = Registration {
        observe_websocket: std::env::var("USAGE_GATEWAY_TRANSPORT")
            .is_ok_and(|v| matches!(v.as_str(), "ws" | "websocket")),
        run: run.into(),
        scope: binding.scope(),
        provider: binding.provider.clone(),
        upstream: upstream.into(),
        model_provider: model_provider.into(),
        auth,
        token: uuid::Uuid::new_v4().to_string(),
        lease: directory.join(format!("{run}.lease")),
    };
    // blocking reqwest owns its own runtime: never create/drop it on a Tokio thread.
    std::thread::Builder::new()
        .name("usage-gateway-prepare".into())
        .spawn(move || {
            let deadline = Instant::now() + STARTUP_BUDGET;
            let result = (|| {
                private_dir(&directory)?;
                // Simultaneous launches wait their turn within the budget instead of skipping
                // observation (or failing a plugin launch) on the first contention.
                let start = private_file(&directory.join("start.lock"))?;
                while start.try_lock_exclusive().is_err() {
                    if Instant::now() >= deadline {
                        return Err("观测网关正在由其他启动准备".into());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                let lease = private_file(&registration.lease)?;
                FileExt::lock_shared(&lease).map_err(|_| "无法创建网关运行租约")?;
                let client = control_client()?;
                registration.upstream = registration.upstream.trim_end_matches('/').into();
                let mut registered = None;
                for _ in 0..2 {
                    if Instant::now() >= deadline {
                        return Err("观测网关准备超时".into());
                    }
                    let state = ensure(&directory, &client, deadline)?.state;
                    // Registration may wait on the metrics store; it gets the remaining budget,
                    // not the short health-probe timeout.
                    let response = client
                        .post(url(&state, "/_usage/routes"))
                        .timeout(deadline.saturating_duration_since(Instant::now()))
                        .bearer_auth(&state.token)
                        .json(&registration)
                        .send()
                        .map_err(|_| "无法注册网关路由")?;
                    if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                        continue;
                    }
                    if !response.status().is_success() {
                        return Err("观测网关拒绝启动配置".into());
                    }
                    let route: String = response.json().map_err(|_| "网关注册响应无效")?;
                    registered = Some((state, route));
                    break;
                }
                let (state, route) = registered.ok_or("观测网关暂不可用，请重试")?;
                Ok(Attachment {
                    base: url(&state, &format!("/r/{route}")),
                    token: registration.token.clone(),
                    lease,
                })
            })();
            if result.is_err() {
                let _ = fs::remove_file(&registration.lease);
            }
            result
        })
        .map_err(|_| "无法创建网关准备线程")?
        .join()
        .map_err(|_| "网关启动线程失败")?
}

pub async fn execute(command: GatewayCommand) -> Result<(), String> {
    match command {
        GatewayCommand::Serve { directory } => server::serve(directory).await,
        GatewayCommand::Status => {
            let directory = directory(&crate::codex_manager::default_root());
            let status = tokio::task::spawn_blocking(
                move || -> Result<Option<serde_json::Value>, String> {
                    let client = control_client()?;
                    let Some(state) = read_state(&directory.join("state.json")) else {
                        return Ok(None);
                    };
                    match client
                        .get(url(&state, "/_usage/status"))
                        .bearer_auth(&state.token)
                        .send()
                    {
                        Ok(response) if response.status().is_success() => response
                            .json()
                            .map(Some)
                            .map_err(|_| "网关状态响应无效".into()),
                        _ => Ok(None),
                    }
                },
            )
            .await
            .map_err(|_| "网关状态查询失败")??;
            match status {
                Some(status) => println!(
                    "{}",
                    serde_json::to_string_pretty(&status).map_err(|_| "无法输出网关状态")?
                ),
                None => println!("观测网关未运行或暂不可用"),
            }
            Ok(())
        }
    }
}

pub(crate) struct PluginControl {
    pub url: String,
    pub token: String,
    pub actions: bool,
}
pub(crate) fn plugin_control() -> Result<PluginControl, String> {
    let directory = directory(&crate::codex_manager::default_root());
    private_dir(&directory)?;
    let lock = private_file(&directory.join("start.lock"))?;
    FileExt::lock_exclusive(&lock).map_err(|_| "插件网关启动锁失败")?;
    let client = control_client()?;
    let gateway = ensure(&directory, &client, Instant::now() + Duration::from_secs(5))?;
    Ok(PluginControl {
        url: url(&gateway.state, "/_usage/plugins/call"),
        token: gateway.state.token,
        actions: gateway.status["plugin_actions"] == 1,
    })
}
