use super::codex::{command, summarize};
use super::store::{prune, read_report, save};
use super::*;
fn run(
    manager: &mut Manager,
    selector: &str,
    due: bool,
    fpd: Option<&Path>,
) -> Result<Option<Report>> {
    FingerprintService::new(manager.root().to_owned()).run(
        selector,
        due,
        fpd,
        &std::sync::atomic::AtomicBool::new(false),
    )
}
use serde_json::json;
#[test]
fn missing_requested_model_is_never_reported_as_identity_failure() {
    let mut r = Report {
        http_status: None,
        quota_resets_at: None,
        model: MODEL.into(),
        channel: "codex-subscription".into(),
        started_at: 0,
        finished_at: None,
        status: ReportStatus::Running,
        directory: PathBuf::new(),
        accepted: 0,
        reference_contains_model: false,
        closest_candidate: None,
        message: String::new(),
    };
    summarize(&mut r, &json!({"schema":"fpd-detection-v1","analysis":{"used_outputs":3,"results":[{"model":"gpt-6-sol"}]}})).unwrap();
    assert_eq!(r.status, ReportStatus::Completed);
    assert!(!r.reference_contains_model);
    assert!(r.message.contains("不能据此判断换模"));
    assert_eq!(r.closest_candidate.as_deref(), Some("gpt-6-sol"));
    summarize(&mut r, &json!({"schema":"fpd-detection-v1","analysis":{"used_outputs":1,"results":[{"model":MODEL}]}})).unwrap();
    assert_eq!(r.status, ReportStatus::Partial);
    assert!(r.reference_contains_model);
    assert!(summarize(&mut r, &json!({"schema":"other"})).is_err());
}
#[test]
fn fingerprint_command_does_not_forward_api_key_endpoint_or_managed_hooks() {
    let c = command(Path::new("/tmp/fpd"), Path::new("/tmp/official-home"));
    let env = c.get_envs().collect::<std::collections::HashMap<_, _>>();
    for name in [
        "API_KEY",
        "BASE_URL",
        "MODEL",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "USAGE_CODEX_RUN_CONTEXT",
        "NODE_OPTIONS",
    ] {
        assert_eq!(env.get(std::ffi::OsStr::new(name)), Some(&None), "{name}");
    }
    assert_eq!(
        env[std::ffi::OsStr::new("CODEX_HOME")],
        Some(std::ffi::OsStr::new("/tmp/official-home"))
    );
}

#[cfg(unix)]
#[test]
fn official_sample_and_offline_score_obey_hourly_failure_and_overlap_guards() {
    use crate::codex_manager::tests::{Fixture, oauth_source};
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let mut manager = f.manager();
    let source = oauth_source(&f, "fingerprint-workspace", "synthetic-refresh");
    let p = manager.import_oauth(None, None, &source, None).unwrap();
    let fake = f.path.join("fpd");
    fs::write(&fake, r#"#!/usr/bin/env python3
import json,os,sys,pathlib
args=sys.argv[1:]
assert not os.environ.get('API_KEY')
assert not os.environ.get('BASE_URL')
assert 'homes' in os.environ['CODEX_HOME']
if args[0]=='sample':
    assert args[args.index('--model')+1]=='gpt-6.1-sol'
    assert args[args.index('--channel')+1]=='codex-subscription'
    assert args[args.index('--count')+1]=='3'
    assert args[args.index('--max-attempts')+1]=='1'
    assert '--baseurl' not in args and '--apikey' not in args
    out=pathlib.Path(args[args.index('--output-dir')+1]);out.mkdir()
    (out/'result.json').write_text('{}')
    print('{}')
else:
    assert '--input' in args and '--json' in args
    print(json.dumps({'schema':'fpd-detection-v1','analysis':{'used_outputs':3,'results':[{'model':'gpt-6-sol'}]}}))
"#).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let report = run(&mut manager, &p.id, true, Some(&fake))
        .unwrap()
        .unwrap();
    assert_eq!(report.status, ReportStatus::Completed);
    assert!(!report.reference_contains_model);
    assert!(report.directory.join("detection.json").is_file());
    assert!(
        run(&mut manager, &p.id, true, Some(&fake))
            .unwrap()
            .is_none()
    );
    let base = directory(&manager, &p.id);
    let mut aged = report.clone();
    aged.started_at -= INTERVAL;
    save(&base.join("latest.json"), &aged).unwrap();
    let next = run(&mut manager, &p.id, true, Some(&fake))
        .unwrap()
        .unwrap();
    assert_eq!(next.status, ReportStatus::Completed);
    assert_ne!(next.directory, report.directory);

    let lock = fs::File::open(base.join("run.lock")).unwrap();
    lock.lock_exclusive().unwrap();
    assert!(
        run(&mut manager, &p.id, false, Some(&fake))
            .unwrap()
            .is_none()
    );
    drop(lock);
    fs::write(&fake, "#!/bin/sh\nexit 1\n").unwrap();
    let report = run(&mut manager, &p.id, false, Some(&fake))
        .unwrap()
        .unwrap();
    assert_eq!(report.status, ReportStatus::Failed);
    assert!(
        run(&mut manager, &p.id, true, Some(&fake))
            .unwrap()
            .is_none()
    );
    let byok = manager
        .add(
            "byok",
            &super::super::tests::native("https://example.test/v1", MODEL),
            "synthetic-key",
        )
        .unwrap();
    assert!(run(&mut manager, &byok.id, false, Some(&fake)).is_err());
}

#[test]
fn retention_only_removes_old_owned_runs_and_preserves_latest_and_foreign_entries() {
    let root = std::env::temp_dir().join(format!(
        "usage-fingerprint-retention-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(root.join("user-notes")).unwrap();
    let keep = root.join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&keep).unwrap();
    for _ in 0..RETAIN + 3 {
        fs::create_dir(root.join(uuid::Uuid::new_v4().to_string())).unwrap();
    }
    prune(&root, &keep).unwrap();
    assert!(keep.is_dir());
    assert!(root.join("user-notes").is_dir());
    assert_eq!(fs::read_dir(&root).unwrap().count(), RETAIN + 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn quota_errors_expose_safe_status_and_reset_time_without_echoing_upstream_prose() {
    let root =
        std::env::temp_dir().join(format!("usage-fingerprint-quota-{}", uuid::Uuid::new_v4()));
    let trace = root.join("samples/trace/query-01/0001");
    fs::create_dir_all(&trace).unwrap();
    fs::write(trace.join("attempt-1.response.json"), r#"{"status":429}"#).unwrap();
    fs::write(trace.join("attempt-1.body.txt"), r#"{"error":{"type":"usage_limit_reached","resets_at":1792314948,"message":"private-echo-do-not-log"}}"#).unwrap();
    let path = root.join("report.json");
    // Older state files without the new optional fields remain readable.
    fs::write(
        &path,
        json!({"model":MODEL,"channel":"codex-subscription","started_at":0,"finished_at":1,
        "status":"failed","directory":root,"accepted":0,"reference_contains_model":false,
        "closest_candidate":null,"message":"generic failure"})
        .to_string(),
    )
    .unwrap();
    let report = read_report(&path).unwrap().unwrap();
    assert_eq!(report.http_status, Some(429));
    assert_eq!(report.quota_resets_at, Some(1792314948));
    assert!(report.message.contains("额度已用尽"));
    assert!(!report.message.contains("private-echo"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn history_cache_reloads_after_new_report_and_preserves_legacy_status_encoding() {
    use crate::codex_manager::tests::{Fixture, oauth_source};
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    let mut manager = f.manager();
    let source = oauth_source(&f, "cache-workspace", "synthetic-refresh");
    let profile = manager
        .import_oauth(Some("cache"), None, &source, None)
        .unwrap();
    let service = FingerprintService::new(manager.root().to_owned());
    let base = service.store.account_dir(&profile.id);
    let run = base.join("runs").join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&run).unwrap();
    let report = json!({"model":MODEL,"channel":"codex-subscription","started_at":1,"finished_at":2,
        "status":"completed","directory":run,"accepted":3,"reference_contains_model":false,
        "closest_candidate":"gpt-6-sol","message":"reference only"});
    fs::write(run.join("report.json"), report.to_string()).unwrap();
    fs::write(base.join("latest.json"), report.to_string()).unwrap();
    assert_eq!(service.snapshots().unwrap()[0].history.len(), 1);
    let reads = service.store.history_loads.load(Ordering::Relaxed);
    for _ in 0..3 {
        assert_eq!(service.snapshots().unwrap()[0].history.len(), 1);
    }
    assert_eq!(
        service.store.history_loads.load(Ordering::Relaxed),
        reads,
        "unchanged refreshes must not decode history again"
    );
    let mut changed: Report = serde_json::from_value(report).unwrap();
    changed.status = ReportStatus::Interrupted;
    changed.message = "application closed".into();
    save(&base.join("latest.json"), &changed).unwrap();
    assert_eq!(
        service.snapshots().unwrap()[0]
            .latest
            .as_ref()
            .unwrap()
            .status,
        ReportStatus::Interrupted
    );
    assert_eq!(
        service.store.history_loads.load(Ordering::Relaxed),
        reads + 1
    );
    assert_eq!(
        serde_json::to_value(&changed).unwrap()["status"],
        "interrupted"
    );
    let mut future = serde_json::to_value(changed).unwrap();
    future["status"] = json!("a_future_state");
    assert_eq!(
        serde_json::from_value::<Report>(future).unwrap().status,
        ReportStatus::Unknown
    );
}
