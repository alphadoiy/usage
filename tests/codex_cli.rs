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
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_usage"));
        command
            .args(["codex"])
            .args(args)
            .env("USAGE_CODEX_DIR", self.path.join("manager"));
        command
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !capture.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(capture.exists(), "mock Codex did not start");
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
fn byok_native_trust_update_does_not_break_the_next_cli_launch() {
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
    config.write_text(text+'\n[projects."/fixture/project"]\ntrust_level="trusted"\n')
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
