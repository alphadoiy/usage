#[cfg(unix)]
#[test]
fn lifecycle_callbacks_bind_concurrent_sessions_and_resumes_without_bark() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    let f = Fixture::new();
    let account = f.add();
    let binary = f.path.join("event codex");
    fs::write(&binary, r#"#!/usr/bin/env python3
import json,os,pathlib,shlex,sqlite3,subprocess,sys,time,tomllib
if sys.argv[1:] == ['--help']:
    print('--no-daemon --dangerously-bypass-hook-trust'); sys.exit(0)
assert '--dangerously-bypass-hook-trust' not in sys.argv[1:]
assert 'USAGE_CODEX_BARK_CONTEXT' not in os.environ
home=pathlib.Path(os.environ['CODEX_HOME'])
session=os.environ['FIXTURE_SESSION']
if 'resume' in sys.argv[1:]: assert sys.argv[sys.argv.index('resume')+1] == session
state=home
for arg in sys.argv[1:]:
    if arg.startswith('sqlite_home='): state=pathlib.Path(tomllib.loads(arg)['sqlite_home'])
conn=sqlite3.connect(state/'state_5.sqlite')
conn.execute('CREATE TABLE IF NOT EXISTS threads(id TEXT PRIMARY KEY,source TEXT,thread_source TEXT)')
conn.execute("INSERT OR REPLACE INTO threads VALUES (?,'cli','user')",(session,))
conn.commit(); conn.close()
commands={}
for arg in sys.argv[1:]:
    if arg.startswith('hooks.'):
        for event,groups in tomllib.loads(arg)['hooks'].items():
            assert groups[-1]['hooks'][0]['timeout'] == 3
            commands[event]=groups[-1]['hooks'][0]['command']
def event(kind):
    data={'hook_event_name':kind,'session_id':session,'turn_id':'fixture-turn','cwd':os.getcwd(),
          'model':'runtime-model','source':'resume' if 'resume' in sys.argv else 'startup',
          'prompt':'sentinel-private-prompt','last_assistant_message':'sentinel-private-reply',
          'tool_input':{'command':'sentinel-private-command'}}
    p=subprocess.run(shlex.split(commands[kind]),input=json.dumps(data).encode(),capture_output=True,timeout=3)
    assert p.returncode == 0 and json.loads(p.stdout) == {}
event('SessionStart'); event('UserPromptSubmit')
gate=os.environ.get('FIXTURE_GATE')
if gate:
    pathlib.Path(gate+'.ready').touch()
    until=time.monotonic()+15
    while not pathlib.Path(gate).exists():
        if time.monotonic()>until: sys.exit(95)
        time.sleep(.01)
event('Stop'); event('SessionEnd')
"#).unwrap();
    fs::set_permissions(&binary,fs::Permissions::from_mode(0o700)).unwrap();
    let first = "123e4567-e89b-12d3-a456-426614174001";
    let second = "123e4567-e89b-12d3-a456-426614174002";
    let launch = |session: &str, gate: Option<&str>, resume: bool| {
        let mut command = f.command(&["run", &account, "--cwd", f.path.to_str().unwrap(), "--binary", binary.to_str().unwrap()]);
        if resume { command.args(["--resume",session]); }
        command.env("FIXTURE_SESSION",session);
        if let Some(gate) = gate { command.env("FIXTURE_GATE",f.path.join(gate)); }
        command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    };
    let a = launch(first,Some("first"),false);
    let b = launch(second,Some("second"),false);
    let until = Instant::now()+Duration::from_secs(10);
    while !f.path.join("first.ready").exists() || !f.path.join("second.ready").exists() {
        assert!(Instant::now()<until,"lifecycle callbacks did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
    let runs: serde_json::Value = serde_json::from_str(&f.run(&["runs","--json"])).unwrap();
    assert_eq!(runs.as_array().unwrap().len(),2);
    for run in runs.as_array().unwrap() {
        assert!(run["finished_at"].is_null());
        assert_eq!(run["sessions"].as_array().unwrap().len(),1);
        assert_eq!(run["sessions"][0]["activity"],"working");
        assert_eq!(run["sessions"][0]["model"],"runtime-model");
    }
    assert_ne!(runs[0]["sessions"][0]["id"],runs[1]["sessions"][0]["id"]);
    fs::write(f.path.join("first"),"").unwrap();
    fs::write(f.path.join("second"),"").unwrap();
    success(a.wait_with_output().unwrap());
    success(b.wait_with_output().unwrap());
    success(launch(first,None,true).wait_with_output().unwrap());
    let runs: serde_json::Value = serde_json::from_str(&f.run(&["runs","--json"])).unwrap();
    assert_eq!(runs.as_array().unwrap().len(),3);
    assert_eq!(runs.as_array().unwrap().iter().filter(|run| run["sessions"][0]["id"]==first).count(),2);
    assert_eq!(runs[0]["context"]["identity"]["state"],"unresolved");
    for run in runs.as_array().unwrap() {
        assert_eq!(run["exit_code"],0);
        assert_eq!(run["sessions"][0]["activity"],"ended");
        let events: serde_json::Value = serde_json::from_str(&f.run(&["run-events",run["id"].as_str().unwrap(),"--json"])).unwrap();
        let kinds: Vec<_> = events.as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds,vec!["process_started","SessionStart","UserPromptSubmit","Stop","SessionEnd","process_exited"]);
    }
    let serialized = format!("{}{}", runs, serde_json::to_string(&f.events()).unwrap());
    assert!(!serialized.contains("sentinel-private-"));
    let db = fs::read(f.path.join("manager/manager.sqlite3")).unwrap();
    assert!(!String::from_utf8_lossy(&db).contains("sentinel-private-"));
    assert!(!f.run(&["show",&account]).contains("codex event"));
    assert!(!f.path.join("manager/bark.json").exists());
}
