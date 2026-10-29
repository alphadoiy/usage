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
            .env("USAGE_CODEX_BASE_HOME", self.path.join("user-codex"))
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
fn local_automatic_login_accepts_no_url_and_invalid_browser_urls_cannot_save_accounts() {
    let f = Fixture::new();
    f.run(&[
        "plugins",
        "add",
        "fixture",
        f.path.join("fixture.mjs").to_str().unwrap(),
        "--runtime",
        "node",
    ]);
    let invalid = f
        .command(&[
            "plugins",
            "login",
            "fixture",
            "fixture",
            "--method",
            "3",
            "--input",
            "url=invalid-url",
        ])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("插件登录地址无效"));
    assert!(
        f.json(&["plugins", "accounts"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    let encrypted = f
        .command(&[
            "plugins",
            "login",
            "fixture",
            "fixture",
            "--method",
            "3",
            "--input",
            "error=Desktop keeps its sign-in encrypted; private-echo-do-not-display",
        ])
        .output()
        .unwrap();
    assert!(!encrypted.status.success());
    let error = String::from_utf8_lossy(&encrypted.stderr);
    assert!(error.contains("桌面登录凭据已加密"));
    assert!(error.contains("account (browser)"));
    assert!(!error.contains("HTTP 502"));
    assert!(!error.contains("private-echo"));
    assert!(
        f.json(&["plugins", "accounts"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Leave browser opening enabled: a local callback must never call `open`.
    f.run(&["plugins", "login", "fixture", "fixture", "--method", "3"]);
    let accounts = f.json(&["plugins", "accounts"]);
    assert_eq!(accounts.as_array().unwrap().len(), 1);
    assert_eq!(accounts[0]["name"], "fixture-desktop");
    let credential: Value = serde_json::from_slice(
        &fs::read(
            f.path
                .join("plugins/accounts")
                .join(format!("{}.json", accounts[0]["id"].as_str().unwrap())),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(credential["auth"]["source"], "desktop");
    assert_eq!(credential["auth"]["type"], "oauth");
    assert_eq!(credential["auth"]["access"], "");
    let logs = fs::read_dir(f.path.join("logs"))
        .unwrap()
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .collect::<String>();
    assert!(logs.contains("plugin_login_rejected"));
    assert!(logs.contains("\"flow\":\"local\""));
    assert!(!logs.contains("invalid-url"));
    assert!(logs.contains("plugin_auth_failed"));
    assert!(logs.contains("desktop_encrypted"));
    assert!(!logs.contains("private-echo"));
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

#[test]
fn explicit_checkin_cli_previews_without_claiming_and_repeats_are_account_scoped() {
    let f = Fixture::new();
    f.run(&[
        "plugins",
        "add",
        "fixture",
        f.path.join("fixture.mjs").to_str().unwrap(),
        "--runtime",
        "node",
    ]);
    f.run(&[
        "plugins",
        "login",
        "fixture",
        "fixture",
        "--method",
        "0",
        "--input",
        "uid=checkin-user",
        "--no-browser",
    ]);
    let accounts = f.json(&["plugins", "accounts"]);
    let id = accounts[0]["id"].as_str().unwrap();
    assert_eq!(f.json(&["plugins", "checkin", id])["outcome"], "ready");
    assert_eq!(f.json(&["plugins", "checkin", id])["outcome"], "ready");
    let claimed = f.json(&["plugins", "checkin", id, "--confirm"]);
    assert_eq!(claimed["outcome"], "claimed");
    assert_eq!(claimed["credit"], 10);
    assert!(!claimed.to_string().contains("private-fixture-sentinel"));
    assert_eq!(
        f.json(&["plugins", "checkin", id, "--confirm"])["outcome"],
        "done"
    );
}

#[test]
fn checkin_reuses_health_capabilities_and_refuses_old_gateways_before_posting() {
    use std::sync::{Arc, Mutex};
    for capability in [None, Some(0), Some(1), Some(2)] {
        let f = Fixture::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let records = calls.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let app =
                        axum::Router::new().fallback(move |request: axum::extract::Request| {
                            let records = records.clone();
                            async move {
                                assert_eq!(
                                    request.headers()["authorization"],
                                    "Bearer synthetic-control"
                                );
                                let path = request.uri().path();
                                records
                                    .lock()
                                    .unwrap()
                                    .push(format!("{} {path}", request.method()));
                                let value = if path == "/_usage/status" {
                                    let mut value =
                                        serde_json::json!({"service":"usage-gateway","protocol":12});
                                    if let Some(version) = capability {
                                        value["plugin_actions"] = version.into();
                                    }
                                    value
                                } else {
                                    assert_eq!(path, "/_usage/plugins/call");
                                    serde_json::json!({"outcome":"done"})
                                };
                                axum::Json(value)
                            }
                        });
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = stopped.await;
                        })
                        .await
                        .unwrap();
                });
        });
        let directory = f.path.join("manager/gateway-v12");
        fs::create_dir_all(&directory).unwrap();
        let state = directory.join("state.json");
        fs::write(
            &state,
            serde_json::json!({"port":port,"token":"synthetic-control"}).to_string(),
        )
        .unwrap();
        let output = f
            .command(&["plugins", "checkin", "synthetic-account"])
            .output()
            .unwrap();
        let _ = stop.send(());
        task.join().unwrap();
        fs::remove_file(state).unwrap();
        let mut expected = vec!["GET /_usage/status"];
        if capability == Some(1) {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap()["outcome"],
                "done"
            );
            expected.push("POST /_usage/plugins/call");
        } else {
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("网关尚不支持账号操作"));
        }
        assert_eq!(*calls.lock().unwrap(), expected);
    }
}

#[test]
fn repeated_plugin_login_and_provider_binding_do_not_create_duplicate_entries() {
    let f = Fixture::new();
    f.run(&[
        "plugins",
        "add",
        "fixture",
        f.path.join("fixture.mjs").to_str().unwrap(),
        "--runtime",
        "node",
    ]);
    let mut first = String::new();
    for _ in 0..2 {
        f.run(&[
            "plugins",
            "login",
            "fixture",
            "fixture",
            "--input",
            "uid=same-user",
            "--no-browser",
        ]);
        let accounts = f.json(&["plugins", "accounts"]);
        assert_eq!(accounts.as_array().unwrap().len(), 1);
        let id = accounts[0]["id"].as_str().unwrap();
        if first.is_empty() {
            first = id.into();
        } else {
            assert_eq!(id, first);
        }
    }
    f.run(&[
        "plugins",
        "bind",
        &first,
        "--name",
        "Existing",
        "--model",
        "fixture-model",
    ]);
    let duplicate = f
        .command(&[
            "plugins",
            "bind",
            &first,
            "--name",
            "Renamed",
            "--model",
            "fixture-model",
        ])
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("Provider 配置已存在：Existing"));
    assert_eq!(
        f.json(&["codex", "list", "--json"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.run(&[
        "plugins",
        "bind",
        &first,
        "--name",
        "Another model",
        "--model",
        "another-model",
    ]);
    assert_eq!(
        f.json(&["codex", "list", "--json"])
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
