use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("usage-resource-cli-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn config(&self, text: &str) {
        fs::write(self.0.join("config.toml"), text).unwrap();
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_usage"));
        command
            .args(args)
            .env("USAGE_CONFIG", self.0.join("config.toml"))
            .env("USAGE_CODEX_DIR", self.0.join("manager"))
            .env("USAGE_CODEX_BASE_HOME", self.0.join("user-codex"))
            .env("USAGE_LOG_DIR", self.0.join("logs"))
            .env("USAGE_LOG", "debug")
            .env("USAGE_GATEWAY", "off")
            .env_remove("USAGE_RESOURCE_FIXTURE_KEY");
        command
    }
    fn logs(&self) -> String {
        fs::read_dir(self.0.join("logs"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[cfg(unix)]
#[test]
fn run_bound_query_uses_pinned_token_and_rejects_rotation_before_http() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.config("");
    fs::create_dir(fixture.0.join(".git")).unwrap();
    let server = Server::new(
        r#"{"code":true,"data":{"object":"token_usage","total_available":50,"total_used":1,"unlimited_quota":false}}"#,
    );
    let endpoint = format!("{}/v1", server.url);
    let mut child = fixture
        .command(&[
            "codex",
            "add",
            "Bound",
            "--base-url",
            &endpoint,
            "--model",
            "fixture",
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
        .write_all(b"synthetic-bound-token\n")
        .unwrap();
    let added = child.wait_with_output().unwrap();
    assert!(added.status.success(), "{added:?}");
    let profiles = fixture
        .command(&["codex", "list", "--json"])
        .output()
        .unwrap();
    let profiles: serde_json::Value = serde_json::from_slice(&profiles.stdout).unwrap();
    let profile = profiles[0]["id"].as_str().unwrap();
    fixture.config(&format!("{}\nprofile_ids=[\"{profile}\"]\ninference={{base_url=\"{endpoint}\",credential_env=\"USAGE_RESOURCE_FIXTURE_KEY\"}}",config(&server.url,"new_api_token","{}")));
    let binary = fixture.0.join("mock-codex");
    fs::write(
        &binary,
        "#!/bin/sh\nif [ \"$1\" = --help ]; then echo --no-daemon; fi\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let launched = fixture
        .command(&[
            "codex",
            "run",
            profile,
            "--cwd",
            fixture.0.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .env("USAGE_RESOURCE_FIXTURE_KEY", "synthetic-bound-token")
        .output()
        .unwrap();
    assert!(launched.status.success(), "{launched:?}");
    let runs = fixture
        .command(&["codex", "runs", "--json"])
        .output()
        .unwrap();
    let runs: serde_json::Value = serde_json::from_slice(&runs.stdout).unwrap();
    assert_eq!(runs[0]["context"]["resources"][0]["state"], "matched");
    let run = runs[0]["id"].as_str().unwrap();
    let output = fixture
        .command(&["resources", "--run", run, "--json"])
        .env("USAGE_RESOURCE_FIXTURE_KEY", "synthetic-bound-token")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value[0]["snapshot"]["metrics"][0]["remaining"], "50");
    let request = server
        .requests
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    assert!(request.starts_with("GET /api/usage/token HTTP/1.1"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer synthetic-bound-token")
    );
    let rotated = fixture
        .command(&["resources", "--run", run, "--json"])
        .env("USAGE_RESOURCE_FIXTURE_KEY", "rotated-bound-token")
        .output()
        .unwrap();
    assert_eq!(rotated.status.code(), Some(2));
    assert!(server.requests.try_recv().is_err());
    assert!(!fixture.logs().contains("synthetic-bound-token"));
    assert!(!stdout(&output).contains("synthetic-bound-token"));
}
fn config(base: &str, kind: &str, options: &str) -> String {
    format!(
        r#"
[[resources]]
id = "work"
label = "工作账号"
service = "{kind}"
base_url = "{base}"
credential_env = "USAGE_RESOURCE_FIXTURE_KEY"
options = {options}
"#
    )
}
struct Server {
    url: String,
    requests: mpsc::Receiver<String>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new(body: &'static str) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (sender, requests) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let Ok((mut socket, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut buffer).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buffer[..n]);
                }
                sender.send(String::from_utf8(request).unwrap()).unwrap();
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

#[test]
fn listing_and_missing_credentials_never_make_network_calls_or_expose_references() {
    let fixture = Fixture::new();
    fixture.config(&config("https://example.invalid", "new_api_token", "{}"));
    let list = fixture
        .command(&["resources", "--list", "--json"])
        .output()
        .unwrap();
    assert!(list.status.success(), "{:?}", list);
    let list: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list[0]["service"], "new_api_token");
    assert_eq!(list[0]["label"], "工作账号");
    let output = fixture.command(&["resources", "--json"]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value[0]["snapshot"]["state"], "needs_configuration");
    assert_eq!(value[0]["snapshot"]["scope"]["kind"], "token");
    assert!(!stdout(&output).contains("USAGE_RESOURCE_FIXTURE_KEY"));
    let invalid = fixture
        .command(&["resources", "missing", "--json"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    let logs = fixture.logs();
    assert!(!logs.contains("resource_http_result"));
}

#[test]
fn real_cli_queries_read_only_token_endpoint_and_keeps_credentials_out_of_output_and_logs() {
    let fixture = Fixture::new();
    let server = Server::new(
        r#"{"code":true,"data":{"object":"token_usage","total_available":1250000,"total_used":99,"unlimited_quota":false}}"#,
    );
    fixture.config(&config(
        &format!("{}/panel", server.url),
        "new_api_token",
        r#"{ quota_per_unit = "500000", currency = "USD" }"#,
    ));
    let output = fixture
        .command(&["resources", "work", "--json"])
        .env("USAGE_RESOURCE_FIXTURE_KEY", "fixture-resource-secret")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value[0]["snapshot"]["metrics"][1]["remaining"]["amount"],
        "2.5"
    );
    assert_eq!(
        value[0]["snapshot"]["metrics"][1]["remaining"]["currency"],
        "USD"
    );
    let request = server
        .requests
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .to_lowercase();
    assert!(request.starts_with("get /panel/api/usage/token http/1.1"));
    assert!(request.contains("authorization: bearer fixture-resource-secret"));
    assert!(!stdout(&output).contains("fixture-resource-secret"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-resource-secret"));
    let logs = fixture.logs();
    assert!(!logs.contains("fixture-resource-secret"));
    let events: Vec<serde_json::Value> = logs
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let http = events
        .iter()
        .find(|event| event["fields"]["event"] == "resource_http_result")
        .unwrap();
    let result = events
        .iter()
        .find(|event| event["fields"]["event"] == "resource_query_result")
        .unwrap();
    assert_eq!(http["span"]["operation"], "resource.query");
    assert_eq!(http["span"]["operation_id"], result["span"]["operation_id"]);
    assert_eq!(http["fields"]["target"], result["fields"]["target"]);
    assert!(
        http["fields"]["target"]
            .as_str()
            .unwrap()
            .starts_with("resource-v1:")
    );
    assert_eq!(result["fields"]["successful"], true);
}

#[test]
fn official_cost_query_outputs_cost_with_utc_range_not_a_credit_balance() {
    let fixture = Fixture::new();
    let server = Server::new(
        r#"{"object":"page","data":[{"results":[{"object":"organization.costs.result","amount":{"value":0.125,"currency":"usd"}}]}],"has_more":false,"next_page":null}"#,
    );
    fixture.config(&config(
        &format!("{}/v1", server.url),
        "openai_costs",
        r#"{ days = 7, project_id = "proj-fixture" }"#,
    ));
    let output = fixture
        .command(&["resources"])
        .env("USAGE_RESOURCE_FIXTURE_KEY", "fixture-admin-secret")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let text = stdout(&output);
    assert!(text.contains("费用 0.125 USD"));
    assert!(text.contains("项目 proj-fixture"));
    assert!(text.contains("UTC"));
    assert!(!text.contains("余额"));
    let request = server
        .requests
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    assert!(request.starts_with("GET /v1/organization/costs?"));
    assert!(request.contains("project_ids=proj-fixture"));
    assert!(!fixture.logs().contains("fixture-admin-secret"));
}

#[test]
fn profile_filter_uses_explicit_resource_association() {
    let fixture = Fixture::new();
    fixture.config("");
    let mut add = fixture
        .command(&[
            "codex",
            "add",
            "Fixture",
            "--base-url",
            "https://example.invalid/v1",
            "--model",
            "fixture",
            "--key-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    add.stdin
        .take()
        .unwrap()
        .write_all(b"fixture-inference-secret\n")
        .unwrap();
    let added = add.wait_with_output().unwrap();
    assert!(added.status.success(), "{:?}", added);
    let profiles = fixture
        .command(&["codex", "list", "--json"])
        .output()
        .unwrap();
    assert!(profiles.status.success());
    let value: serde_json::Value = serde_json::from_slice(&profiles.stdout).unwrap();
    let id = value[0]["id"].as_str().unwrap();
    fixture.config(&format!(
        "{}\nprofile_ids = [\"{id}\"]",
        config("https://example.invalid", "new_api_token", "{}")
    ));
    let output = fixture
        .command(&["resources", "--profile", "Fixture", "--list", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value[0]["profile_ids"][0], id);
    fixture.config(&config("https://example.invalid", "new_api_token", "{}"));
    let output = fixture
        .command(&["resources", "--profile", "Fixture", "--list"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
