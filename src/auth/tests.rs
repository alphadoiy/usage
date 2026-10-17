use super::*;
use crate::agents::AgentContext;
use crate::hub::{CapabilityError, Hub, ProviderDefinition};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

struct CustomAuth {
    context: AuthContext,
    starts: Arc<AtomicUsize>,
}
impl AuthAdapter for CustomAuth {
    fn service(&self) -> &str {
        "custom_sso"
    }
    fn context(&self) -> &AuthContext {
        &self.context
    }
    fn methods(&self) -> &[AuthMethod] {
        &[AuthMethod {
            id: "company_sso",
            label: "企业登录",
        }]
    }
    fn start(&self, _: AuthRequest) -> Result<AuthTask, AuthError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        AuthTask::spawn(|control| {
            control.waiting()?;
            control.begin_save()?;
            Ok(account())
        })
    }
}
fn account() -> AuthAccount {
    AuthAccount {
        id: "account:custom".into(),
        name: "Work".into(),
    }
}
fn request() -> AuthRequest {
    AuthRequest {
        method: "company_sso".into(),
        name: None,
        config: None,
        program: "unavailable".into(),
    }
}

#[test]
fn registered_auth_is_lazy_optional_and_accepts_provider_specific_methods() {
    let starts = Arc::new(AtomicUsize::new(0));
    let mut hub = Hub::default();
    let count = starts.clone();
    hub.auth
        .register("custom_sso", move |context| {
            Ok(Arc::new(CustomAuth {
                context,
                starts: count.clone(),
            }) as Arc<dyn AuthAdapter>)
        })
        .unwrap();
    let root = std::env::temp_dir().join(format!("usage-auth-lazy-{}", uuid::Uuid::new_v4()));
    let provider = hub
        .provider(ProviderDefinition {
            id: "work".into(),
            agent: "claude".into(),
            context: AgentContext::Local { home: root.clone() },
            resources: Vec::new(),
            auth: Some(AuthSpec {
                service: "custom_sso".into(),
                context: AuthContext {
                    root: root.clone(),
                    account: None,
                },
            }),
        })
        .unwrap();
    assert!(!root.exists());
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    assert!(provider.capabilities().auth);
    let adapter = provider.authentication().unwrap();
    assert_eq!(adapter.methods()[0].id, "company_sso");
    let mut task = adapter.start(request()).unwrap();
    assert_eq!(task.wait().unwrap(), account());
    assert_eq!(task.wait().unwrap(), account());
    assert_eq!(task.snapshot().phase, AuthPhase::Done);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    let provider = hub
        .provider(ProviderDefinition {
            id: "plain".into(),
            agent: "claude".into(),
            context: AgentContext::Local { home: root },
            resources: Vec::new(),
            auth: None,
        })
        .unwrap();
    assert!(!provider.capabilities().auth);
    assert!(matches!(
        provider.authentication(),
        Err(CapabilityError::Unsupported(_))
    ));
}

#[test]
fn auth_factory_cannot_switch_the_requested_account_or_accept_an_unknown_method() {
    let context = AuthContext {
        root: "/nonexistent".into(),
        account: Some("expected".into()),
    };
    let mut registry = AuthRegistry::new();
    registry
        .register("custom_sso", |mut context: AuthContext| {
            context.account = Some("another".into());
            Ok(Arc::new(CustomAuth {
                context,
                starts: Arc::new(AtomicUsize::new(0)),
            }) as Arc<dyn AuthAdapter>)
        })
        .unwrap();
    assert!(
        create(
            &registry,
            AuthSpec {
                service: "custom_sso".into(),
                context: context.clone()
            }
        )
        .is_err()
    );
    let adapter = create(
        &builtin_registry(),
        AuthSpec {
            service: "codex_oauth".into(),
            context,
        },
    )
    .unwrap();
    assert!(matches!(
        adapter.start(request()),
        Err(AuthError::UnsupportedMethod)
    ));
}

#[test]
fn cancellation_and_credential_commit_have_one_ordered_boundary() {
    let (ready, wait_ready) = mpsc::channel();
    let (release, wait_release) = mpsc::channel();
    let mut task = AuthTask::spawn(move |control| {
        control.waiting()?;
        ready.send(()).unwrap();
        wait_release.recv().unwrap();
        control.begin_save()?;
        Ok(account())
    })
    .unwrap();
    wait_ready.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(task.snapshot().phase, AuthPhase::Waiting);
    assert!(task.cancel());
    release.send(()).unwrap();
    assert_eq!(task.wait(), Err(AuthError::Cancelled));
    assert_eq!(task.snapshot().phase, AuthPhase::Cancelled);

    let (ready, wait_ready) = mpsc::channel();
    let (release, wait_release) = mpsc::channel();
    let mut task = AuthTask::spawn(move |control| {
        control.begin_save()?;
        ready.send(()).unwrap();
        wait_release.recv().unwrap();
        Ok(account())
    })
    .unwrap();
    wait_ready.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(task.snapshot().phase, AuthPhase::Saving);
    assert!(!task.cancel());
    release.send(()).unwrap();
    assert_eq!(task.wait().unwrap(), account());
}

#[test]
fn dropping_a_pending_task_requests_cleanup_and_panics_become_failures() {
    let (stopped, wait_stopped) = mpsc::channel();
    let task = AuthTask::spawn(move |control| {
        while control.checkpoint().is_ok() {
            std::thread::sleep(Duration::from_millis(5));
        }
        stopped.send(()).unwrap();
        Err(AuthError::Cancelled)
    })
    .unwrap();
    drop(task);
    wait_stopped.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut failed = AuthTask::spawn(|_| panic!("synthetic failure")).unwrap();
    assert!(matches!(failed.wait(), Err(AuthError::Failed(_))));
    assert_eq!(failed.snapshot().phase, AuthPhase::Failed);
}
