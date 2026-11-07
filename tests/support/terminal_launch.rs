#[cfg(unix)]
#[test]
fn split_launch_in_rex_splits_beside_usage_for_each_managed_instance() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let account = f.add();
    let bin_dir = f.path.join("rex bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let rex = bin_dir.join("rex");
    let inspect = r#"import json,os,sys
if sys.argv[1]=='session':
    print(json.dumps({'windows':[{'window_id':'w0','layers':[{'layout':{'block_id':'block:usage'},'blocks':[{'block_id':'block:usage'}]}]}]}))
    sys.exit(0)
"#;
    fs::write(&rex, format!(r#"#!/usr/bin/env python3
{inspect}
with open(os.environ['CAPTURE_FILE'], 'a') as f:
    f.write(json.dumps(sys.argv[1:])+'\n')
"#)).unwrap();
    fs::set_permissions(&rex, fs::Permissions::from_mode(0o700)).unwrap();
    let capture = f.path.join("tabs.jsonl");
    let config = f.path.join("layout.toml");
    fs::write(&config, "[sessions]\nlayout = \"split\"\n").unwrap();
    for _ in 0..2 {
        success(f.root_command(&["--config", config.to_str().unwrap(), "codex", "run", &account, "--cwd", f.path.to_str().unwrap(), "--split", "--binary", "/tmp/codex with spaces", "--", "--model", "fixture-model"])
            .env("REX_SESSION", "session:fixture")
            .env("REX_BLOCK", "block:usage")
            .env("REX_BIN_DIR", &bin_dir)
            .env("GHOSTTY_RESOURCES_DIR", "/inherited/ghostty")
            .env("CAPTURE_FILE", &capture)
            .output().unwrap());
    }
    let lines: Vec<String> = fs::read_to_string(capture).unwrap().lines()
        .map(|line| serde_json::from_str::<Vec<String>>(line).unwrap())
        .filter(|args| args.first().map(String::as_str) == Some("run"))
        .map(|args| serde_json::to_string(&args).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    for line in lines {
        let args: Vec<String> = serde_json::from_str(&line).unwrap();
        assert_eq!(&args[..3], ["run", "--session", "session:fixture"]);
        assert_eq!(&args[3..5], ["--cwd", f.path.to_str().unwrap()]);
        assert!(args.windows(2).any(|pair| pair == ["--block", "block:usage"]));
        assert!(args.windows(2).any(|pair| pair == ["--split", "right"] || pair == ["--split", "below"]));
        let shell_start = args.iter().position(|arg| arg == "--").unwrap() + 3;
        let shell = shlex::split(&args[shell_start]).unwrap();
        assert!(shell.windows(2).any(|pair| pair == ["run", account.as_str()]));
        assert!(shell.windows(2).any(|pair| pair == ["--binary", "/tmp/codex with spaces"]));
        assert!(shell.windows(2).any(|pair| pair == ["--model", "fixture-model"]));
        assert!(shell.iter().any(|arg| arg.starts_with("USAGE_CODEX_DIR=")));
    }
}

/// The configured layout and focus reach the terminal adapter through the real CLI.
#[cfg(unix)]
#[test]
fn configured_layout_and_focus_reach_rex_from_the_cli() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let account = f.add();
    let bin_dir = f.path.join("rex bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let rex = bin_dir.join("rex");
    let inspect = r#"import json,os,sys
if sys.argv[1]=='session':
    print(json.dumps({'windows':[{'window_id':'w0','layers':[{'layout':{'block_id':'block:usage'},'blocks':[{'block_id':'block:usage'}]}]}]}))
    sys.exit(0)
"#;
    fs::write(&rex, format!("#!/usr/bin/env python3\n{inspect}\nopen(os.environ['CAPTURE_FILE'],'a').write(json.dumps(sys.argv[1:])+'\\n')\n")).unwrap();
    fs::set_permissions(&rex, fs::Permissions::from_mode(0o700)).unwrap();
    let capture = f.path.join("calls.jsonl");
    for (layout, focus, split) in [("tab", "new", Some(false)), ("split", "keep", Some(true)), ("auto", "keep", None), ("auto", "", None)] {
        let config = f.path.join(format!("{layout}-{focus}.toml"));
        let focus_config = if focus.is_empty() { String::new() } else { format!("focus = \"{focus}\"\n") };
        fs::write(&config, format!("[sessions]\nlayout = \"{layout}\"\n{focus_config}")).unwrap();
        fs::write(&capture, "").unwrap();
        success(f.root_command(&["--config", config.to_str().unwrap(), "codex", "run", &account, "--cwd", f.path.to_str().unwrap(), "--split", "--binary", "/tmp/codex"])
            .env("REX_SESSION", "session:fixture")
            .env("REX_BLOCK", "block:usage")
            .env("REX_BIN_DIR", &bin_dir)
            .env("CAPTURE_FILE", &capture)
            .output().unwrap());
        let text = fs::read_to_string(&capture).unwrap();
        let mut launches = text.lines()
            .filter_map(|line| serde_json::from_str::<Vec<String>>(line).ok())
            .filter(|args| args.first().map(String::as_str) == Some("run"));
        assert_eq!(launches.clone().count(), 1, "{layout}/{focus}");
        let args: Vec<String> = launches.next().unwrap();
        if let Some(split) = split {
            assert_eq!(args.iter().any(|a| a == "--split"), split, "{layout}/{focus}: {args:?}");
        }
        let expected = if focus != "keep" { "--focus=true" } else { "--focus=false" };
        assert!(args.iter().any(|a| a == expected), "{layout}/{focus}: {args:?}");
    }
}

/// Exercise the real Usage launcher inside separate PTYs. The agent fixture only
/// reports when attached to a TTY, like Codex's native OSC 7501 implementation.
#[cfg(unix)]
#[test]
fn rex_tabs_preserve_native_program_status_without_bark_hooks() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let account = f.add();
    let bin_dir = f.path.join("rex bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let rex = bin_dir.join("rex");
    fs::write(&rex, r#"#!/usr/bin/env python3
import errno,json,os,pty,select,subprocess,sys,time,uuid
args=sys.argv[1:]
if args[0]=='session':
    print(json.dumps({'windows':[{'window_id':'w0','layers':[{'layout':{'block_id':'block:usage'},'blocks':[{'block_id':'block:usage'}]}]}]}))
    sys.exit(0)
master,slave=pty.openpty()
env=dict(os.environ,REX_BLOCK='block:'+str(uuid.uuid4()))
proc=subprocess.Popen(args[args.index('--')+1:],stdin=slave,stdout=slave,stderr=slave,env=env)
os.close(slave)
data=b''
try:
    until=time.monotonic()+15
    while True:
        if time.monotonic()>until: raise RuntimeError('tab launch timed out')
        if not select.select([master],[],[],.1)[0]: continue
        try: chunk=os.read(master,65536)
        except OSError as e:
            if e.errno==errno.EIO: break
            raise
        if not chunk: break
        data+=chunk
    code=proc.wait(timeout=2)
    with open(os.environ['CAPTURE_FILE'],'a') as f:
        f.write(json.dumps({'block':env['REX_BLOCK'],'output':data.decode(),'code':code})+'\n')
    sys.exit(code)
finally:
    if proc.poll() is None: proc.kill();proc.wait()
    os.close(master)
"#).unwrap();
    fs::set_permissions(&rex, fs::Permissions::from_mode(0o700)).unwrap();
    let binary = bark_binary(&f);
    let script = fs::read_to_string(&binary).unwrap();
    let start = script.find("with open(os.environ['CAPTURE_FILE']").unwrap();
    fs::write(&binary, format!("{}{}", &script[..start], r#"
assert all(os.isatty(fd) for fd in [0,1,2])
assert 'USAGE_CODEX_BARK_CONTEXT' not in os.environ
assert 'USAGE_CODEX_RUN_CONTEXT' not in os.environ
assert not any(arg.startswith('hooks.') or arg.startswith('notify=') for arg in args)
print(json.dumps({'block':os.environ['REX_BLOCK'],'session':os.environ['REX_SESSION']}),flush=True)
for state in ['idle','working','blocked:kind=permission','working','done','error','clear']:
    os.write(1,('\x1b]7501;state='+state+':app=codex\x1b\\').encode())
"#)).unwrap();
    let capture = f.path.join("status-tabs.jsonl");
    let session = uuid::Uuid::new_v4().to_string();
    for resume in [false, true] {
        let mut command = f.command(&["run", &account, "--cwd", f.path.to_str().unwrap(), "--split", "--binary", binary.to_str().unwrap()]);
        if resume {command.args(["--resume", &session]);}
        success(command.env("REX_SESSION", "session:fixture")
            .env("REX_BLOCK", "block:usage")
            .env("REX_BIN_DIR", &bin_dir)
            .env("CAPTURE_FILE", &capture).output().unwrap());
    }
    let records: Vec<serde_json::Value> = fs::read_to_string(&capture).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(records.len(), 2);
    assert_ne!(records[0]["block"], records[1]["block"]);
    for record in records {
        assert_eq!(record["code"], 0);
        assert_ne!(record["block"], "block:usage");
        let output = record["output"].as_str().unwrap();
        assert!(output.contains(record["block"].as_str().unwrap()));
        for state in ["idle", "working", "blocked:kind=permission", "done", "error", "clear"] {
            assert!(output.contains(&format!("\x1b]7501;state={state}:app=codex\x1b\\")), "{output:?}");
        }
    }
    assert!(!f.path.join("manager/bark.json").exists());
}

#[cfg(unix)]
#[test]
fn herdr_tabs_launch_and_resume_managed_agents_in_distinct_ptys() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let account = f.add();
    let project = f.path.join("project ' with spaces");
    fs::create_dir_all(&project).unwrap();
    let app_home = f.path.join("herdr-app-home");
    let app_dir = app_home.join("bin");
    fs::create_dir_all(&app_dir).unwrap();
    let herdr = app_dir.join("herdr-app");
    fs::write(
        &herdr,
        format!(
            r#"#!/usr/bin/env python3
import errno,json,os,pty,select,subprocess,sys,time,uuid
args=sys.argv[1:]
if args[0]=='ls':
    print(json.dumps({{'spaces':[{{'id':'sp-fixture','tabs':[{{'id':'tb-parent','terminals':[{{'terminal':'t-parent','pane':'p-parent'}}]}}]}}]}}))
    sys.exit(0)
if args[0] != 'do' or args[1] not in ('pane.split', 'tab.select', 'pane.focus'):
    print('unexpected command: %r' % (args,), file=sys.stderr)
    sys.exit(9)
if args[1] != 'pane.split':
    print(json.dumps({{'ok':True}}))
    sys.exit(0)
payload=json.loads(args[2])
assert payload['direction']=='right'
argv=payload['argv']
assert argv[:3]==['sh','-c',{wrapper:?}]
cwd=argv[3]
pane='p'+uuid.uuid4().hex
terminal='t'+uuid.uuid4().hex
print(json.dumps({{'pane':pane,'terminal':terminal}}))
sys.stdout.flush()
master,slave=pty.openpty()
env=dict(os.environ,HC_TERMINAL_ID=terminal)
proc=subprocess.Popen(argv,env=env,stdin=slave,stdout=slave,stderr=slave)
os.close(slave)
data=b''
try:
    until=time.monotonic()+20
    while True:
        if time.monotonic()>until: raise RuntimeError('pane timed out')
        if not select.select([master],[],[],.1)[0]: continue
        try: chunk=os.read(master,65536)
        except OSError as e:
            if e.errno==errno.EIO: break
            raise
        if not chunk: break
        data+=chunk
    code=proc.wait(timeout=2)
    if code: print(data.decode(),file=sys.stderr)
    sys.exit(code)
finally:
    if proc.poll() is None: proc.kill();proc.wait()
    os.close(master)
"#,
            wrapper = usage::terminals::AGENT_WRAPPER
        ),
    )
    .unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o700)).unwrap();
    let binary = f.path.join("mock codex tty");
    fs::write(&binary, r#"#!/usr/bin/env python3
import json,os,sys
if '--help' in sys.argv: print('--no-daemon');sys.exit(0)
assert all(os.isatty(fd) for fd in [0,1,2])
assert os.environ['HC_TERMINAL_ID']!='t-parent'
with open(os.environ['CAPTURE_FILE'],'a') as f:
    f.write(json.dumps({'pane':os.environ['HC_TERMINAL_ID'],'cwd':os.getcwd(),'args':sys.argv[1:]})+'\n')
"#).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let capture = f.path.join("agents.jsonl");
    let config = f.path.join("layout.toml");
    fs::write(&config, "[sessions]\nlayout = \"split\"\n").unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    for resume in [false, true] {
        let mut command = f.root_command(&["--config", config.to_str().unwrap(), "codex", "run", &account, "--cwd", project.to_str().unwrap(), "--split", "--binary", binary.to_str().unwrap()]);
        if resume { command.args(["--resume", &session]); }
        success(command.env("HC_TERMINAL_ID", "t-parent")
            .env("HC_TERMINAL_SOCKET", "/tmp/herdr named.sock")
            .env("HERDR_APP_HOME", app_home.to_str().unwrap())
            .env("PATH", format!("{}:{}", app_dir.display(), std::env::var("PATH").unwrap_or_default()))
            .env("REX_SESSION", "outer-rex").env("TERM_PROGRAM", "ghostty")
            .env("CAPTURE_FILE", &capture)
            .output().unwrap());
    }
    let records: Vec<serde_json::Value> = fs::read_to_string(capture).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(records.len(), 2);
    assert_ne!(records[0]["pane"], records[1]["pane"]);
    for record in &records { assert_eq!(record["cwd"], project.to_str().unwrap()); }
    let args: Vec<_> = records[1]["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
    assert!(args.windows(2).any(|pair| pair == ["resume", session.as_str()]));
}

#[test]
fn invalid_layout_config_does_not_silently_launch_with_defaults() {
    let f = Fixture::new();
    let config = f.path.join("invalid-layout.toml");
    fs::write(&config, "[sessions]\nlayout = \"grid\"\n").unwrap();
    let output = f.root_command(&["--config", config.to_str().unwrap(), "codex", "run", "missing", "--cwd", f.path.to_str().unwrap(), "--split"])
        .env("REX_SESSION", "session:fixture").env("REX_BLOCK", "block:usage")
        .env("REX_BIN_DIR", "/nonexistent").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("grid"));
    assert!(!f.path.join("manager").exists());
}
