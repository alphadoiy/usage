#![cfg(unix)]
//! Real Usage subprocesses and synthetic loopback upstreams; no vendor requests.
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

struct Upstream {
    port: u16,
    stopped: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<(String, String)>>>,
    task: Option<std::thread::JoinHandle<()>>,
}
impl Upstream {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stop = stopped.clone();
        let records = calls.clone();
        let task = std::thread::spawn(move || {
            let mut workers = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let records = records.clone();
                        workers.push(std::thread::spawn(move || serve(stream, records)));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            port,
            stopped,
            calls,
            task: Some(task),
        }
    }
}
fn serve(stream: TcpStream, calls: Arc<Mutex<Vec<(String, String)>>>) {
    // Darwin's accepted sockets can inherit O_NONBLOCK from the listener.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut length = 0;
    let mut auth = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse().unwrap();
        }
        if name.eq_ignore_ascii_case("authorization") {
            auth = value.trim().to_string();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let payload: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload["input"], "private-prompt");
    assert!(first.starts_with("POST /relay/v1/responses "));
    calls
        .lock()
        .unwrap()
        .push((auth, payload["model"].as_str().unwrap().into()));
    let mut stream = reader.into_inner();
    if payload["fixture_mode"] == "json" {
        let body = json!({"model":"json-served","usage":{"output_tokens":250,"input_tokens":100}})
            .to_string();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        return;
    }
    if payload["fixture_mode"] == "partial" {
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}}\n\n").unwrap();
        return;
    }
    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Upstream: preserved\r\nConnection: close\r\n\r\n").unwrap();
    write!(stream, "data: {{\"type\":\"response.created\"}}\n\n").unwrap();
    stream.flush().unwrap();
    std::thread::sleep(Duration::from_millis(60));
    for _ in 0..11 {
        write!(
            stream,
            "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"private-answer\"}}\n\n"
        )
        .unwrap();
        stream.flush().unwrap();
        std::thread::sleep(Duration::from_millis(60));
    }
    let event = json!({"type":"response.completed","response":{"id":"resp-fixture","model":"served-model","usage":{"output_tokens":400,"output_tokens_details":{"reasoning_tokens":100}}}});
    write!(stream, "data: {event}\n\n").unwrap();
    stream.flush().unwrap();
}
impl Drop for Upstream {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            let result = task.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}
struct Fixture {
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("usage-gateway-e2e-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.join(".git")).unwrap();
        let path = path.canonicalize().unwrap();
        fs::write(path.join("config.toml"), "").unwrap();
        fs::write(path.join("fake-codex"),r#"#!/usr/bin/env python3
import os,sys,json,time,urllib.request,urllib.error
from pathlib import Path
if sys.argv[1:]==['--help']:
    print('--no-daemon'); sys.exit(0)
base=None
for i,arg in enumerate(sys.argv):
    if arg=='-c' and sys.argv[i+1].split('=',1)[0].endswith('.base_url'):
        key=sys.argv[i+1].split('=',1)[0]
        # Codex splits override keys on dots without TOML quoting.
        assert all(part.replace('_','').replace('-','').isalnum() for part in key.split('.')), key
        base=json.loads(sys.argv[i+1].split('=',1)[1])
direct=os.environ.get('FIXTURE_EXPECT_DIRECT')=='1'
assert 'CODEX_SQLITE_HOME' not in os.environ, 'managed Home inherited a shared state directory'
if direct:
    import re
    assert base is None, 'fallback retained a gateway override'
    saved=(Path(os.environ['CODEX_HOME'])/'config.toml').read_text()
    base=json.loads(re.search(r'\bbase_url\s*=\s*("[^"]*")',saved).group(1))
    assert os.environ['USAGE_CODEX_API_KEY']=='synthetic-key'
assert base and base.startswith('http://127.0.0.1:')
root=Path(os.environ['FIXTURE_ROOT']); name=os.environ['FIXTURE_NAME']
(root/(name+'.ready')).write_text(str(os.getpid()))
deadline=time.monotonic()+15
while not (root/(name+'.go')).exists():
    if time.monotonic()>deadline: raise RuntimeError('fixture timeout')
    time.sleep(.01)
mode=os.environ.get('FIXTURE_MODE','full')
body=json.dumps({'model':'requested-model','input':'private-prompt','stream':mode!='json','fixture_mode':mode}).encode()
if not direct:
    request=urllib.request.Request(base+'/responses',data=body,headers={'Authorization':'Bearer wrong','Content-Type':'application/json'})
    try: urllib.request.urlopen(request,timeout=5); raise AssertionError('missing authorization')
    except urllib.error.HTTPError as e: assert e.code==401
request=urllib.request.Request(base+'/responses',data=body,headers={'Authorization':'Bearer '+os.environ['USAGE_CODEX_API_KEY'],'session_id':os.environ['FIXTURE_SESSION'],'Content-Type':'application/json'})
with urllib.request.urlopen(request,timeout=5) as response:
    reply=response.read().decode()
    if mode=='full':
        assert response.headers['X-Upstream']=='preserved'
        assert reply.count('private-answer')==11 and 'response.completed' in reply
    elif mode=='json': assert json.loads(reply)['usage']['output_tokens']==250
    else: assert 'partial' in reply and 'response.completed' not in reply
(root/(name+'.result')).write_text('ok')
"#).unwrap();
        fs::set_permissions(path.join("fake-codex"), fs::Permissions::from_mode(0o700)).unwrap();
        Self { path }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_usage"));
        c.args(args)
            .env("USAGE_CODEX_DIR", self.path.join("manager"))
            .env("XDG_DATA_HOME", self.path.join("data"))
            .env("USAGE_CONFIG", self.path.join("config.toml"))
            .env("USAGE_LOG_DIR", self.path.join("logs"))
            .env("USAGE_LOG", "debug")
            .env("USAGE_GATEWAY", "on")
            .env_remove("USAGE_GATEWAY_TRANSPORT")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env("FIXTURE_ROOT", &self.path)
            .env("CODEX_SQLITE_HOME", self.path.join("shared-state"))
            .current_dir(&self.path);
        c
    }
    fn add(&self, name: &str, port: u16, key: &str) {
        let mut child = self
            .command(&[
                "codex",
                "add",
                name,
                "--base-url",
                &format!("http://127.0.0.1:{port}/relay/v1"),
                "--model",
                "requested-model",
                "--key-stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{key}\n").as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn start(&self, name: &str, session: &str) -> std::process::Child {
        self.start_mode(name, session, "full")
    }
    fn copy(&self, source: &str, name: &str) {
        let output = self
            .command(&["codex", "copy", source, name])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn start_mode(&self, name: &str, session: &str, mode: &str) -> std::process::Child {
        self.start_expect(name, session, mode, false)
    }
    fn start_expect(
        &self,
        name: &str,
        session: &str,
        mode: &str,
        direct: bool,
    ) -> std::process::Child {
        self.command(&[
            "codex",
            "run",
            name,
            "--cwd",
            self.path.to_str().unwrap(),
            "--binary",
            self.path.join("fake-codex").to_str().unwrap(),
        ])
        .env("FIXTURE_NAME", name)
        .env("FIXTURE_EXPECT_DIRECT", if direct { "1" } else { "0" })
        .env("FIXTURE_MODE", mode)
        .env("FIXTURE_SESSION", session)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(self.path.join(format!("{name}.stderr"))).unwrap())
        .spawn()
        .unwrap()
    }
    fn status(&self) -> Value {
        let output = self.command(&["gateway", "status"]).output().unwrap();
        assert!(output.status.success());
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn wait(&self, name: &str) {
        let began = Instant::now();
        while !self.path.join(name).exists() {
            assert!(
                began.elapsed() < Duration::from_secs(15),
                "timed out waiting for {name}: {}",
                fs::read_to_string(
                    self.path
                        .join(format!("{}.stderr", name.split('.').next().unwrap()))
                )
                .unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.path.join("manager/gateway/state.json").exists()
            && let Ok(output) = self.command(&["gateway", "status"]).output()
            && let Ok(value) = serde_json::from_slice::<Value>(&output.stdout)
            && value["service"] == "usage-gateway"
            && let Some(pid) = value["pid"].as_i64()
        {
            // SAFETY: the authenticated service PID comes from this fixture's private Manager root.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        for name in ["one.ready", "two.ready"] {
            if !self.path.join(name.replace(".ready", ".result")).exists()
                && let Ok(text) = fs::read_to_string(self.path.join(name))
                && let Ok(pid) = text.parse::<i32>()
                && let Ok(process) = Command::new("ps")
                    .args(["-p", &pid.to_string(), "-o", "command="])
                    .output()
                && String::from_utf8_lossy(&process.stdout)
                    .contains(self.path.join("fake-codex").to_str().unwrap())
            {
                // SAFETY: fake Agents write their own PID only inside this fixture's private root.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}
#[test]
fn shared_gateway_survives_launcher_exit_isolates_credentials_and_populates_metrics() {
    let f = Fixture::new();
    let upstream = Upstream::new();
    f.add("one", upstream.port, "synthetic-upstream-one");
    f.add("two", upstream.port, "synthetic-upstream-two");
    let mut one = f.start("one", "123e4567-e89b-12d3-a456-426614174001");
    f.wait("one.ready");
    let before = f.status();
    let mut two = f.start("two", "123e4567-e89b-12d3-a456-426614174002");
    f.wait("two.ready");
    let after = f.status();
    assert_eq!(before["pid"], after["pid"]);
    assert_eq!(after["routes"], 2);
    one.kill().unwrap();
    one.wait().unwrap();
    std::thread::sleep(Duration::from_millis(2200));
    assert_eq!(
        f.status()["routes"],
        2,
        "the orphan Agent still owns its inherited lease"
    );
    fs::write(f.path.join("one.go"), "").unwrap();
    fs::write(f.path.join("two.go"), "").unwrap();
    f.wait("one.result");
    f.wait("two.result");
    assert!(two.wait().unwrap().success());
    let db_path = f.path.join("data/usage/performance.sqlite3");
    let db = rusqlite::Connection::open(db_path).unwrap();
    let began = Instant::now();
    loop {
        let calls: u64 = db
            .query_row("SELECT COUNT(*) FROM perf_gateway_calls", [], |r| r.get(0))
            .unwrap();
        if calls == 2 {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
    let stats: (u64, u64, u64) = db
        .query_row(
            "SELECT COUNT(DISTINCT scope),SUM(decode_out),SUM(status=200) FROM perf_gateway_calls",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(stats, (2, 600, 2));
    let calls = upstream.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().any(|v| v.0 == "Bearer synthetic-upstream-one"));
    assert!(calls.iter().any(|v| v.0 == "Bearer synthetic-upstream-two"));
    for entry in fs::read_dir(f.path.join("logs")).unwrap() {
        let log = fs::read_to_string(entry.unwrap().path()).unwrap();
        for secret in [
            "synthetic-upstream-one",
            "synthetic-upstream-two",
            "private-prompt",
            "private-answer",
        ] {
            assert!(!log.contains(secret));
        }
    }
    let expired = Instant::now();
    loop {
        let ended: u64 = db
            .query_row(
                "SELECT COUNT(*) FROM perf_gateway_runs WHERE ended IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if f.status()["routes"] == 0 && ended == 2 {
            break;
        }
        assert!(expired.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(50));
    }
    let mode = fs::metadata(f.path.join("manager/gateway/state.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn partial_stream_and_nonstream_json_do_not_invent_first_token_or_tps() {
    let f = Fixture::new();
    let upstream = Upstream::new();
    f.add("one", upstream.port, "synthetic-key");
    f.copy("one", "two");
    for (name, mode, session) in [
        ("one", "partial", "123e4567-e89b-12d3-a456-426614174001"),
        ("two", "json", "123e4567-e89b-12d3-a456-426614174002"),
    ] {
        let mut child = f.start_mode(name, session, mode);
        f.wait(&format!("{name}.ready"));
        fs::write(f.path.join(format!("{name}.go")), "").unwrap();
        f.wait(&format!("{name}.result"));
        assert!(child.wait().unwrap().success());
    }
    let db = rusqlite::Connection::open(f.path.join("data/usage/performance.sqlite3")).unwrap();
    let began = Instant::now();
    loop {
        let count: u64 = db
            .query_row("SELECT COUNT(*) FROM perf_gateway_calls", [], |r| r.get(0))
            .unwrap();
        if count == 2 {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
    let rows:Vec<(String,bool,bool,Value)>=db.prepare("SELECT model,ttft IS NULL,decode_out IS NULL,metadata FROM perf_gateway_calls ORDER BY model").unwrap()
        .query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,serde_json::from_str::<Value>(&r.get::<_,String>(3)?).unwrap())))
        .unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.1 && r.2));
    assert!(rows.iter().any(|r| r.3["complete"] == false
        && r.3["output"].is_null()
        && r.3["speed_unavailable"] == "incomplete_response"));
    assert!(rows.iter().any(|r| r.3["complete"] == true
        && r.3["output"] == 250
        && r.3["speed_unavailable"] == "no_stream_content_timing"));
}

#[test]
fn unavailable_gateway_preparation_keeps_direct_endpoint_credentials_and_launch() {
    use fs2::FileExt;
    // An unavailable metrics store no longer blocks the route: the launch is still proxied and
    // its statistics come from the session log. The other obstructions launch directly.
    for (obstruction, direct) in [("busy", true), ("filesystem", true), ("metrics", false)] {
        let f = Fixture::new();
        let upstream = Upstream::new();
        f.add("one", upstream.port, "synthetic-key");
        let path = f.path.join("manager/gateway");
        let _locked = match obstruction {
            "busy" => {
                fs::create_dir_all(&path).unwrap();
                let file = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(path.join("start.lock"))
                    .unwrap();
                file.lock_exclusive().unwrap();
                Some(file)
            }
            "filesystem" => {
                fs::write(&path, "not a directory").unwrap();
                None
            }
            _ => {
                fs::create_dir_all(f.path.join("data/usage/performance.sqlite3")).unwrap();
                None
            }
        };
        let start = Instant::now();
        let mut child = f.start_expect(
            "one",
            "123e4567-e89b-12d3-a456-426614174001",
            "json",
            direct,
        );
        f.wait("one.ready");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "observation preparation blocked launch: {obstruction}"
        );
        fs::write(f.path.join("one.go"), "").unwrap();
        f.wait("one.result");
        assert!(child.wait().unwrap().success());
        assert_eq!(upstream.calls.lock().unwrap().len(), 1);
        let logs = fs::read_dir(f.path.join("logs"))
            .unwrap()
            .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
            .collect::<String>();
        assert!(logs.contains(if direct {
            "gateway_fallback"
        } else {
            "gateway_run_unregistered"
        }));
        assert!(!logs.contains("synthetic-key"));
    }
}

#[test]
fn unaddressable_provider_id_fails_launch_instead_of_connecting_directly() {
    let f = Fixture::new();
    let upstream = Upstream::new();
    let config = f.path.join("dotted.toml");
    fs::write(
        &config,
        format!(
            "model = \"requested-model\"\nmodel_provider = \"my.relay\"\n\n[model_providers.\"my.relay\"]\nname = \"Relay\"\nbase_url = \"http://127.0.0.1:{}/relay/v1\"\n",
            upstream.port
        ),
    )
    .unwrap();
    let mut child = f
        .command(&[
            "codex",
            "add",
            "dotted",
            "--config",
            config.to_str().unwrap(),
            "--key-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"synthetic-key\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let status = f
        .start_expect(
            "dotted",
            "123e4567-e89b-12d3-a456-426614174009",
            "json",
            true,
        )
        .wait()
        .unwrap();
    assert!(!status.success());
    let stderr = fs::read_to_string(f.path.join("dotted.stderr")).unwrap();
    assert!(stderr.contains("启动失败"), "{stderr}");
    assert!(
        !f.path.join("dotted.ready").exists(),
        "Codex must not start directly"
    );
    assert!(upstream.calls.lock().unwrap().is_empty());
}

#[test]
fn a_launch_waits_for_a_brief_concurrent_preparation_instead_of_skipping_observation() {
    use fs2::FileExt;
    let f = Fixture::new();
    let upstream = Upstream::new();
    f.add("one", upstream.port, "synthetic-key");
    let path = f.path.join("manager/gateway");
    fs::create_dir_all(&path).unwrap();
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path.join("start.lock"))
        .unwrap();
    held.lock_exclusive().unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(held);
    });
    let mut child = f.start_expect("one", "123e4567-e89b-12d3-a456-426614174003", "json", false);
    f.wait("one.ready");
    release.join().unwrap();
    fs::write(f.path.join("one.go"), "").unwrap();
    f.wait("one.result");
    assert!(child.wait().unwrap().success());
    assert_eq!(upstream.calls.lock().unwrap().len(), 1);
}

#[test]
fn new_launch_restarts_crashed_gateway_without_replaying_old_requests() {
    let f = Fixture::new();
    let upstream = Upstream::new();
    f.add("one", upstream.port, "synthetic-key");
    f.copy("one", "two");
    let mut one = f.start("one", "123e4567-e89b-12d3-a456-426614174001");
    f.wait("one.ready");
    let old = f.status();
    let pid = old["pid"].as_i64().unwrap() as i32;
    // SAFETY: this PID is supplied by the authenticated gateway of this fixture's private Manager.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    let began = Instant::now();
    loop {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        if !output.status.success()
            || String::from_utf8_lossy(&output.stdout)
                .trim()
                .starts_with('Z')
        {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut two = f.start("two", "123e4567-e89b-12d3-a456-426614174002");
    f.wait("two.ready");
    let fresh = f.status();
    assert_ne!(fresh["pid"], old["pid"]);
    fs::write(f.path.join("two.go"), "").unwrap();
    f.wait("two.result");
    assert!(two.wait().unwrap().success());
    assert_eq!(
        upstream.calls.lock().unwrap().len(),
        1,
        "crash recovery must never replay the orphan launch"
    );
    one.kill().unwrap();
    let _ = one.wait();
}

#[test]
fn oauth_launch_always_connects_directly_and_keeps_native_auth_refresh() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    // Official subscriptions never use the gateway, whatever transport is requested.
    for websocket in [false, true] {
        let f = Fixture::new();
        let claims = json!({"sub":"synthetic-user","email":"synthetic@example.test","https://api.openai.com/auth":{"chatgpt_account_id":"workspace","chatgpt_user_id":"synthetic-user"}});
        let token = format!(
            "e30.{}.synthetic",
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let source = f.path.join("oauth-source.json");
        fs::write(&source,json!({"auth_mode":"chatgpt","tokens":{"id_token":token,"access_token":"synthetic-access-before","refresh_token":"synthetic-refresh","account_id":"workspace"}}).to_string()).unwrap();
        let imported = f
            .command(&[
                "codex",
                "import-auth",
                "--source",
                source.to_str().unwrap(),
                "--name",
                "oauth",
            ])
            .output()
            .unwrap();
        assert!(
            imported.status.success(),
            "{}",
            String::from_utf8_lossy(&imported.stderr)
        );
        let before = f
            .command(&["codex", "show", "oauth"])
            .output()
            .unwrap()
            .stdout;
        fs::write(f.path.join("fake-codex"),r#"#!/usr/bin/env python3
import os,sys,json
from pathlib import Path
if sys.argv[1:]==['--version']:
    print('codex-cli 0.163.0');sys.exit(0)
if sys.argv[1:]==['--help']:
    print('--no-daemon --profile');sys.exit(0)
args=sys.argv[1:];assert '--profile' in args
assert not os.getenv('USAGE_CODEX_API_KEY') and not os.getenv('OPENAI_API_KEY')
overrides=[args[i+1] for i,arg in enumerate(args[:-1]) if arg=='-c']
assert 'model_provider="openai"' in overrides and 'forced_login_method="chatgpt"' in overrides
assert not any(value.startswith('model_providers.') for value in overrides)
assert not any(value.startswith(('openai_base_url=','chatgpt_base_url=')) for value in overrides), overrides
assert os.environ['NO_PROXY']==os.environ['no_proxy']=='127.0.0.1,localhost', 'proxy bypass was extended for a gateway'
home=Path(os.environ['CODEX_HOME']);config=(home/'config.toml').read_text()
assert '127.0.0.1' not in config
saved=json.loads((home/'auth.json').read_text());assert saved['tokens']['access_token']=='synthetic-access-before'
saved['tokens']['access_token']='synthetic-access-rotated';saved['tokens']['refresh_token']='synthetic-refresh-rotated'
(home/'auth.json').write_text(json.dumps(saved))
(Path(os.environ['FIXTURE_ROOT'])/'oauth-capture.json').write_text(json.dumps({'home':str(home)}))
"#).unwrap();
        let output = f
            .command(&[
                "codex",
                "run",
                "oauth",
                "--cwd",
                f.path.to_str().unwrap(),
                "--binary",
                f.path.join("fake-codex").to_str().unwrap(),
            ])
            .env(
                "USAGE_GATEWAY_TRANSPORT",
                if websocket { "ws" } else { "sse" },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let capture: Value =
            serde_json::from_slice(&fs::read(f.path.join("oauth-capture.json")).unwrap()).unwrap();
        let auth: Value = serde_json::from_slice(
            &fs::read(PathBuf::from(capture["home"].as_str().unwrap()).join("auth.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(auth["tokens"]["access_token"], "synthetic-access-rotated");
        assert_eq!(
            f.command(&["codex", "show", "oauth"])
                .output()
                .unwrap()
                .stdout,
            before
        );
        let mut skipped = false;
        for entry in fs::read_dir(f.path.join("logs")).unwrap() {
            let text = fs::read_to_string(entry.unwrap().path()).unwrap();
            for line in text.lines() {
                if let Ok(event) = serde_json::from_str::<Value>(line) {
                    assert_ne!(event["fields"]["event"], "gateway_route_registered");
                    assert_ne!(event["fields"]["event"], "gateway_injected");
                    skipped |= event["fields"]["event"] == "gateway_skipped"
                        && event["fields"]["reason"] == "official_direct";
                }
            }
            for secret in [
                "synthetic-access-before",
                "synthetic-access-rotated",
                "synthetic-refresh",
            ] {
                assert!(!text.contains(secret));
            }
        }
        assert!(skipped, "the direct launch is logged");
        assert!(
            !f.path.join("manager/gateway/state.json").exists(),
            "an official launch must not start the gateway"
        );
    }
}
