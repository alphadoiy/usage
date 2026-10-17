#[cfg(unix)]
fn local_bark_binary(f: &Fixture) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let binary = f.path.join("codex");
    fs::write(&binary, r#"#!/usr/bin/env python3
import json,os,pathlib,sqlite3,subprocess,sys,tomllib
if sys.argv[1:] == ['--help']:
    print('--no-daemon'); sys.exit(0)
args=sys.argv[1:]
home=pathlib.Path(os.environ['CODEX_HOME'])
with open(os.environ['CAPTURE_FILE'],'w') as file:
    json.dump({'args':args,'context':os.environ.get('USAGE_CODEX_BARK_CONTEXT'),
        'runtime':os.environ.get('USAGE_CODEX_RUN_CONTEXT'),'home':str(home),
        'auth':os.environ.get('OPENAI_API_KEY'),'cwd':os.getcwd()},file)
if os.environ.get('FIXTURE_CALLBACK') == '1':
    session=args[args.index('resume')+1]
    conn=sqlite3.connect(home/'state_5.sqlite')
    conn.execute('CREATE TABLE IF NOT EXISTS threads(id TEXT PRIMARY KEY,source TEXT,thread_source TEXT)')
    conn.execute("INSERT OR REPLACE INTO threads VALUES (?,'cli','user')",(session,))
    conn.commit();conn.close()
    notify=next(tomllib.loads(arg)['notify'] for arg in args if arg.startswith('notify='))
    payload=json.dumps({'type':'agent-turn-complete','thread-id':session,'turn-id':'fixture-turn',
        'cwd':os.getcwd(),'last-assistant-message':'Local resume finished'})
    subprocess.run(notify+[payload],check=True,timeout=10)
sys.exit(int(os.environ.get('FIXTURE_EXIT','0')))
"#).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    binary
}

#[cfg(unix)]
#[test]
fn local_resume_cli_injects_bark_and_chains_notify_without_importing_home() {
    let f = Fixture::new();
    let home = f.path.join("original home ' $ literal");
    fs::create_dir_all(&home).unwrap();
    let previous = f.path.join("previous notify.sh");
    let count = home.join("notify-count");
    fs::write(&previous, "printf 'called\\n' >> \"$CODEX_HOME/notify-count\"\n").unwrap();
    let config = format!("model = 'original-model'\nnotify = {}\n", toml::Value::try_from(vec!["/bin/sh".to_string(), previous.to_string_lossy().into_owned()]).unwrap());
    fs::write(home.join("config.toml"), &config).unwrap();
    fs::write(home.join("auth.json"), "fixture-original-auth").unwrap();
    let server = BarkServer::new();
    configure_bark(&f, &server);
    let binary = local_bark_binary(&f);
    let session = uuid::Uuid::new_v4().to_string();
    let capture = f.path.join("capture.json");
    let launch = || {
        let mut command = f.command(&["resume-local", "--home",home.to_str().unwrap(),
            "--cwd",f.path.to_str().unwrap(),"--session",&session,"--binary",binary.to_str().unwrap()]);
        command.env("CAPTURE_FILE",&capture)
            .env("OPENAI_API_KEY","fixture-existing-auth-env")
            .env("USAGE_CODEX_BARK_CONTEXT","invalid-inherited-context")
            .env("USAGE_CODEX_RUN_CONTEXT","invalid-parent-managed-run");
        command
    };
    let mut runs = std::collections::HashSet::new();
    for _ in 0..2 {
        success(launch().env("FIXTURE_CALLBACK","1").output().unwrap());
        let record: serde_json::Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
        assert_eq!(record["home"],home.to_str().unwrap());
        assert_eq!(record["cwd"],f.path.to_str().unwrap());
        assert_eq!(record["auth"],"fixture-existing-auth-env");
        assert!(record["runtime"].is_null());
        let args = record["args"].as_array().unwrap();
        assert!(args.iter().any(|arg| arg == "--no-daemon"));
        assert!(args.windows(2).any(|pair| pair == ["resume",session.as_str()]));
        let context: serde_json::Value = serde_json::from_str(record["context"].as_str().unwrap()).unwrap();
        assert_eq!(context["home"],record["home"]);
        assert!(runs.insert(context["run_id"].as_str().unwrap().to_string()));
    }
    assert_eq!(fs::read_to_string(&count).unwrap(),"called\ncalled\n");
    let received = f.events().iter().filter(|event| event["fields"]["event"] == "bark_event_received").count();
    assert_eq!(received,2,"completion callbacks must reach Usage");
    f.run(&["bark","disable"]);
    let output = launch().env("FIXTURE_EXIT","42").output().unwrap();
    assert_eq!(output.status.code(),Some(42));
    let record: serde_json::Value = serde_json::from_slice(&fs::read(&capture).unwrap()).unwrap();
    assert!(record["context"].is_null());
    assert!(record["runtime"].is_null());
    assert_eq!(record["args"],serde_json::json!(["resume",session]));
    assert_eq!(fs::read_to_string(home.join("config.toml")).unwrap(),config);
    assert_eq!(fs::read_to_string(home.join("auth.json")).unwrap(),"fixture-original-auth");
    assert!(!home.join("hooks.json").exists());
    assert!(!f.path.join("manager/manager.sqlite3").exists());
    let logs = f.run(&["bark", "logs", "--tail", "1000"]);
    let launches: Vec<serde_json::Value> = logs.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|event: &serde_json::Value| event["fields"]["event"] == "bark_local_launch")
        .collect();
    assert_eq!(launches.len(), 3);
    assert_eq!(launches[0]["fields"]["session_id"], session);
    assert_eq!(launches[0]["fields"]["bark_injected"], true);
    assert_eq!(launches[2]["fields"]["bark_injected"], false);
    assert!(!logs.contains("fixture-existing-auth-env"));
    assert!(!logs.contains("fixture-bark-device-secret"));
}

#[cfg(unix)]
#[test]
fn local_session_resume_paths_all_use_notification_injection() {
    use usage::agents::{AgentContext, AgentKind, SpawnRequest};
    let f = Fixture::new();
    // Isolate PATH and the manager root from other tests and the user's settings.
    let name = "local_session_resume_paths_all_use_notification_injection";
    if std::env::var("USAGE_LOCAL_BARK_TEST").ok().as_deref() != Some(name) {
        let binary = local_bark_binary(&f);
        assert!(binary.exists());
        let server = BarkServer::new();
        configure_bark(&f,&server);
        success(Command::new(std::env::current_exe().unwrap()).args(["--exact",name,"--nocapture"])
            .env("USAGE_LOCAL_BARK_TEST",name)
            .env("USAGE_CODEX_DIR",f.path.join("manager"))
            .env("PATH",format!("{}:{}",f.path.display(),std::env::var("PATH").unwrap_or_default()))
            .env("CAPTURE_FILE",f.path.join("capture.json"))
            .env("OPENAI_API_KEY","fixture-existing-auth-env")
            .env_remove("FIXTURE_CALLBACK")
            .output().unwrap());
        return;
    }
    let home = f.path.join("home ' with $ punctuation");
    let cwd = f.path.join("project ' with $ punctuation");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    let agent = usage::agents::builtin_registry().create("codex",AgentContext::Local {home:home.clone()}).unwrap();
    let mut session = usage::sessions::Session {
        agent:Some(agent),provider:AgentKind::CODEX,instance:"default".into(),
        id:uuid::Uuid::new_v4().to_string(),cwd:cwd.clone(),home:home.clone(),
        title:"task".into(),recency_at_ms:0,
    };
    let request = SpawnRequest {cwd,program:"codex".into(),args:Vec::new(),resume:Some(session.id.clone()),usage_config:None};
    let mut groups = std::collections::HashSet::new();
    for adapter in [true,false] {
        if !adapter {session.agent=None;}
        if adapter {
            let outcome = session.agent.as_ref().unwrap().launcher().unwrap().start(&request).unwrap().wait().unwrap();
            assert!(outcome.status.success());
            assert!(outcome.run.is_none());
        } else {
            assert!(usage::sessions::resume_in_current(&session).unwrap().success());
        }
        let capture = fs::read(std::env::var_os("CAPTURE_FILE").unwrap()).unwrap();
        let record:serde_json::Value=serde_json::from_slice(&capture).unwrap();
        let context:serde_json::Value=serde_json::from_str(record["context"].as_str().unwrap()).unwrap();
        assert!(groups.insert(context["run_id"].as_str().unwrap().to_string()));
        assert_eq!(record["home"],home.to_str().unwrap());
        assert_eq!(record["auth"],"fixture-existing-auth-env");
        let shell = usage::sessions::resume_shell_command(&session).unwrap();
        let argv = shlex::split(&shell).unwrap();
        assert!(argv.windows(2).any(|pair| pair == ["codex","resume-local"]));
        assert!(argv.windows(2).any(|pair| pair == ["--home",home.to_str().unwrap()]));
        assert!(argv.windows(2).any(|pair| pair == ["--session",session.id.as_str()]));
        assert!(argv.iter().any(|arg| arg.starts_with("USAGE_CODEX_DIR=")));
    }
    assert!(!home.join("config.toml").exists());
    assert!(!usage::codex_manager::default_root().join("manager.sqlite3").exists());
}
