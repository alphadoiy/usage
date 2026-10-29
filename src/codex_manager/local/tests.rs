use super::*;
use crate::agents::RunningAgent;
use crate::codex_manager::tests::Fixture;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn fixture_binary(fixture: &Fixture) -> std::path::PathBuf {
    let binary = fixture.path.join("codex-fixture");
    fs::write(&binary, r#"#!/usr/bin/env python3
import json,os,pathlib,sys
if sys.argv[1:] == ['--help']:
    print('--no-daemon'); sys.exit(0)
if 'app-server' in sys.argv:
    requests = []
    for line in sys.stdin:
        request = json.loads(line)
        requests.append(request)
        if request['method'] == 'initialize':
            print(json.dumps({'id':request['id'],'result':{}}),flush=True)
        elif request['method'] == 'config/read':
            assert [r['method'] for r in requests] == ['initialize','initialized','config/read']
            assert request['params']['cwd'] == str(pathlib.Path.cwd())
            assert os.environ['CODEX_HOME'] == str(pathlib.Path.cwd()/'native')
            provider=json.loads(pathlib.Path('effective-provider.json').read_text())
            print(json.dumps({'id':request['id'],'result':{'config':{'model_provider':provider}}}),flush=True)
            pathlib.Path('probe.json').write_text(json.dumps(requests))
        else:
            assert request['method'] == 'initialized'
    sys.exit(0)
pathlib.Path('capture.json').write_text(json.dumps(sys.argv[1:]))
"#).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    binary
}

#[test]
fn local_resume_uses_native_effective_provider_without_freezing_model_or_effort() {
    let fixture = Fixture::new();
    let home = fixture.path.join("native");
    fs::create_dir(&home).unwrap();
    let config =
        "model_provider='home-provider'\nmodel='home-model'\nmodel_reasoning_effort='low'\n";
    fs::write(home.join("config.toml"), config).unwrap();
    let binary = fixture_binary(&fixture);
    for (native_provider, expected) in [
        (serde_json::json!("project-provider"), "project-provider"),
        (serde_json::Value::Null, "openai"),
    ] {
        fs::write(
            fixture.path.join("effective-provider.json"),
            native_provider.to_string(),
        )
        .unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let request = SpawnRequest {
            cwd: fixture.path.clone(),
            program: binary.clone(),
            args: Vec::new(),
            resume: Some(id.clone()),
            usage_config: None,
        };
        assert!(
            start_local_codex(&home, &request)
                .unwrap()
                .wait()
                .unwrap()
                .status
                .success()
        );
        let args: Vec<String> =
            serde_json::from_slice(&fs::read(fixture.path.join("capture.json")).unwrap()).unwrap();
        assert_eq!(
            args,
            [
                "-c",
                &format!("model_provider=\"{expected}\""),
                "resume",
                &id
            ]
        );
        assert!(fixture.path.join("probe.json").exists());
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            config
        );
    }
}

#[test]
fn explicit_model_choices_launch_unchanged_without_config_probe() {
    let fixture = Fixture::new();
    let home = fixture.path.join("native");
    fs::create_dir(&home).unwrap();
    let binary = fixture_binary(&fixture);
    for args in [
        vec!["--profile", "selected"],
        vec!["--model", "selected"],
        vec!["-c", "model_provider='selected'"],
        vec!["--config", "model_reasoning_effort='high'"],
    ] {
        let id = uuid::Uuid::new_v4().to_string();
        let request = SpawnRequest {
            cwd: fixture.path.clone(),
            program: binary.clone(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            resume: Some(id.clone()),
            usage_config: None,
        };
        assert!(
            start_local_codex(&home, &request)
                .unwrap()
                .wait()
                .unwrap()
                .status
                .success()
        );
        let actual: Vec<String> =
            serde_json::from_slice(&fs::read(fixture.path.join("capture.json")).unwrap()).unwrap();
        let expected: Vec<String> = ["resume", id.as_str()]
            .into_iter()
            .chain(args)
            .map(String::from)
            .collect();
        assert_eq!(actual, expected);
        assert!(!fixture.path.join("probe.json").exists());
    }
}
