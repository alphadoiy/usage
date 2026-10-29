use super::*;
use crate::codex_manager::tests::Fixture;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;

fn command(fixture: &Fixture, script: &str) -> Command {
    let binary = fixture.path.join("probe");
    fs::write(&binary, format!("#!/usr/bin/env python3\n{script}")).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = Command::new(binary);
    command
        .current_dir(&fixture.path)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

#[test]
fn explicit_model_flags_bypass_probe_and_prompts_do_not() {
    let fixture = Fixture::new();
    for args in [
        vec!["--profile", "work"],
        vec!["--profile=work"],
        vec!["-pwork"],
        vec!["--model", "work"],
        vec!["--model=work"],
        vec!["-mwork"],
        vec!["--oss"],
        vec!["--local-provider=ollama", "--oss"],
        vec!["-c", "model='work'"],
        vec!["--config=model_provider='work'"],
        vec!["-cmodel_reasoning_effort='high'"],
        vec!["-c", "\"model\"='work'"],
    ] {
        let args = args.into_iter().map(String::from).collect::<Vec<_>>();
        assert!(
            probe_args(&fixture.path, &args).unwrap().is_none(),
            "{args:?}"
        );
    }
    let args = ["--", "--profile=prompt", "-c", "model=prompt"].map(String::from);
    assert_eq!(
        probe_args(&fixture.path, &args).unwrap(),
        Some((vec![], fixture.path.clone()))
    );
    assert!(
        probe_args(&fixture.path, &["--local-provider=ollama".into()])
            .unwrap()
            .is_some()
    );
}

#[test]
fn config_overrides_and_final_cd_reach_the_native_resolver() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path.join("project")).unwrap();
    for cd in [
        vec!["--cd", "project"],
        vec!["--cd=project"],
        vec!["-Cproject"],
    ] {
        let args = [
            "-c",
            "projects.work.trust_level='trusted'",
            "--config=features.foo=true",
        ]
        .into_iter()
        .chain(cd)
        .map(String::from)
        .collect::<Vec<_>>();
        assert_eq!(
            probe_args(&fixture.path, &args).unwrap(),
            Some((
                [
                    "-c",
                    "projects.work.trust_level='trusted'",
                    "-c",
                    "features.foo=true"
                ]
                .map(String::from)
                .to_vec(),
                fixture.path.join("project"),
            ))
        );
    }
}

#[tokio::test]
async fn handshake_finishes_before_read_and_fragmented_eof_response_is_accepted() {
    let fixture = Fixture::new();
    let command = command(
        &fixture,
        r#"import json,os,sys
initialize=json.loads(sys.stdin.readline())
assert initialize['method']=='initialize'
os.write(1,b'{"id":1,"result":{}}\n')
assert json.loads(sys.stdin.readline())['method']=='initialized'
request=json.loads(sys.stdin.readline())
assert request['method']=='config/read' and request['params']['cwd']==os.getcwd()
assert request['params']['includeLayers'] is False
os.write(1,b'{"method":"notice","params":{}}\n{"id":2,')
os.write(1,b'"result":{"config":{"model_provider":"project-choice","api_key":"private-fixture"}}}')
"#,
    );
    let provider = read_provider(command, fixture.path.clone(), Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(provider, "project-choice");
}

#[tokio::test]
async fn rpc_errors_do_not_expose_config_or_server_error_text() {
    let fixture = Fixture::new();
    let command = command(
        &fixture,
        r#"import json,sys
request=json.loads(sys.stdin.readline())
print(json.dumps({'id':request['id'],'error':{'message':'private-fixture-key'}}),flush=True)
"#,
    );
    let error = read_provider(command, fixture.path.clone(), Duration::from_secs(2))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Codex 拒绝配置读取请求"));
    assert!(!error.contains("private-fixture-key"));
}

#[tokio::test]
async fn stalled_probe_and_descendant_are_terminated_within_deadline() {
    let fixture = Fixture::new();
    let command = command(
        &fixture,
        r#"import os,pathlib,time
child=os.fork()
if child==0:
    time.sleep(2)
    pathlib.Path('descendant-survived').write_text('unexpected')
    os._exit(0)
pathlib.Path('probe-pid').write_text(str(os.getpid()))
time.sleep(30)
"#,
    );
    let started = std::time::Instant::now();
    let error = read_provider(command, fixture.path.clone(), Duration::from_millis(150))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("配置读取超时"));
    assert!(started.elapsed() < Duration::from_secs(2));
    let pid: i32 = fs::read_to_string(fixture.path.join("probe-pid"))
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only checks this synthetic child's existence.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!fixture.path.join("descendant-survived").exists());
}

#[tokio::test]
async fn successful_probe_reaps_wrapper_and_terminates_its_pipe_holding_descendant() {
    let fixture = Fixture::new();
    let command = command(
        &fixture,
        r#"import json,os,pathlib,sys,time
request=json.loads(sys.stdin.readline())
print(json.dumps({'id':request['id'],'result':{}}),flush=True)
assert json.loads(sys.stdin.readline())['method']=='initialized'
request=json.loads(sys.stdin.readline())
assert request['method']=='config/read'
if os.fork()==0:
    time.sleep(1)
    pathlib.Path('descendant-survived').write_text('unexpected')
    os._exit(0)
print(json.dumps({'id':request['id'],'result':{'config':{'model_provider':'selected'}}}),flush=True)
os._exit(0)
"#,
    );
    assert_eq!(
        read_provider(command, fixture.path.clone(), Duration::from_secs(2))
            .await
            .unwrap(),
        "selected"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!fixture.path.join("descendant-survived").exists());
}

#[tokio::test]
async fn oversized_protocol_output_is_bounded() {
    let fixture = Fixture::new();
    let command = command(
        &fixture,
        "import sys,time\nsys.stdout.write('x'*(5*1024*1024)); sys.stdout.flush()\ntime.sleep(30)\n",
    );
    let started = std::time::Instant::now();
    let error = read_provider(command, fixture.path.clone(), Duration::from_secs(2))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("配置读取协议不受支持"));
    assert!(error.len() < 400);
    assert!(started.elapsed() < Duration::from_secs(2));
}
