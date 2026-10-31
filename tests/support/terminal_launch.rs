#[cfg(unix)]
#[test]
fn split_launch_in_rex_creates_a_tab_for_each_managed_instance() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let account = f.add();
    let bin_dir = f.path.join("rex bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let rex = bin_dir.join("rex");
    fs::write(&rex, r#"#!/usr/bin/env python3
import json,os,sys
with open(os.environ['CAPTURE_FILE'], 'a') as f:
    f.write(json.dumps(sys.argv[1:])+'\n')
"#).unwrap();
    fs::set_permissions(&rex, fs::Permissions::from_mode(0o700)).unwrap();
    let capture = f.path.join("tabs.jsonl");
    for _ in 0..2 {
        success(f.command(&["run", &account, "--cwd", f.path.to_str().unwrap(), "--split", "--binary", "/tmp/codex with spaces", "--", "--model", "fixture-model"])
            .env("REX_SESSION", "session:fixture")
            .env("REX_BIN_DIR", &bin_dir)
            .env("GHOSTTY_RESOURCES_DIR", "/inherited/ghostty")
            .env("CAPTURE_FILE", &capture)
            .output().unwrap());
    }
    let lines = fs::read_to_string(capture).unwrap();
    assert_eq!(lines.lines().count(), 2);
    for line in lines.lines() {
        let args: Vec<String> = serde_json::from_str(line).unwrap();
        assert_eq!(&args[..3], ["run", "--session", "session:fixture"]);
        assert_eq!(&args[3..5], ["--cwd", f.path.to_str().unwrap()]);
        assert_eq!(&args[5..10], ["--focus=false", "--keep-open", "--", "sh", "-lc"]);
        let shell = shlex::split(&args[10]).unwrap();
        assert!(shell.windows(2).any(|pair| pair == ["run", account.as_str()]));
        assert!(shell.windows(2).any(|pair| pair == ["--binary", "/tmp/codex with spaces"]));
        assert!(shell.windows(2).any(|pair| pair == ["--model", "fixture-model"]));
        assert!(shell.iter().any(|arg| arg.starts_with("USAGE_CODEX_DIR=")));
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
            .env("REX_BLOCK", "block:usage-parent")
            .env("REX_BIN_DIR", &bin_dir)
            .env("CAPTURE_FILE", &capture).output().unwrap());
    }
    let records: Vec<serde_json::Value> = fs::read_to_string(&capture).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(records.len(), 2);
    assert_ne!(records[0]["block"], records[1]["block"]);
    for record in records {
        assert_eq!(record["code"], 0);
        assert_ne!(record["block"], "block:usage-parent");
        let output = record["output"].as_str().unwrap();
        assert!(output.contains(record["block"].as_str().unwrap()));
        for state in ["idle", "working", "blocked:kind=permission", "done", "error", "clear"] {
            assert!(output.contains(&format!("\x1b]7501;state={state}:app=codex\x1b\\")), "{output:?}");
        }
    }
    assert!(!f.path.join("manager/bark.json").exists());
}
