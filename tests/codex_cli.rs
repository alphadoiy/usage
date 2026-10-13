use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Fixture {
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("usage-codex-cli-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self {
            path: path.canonicalize().unwrap(),
        }
    }
    fn root_command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_usage"));
        command
            .args(args)
            .env("USAGE_CODEX_DIR", self.path.join("manager"))
            .env("USAGE_LOG_DIR", self.path.join("logs"))
            .env("USAGE_LOG", "debug");
        command
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = self.root_command(&["codex"]);
        command.args(args);
        command
    }
    fn log_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<_> = fs::read_dir(self.path.join("logs"))
            .into_iter()
            .flatten()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        files
    }
    fn events(&self) -> Vec<serde_json::Value> {
        self.log_files()
            .iter()
            .flat_map(|path| {
                fs::read_to_string(path)
                    .unwrap()
                    .lines()
                    .map(|line| {
                        serde_json::from_str(line).expect("complete JSON event after command exits")
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
    fn run(&self, args: &[&str]) -> String {
        success(self.command(args).output().unwrap())
    }
    fn add(&self) -> String {
        let mut child = self
            .command(&[
                "add",
                "Account A",
                "--base-url",
                "https://example.com/a/v1",
                "--model",
                "fixture-model",
                "--key-stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"fixture-cli-secret\n")
            .unwrap();
        let output = success(child.wait_with_output().unwrap());
        assert!(!output.contains("fixture-cli-secret"));
        let profiles: serde_json::Value =
            serde_json::from_str(&self.run(&["list", "--json"])).unwrap();
        profiles[0]["id"].as_str().unwrap().to_string()
    }
    #[cfg(unix)]
    fn binary(&self) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.path.join("mock codex");
        fs::write(&path, r#"#!/bin/sh
if [ "$1" = '--help' ]; then printf '%s\n' '--no-daemon'; exit 0; fi
{
printf '%s\n' "$CODEX_HOME" "$USAGE_CODEX_API_KEY" "${OPENAI_API_KEY-unset}" "${OPENAI_BASE_URL-unset}" "${CODEX_API_KEY-unset}" "${CODEX_AUTH_TOKEN-unset}"
printf '%s\n' "$@"
} > "$CAPTURE_FILE"
sleep 0.2
"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

include!("support/bark_server.rs");

#[cfg(unix)]
fn configure_bark(f: &Fixture, server: &BarkServer) {
    let output = success(
        f.command(&["bark", "configure", "--url-env", "FIXTURE_BARK_URL"])
            .env("FIXTURE_BARK_URL", &server.url)
            .output()
            .unwrap(),
    );
    assert!(!output.contains("fixture-bark-device-secret"));
}

#[cfg(unix)]
fn bark_binary(f: &Fixture) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let binary = f.path.join("mock bark codex");
    fs::write(&binary, r#"#!/usr/bin/env python3
import json, os, sys
if '--help' in sys.argv:
    print('--profile --no-daemon'); sys.exit(0)
if '--version' in sys.argv:
    print('codex-cli 0.162.1'); sys.exit(0)
args = sys.argv[1:]
with open(os.environ['CAPTURE_FILE'], 'w') as file:
    json.dump({'args':args,'context':os.environ.get('USAGE_CODEX_BARK_CONTEXT'),'home':os.environ['CODEX_HOME']}, file)
"#).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    binary
}

#[cfg(unix)]
#[test]
fn bark_is_injected_per_managed_run_without_changing_byok_or_oauth_homes() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let server = BarkServer::new();
    configure_bark(&f, &server);
    assert_eq!(
        fs::metadata(f.path.join("manager/bark.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let accounts = [
        f.add(),
        oauth_id(&f, &oauth_source(&f, "workspace-bark"), "OAuth Bark"),
    ];
    let binary = bark_binary(&f);
    let mut groups = std::collections::HashSet::new();
    for id in &accounts {
        let before = f.run(&["show", id]);
        let mut children = Vec::new();
        for i in 0..2 {
            let capture = f.path.join(format!("bark-{id}-{i}.json"));
            let child = f
                .command(&[
                    "run",
                    id,
                    "--cwd",
                    f.path.to_str().unwrap(),
                    "--binary",
                    binary.to_str().unwrap(),
                ])
                .env("CAPTURE_FILE", &capture)
                .spawn()
                .unwrap();
            children.push((child, capture));
        }
        for (mut child, capture) in children {
            assert!(child.wait().unwrap().success());
            let run: serde_json::Value =
                serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
            let args = run["args"].as_array().unwrap();
            assert!(
                args.iter()
                    .any(|arg| arg.as_str().is_some_and(|arg| arg.starts_with("notify=")))
            );
            let hook = args
                .iter()
                .filter_map(|arg| arg.as_str())
                .find(|arg| arg.starts_with("hooks.PermissionRequest="))
                .unwrap();
            let parsed: toml::Value = toml::from_str(hook).unwrap();
            let callback = parsed["hooks"]["PermissionRequest"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert!(callback.contains("codex notify --event approval"));
            assert!(
                !args
                    .iter()
                    .any(|arg| arg == "--dangerously-bypass-hook-trust")
            );
            assert!(
                !serde_json::to_string(&run)
                    .unwrap()
                    .contains("fixture-bark-device-secret")
            );
            let home = PathBuf::from(run["home"].as_str().unwrap());
            assert!(
                !fs::read_to_string(home.join("config.toml"))
                    .unwrap()
                    .contains("notify")
            );
            assert!(!home.join("hooks.json").exists());
            let context: serde_json::Value =
                serde_json::from_str(run["context"].as_str().unwrap()).unwrap();
            assert_eq!(context["home"], run["home"]);
            assert!(groups.insert(context["run_id"].as_str().unwrap().to_string()));
            // A direct Codex launch using even the SAME managed home has no flags or marker.
            success(
                Command::new(&binary)
                    .env("CODEX_HOME", &home)
                    .env("CAPTURE_FILE", &capture)
                    .env_remove("USAGE_CODEX_BARK_CONTEXT")
                    .output()
                    .unwrap(),
            );
            let direct: serde_json::Value =
                serde_json::from_slice(&fs::read(capture).unwrap()).unwrap();
            assert_eq!(direct["args"], serde_json::json!([]));
            assert!(direct["context"].is_null());
        }
        assert_eq!(f.run(&["show", id]), before);
    }
    assert_eq!(groups.len(), 4);
    assert!(server.messages.try_recv().is_err());
    let logs = f
        .log_files()
        .iter()
        .map(|file| fs::read_to_string(file).unwrap())
        .collect::<String>();
    assert!(!logs.contains("fixture-bark-device-secret"));
    assert!(!logs.contains("private prompt"));
}

#[cfg(unix)]
#[test]
fn bark_callback_is_advisory_and_disable_clears_inherited_marker() {
    let f = Fixture::new();
    let server = BarkServer::new();
    configure_bark(&f, &server);
    let id = f.add();
    let binary = bark_binary(&f);
    let capture = f.path.join("bark-context.json");
    success(
        f.command(&[
            "run",
            &id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("CAPTURE_FILE", &capture)
        .output()
        .unwrap(),
    );
    let run: serde_json::Value = serde_json::from_slice(&fs::read(capture).unwrap()).unwrap();
    let context = run["context"].as_str().unwrap();
    let callback = |context: Option<&str>| {
        let mut command = f.command(&["notify", "--event", "approval"]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(context) = context {
            command.env("USAGE_CODEX_BARK_CONTEXT", context);
        } else {
            command.env_remove("USAGE_CODEX_BARK_CONTEXT");
        }
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(br#"{"hook_event_name":"PermissionRequest","session_id":"fixture-session","turn_id":"fixture-turn","cwd":"/private/project","tool_input":{"command":"secret-command"}}"#).unwrap();
        let output = success(child.wait_with_output().unwrap());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&output).unwrap(),
            serde_json::json!({})
        );
    };
    callback(None);
    assert!(server.messages.try_recv().is_err());
    f.run(&["bark", "disable"]);
    callback(Some(context));
    let run_id = serde_json::from_str::<serde_json::Value>(context).unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let linked = success(
        f.root_command(&["logs", "--operation", &run_id, "--tail", "100"])
            .output()
            .unwrap(),
    );
    assert!(
        linked
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .any(|row| row["fields"]["event"] == "bark_suppressed"
                && row["span"]["run_id"] == run_id)
    );
    let new_payload =
        r#"{"type":"agent-turn-complete","thread-id":"fixture-thread","turn-id":"another-turn"}"#;
    success(
        f.command(&["notify", "--event", "complete", new_payload])
            .env("USAGE_CODEX_BARK_CONTEXT", context)
            .output()
            .unwrap(),
    );
    assert!(server.messages.try_recv().is_err());
    let capture = f.path.join("bark-disabled.json");
    success(
        f.command(&[
            "run",
            &id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("CAPTURE_FILE", &capture)
        .env("USAGE_CODEX_BARK_CONTEXT", context)
        .output()
        .unwrap(),
    );
    let disabled: serde_json::Value = serde_json::from_slice(&fs::read(capture).unwrap()).unwrap();
    assert!(disabled["context"].is_null());
    assert!(
        !disabled["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg.as_str().unwrap().starts_with("notify="))
    );
}

#[cfg(unix)]
#[test]
fn bark_preserves_existing_notify_and_never_saves_it_into_account_revision() {
    let f = Fixture::new();
    let server = BarkServer::new();
    configure_bark(&f, &server);
    let id = f.add();
    let config = f.path.join("saved-notify.toml");
    let capture = f.path.join("original-notify.json");
    let argv = toml::Value::try_from(vec![
        "/bin/sh",
        "-c",
        "printf '%s' \"$1\" > \"$CAPTURE_ORIGINAL_NOTIFY\"",
        "notify",
    ])
    .unwrap();
    fs::write(&config, format!("notify={argv}\n{}", f.run(&["show", &id]))).unwrap();
    f.run(&["edit", &id, "--config", config.to_str().unwrap()]);
    let before = f.run(&["show", &id]);
    let binary = bark_binary(&f);
    success(
        f.command(&[
            "run",
            &id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("CAPTURE_FILE", f.path.join("bark-run.json"))
        .output()
        .unwrap(),
    );
    let run: serde_json::Value =
        serde_json::from_slice(&fs::read(f.path.join("bark-run.json")).unwrap()).unwrap();
    let payload = serde_json::json!({"type":"agent-turn-complete", "thread-id":"fixture-thread",
        "turn-id":"fixture-turn", "last-assistant-message":"private assistant output"})
    .to_string();
    f.run(&["bark", "disable"]);
    success(
        f.command(&["notify", "--event", "complete", &payload])
            .env("USAGE_CODEX_BARK_CONTEXT", run["context"].as_str().unwrap())
            .env("CAPTURE_ORIGINAL_NOTIFY", &capture)
            .output()
            .unwrap(),
    );
    let original: serde_json::Value = serde_json::from_slice(&fs::read(capture).unwrap()).unwrap();
    assert_eq!(original["type"], "agent-turn-complete");
    assert_eq!(
        original["last-assistant-message"],
        "private assistant output"
    );
    assert_eq!(f.run(&["show", &id]), before);
    let status = f.run(&["bark", "status"]);
    assert!(!status.contains("fixture-bark-device-secret"));
    assert!(!server.messages.try_iter().any(|_| true));
}

#[cfg(unix)]
#[test]
fn bark_explicit_test_reports_backend_failure_and_sends_without_presence_gate() {
    let f = Fixture::new();
    let server = BarkServer::new();
    configure_bark(&f, &server);
    server.code.store(400, std::sync::atomic::Ordering::Relaxed);
    let rejected = f.command(&["bark", "test"]).output().unwrap();
    assert!(!rejected.status.success());
    assert!(!String::from_utf8_lossy(&rejected.stderr).contains("fixture-bark-device-secret"));
    server.receive();
    server.code.store(200, std::sync::atomic::Ordering::Relaxed);
    f.run(&["bark", "test"]);
    assert_eq!(server.receive()["group"], "usage-test");
    let status = f.run(&["bark", "status"]);
    assert!(status.contains("仅锁屏或所有显示器熄屏时发送"));
    assert!(!status.contains("fixture-bark-device-secret"));
    assert!(
        !f.command(&["bark", "configure", "--always"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let files_before = f.log_files();
    let logs = success(
        f.command(&["bark", "logs", "--tail", "100"])
            .output()
            .unwrap(),
    );
    assert_eq!(f.log_files(), files_before); // inspecting notifications has no logging side effects
    let events: Vec<serde_json::Value> = logs
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events
            .iter()
            .any(|row| row["fields"]["reason"] == "backend_rejected"
                && row["fields"]["backend_code"] == 400)
    );
    assert!(
        events
            .iter()
            .any(|row| row["fields"]["event"] == "bark_delivered")
    );
    assert!(!logs.contains("fixture-bark-device-secret"));
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn command_help_parses_all_subcommands() {
    let f = Fixture::new();
    for command in [
        "list",
        "add",
        "edit",
        "copy",
        "remove",
        "detach",
        "show",
        "export",
        "import",
        "sync",
        "follow",
        "run",
        "test",
        "login",
        "import-auth",
    ] {
        f.run(&[command, "--help"]);
    }
    assert!(!f.path.join("manager").exists());
}

#[test]
fn stdin_add_edit_export_and_conflicting_key_inputs() {
    let f = Fixture::new();
    let id = f.add();
    assert!(!f.run(&["list", "--json"]).contains("fixture-cli-secret"));
    assert!(
        f.run(&["edit", &id, "--model", "updated-model"])
            .contains("v2")
    );
    let text = f.run(&["show", &id]);
    assert!(text.contains("updated-model"));
    assert!(!text.contains("fixture-cli-secret"));
    let config = f.path.join("supplied.toml");
    fs::write(&config, &text).unwrap();
    // Supplying an identical file is an explicit edit, never an editor invocation.
    success(
        f.command(&["edit", &id, "--config", config.to_str().unwrap()])
            .env("EDITOR", "missing-editor-for-fixture")
            .output()
            .unwrap(),
    );
    let export = f.path.join("export.toml");
    f.run(&["export", &id, "--output", export.to_str().unwrap()]);
    assert_eq!(fs::read_to_string(export).unwrap(), f.run(&["show", &id]));
    let output = f
        .command(&[
            "add",
            "bad",
            "--base-url",
            "https://example.com/v1",
            "--model",
            "m",
            "--key-stdin",
            "--key-env",
            "A",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn cc_switch_cli_preview_import_follow_and_sync() {
    let f = Fixture::new();
    let source = f.path.join("cc-switch.db");
    let conn = rusqlite::Connection::open(&source).unwrap();
    conn.execute_batch("CREATE TABLE providers(id TEXT, app_type TEXT, name TEXT, settings_config TEXT, meta TEXT);").unwrap();
    let settings = |model: &str| {
        serde_json::json!({"auth":{"OPENAI_API_KEY":"fixture-import-secret"},"config":format!("model = '{model}'\nmodel_provider = 'vendor'\n[model_providers.vendor]\nname = 'Vendor'\nbase_url = 'https://example.com/v1'\n")}).to_string()
    };
    conn.execute(
        "INSERT INTO providers VALUES ('vendor-id', 'codex', 'Imported', ?1, '{}')",
        [settings("before")],
    )
    .unwrap();
    let path = source.to_str().unwrap();
    assert!(
        f.run(&["import", "--source", path, "--dry-run"])
            .contains("新增 1")
    );
    assert_eq!(f.run(&["list", "--json"]).trim(), "[]");
    assert!(
        f.run(&["import", "--source", path, "--follow"])
            .contains("新增 1")
    );
    conn.execute(
        "UPDATE providers SET settings_config=?1",
        [settings("after")],
    )
    .unwrap();
    assert!(f.run(&["sync"]).contains("更新 1"));
    assert!(f.run(&["show", "Imported"]).contains("after"));
    f.run(&["edit", "Imported", "--model", "local"]);
    conn.execute(
        "UPDATE providers SET settings_config=?1",
        [settings("other")],
    )
    .unwrap();
    f.run(&["sync"]);
    assert!(f.run(&["show", "Imported"]).contains("local"));
    assert!(!f.run(&["list", "--json"]).contains("fixture-import-secret"));
}

#[cfg(unix)]
#[test]
fn same_account_opens_twice_and_resume_pins_original_revision() {
    let f = Fixture::new();
    let id = f.add();
    let binary = f.binary();
    let args = [
        "run",
        &id,
        "--cwd",
        f.path.to_str().unwrap(),
        "--binary",
        binary.to_str().unwrap(),
    ];
    let first = f.path.join("first");
    let second = f.path.join("second");
    let mut a = f
        .command(&args)
        .env("CAPTURE_FILE", &first)
        .spawn()
        .unwrap();
    let mut b = f
        .command(&args)
        .env("CAPTURE_FILE", &second)
        .spawn()
        .unwrap();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    let first_text = fs::read_to_string(&first).unwrap();
    assert_eq!(first_text, fs::read_to_string(second).unwrap());
    assert!(first_text.contains("--no-daemon"));
    let home = first_text.lines().next().unwrap();
    assert!(home.ends_with("/r1"));
    f.run(&["edit", &id, "--model", "new-model"]);
    let capture = f.path.join("resumed");
    let session = uuid::Uuid::new_v4().to_string();
    let output = f
        .command(&[
            "run",
            &id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
            "--revision",
            "1",
            "--resume",
            &session,
        ])
        .env("CAPTURE_FILE", &capture)
        .env("OPENAI_API_KEY", "wrong-default")
        .env("OPENAI_BASE_URL", "https://wrong.example")
        .env("CODEX_HOME", "/wrong-home")
        .env("CODEX_API_KEY", "wrong-default")
        .env("CODEX_AUTH_TOKEN", "wrong-token")
        .env("USAGE_CODEX_API_KEY", "wrong-managed-key")
        .output()
        .unwrap();
    success(output);
    let resumed = fs::read_to_string(capture).unwrap();
    let lines: Vec<_> = resumed.lines().collect();
    assert_eq!(lines[0], home);
    assert_eq!(lines[1], "fixture-cli-secret");
    assert_eq!(&lines[2..6], ["unset", "unset", "unset", "unset"]);
    assert_eq!(&lines[6..], ["--no-daemon", "resume", &session]);
    assert!(
        fs::read_to_string(PathBuf::from(home).join("config.toml"))
            .unwrap()
            .contains("fixture-model")
    );
}

fn oauth_source(f: &Fixture, account: &str) -> PathBuf {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let claims = serde_json::json!({"sub":"fixture-user","email":"fixture@example.test",
        "https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_user_id":"fixture-user","chatgpt_plan_type":"plus"}});
    let jwt = format!("e30.{}.fixture", URL_SAFE_NO_PAD.encode(claims.to_string()));
    let path = f.path.join(format!("{account}.json"));
    fs::write(&path,serde_json::json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"id_token":jwt,"access_token":jwt,"refresh_token":"fixture-refresh-original","account_id":account},"last_refresh":"2026-10-11T00:00:00Z"}).to_string()).unwrap();
    path
}
#[cfg(unix)]
fn oauth_binary(f: &Fixture) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = f.path.join("mock oauth codex");
    fs::write(&path,r#"#!/usr/bin/env python3
import os,sys,json,time
from pathlib import Path
if sys.argv[1:]==['--version']:
    print('codex-cli 0.159.3'); sys.exit(0)
if sys.argv[1:]==['--help']:
    print('--no-daemon --profile'); sys.exit(0)
home=Path(os.environ['CODEX_HOME'])
env_names=['OPENAI_API_KEY','CODEX_ACCESS_TOKEN','USAGE_CODEX_API_KEY','OPENAI_BASE_URL','CODEX_AUTH_TOKEN','CODEX_API_KEY','CODEX_REFRESH_TOKEN_URL_OVERRIDE']
record={'pid':os.getpid(),'home':str(home),'args':sys.argv[1:],'env':{k:os.getenv(k) for k in env_names},'config':(home/'config.toml').read_text()}
def capture_record():
    path=Path(os.environ['CAPTURE_FILE']); tmp=path.with_name(path.name+'.'+str(os.getpid())+'.tmp')
    tmp.write_text(json.dumps(record)); os.replace(tmp,path)
if sys.argv[1]=='login':
    capture_record()
    if os.getenv('LOGIN_FAIL'): sys.exit(1)
    (home/'auth.json').write_bytes(Path(os.environ['SOURCE_AUTH']).read_bytes()); sys.exit(0)
auth=json.loads((home/'auth.json').read_text())
record['account_id']=auth['tokens']['account_id']
record['refresh_before']=auth['tokens']['refresh_token']
capture_record()
release_file=os.getenv('RUN_RELEASE_FILE')
if release_file:
    deadline=time.monotonic()+5
    while not Path(release_file).exists():
        if time.monotonic()>deadline: sys.exit(2)
        time.sleep(0.01)
time.sleep(float(os.getenv('RUN_DELAY','0.01')))
auth['tokens']['refresh_token']='fixture-refresh-rotated'
tmp=home/('rotated.'+str(os.getpid())+'.tmp'); tmp.write_text(json.dumps(auth)); os.chmod(tmp,0o600); os.replace(tmp,home/'auth.json')
if os.getenv('RUN_FINISHED_FILE'): Path(os.environ['RUN_FINISHED_FILE']).write_text('finished')
"#).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}
fn oauth_id(f: &Fixture, source: &std::path::Path, name: &str) -> String {
    f.run(&[
        "import-auth",
        "--source",
        source.to_str().unwrap(),
        "--name",
        name,
    ]);
    let list: serde_json::Value = serde_json::from_str(&f.run(&["list", "--json"])).unwrap();
    list.as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == name)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .into()
}

#[cfg(unix)]
#[test]
fn oauth_cli_login_import_edit_and_refresh_keep_credentials_in_one_home() {
    let f = Fixture::new();
    let source = oauth_source(&f, "workspace-a");
    let before = fs::read(&source).unwrap();
    let binary = oauth_binary(&f);
    let capture = f.path.join("login-capture.json");
    let output = success(
        f.command(&[
            "login",
            "OAuth A",
            "--device-auth",
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("SOURCE_AUTH", &source)
        .env("CAPTURE_FILE", &capture)
        .env("OPENAI_API_KEY", "fixture-inherited")
        .env("CODEX_ACCESS_TOKEN", "fixture-inherited")
        .output()
        .unwrap(),
    );
    assert!(!output.contains("fixture-refresh"));
    let login: serde_json::Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    assert_eq!(login["args"], serde_json::json!(["login", "--device-auth"]));
    assert!(login["config"].as_str().unwrap().contains("\"file\""));
    assert!(
        login["env"]
            .as_object()
            .unwrap()
            .values()
            .all(serde_json::Value::is_null)
    );
    assert!(!PathBuf::from(login["home"].as_str().unwrap()).exists());
    let list: serde_json::Value = serde_json::from_str(&f.run(&["list", "--json"])).unwrap();
    let id = list[0]["id"].as_str().unwrap();
    assert_eq!(list[0]["auth_mode"], "oauth");
    let home = f.path.join("manager/homes").join(id).join("r1");
    success(
        f.command(&[
            "run",
            id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("CAPTURE_FILE", &capture)
        .env("CODEX_ACCESS_TOKEN", "fixture-inherited")
        .env("USAGE_CODEX_API_KEY", "fixture-inherited")
        .env("CODEX_REFRESH_TOKEN_URL_OVERRIDE", "https://wrong.test")
        .output()
        .unwrap(),
    );
    let run: serde_json::Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    assert!(
        run["env"]
            .as_object()
            .unwrap()
            .values()
            .all(serde_json::Value::is_null)
    );
    assert_eq!(run["account_id"], "workspace-a");
    assert!(
        run["args"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("usage-r1"))
    );
    assert!(run["args"].as_array().unwrap().contains(&serde_json::json!(
        "forced_chatgpt_workspace_id=\"workspace-a\""
    )));
    f.run(&["edit", id, "--model", "fixture-model-b"]);
    success(
        f.command(&[
            "run",
            id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("CAPTURE_FILE", &capture)
        .output()
        .unwrap(),
    );
    let next: serde_json::Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    assert_eq!(next["home"], run["home"]);
    assert_eq!(next["refresh_before"], "fixture-refresh-rotated");
    assert!(
        next["args"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("usage-r2"))
    );
    assert!(
        fs::read_to_string(home.join("usage-r1.config.toml"))
            .unwrap()
            .contains("\"chatgpt\"")
    );
    assert!(
        fs::read_to_string(home.join("usage-r2.config.toml"))
            .unwrap()
            .contains("fixture-model-b")
    );
    success(
        f.command(&[
            "run",
            id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
            "--revision",
            "1",
            "--resume",
            "12345678-1234-1234-1234-123456789abc",
        ])
        .env("CAPTURE_FILE", &capture)
        .output()
        .unwrap(),
    );
    let resumed: serde_json::Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    assert!(
        resumed["args"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("usage-r1"))
    );
    assert!(
        resumed["args"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("resume"))
    );
    assert_eq!(resumed["refresh_before"], "fixture-refresh-rotated");
    let export = f.path.join("export.toml");
    f.run(&["export", id, "--output", export.to_str().unwrap()]);
    assert!(
        !fs::read_to_string(export)
            .unwrap()
            .contains("fixture-refresh")
    );
    assert!(!f.run(&["list", "--json"]).contains("fixture-refresh"));
    f.run(&["test", id]);
    let failed = f
        .command(&[
            "login",
            "--account",
            id,
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("SOURCE_AUTH", &source)
        .env("CAPTURE_FILE", &capture)
        .env("LOGIN_FAIL", "1")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(
        fs::read_to_string(home.join("auth.json"))
            .unwrap()
            .contains("fixture-refresh-rotated")
    );
    let wrong_source = oauth_source(&f, "other-workspace");
    let mismatch = f
        .command(&[
            "login",
            "--account",
            id,
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("SOURCE_AUTH", &wrong_source)
        .env("CAPTURE_FILE", &capture)
        .output()
        .unwrap();
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("身份"));
    assert!(
        fs::read_to_string(home.join("auth.json"))
            .unwrap()
            .contains("fixture-refresh-rotated")
    );
    success(
        f.command(&[
            "login",
            "--account",
            id,
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("SOURCE_AUTH", &source)
        .env("CAPTURE_FILE", &capture)
        .output()
        .unwrap(),
    );
    let relogged: serde_json::Value = serde_json::from_str(&f.run(&["list", "--json"])).unwrap();
    assert_eq!(relogged[0]["name"], "OAuth A");
    assert_eq!(relogged[0]["revision"], 2);
    assert_eq!(relogged[0]["model"], "fixture-model-b");
    assert_eq!(fs::read(&source).unwrap(), before);
    assert_eq!(
        fs::read_dir(f.path.join("manager/login-staging"))
            .unwrap()
            .count(),
        0
    );
}

#[cfg(unix)]
#[test]
fn oauth_same_and_different_accounts_run_concurrently_without_replacing_login() {
    let f = Fixture::new();
    let source = oauth_source(&f, "workspace-a");
    let a = oauth_id(&f, &source, "OAuth A");
    let b = oauth_id(&f, &oauth_source(&f, "workspace-b"), "OAuth B");
    let binary = oauth_binary(&f);
    let capture_a = f.path.join("capture-a.json");
    let capture_b = f.path.join("capture-b.json");
    let release_a = f.path.join("release-a");
    let mut child_a = f
        .command(&[
            "run",
            &a,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("CAPTURE_FILE", &capture_a)
        .env("RUN_RELEASE_FILE", &release_a)
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while !capture_a.exists() && start.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(capture_a.exists());
    let capture_same = f.path.join("capture-same-account.json");
    let mut same = f
        .command(&[
            "run",
            &a,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("CAPTURE_FILE", &capture_same)
        .env("RUN_DELAY", "0.1")
        .spawn()
        .unwrap();
    assert!(same.wait().unwrap().success());
    assert!(child_a.try_wait().unwrap().is_none());
    let same_run: serde_json::Value =
        serde_json::from_slice(&fs::read(&capture_same).unwrap()).unwrap();
    let first_run: serde_json::Value =
        serde_json::from_slice(&fs::read(&capture_a).unwrap()).unwrap();
    assert_eq!(same_run["home"], first_run["home"]);
    assert_eq!(same_run["account_id"], first_run["account_id"]);
    let replace = f
        .command(&["import-auth", "--source", source.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!replace.status.success());
    let mut child_b = f
        .command(&[
            "run",
            &b,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("CAPTURE_FILE", &capture_b)
        .env("RUN_DELAY", "0.1")
        .spawn()
        .unwrap();
    assert!(child_b.wait().unwrap().success());
    assert!(child_a.try_wait().unwrap().is_none());
    fs::write(&release_a, b"continue").unwrap();
    assert!(child_a.wait().unwrap().success());
    let a_run: serde_json::Value = serde_json::from_slice(&fs::read(&capture_a).unwrap()).unwrap();
    let b_run: serde_json::Value = serde_json::from_slice(&fs::read(&capture_b).unwrap()).unwrap();
    assert_ne!(a_run["home"], b_run["home"]);
    assert_eq!(a_run["account_id"], "workspace-a");
    assert_eq!(b_run["account_id"], "workspace-b");
    success(
        f.command(&[
            "run",
            &a,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("CAPTURE_FILE", &capture_a)
        .output()
        .unwrap(),
    );
}

#[cfg(unix)]
#[test]
fn oauth_rejects_legacy_cli_before_login_or_startup() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let binary = f.path.join("legacy codex");
    fs::write(&binary, "#!/bin/sh\nprintf '%s\\n' 'codex-cli 0.133.0'\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let result = f
        .command(&["login", "Account", "--binary", binary.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("0.134.0"));
    let list: serde_json::Value = serde_json::from_str(&f.run(&["list", "--json"])).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 0);
    assert!(!f.path.join("manager/login-staging").exists());
}

// Isolate public APIs that resolve the manager root without changing process-global env
// in the parallel test runner. Both child helpers use synthetic accounts only.
fn in_isolated_manager_process(name: &str, f: &Fixture) -> bool {
    if std::env::var("USAGE_MANAGER_TEST").ok().as_deref() == Some(name) {
        assert!(std::env::var_os("USAGE_CODEX_DIR").is_some());
        return true;
    }
    success(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("USAGE_MANAGER_TEST", name)
            .env("USAGE_CODEX_DIR", f.path.join("manager"))
            .output()
            .unwrap(),
    );
    false
}

#[test]
fn managed_homes_are_discovered_by_history_configuration() {
    let f = Fixture::new();
    if !in_isolated_manager_process("managed_homes_are_discovered_by_history_configuration", &f) {
        return;
    }
    let mut m = usage::codex_manager::Manager::open_default().unwrap();
    let config =
        usage::codex_manager::config::new_config("https://example.com/v1", "m", None).unwrap();
    let p = m.add("Discovery", &config, "fixture-key").unwrap();
    let home = m.materialize(&p.id, 1).unwrap();
    assert!(
        usage::codex_manager::managed_instances()
            .unwrap()
            .iter()
            .any(|i| i.path == home)
    );
    assert!(
        usage::config::AppConfig::default()
            .data_instances()
            .iter()
            .any(|i| i.path == home)
    );
}

#[test]
fn managed_resume_command_reenters_the_account_launcher() {
    let f = Fixture::new();
    if !in_isolated_manager_process("managed_resume_command_reenters_the_account_launcher", &f) {
        return;
    }
    let mut m = usage::codex_manager::Manager::open_default().unwrap();
    let config =
        usage::codex_manager::config::new_config("https://example.com/v1", "m", None).unwrap();
    let p = m.add("Resume", &config, "fixture-key").unwrap();
    let home = m.materialize(&p.id, 1).unwrap();
    let session = usage::sessions::Session {
        provider: usage::sessions::AgentKind::CODEX,
        instance: "probe".into(),
        id: uuid::Uuid::new_v4().to_string(),
        cwd: f.path.clone(),
        title: "probe".into(),
        recency_at_ms: 0,
        home,
    };
    let command = usage::sessions::resume_shell_command(&session).unwrap();
    let args = shlex::split(&command).unwrap();
    assert!(args.windows(2).any(|w| w == ["codex", "run"]), "{command}");
    assert!(args.contains(&p.id));
    assert!(args.windows(2).any(|w| w == ["--revision", "1"]));
    assert!(!command.contains("fixture-key"));
}

#[cfg(unix)]
struct HeldOauthRun {
    launcher: std::process::Child,
    release: PathBuf,
    finished: PathBuf,
}
#[cfg(unix)]
impl Drop for HeldOauthRun {
    fn drop(&mut self) {
        let _ = fs::write(&self.release, b"release");
        let _ = self.launcher.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !self.finished.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

#[cfg(unix)]
#[test]
fn oauth_child_retains_the_run_lease_after_its_launcher_is_killed() {
    let f = Fixture::new();
    let source = oauth_source(&f, "workspace-orphan");
    let id = oauth_id(&f, &source, "Orphan");
    let binary = oauth_binary(&f);
    let capture = f.path.join("orphan-capture.json");
    let release = f.path.join("release");
    let finished = f.path.join("finished");
    let launcher = f
        .command(&[
            "run",
            &id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("CAPTURE_FILE", &capture)
        .env("RUN_RELEASE_FILE", &release)
        .env("RUN_FINISHED_FILE", &finished)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut held = HeldOauthRun {
        launcher,
        release,
        finished,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !capture.exists() && std::time::Instant::now() < deadline {
        if held.launcher.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        capture.exists(),
        "mock Codex did not start; exit={:?}; events={}",
        held.launcher.try_wait().unwrap(),
        serde_json::to_string(&f.events()).unwrap()
    );
    let record: serde_json::Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    let pid = record["pid"].as_i64().unwrap() as libc::pid_t;
    held.launcher.kill().unwrap();
    held.launcher.wait().unwrap();
    // SAFETY: signal 0 checks the synthetic child's existence without sending a signal.
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    let before = fs::read(f.path.join("manager/homes").join(&id).join("r1/auth.json")).unwrap();
    let replacement = f
        .command(&[
            "import-auth",
            "--source",
            source.to_str().unwrap(),
            "--account",
            &id,
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&replacement.stderr).contains("正在运行"));
    assert!(
        !replacement.status.success(),
        "replaced credentials while orphaned Codex was alive"
    );
    assert_eq!(
        fs::read(f.path.join("manager/homes").join(&id).join("r1/auth.json")).unwrap(),
        before
    );
    fs::write(&held.release, b"release").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !held.finished.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(held.finished.exists(), "mock Codex did not finish");
    // Completion markers precede process exit; retry until the final inherited fd closes.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let output = f
            .command(&[
                "import-auth",
                "--source",
                source.to_str().unwrap(),
                "--account",
                &id,
            ])
            .output()
            .unwrap();
        if output.status.success() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "lease outlived all Codex processes"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn byok_native_runtime_state_does_not_break_the_next_cli_launch() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let id = f.add();
    let binary = f.path.join("trust-writing-codex");
    fs::write(
        &binary,
        r#"#!/usr/bin/env python3
import os,sys
from pathlib import Path
if sys.argv[1:]==['--help']:
    print('--no-daemon'); sys.exit(0)
assert os.environ['USAGE_CODEX_API_KEY']=='fixture-cli-secret'
config=Path(os.environ['CODEX_HOME'])/'config.toml'
text=config.read_text()
if '[projects.' not in text:
    config.write_text(text+'\n[projects."/fixture/project"]\ntrust_level="trusted"\n[notice.model_migrations]\nfixture-model="fixture-model-v2"\n[tui]\nscreen_reader_detection_done=true\n[tui.model_availability_nux]\nfixture-model-v2=1\n')
"#,
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    for _ in 0..2 {
        f.run(&[
            "run",
            &id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ]);
    }
    let saved = f.run(&["show", &id]);
    assert!(!saved.contains("[projects."));
    assert!(!saved.contains("model_migrations"));
    assert!(!saved.contains("screen_reader_detection_done"));
    let runtime = fs::read_to_string(
        f.path
            .join("manager/homes")
            .join(&id)
            .join("r1/config.toml"),
    )
    .unwrap();
    assert!(runtime.contains("trust_level=\"trusted\""));
    let profiles: serde_json::Value = serde_json::from_str(&f.run(&["list", "--json"])).unwrap();
    assert_eq!(profiles[0]["revision"], 1);
}

#[cfg(unix)]
#[test]
fn structured_launch_logs_correlate_database_run_and_flush_on_nonzero_exit() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let id = f.add();
    let binary = f.path.join("exit codex");
    fs::write(
        &binary,
        "#!/bin/sh\nif [ \"$1\" = '--help' ]; then echo '--no-daemon'; exit 0; fi\nexit 7\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let output = f
        .command(&[
            "run",
            &id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
            "--",
            "fixture-private-prompt",
        ])
        .env("USAGE_LOG", "trace")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    let events = f.events();
    let exit = events
        .iter()
        .find(|e| e["fields"]["event"] == "child_exited")
        .unwrap();
    assert_eq!(exit["fields"]["exit_code"], 7);
    assert_eq!(exit["span"]["profile_id"], id);
    let run_id: String = rusqlite::Connection::open(f.path.join("manager/manager.sqlite3"))
        .unwrap()
        .query_row("SELECT id FROM runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(exit["span"]["operation_id"], run_id);
    assert!(
        events
            .iter()
            .any(|e| e["fields"]["event"] == "process_exited" && e["fields"]["exit_code"] == 7)
    );
    assert!(
        events
            .iter()
            .any(|e| e["fields"]["event"] == "operation_failed"
                && e["span"]["operation"] == "cli.dispatch")
    );
    let logs = serde_json::to_string(&events).unwrap();
    assert!(!logs.contains("fixture-cli-secret"));
    assert!(!logs.contains("fixture-private-prompt"));
    assert!(!logs.contains("fixture-model"));
    for file in f.log_files() {
        let lines: Vec<serde_json::Value> = fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        let pid = &lines[0]["span"]["pid"];
        assert!(lines.iter().all(|e| &e["spans"][0]["pid"] == pid));
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(
        fs::metadata(f.path.join("logs"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let count = f.log_files().len();
    let filtered = success(
        f.root_command(&["logs", "--operation", &run_id, "--tail", "10000"])
            .output()
            .unwrap(),
    );
    assert!(filtered.contains("child_spawned"));
    assert!(filtered.contains("child_exited"));
    assert!(!filtered.contains("account_add"));
    assert_eq!(
        f.log_files().len(),
        count,
        "reading logs must not create a new log"
    );
}

#[cfg(unix)]
#[test]
fn tampered_config_logs_only_safe_field_names_and_identifies_failed_stage() {
    let f = Fixture::new();
    let id = f.add();
    let manager = usage::codex_manager::Manager::open(f.path.join("manager")).unwrap();
    let home = manager.materialize(&id, 1).unwrap();
    let config = home.join("config.toml");
    let mut doc: toml::Value = fs::read_to_string(&config).unwrap().parse().unwrap();
    doc["model"] = toml::Value::String("private-tampered-value".into());
    doc["model_providers"]["usage_byok"]["base_url"] =
        toml::Value::String("https://private-endpoint.test/secret".into());
    doc.as_table_mut().unwrap().insert(
        "private-field-key".into(),
        toml::Value::String("private-field-value".into()),
    );
    let changed = toml::to_string(&doc).unwrap();
    fs::write(&config, &changed).unwrap();
    let output = f
        .command(&[
            "run",
            &id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            "nonexistent-codex-fixture",
        ])
        .env("USAGE_LOG", "error")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("实例配置被手动修改"));
    assert!(stderr.contains("操作 ID："));
    let events = f.events();
    let mismatch = events
        .iter()
        .find(|e| e["fields"]["event"] == "config_mismatch")
        .unwrap();
    let fields = mismatch["fields"]["changed_fields"].as_str().unwrap();
    assert!(fields.contains("model_providers.<entry>.base_url"));
    assert!(fields.contains("model"));
    assert!(fields.contains("<unknown>"));
    assert_eq!(mismatch["span"]["profile_id"], id);
    assert!(
        events.iter().any(|e| e["fields"]["event"] == "stage_failed"
            && e["fields"]["stage"] == "home_materialize")
    );
    assert!(
        !events
            .iter()
            .any(|e| e["fields"]["event"] == "child_spawned")
    );
    assert_eq!(fs::read_to_string(config).unwrap(), changed);
    let logs = serde_json::to_string(&events).unwrap();
    for secret in [
        "private-tampered-value",
        "private-endpoint",
        "private-field-key",
        "private-field-value",
        "fixture-cli-secret",
    ] {
        assert!(!logs.contains(secret));
    }
    let root_id = stderr
        .split("操作 ID：")
        .nth(1)
        .unwrap()
        .split('）')
        .next()
        .unwrap();
    let tail = success(
        f.root_command(&["logs", "--operation", root_id])
            .output()
            .unwrap(),
    );
    assert!(tail.contains("config_mismatch"));
}

#[test]
fn logging_off_help_and_read_commands_have_no_side_effects_and_failure_is_optional() {
    let f = Fixture::new();
    success(f.root_command(&["--help"]).output().unwrap());
    success(f.command(&["run", "--help"]).output().unwrap());
    assert!(!f.path.join("logs").exists());
    assert_eq!(
        success(f.root_command(&["logs", "--path"]).output().unwrap()).trim(),
        f.path.join("logs").to_str().unwrap()
    );
    assert!(success(f.root_command(&["logs"]).output().unwrap()).is_empty());
    assert!(!f.path.join("logs").exists());
    assert_eq!(
        success(
            f.command(&["list", "--json"])
                .env("USAGE_LOG", "off")
                .output()
                .unwrap()
        )
        .trim(),
        "[]"
    );
    assert!(!f.path.join("logs").exists());
    let blocked = f.path.join("blocked-log-path");
    fs::write(&blocked, "existing file").unwrap();
    let output = f
        .command(&["list", "--json"])
        .env("USAGE_LOG_DIR", &blocked)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("无法初始化文件日志"));
    assert_eq!(success(output).trim(), "[]");
    assert_eq!(fs::read_to_string(blocked).unwrap(), "existing file");
    let output = f
        .command(&["list", "--json"])
        .env("USAGE_LOG", "private-filter-invalid=bogus")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("日志级别配置无效"));
    assert!(!stderr.contains("private-filter-invalid"));
    assert_eq!(success(output).trim(), "[]");
    assert!(
        !serde_json::to_string(&f.events())
            .unwrap()
            .contains("private-filter-invalid")
    );
    let files = f.log_files().len();
    let tail = success(f.root_command(&["logs", "--tail", "1"]).output().unwrap());
    assert_eq!(tail.lines().count(), 1);
    assert_eq!(f.log_files().len(), files);
}

#[cfg(unix)]
#[test]
fn oauth_logs_record_shared_lock_contention_without_identity_or_tokens() {
    use fs2::FileExt;
    let f = Fixture::new();
    let source = oauth_source(&f, "private-workspace");
    let id = oauth_id(&f, &source, "Private OAuth Account");
    let binary = oauth_binary(&f);
    let capture = f.path.join("capture.json");
    success(
        f.command(&[
            "run",
            &id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("CAPTURE_FILE", &capture)
        .env("USAGE_LOG", "trace")
        .output()
        .unwrap(),
    );
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.path.join("manager/locks").join(format!("{id}.lock")))
        .unwrap();
    FileExt::try_lock_exclusive(&lock).unwrap();
    let output = f
        .command(&[
            "run",
            &id,
            "--cwd",
            f.path.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("USAGE_LOG", "trace")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let events = f.events();
    assert!(
        events
            .iter()
            .any(|e| e["fields"]["event"] == "lease_acquired" && e["fields"]["exclusive"] == false)
    );
    assert!(
        events
            .iter()
            .any(|e| e["fields"]["event"] == "lease_failed" && e["fields"]["busy"] == true)
    );
    let logs = serde_json::to_string(&events).unwrap();
    let auth: serde_json::Value = serde_json::from_slice(&fs::read(source).unwrap()).unwrap();
    for field in ["id_token", "access_token", "refresh_token"] {
        assert!(!logs.contains(auth["tokens"][field].as_str().unwrap()));
    }
    for private in [
        "fixture-refresh-rotated",
        "Private OAuth Account",
        "private-workspace",
        "fixture@example.com",
    ] {
        assert!(!logs.contains(private));
    }
}

#[test]
fn async_http_failure_logs_status_and_context_without_request_or_response_secrets() {
    use std::io::Read;
    use std::net::TcpListener;
    let f = Fixture::new();
    let id = f.add();
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = server.local_addr().unwrap();
    f.run(&["edit", &id, "--base-url", &format!("http://{address}/v1")]);
    let thread = std::thread::spawn(move || {
        server.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut socket = loop {
            match server.accept() {
                Ok((socket, _)) => break socket,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "test client never connected"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("mock server: {e}"),
            }
        };
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 1024];
        loop {
            let len = socket.read(&mut buf).unwrap();
            assert!(len > 0);
            request.extend_from_slice(&buf[..len]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let body = "{\"error\":\"private-upstream-response-token\"}";
        write!(socket, "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let output = f
        .command(&["test", &id, "--request"])
        .env("USAGE_LOG", "trace")
        .output()
        .unwrap();
    thread.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let events = f.events();
    let http = events
        .iter()
        .find(|e| e["fields"]["event"] == "http_response")
        .unwrap();
    assert_eq!(http["fields"]["status"], 401);
    assert_eq!(http["level"], "WARN");
    assert_eq!(http["span"]["operation"], "codex.connection_test");
    assert_eq!(http["spans"][0]["operation"], "cli.dispatch");
    assert!(
        events
            .iter()
            .any(|e| e["fields"]["event"] == "operation_failed"
                && e["span"]["operation"] == "codex.connection_test")
    );
    let logs = serde_json::to_string(&events).unwrap();
    for private in [
        "fixture-cli-secret",
        "private-upstream-response-token",
        "Reply with OK",
        "127.0.0.1",
    ] {
        assert!(!logs.contains(private));
    }
}

#[test]
fn cc_switch_skip_diagnostics_use_static_reasons_and_counts() {
    let f = Fixture::new();
    let path = f.path.join("source.sqlite3");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE providers(id TEXT,app_type TEXT,name TEXT,settings_config TEXT,meta TEXT)",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO providers VALUES ('private-source-id','codex','private-source-name',?1,'{}')",
        [serde_json::json!({"config":"model = 'private-source-model'", "auth":{}}).to_string()],
    )
    .unwrap();
    let output = f.run(&["import", "--source", path.to_str().unwrap(), "--dry-run"]);
    assert!(output.contains("跳过 1"));
    let events = f.events();
    assert!(
        events
            .iter()
            .any(|e| e["fields"]["event"] == "source_provider_skipped"
                && e["fields"]["reason"] == "missing_api_key")
    );
    assert!(
        events
            .iter()
            .any(|e| e["fields"]["event"] == "source_sync_summary"
                && e["fields"]["skipped"] == 1
                && e["fields"]["dry_run"] == true)
    );
    let logs = serde_json::to_string(&events).unwrap();
    for private in [
        "private-source-id",
        "private-source-name",
        "private-source-model",
    ] {
        assert!(!logs.contains(private));
    }
}

#[test]
fn successful_statusline_defaults_to_warn_without_creating_an_empty_log_file() {
    let f = Fixture::new();
    let config = f.path.join("usage.toml");
    fs::write(&config, "providers = ['claude']").unwrap();
    let mut child = f
        .root_command(&[
            "statusline",
            "--config",
            config.to_str().unwrap(),
            "--no-color",
        ])
        .env_remove("USAGE_LOG")
        .env("XDG_CACHE_HOME", f.path.join("cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"rate_limits\":{\"five_hour\":{\"used_percentage\":57.4}}}\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.stderr.is_empty());
    assert!(success(output).contains("57.4%"));
    assert!(f.log_files().is_empty());
}

#[test]
fn concurrently_spawned_provider_query_keeps_cli_operation_context() {
    let f = Fixture::new();
    let config = f.path.join("usage.toml");
    let mut value = toml::map::Map::new();
    value.insert(
        "providers".into(),
        toml::Value::Array(vec![toml::Value::String("codex".into())]),
    );
    let codex_home = f.path.join("empty-codex");
    let mut paths = toml::map::Map::new();
    paths.insert(
        "codex".into(),
        toml::Value::String(codex_home.to_string_lossy().into()),
    );
    value.insert("paths".into(), toml::Value::Table(paths));
    fs::write(&config, toml::to_string(&value).unwrap()).unwrap();
    assert_eq!(
        usage::config::load_config(&config, true).unwrap().codex_dir,
        Some(codex_home)
    );
    let output = f
        .root_command(&["--once", "--config", config.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let events = f.events();
    let event = events
        .iter()
        .find(|e| e["fields"]["event"] == "provider_query_result")
        .unwrap();
    assert_eq!(event["fields"]["provider"], "codex");
    assert_eq!(event["span"]["operation"], "provider.query");
    assert_eq!(event["spans"][0]["operation"], "cli.dispatch");
    assert_eq!(event["fields"]["credential_status"], "NotFound");
}
