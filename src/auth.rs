//! Authentication capabilities and owned, cancellable login tasks. No tokens cross this API.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::registry::{FactoryRegistry, RegistryError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthContext {
    pub root: PathBuf,
    /// Omit for a new account; an adapter must validate replacements against this account.
    pub account: Option<String>,
}

pub struct AuthSpec {
    pub service: String,
    pub context: AuthContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthMethod {
    pub id: &'static str,
    pub label: &'static str,
}
impl AuthMethod {
    pub const BROWSER: Self = Self {
        id: "browser",
        label: "浏览器登录",
    };
    pub const DEVICE_CODE: Self = Self {
        id: "device_code",
        label: "设备码登录",
    };
}

/// Configuration can contain private values. Never Debug/Serialize this request.
pub struct AuthRequest {
    pub method: String,
    pub name: Option<String>,
    pub config: Option<String>,
    pub program: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthAccount {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPhase {
    Preparing,
    Waiting,
    Cancelling,
    Saving,
    Done,
    Failed,
    Cancelled,
}
impl AuthPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Preparing => "正在准备登录…",
            Self::Waiting => "等待官方 CLI 完成授权 · Ctrl+C 取消",
            Self::Cancelling => "正在取消登录…",
            Self::Saving => "正在保存账号…",
            Self::Done => "登录已保存",
            Self::Failed => "登录失败",
            Self::Cancelled => "登录已取消",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuthSnapshot {
    pub id: String,
    pub phase: AuthPhase,
    pub account: Option<AuthAccount>,
    /// Adapter errors must be safe to show; native output and tokens are never captured here.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    UnsupportedMethod,
    Cancelled,
    Failed(String),
}
impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedMethod => write!(f, "此提供方不支持该登录方式"),
            Self::Cancelled => write!(f, "登录已取消"),
            Self::Failed(message) => message.fmt(f),
        }
    }
}
impl std::error::Error for AuthError {}

#[derive(Clone)]
pub struct AuthControl(Arc<Mutex<AuthSnapshot>>);
impl AuthControl {
    /// Request cancellation before the credential commit boundary.
    pub fn cancel(&self) -> bool {
        let mut state = self.0.lock().unwrap();
        if state.phase.is_terminal() || state.phase == AuthPhase::Saving {
            return false;
        }
        state.phase = AuthPhase::Cancelling;
        true
    }
    pub fn checkpoint(&self) -> Result<(), AuthError> {
        if matches!(
            self.0.lock().unwrap().phase,
            AuthPhase::Cancelling | AuthPhase::Cancelled
        ) {
            Err(AuthError::Cancelled)
        } else {
            Ok(())
        }
    }
    pub fn waiting(&self) -> Result<(), AuthError> {
        self.advance(AuthPhase::Waiting)
    }
    /// The adapter must cross this boundary immediately before persisting credentials.
    /// Cancellation and committing serialize under the same lock.
    pub fn begin_save(&self) -> Result<(), AuthError> {
        self.advance(AuthPhase::Saving)
    }
    fn advance(&self, next: AuthPhase) -> Result<(), AuthError> {
        let mut state = self.0.lock().unwrap();
        if state.phase == AuthPhase::Cancelling {
            return Err(AuthError::Cancelled);
        }
        if state.phase.is_terminal() || state.phase == AuthPhase::Saving {
            return Err(AuthError::Failed("登录任务已结束或正在保存".into()));
        }
        state.phase = next;
        tracing::info!(event = "auth_progress", phase = ?next);
        Ok(())
    }
}

pub struct AuthTask {
    state: AuthControl,
    worker: Option<JoinHandle<()>>,
}
impl AuthTask {
    /// Providers may implement native CLI, browser, or device flows behind the same task API.
    pub fn spawn(
        run: impl FnOnce(AuthControl) -> Result<AuthAccount, AuthError> + Send + 'static,
    ) -> Result<Self, AuthError> {
        let op = crate::logging::Operation::new("auth.login");
        let state = AuthControl(Arc::new(Mutex::new(AuthSnapshot {
            id: op.id().into(),
            phase: AuthPhase::Preparing,
            account: None,
            error: None,
        })));
        let control = state.clone();
        let worker = std::thread::Builder::new()
            .name("usage-login".into())
            .spawn(move || {
                let op = op;
                let result = op.span().in_scope(|| {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(control.clone())))
                        .unwrap_or_else(|_| Err(AuthError::Failed("登录任务异常结束".into())))
                });
                let mut snapshot = control.0.lock().unwrap();
                match result {
                    _ if snapshot.phase == AuthPhase::Cancelling => {
                        snapshot.phase = AuthPhase::Cancelled
                    }
                    Ok(account) => {
                        snapshot.phase = AuthPhase::Done;
                        snapshot.account = Some(account);
                    }
                    Err(AuthError::Cancelled) => snapshot.phase = AuthPhase::Cancelled,
                    Err(error) => {
                        snapshot.phase = AuthPhase::Failed;
                        snapshot.error = Some(error.to_string());
                    }
                }
                op.span()
                    .in_scope(|| tracing::info!(event = "auth_finished", phase = ?snapshot.phase));
                op.finish(snapshot.phase == AuthPhase::Done);
            })
            .map_err(|_| AuthError::Failed("无法创建登录任务".into()))?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }
    pub fn snapshot(&self) -> AuthSnapshot {
        self.state.0.lock().unwrap().clone()
    }
    /// Returns false after saving starts or the task ends. Accepted cancellation keeps old credentials.
    pub fn cancel(&self) -> bool {
        self.state.cancel()
    }
    pub fn wait(&mut self) -> Result<AuthAccount, AuthError> {
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| AuthError::Failed("登录任务异常结束".into()))?;
        }
        let state = self.snapshot();
        match state.phase {
            AuthPhase::Done => state
                .account
                .ok_or_else(|| AuthError::Failed("登录未返回账号".into())),
            AuthPhase::Cancelled => Err(AuthError::Cancelled),
            _ => Err(AuthError::Failed(
                state.error.unwrap_or_else(|| "登录任务尚未完成".into()),
            )),
        }
    }
}
impl Drop for AuthTask {
    fn drop(&mut self) {
        // The owned worker observes cancellation and reaps its native child; UI drop never blocks.
        self.cancel();
    }
}

pub trait AuthAdapter: Send + Sync {
    fn service(&self) -> &str;
    fn context(&self) -> &AuthContext;
    fn methods(&self) -> &[AuthMethod];
    fn start(&self, request: AuthRequest) -> Result<AuthTask, AuthError>;
}
pub type AuthRegistry = FactoryRegistry<AuthContext, Arc<dyn AuthAdapter>>;

pub fn builtin_registry() -> AuthRegistry {
    let mut registry = AuthRegistry::new();
    registry
        .register("codex_oauth", |context| {
            Ok(Arc::new(CodexOAuth(context)) as Arc<dyn AuthAdapter>)
        })
        .expect("内置认证服务 ID 唯一");
    registry
}

struct CodexOAuth(AuthContext);
impl AuthAdapter for CodexOAuth {
    fn service(&self) -> &str {
        "codex_oauth"
    }
    fn context(&self) -> &AuthContext {
        &self.0
    }
    fn methods(&self) -> &[AuthMethod] {
        &[AuthMethod::BROWSER, AuthMethod::DEVICE_CODE]
    }
    fn start(&self, request: AuthRequest) -> Result<AuthTask, AuthError> {
        if !self
            .methods()
            .iter()
            .any(|method| method.id == request.method)
        {
            return Err(AuthError::UnsupportedMethod);
        }
        let context = self.0.clone();
        AuthTask::spawn(move |control| {
            tracing::info!(event = "auth_started", service = "codex_oauth", method = %request.method);
            let result = (|| {
                control
                    .checkpoint()
                    .map_err(|e| crate::codex_manager::Error::message(e.to_string()))?;
                crate::codex_manager::Manager::open(context.root)?.login_oauth_controlled(
                    request.name.as_deref(),
                    request.config.as_deref(),
                    request.method == AuthMethod::DEVICE_CODE.id,
                    &request.program,
                    context.account.as_deref(),
                    &control,
                )
            })();
            match result {
                Ok(profile) => Ok(AuthAccount {
                    id: profile.id,
                    name: profile.name,
                }),
                Err(_) if control.checkpoint().is_err() => Err(AuthError::Cancelled),
                Err(error) => Err(AuthError::Failed(error.to_string())),
            }
        })
    }
}

pub fn create(
    registry: &AuthRegistry,
    spec: AuthSpec,
) -> Result<Arc<dyn AuthAdapter>, RegistryError> {
    let adapter = registry.create(&spec.service, spec.context.clone())?;
    if adapter.service() != spec.service || adapter.context() != &spec.context {
        return Err(RegistryError::Factory(
            "认证工厂返回的服务或账号绑定不匹配".into(),
        ));
    }
    Ok(adapter)
}

/// Preserve native CLI instructions. No raw login output is parsed, retained, or logged.
pub async fn run_in_terminal(
    adapter: &dyn AuthAdapter,
    request: AuthRequest,
) -> Result<AuthAccount, AuthError> {
    #[cfg(unix)]
    let mut interrupts = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|_| AuthError::Failed("无法监听登录取消信号".into()))?;
    let mut task = adapter.start(request)?;
    let mut shown = None;
    loop {
        let state = task.snapshot();
        if shown != Some(state.phase) {
            eprintln!("{}", state.phase.label());
            shown = Some(state.phase);
        }
        if state.phase.is_terminal() {
            return task.wait();
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(40)) => {},
            _ = async {
                #[cfg(unix)] { interrupts.recv().await; }
                #[cfg(not(unix))] { let _ = tokio::signal::ctrl_c().await; }
            } => { task.cancel(); }
        }
    }
}

#[cfg(test)]
mod tests;
