#![cfg(unix)]
//! Full Usage CLI, real plugin child, managed Codex and metrics. Synthetic auth only.
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};
struct Fixture {
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("usage-plugin-cli-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        fs::write(
            path.join("fixture.mjs"),
            include_str!("../src/plugins/fixture.mjs"),
        )
        .unwrap();
        fs::write(path.join("config.toml"), "").unwrap();
        fs::write(path.join("codex"),r#"#!/usr/bin/env python3
import sys,os,json,pathlib,tomllib,urllib.request
if '--help' in sys.argv:
 print('--no-daemon --profile');sys.exit(0)
home=pathlib.Path(os.environ['CODEX_HOME'])
c=tomllib.loads((home/'config.toml').read_text())
p=c['model_provider'];base=c['model_providers'][p]['base_url']
for i,arg in enumerate(sys.argv[:-1]):
 if arg=='-c' and '.base_url=' in sys.argv[i+1]:base=tomllib.loads(sys.argv[i+1])['model_providers'][p]['base_url']
body=json.dumps({'model':'fixture-model','input':'hello','stream':True}).encode()
r=urllib.request.Request(base+'/responses',data=body,headers={'authorization':'Bearer '+os.environ['USAGE_CODEX_API_KEY'],'content-type':'application/json','session_id':'11111111-1111-4111-8111-111111111111'})
with urllib.request.urlopen(r,timeout=15) as response:
 result=response.read().decode()
 assert 'response.completed' in result and 'response.function_call_arguments.delta' in result
 assert 'cli-user:11111111-1111-4111-8111-111111111111' in result
print('plugin-inference-ok')
"#).unwrap();
        fs::set_permissions(path.join("codex"), fs::Permissions::from_mode(0o700)).unwrap();
        Self { path }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_usage"));
        c.args(args)
            .current_dir(&self.path)
            .env("USAGE_CODEX_DIR", self.path.join("manager"))
            .env("USAGE_PLUGIN_DIR", self.path.join("plugins"))
            .env("XDG_DATA_HOME", self.path.join("data"))
            .env("USAGE_CONFIG", self.path.join("config.toml"))
            .env("USAGE_LOG_DIR", self.path.join("logs"))
            .env("USAGE_LOG", "debug")
            .env("USAGE_GATEWAY", "on")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env_remove("USAGE_GATEWAY_TRANSPORT");
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        let o = self.command(args).output().unwrap();
        assert!(
            o.status.success(),
            "{:?}\n{}\n{}",
            args,
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        o
    }
    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.run(args).stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(o) = self.command(&["gateway", "status"]).output()
            && let Ok(v) = serde_json::from_slice::<Value>(&o.stdout)
            && let Some(pid) = v["pid"].as_i64()
        {
            // SAFETY: PID obtained from this fixture's private authenticated gateway.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}
#[test]
fn plugin_login_bind_managed_inference_and_metric_source_are_connected() {
    let f = Fixture::new();
    f.run(&[
        "plugins",
        "add",
        "fixture",
        f.path.join("fixture.mjs").to_str().unwrap(),
        "--runtime",
        "node",
    ]);
    let descriptors = f.json(&["plugins", "providers", "fixture"]);
    assert_eq!(descriptors.as_array().unwrap().len(), 2);
    f.run(&[
        "plugins",
        "login",
        "fixture",
        "fixture",
        "--input",
        "uid=cli-user",
        "--no-browser",
    ]);
    let accounts = f.json(&["plugins", "accounts"]);
    let id = accounts[0]["id"].as_str().unwrap();
    assert_eq!(accounts[0]["name"], "cli-user");
    assert!(accounts[0].get("auth").is_none());
    assert_eq!(
        f.json(&["plugins", "usage", id])["windows"][0]["unit"],
        "credits"
    );
    f.run(&[
        "plugins",
        "bind",
        id,
        "--name",
        "Plugin Codex",
        "--model",
        "fixture-model",
    ]);
    f.run(&[
        "codex",
        "run",
        "Plugin Codex",
        "--binary",
        f.path.join("codex").to_str().unwrap(),
    ]);
    f.run(&[
        "codex",
        "run",
        "Plugin Codex",
        "--binary",
        f.path.join("codex").to_str().unwrap(),
        "--resume",
        "11111111-1111-4111-8111-111111111111",
    ]);
    let db_path = f.path.join("data/usage/performance.sqlite3");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let found = rusqlite::Connection::open(&db_path).ok().and_then(|db| {
            db.query_row(
                "SELECT COUNT(*) FROM perf_gateway_calls WHERE output=12 AND usage_known=1",
                [],
                |r| r.get::<_, u64>(0),
            )
            .ok()
        });
        if found == Some(2) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "gateway metrics missing: {:?}",
            found
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    let logs = fs::read_dir(f.path.join("logs"))
        .unwrap()
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .collect::<String>();
    assert!(!logs.contains("synthetic-refresh"));
    assert!(!logs.contains("fresh-cli-user"));
    let profiles = f.json(&["codex", "list", "--json"]);
    assert!(profiles.to_string().contains("usage-plugin.invalid"));
}
