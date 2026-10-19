use super::*;
use crate::codex_manager::tests::{Fixture, oauth_source};
use std::os::unix::fs::PermissionsExt;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_app_cancels_and_reaps_active_sampler_and_preserves_reports() {
    let f = Fixture::new();
    let mut manager = f.manager();
    let source = oauth_source(&f, "schedule-fixture", "synthetic-refresh");
    let p = manager.import_oauth(None, None, &source, None).unwrap();
    let fake = super::super::tests::test_fpd(&f.path);
    fs::write(&fake, "#!/usr/bin/env python3\nimport os,pathlib,time\npathlib.Path(__file__).with_suffix('.pid').write_text(str(os.getpid()))\nwhile True: time.sleep(0.01)\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let service = Arc::new(FingerprintService::new(manager.root().to_owned()));
    service.enable(&p.id, Some(&fake)).unwrap();
    let settings = directory(&manager, &p.id).join("schedule.json");
    assert!(settings.is_file());
    let app = Runtime::with_interval(
        &Handle::current(),
        service.clone(),
        Duration::from_millis(20),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !fake.with_extension("pid").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid: i32 = fs::read_to_string(fake.with_extension("pid"))
        .unwrap()
        .parse()
        .unwrap();
    drop(app);
    let latest = directory(&manager, &p.id).join("latest.json");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store::read_report(&latest)
                .unwrap()
                .is_some_and(|r| r.status == ReportStatus::Interrupted)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // SAFETY: signal 0 only checks existence of this synthetic child's PID.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    let before = fs::read_dir(directory(&manager, &p.id).join("runs"))
        .unwrap()
        .count();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        fs::read_dir(directory(&manager, &p.id).join("runs"))
            .unwrap()
            .count(),
        before
    );
    let views = service.snapshots().unwrap();
    assert_eq!(views[0].history.len(), 1);
    assert_eq!(
        views[0].latest.as_ref().unwrap().status,
        ReportStatus::Interrupted
    );
    service.disable(&p.id).unwrap();
    assert!(!settings.exists());
    assert!(latest.is_file());
    assert!(!service.snapshots().unwrap()[0].enabled);
}

#[test]
fn checking_unconfigured_app_never_initializes_store() {
    let f = Fixture::new();
    let root = f.path.join("missing");
    let service = FingerprintService::new(root.clone());
    assert!(service.jobs().unwrap().is_empty());
    assert!(service.snapshots().unwrap().is_empty());
    assert!(!root.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_account_and_hung_sampler_do_not_block_healthy_accounts() {
    use serde_json::json;
    let f = Fixture::new();
    let mut manager = f.manager();
    let mut profiles = Vec::new();
    for name in ["a-broken", "b-slow", "c-healthy"] {
        let source = oauth_source(&f, name, "synthetic-refresh");
        profiles.push(
            manager
                .import_oauth(Some(name), None, &source, None)
                .unwrap(),
        );
    }
    let fake = super::super::tests::test_fpd(&f.path);
    let script = r#"#!/usr/bin/env python3
import json,os,pathlib,sys,time
args=sys.argv[1:]
if 'SLOW_ACCOUNT' in os.environ['CODEX_HOME']:
    pathlib.Path(__file__).with_suffix('.pid').write_text(str(os.getpid()))
    while True: time.sleep(0.01)
if args[0]=='sample':
    p=pathlib.Path(args[args.index('--output-dir')+1]);p.mkdir()
    (p/'result.json').write_text((pathlib.Path(__file__).parents[2]/'collection-fixture.json').read_text())
    print('{}')
else:
    print(json.dumps({'schema':'fpd-detection-v1','analysis':{'used_outputs':3,'results':[{'model':'gpt-6-sol'}]}}))
"#.replace("SLOW_ACCOUNT", &profiles[1].id);
    fs::write(&fake, script).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let service = Arc::new(FingerprintService::new(manager.root().to_owned()));
    for p in &profiles {
        service.enable(&p.id, Some(&fake)).unwrap();
    }
    let broken = service.store.account_dir(&profiles[0].id);
    fs::write(broken.join("schedule.json"), b"broken settings").unwrap();
    fs::write(broken.join("latest.json"), b"broken latest report").unwrap();
    let app = Runtime::with_interval(
        &Handle::current(),
        service.clone(),
        Duration::from_millis(30),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let latest = service.store.latest(&profiles[2].id).unwrap();
            if latest.is_some_and(|r| r.status == ReportStatus::Completed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid: i32 = fs::read_to_string(fake.with_extension("pid"))
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signal 0 checks the synthetic slow process is still running.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "healthy account completed while slow account was active"
    );
    let healthy = service.store.account_dir(&profiles[2].id);
    let bad_history = healthy.join("runs").join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&bad_history).unwrap();
    fs::write(bad_history.join("report.json"), b"damaged history").unwrap();
    let snapshots = service.snapshots().unwrap();
    let broken_view = snapshots.iter().find(|s| s.id == profiles[0].id).unwrap();
    let healthy_view = snapshots.iter().find(|s| s.id == profiles[2].id).unwrap();
    assert!(!broken_view.warnings.is_empty());
    assert_eq!(
        healthy_view.latest.as_ref().unwrap().status,
        ReportStatus::Completed
    );
    assert_eq!(healthy_view.history.len(), 1);
    assert!(healthy_view.warnings.iter().any(|s| s.contains("历史")));
    assert!(json!(healthy_view.latest.as_ref().unwrap())["status"] == "completed");
    drop(app);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !service
            .store
            .latest(&profiles[1].id)
            .unwrap()
            .is_some_and(|r| r.status == ReportStatus::Interrupted)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
