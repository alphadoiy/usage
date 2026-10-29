use super::*;
use crate::agents::{AgentKind, Launcher, LocalHooks, SpawnOutcome};
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicBool, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("usage-resume-plan-{}", uuid::Uuid::new_v4()));
        for name in ["source", "target", "project"] {
            fs::create_dir_all(path.join(name)).unwrap();
        }
        Self(path.canonicalize().unwrap())
    }
    fn session(&self, kind: AgentKind) -> Session {
        Session {
            agent: None,
            provider: kind,
            instance: "original-instance".into(),
            id: uuid::Uuid::new_v4().to_string(),
            cwd: self.0.join("project"),
            title: "Synthetic conversation".into(),
            recency_at_ms: 0,
            home: self.0.join("source"),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct CaptureAgent {
    kind: AgentKind,
    home: PathBuf,
    available: Arc<AtomicBool>,
}
impl AgentAdapter for CaptureAgent {
    fn kind(&self) -> AgentKind {
        self.kind
    }
    fn launcher(&self) -> Option<&dyn Launcher> {
        Some(self)
    }
}
impl Launcher for CaptureAgent {
    fn validate_resume_source(&self, _: &Session) -> Result<(), String> {
        self.available
            .load(Ordering::SeqCst)
            .then_some(())
            .ok_or("source no longer accessible".into())
    }
    fn spawn(&self, request: &SpawnRequest) -> Result<SpawnOutcome, String> {
        let result = serde_json::json!({"id":request.resume, "cwd":request.cwd, "usage_config":request.usage_config});
        fs::write(self.home.join("foreground.json"), result.to_string()).unwrap();
        Ok(SpawnOutcome {
            status: ExitStatus::from_raw(0),
            run: None,
        })
    }
    fn resume_shell_command_with_config(
        &self,
        session: &Session,
        config: Option<&Path>,
    ) -> Result<String, String> {
        let result = serde_json::json!({"id":session.id, "cwd":session.cwd, "usage_config":config});
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
        Ok(format!(
            "printf %s {} > {}",
            quote(&result.to_string()),
            quote(&self.home.join("exported.json").to_string_lossy())
        ))
    }
}

const KIND: AgentKind = AgentKind::new(
    "plan-fixture",
    "Plan fixture",
    None,
    "fixture resume",
    "FIXTURE_HOME",
    0,
);

#[test]
fn switching_destination_keeps_source_identity_and_both_transports_use_the_same_plan() {
    let fixture = Fixture::new();
    let available = Arc::new(AtomicBool::new(true));
    let source_agent: Arc<dyn AgentAdapter> = Arc::new(CaptureAgent {
        kind: KIND,
        home: fixture.0.join("source"),
        available: available.clone(),
    });
    let destination: Arc<dyn AgentAdapter> = Arc::new(CaptureAgent {
        kind: KIND,
        home: fixture.0.join("target"),
        available,
    });
    let mut source = fixture.session(KIND);
    source.agent = Some(source_agent.clone());
    let config = fixture.0.join("custom-usage.toml");
    let plan = ResumeIntent::new(source.clone(), Some(config.clone()))
        .with_target(ResumeTarget::Instance(destination))
        .prepare()
        .unwrap();
    assert_eq!(plan.session().key(), source.key());
    assert_eq!(plan.session().home, source.home);
    assert!(Arc::ptr_eq(
        plan.session().agent.as_ref().unwrap(),
        &source_agent
    ));
    assert!(
        !fixture.0.join("target/foreground.json").exists(),
        "prepare must not launch"
    );
    assert!(
        !fixture.0.join("target/exported.json").exists(),
        "prepare must not export"
    );
    assert!(plan.run_in_current().unwrap().success());
    assert!(
        Command::new("/bin/sh")
            .args(["-c", &plan.shell_command().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    let foreground = fs::read(fixture.0.join("target/foreground.json")).unwrap();
    assert_eq!(
        foreground,
        fs::read(fixture.0.join("target/exported.json")).unwrap()
    );
    let result: serde_json::Value = serde_json::from_slice(&foreground).unwrap();
    assert_eq!(result["id"], source.id);
    assert_eq!(result["usage_config"], config.to_string_lossy().as_ref());
    assert!(!fixture.0.join("source/foreground.json").exists());
    assert!(!fixture.0.join("source/exported.json").exists());
}

#[test]
fn every_transport_rechecks_target_access_after_preparation() {
    let fixture = Fixture::new();
    let available = Arc::new(AtomicBool::new(true));
    let target: Arc<dyn AgentAdapter> = Arc::new(CaptureAgent {
        kind: KIND,
        home: fixture.0.join("target"),
        available: available.clone(),
    });
    let plan = ResumeIntent::new(fixture.session(KIND), None)
        .with_target(ResumeTarget::Instance(target))
        .prepare()
        .unwrap();
    available.store(false, Ordering::SeqCst);
    for error in [
        plan.run_in_current().unwrap_err(),
        plan.shell_command().unwrap_err(),
        plan.copy_command().unwrap_err(),
        plan.run_in_ghostty().unwrap_err(),
    ] {
        assert_eq!(error, "source no longer accessible");
    }
    assert!(!fixture.0.join("target/foreground.json").exists());
    assert!(!fixture.0.join("target/exported.json").exists());
}

#[test]
fn mismatched_agent_and_other_local_home_are_rejected_before_any_transport() {
    let fixture = Fixture::new();
    let source = fixture.session(KIND);
    let wrong: Arc<dyn AgentAdapter> = Arc::new(CaptureAgent {
        kind: AgentKind::CLAUDE,
        home: fixture.0.join("target"),
        available: Arc::new(AtomicBool::new(true)),
    });
    assert!(
        ResumeIntent::new(source.clone(), None)
            .with_target(ResumeTarget::Instance(wrong))
            .prepare()
            .unwrap_err()
            .contains("类型不匹配")
    );
    let local = crate::agents::local(KIND, |_| Ok(Vec::new()), LocalHooks::default());
    let other = local(AgentContext::Local {
        home: fixture.0.join("target"),
    })
    .unwrap();
    assert!(
        ResumeIntent::new(source.clone(), None)
            .with_target(ResumeTarget::Instance(other))
            .prepare()
            .unwrap_err()
            .contains("来源目录不同")
    );
    let original = local(AgentContext::Local {
        home: source.home.clone(),
    })
    .unwrap();
    assert!(
        ResumeIntent::new(source, None)
            .with_target(ResumeTarget::Instance(original))
            .prepare()
            .is_ok()
    );
}

#[test]
fn original_destination_preserves_custom_adapter_and_source_disappearance_is_rechecked() {
    let fixture = Fixture::new();
    let mut source = fixture.session(KIND);
    source.agent = Some(Arc::new(CaptureAgent {
        kind: KIND,
        home: source.home.clone(),
        available: Arc::new(AtomicBool::new(true)),
    }));
    let request = ResumeRequest {
        session: source,
        usage_config: None,
    };
    let plan = request.prepare().unwrap();
    assert!(plan.run_in_current().unwrap().success());
    assert!(
        Command::new("/bin/sh")
            .args(["-c", &plan.shell_command().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        fs::read(fixture.0.join("source/foreground.json")).unwrap(),
        fs::read(fixture.0.join("source/exported.json")).unwrap()
    );
    fs::remove_dir(fixture.0.join("project")).unwrap();
    assert!(
        plan.run_in_current()
            .unwrap_err()
            .contains("项目目录不存在")
    );
    assert!(plan.shell_command().unwrap_err().contains("项目目录不存在"));
}

#[test]
fn managed_preparation_does_not_materialize_or_migrate_its_legacy_home() {
    let fixture = Fixture::new();
    let mut manager = crate::codex_manager::Manager::open(fixture.0.join("manager")).unwrap();
    let config = crate::codex_manager::config::new_config(
        "https://fixture.invalid/v1",
        "fixture-model",
        None,
    )
    .unwrap();
    let profile = manager.add("Synthetic", &config, "fixture-key").unwrap();
    let home = manager.home(&profile.id, profile.revision).unwrap();
    fs::create_dir_all(&home).unwrap();
    let mut source = fixture.session(AgentKind::CODEX);
    source.home = home.clone();
    let context = crate::codex_manager::managed::ManagedAccount {
        root: manager.root().to_owned(),
        account: profile.id,
        revision: Some(profile.revision),
    }
    .context();
    let agent = crate::hub::shared()
        .agents
        .create("codex", context)
        .unwrap();
    let plan = ResumeIntent::new(source, None)
        .with_target(ResumeTarget::Instance(agent))
        .prepare()
        .unwrap();
    assert_eq!(plan.session().home, home);
    assert_eq!(
        fs::read_dir(&home).unwrap().count(),
        0,
        "prepare must not create config, auth or shared-store markers"
    );
    assert!(manager.recent_runs(10).unwrap().is_empty());
}
