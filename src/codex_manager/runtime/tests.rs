use super::*;
use crate::agents::{AgentKind, RunningAgent};
use crate::runs::{RunState, SessionActivity};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

const FIRST: &str = "123e4567-e89b-12d3-a456-426614174001";
const SECOND: &str = "123e4567-e89b-12d3-a456-426614174002";
const INTERNAL: &str = "123e4567-e89b-12d3-a456-426614174003";

struct Fixture {
    path: PathBuf,
    manager: Manager,
    profile: String,
    binary: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("usage-runtime-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let path = path.canonicalize().unwrap();
        let mut manager = Manager::open(path.join("manager")).unwrap();
        let config =
            super::super::config::new_config("https://relay.invalid/v1", "model", None).unwrap();
        let profile = manager.add("Fixture", &config, "synthetic-key").unwrap().id;
        let binary = path.join("fake codex");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
if [ "$1" = --help ]; then echo '--no-daemon --dangerously-bypass-hook-trust'; exit 0; fi
for value; do done_file="$value"; done
while [ ! -f "$done_file" ]; do sleep 0.01; done
exit 0
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            path,
            manager,
            profile,
            binary,
        }
    }
    fn start(&mut self, name: &str) -> crate::agents::ProcessHandle {
        self.manager
            .start_recorded_with_config(
                &self.profile,
                &self.path,
                None,
                None,
                &self.binary,
                &[self.path.join(name).to_string_lossy().into_owned()],
                None,
            )
            .unwrap()
    }
    fn context(&self, handle: &dyn RunningAgent) -> Context {
        Context {
            root: self.manager.root().into(),
            home: self.manager.home(&self.profile, 1).unwrap(),
            run_id: handle.record().unwrap().id.clone(),
        }
    }
    fn index(&self, home: &Path) {
        let home = crate::codex_native::SessionStore::resolve(home)
            .root()
            .to_path_buf();
        let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads(id TEXT PRIMARY KEY,source TEXT,thread_source TEXT)",
        )
        .unwrap();
        for (id, source) in [(FIRST, "user"), (SECOND, "user"), (INTERNAL, "subagent")] {
            conn.execute(
                "INSERT INTO threads VALUES(?1,'cli',?2)",
                params![id, source],
            )
            .unwrap();
        }
    }
    fn data(&self, kind: &str, session: &str, turn: &str) -> Value {
        json!({"hook_event_name":kind,"session_id":session,"turn_id":turn,"cwd":self.path,
            "model":"reported-model","prompt":"private-prompt","last_assistant_message":"private-reply",
            "tool_input":{"command":"private-command"}})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn concurrent_nonblocking_runs_keep_session_events_separate_and_finish_once() {
    let mut f = Fixture::new();
    let mut first = f.start("first.done");
    let mut second = f.start("second.done");
    assert!(first.try_wait().unwrap().is_none());
    assert!(second.try_wait().unwrap().is_none());
    assert_ne!(first.pid(), second.pid());
    assert_ne!(first.record().unwrap().id, second.record().unwrap().id);
    assert_eq!(
        RunState::observe(first.record().unwrap()),
        RunState::Running
    );
    let a = f.context(&first);
    let b = f.context(&second);
    f.index(&a.home);
    for (context, session) in [(&a, FIRST), (&b, SECOND)] {
        receive(context, &f.data("SessionStart", session, "turn-1")).unwrap();
        receive(context, &f.data("UserPromptSubmit", session, "turn-1")).unwrap();
    }
    let mut compact = f.data("SessionStart", FIRST, "turn-1");
    compact["source"] = "compact".into();
    receive(&a, &compact).unwrap();
    receive(&a, &f.data("Stop", FIRST, "older-turn")).unwrap();
    assert_eq!(
        f.manager.run_record(&a.run_id).unwrap().unwrap().sessions[0].activity,
        SessionActivity::Working
    );
    receive(&a, &f.data("Stop", FIRST, "turn-1")).unwrap();
    assert_eq!(
        f.manager.run_record(&a.run_id).unwrap().unwrap().sessions[0].activity,
        SessionActivity::Idle
    );
    let other = f.manager.run_record(&b.run_id).unwrap().unwrap();
    assert_eq!(other.sessions.len(), 1);
    assert_eq!(other.sessions[0].id, SECOND);
    assert_eq!(other.sessions[0].activity, SessionActivity::Working);
    assert!(receive(&a, &f.data("SessionStart", INTERNAL, "turn-1")).is_err());
    let mut subagent = f.data("Stop", FIRST, "turn-1");
    subagent["agent_id"] = "child-agent".into();
    assert!(receive(&a, &subagent).is_err());
    let unknown = uuid::Uuid::new_v4().to_string();
    assert!(receive(&a, &f.data("Stop", &unknown, "turn-1")).is_err());
    let foreign_context = Context {
        root: a.root.clone(),
        home: f.path.clone(),
        run_id: a.run_id.clone(),
    };
    assert!(receive(&foreign_context, &f.data("Stop", FIRST, "turn-1")).is_err());
    // A second live process may legitimately open the same confirmed session.
    receive(&b, &f.data("SessionStart", FIRST, "turn-1")).unwrap();

    let session = |id: &str, home: &Path| crate::sessions::Session {
        agent: None,
        provider: AgentKind::CODEX,
        instance: "fixture".into(),
        id: id.into(),
        home: home.into(),
        cwd: f.path.clone(),
        title: "task".into(),
        recency_at_ms: 0,
    };
    let mut foreign = session(FIRST, &f.path);
    foreign.instance = "other-home".into();
    let sessions = vec![session(FIRST, &a.home), session(SECOND, &a.home), foreign];
    let activity = f.manager.session_activity(&sessions).unwrap();
    assert_eq!(activity[&sessions[0].key()].runs.len(), 2);
    assert!(
        activity[&sessions[0].key()]
            .runs
            .iter()
            .any(|run| run.run_id == a.run_id)
    );
    assert!(
        activity[&sessions[0].key()]
            .label()
            .contains("2 个运行实例")
    );
    assert_eq!(activity[&sessions[1].key()].runs[0].run_id, b.run_id);
    assert!(!activity.contains_key(&sessions[2].key()));

    std::fs::write(f.path.join("first.done"), "").unwrap();
    let outcome = first.wait().unwrap();
    assert!(outcome.status.success());
    assert_eq!(outcome.run.as_ref().unwrap().sessions[0].id, FIRST);
    assert!(first.try_wait().unwrap().unwrap().status.success());
    assert_eq!(
        f.manager
            .run_events(&a.run_id, 100)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "process_exited")
            .count(),
        1
    );
    assert!(receive(&a, &f.data("UserPromptSubmit", FIRST, "too-late")).is_err());
    assert!(second.try_wait().unwrap().is_none());
    second.terminate().unwrap();
    assert!(!second.wait().unwrap().status.success());
    assert_eq!(
        RunState::observe(&f.manager.run_record(&b.run_id).unwrap().unwrap()),
        RunState::Failed
    );
    let json = serde_json::to_string(&f.manager.runs(None).unwrap()).unwrap();
    assert!(!json.contains("private-"));
    let db = std::fs::read(f.manager.root().join("manager.sqlite3")).unwrap();
    assert!(!String::from_utf8_lossy(&db).contains("private-"));
}

#[test]
fn injection_adds_only_the_usage_hook_and_does_not_bypass_trust_or_write_home() {
    let f = Fixture::new();
    let mut command = Command::new("codex");
    let run = uuid::Uuid::new_v4().to_string();
    let home = f.manager.home(&f.profile, 1).unwrap();
    inject(&mut command,f.manager.root(),&home,&run,
        "[[hooks.SessionStart]]\n[[hooks.SessionStart.hooks]]\ntype='command'\ncommand='existing hook'",true).unwrap();
    let args: Vec<_> = command
        .get_args()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    assert!(!args.iter().any(|s| s == "--dangerously-bypass-hook-trust"));
    let start = args
        .iter()
        .find(|arg| arg.starts_with("hooks.SessionStart="))
        .unwrap();
    let config: toml::Value = toml::from_str(start).unwrap();
    // The account's hook stays in its own config layer; repeating it here would run it twice.
    let groups = config["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert!(!start.contains("existing hook"));
    assert_eq!(groups[0]["hooks"][0]["timeout"].as_integer(), Some(3));
    assert_eq!(groups[0]["hooks"][0]["async"].as_bool(), Some(false));
    assert!(
        !groups[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(&run)
    );
    assert!(!home.exists());
    for (supported, saved) in [(false, ""), (true, "[features]\nhooks=false")] {
        let mut command = Command::new("codex");
        inject(
            &mut command,
            f.manager.root(),
            &home,
            &run,
            saved,
            supported,
        )
        .unwrap();
        assert_eq!(command.get_args().count(), 0);
    }
}

#[test]
fn dropping_handle_reaps_and_finishes_without_waiting_on_the_caller() {
    let mut f = Fixture::new();
    let handle = f.start("release.done");
    let id = handle.record().unwrap().id.clone();
    drop(handle);
    assert!(
        f.manager
            .run_record(&id)
            .unwrap()
            .unwrap()
            .finished_at
            .is_none()
    );
    std::fs::write(f.path.join("release.done"), "").unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if f.manager
            .run_record(&id)
            .unwrap()
            .unwrap()
            .finished_at
            .is_some()
        {
            break;
        }
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        f.manager
            .run_events(&id, 100)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "process_exited")
            .count(),
        1
    );
}
