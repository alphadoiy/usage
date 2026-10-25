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
#[cfg(unix)]
pub(super) fn test_fpd(root: &Path) -> PathBuf {
    let package = root.join("lmfpd");
    fs::create_dir_all(package.join("bin")).unwrap();
    fs::create_dir_all(package.join("data")).unwrap();
    fs::write(
        package.join("package.json"),
        r#"{"name":"lmfpd","version":"0.0.1"}"#,
    )
    .unwrap();
    fs::write(
        package.join("data/unified_bank.json"),
        json!({"models":[{"id":MODEL}]}).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("collection-fixture.json"),
        sample_collection().to_string(),
    )
    .unwrap();
    package.join("bin/fpd")
}
fn sample_collection() -> Value {
    json!({"schema_version":1,"purpose":"reference","model":{"id":MODEL},
        "samples":(0..3).map(|i|json!({"challenge_id":format!("query-{i}"),"condition":format!("environment-{i}"),
        "expected_count":80,"text":"1, 2, 3","completion":"complete","response_model":MODEL})).collect::<Vec<_>>()})
}
#[cfg(unix)]
#[test]
fn unsupported_or_unreadable_reference_never_starts_sampling_or_overwrites_history() {
    use crate::codex_manager::tests::{Fixture, oauth_source};
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let mut manager = f.manager();
    let source = oauth_source(&f, "reference-preflight", "synthetic-refresh");
    let profile = manager.import_oauth(None, None, &source, None).unwrap();
    let fake = test_fpd(&f.path);
    fs::write(&fake, "#!/bin/sh\ntouch \"$0.started\"\nexit 1\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let bank = f.path.join("lmfpd/data/unified_bank.json");
    fs::write(&bank, r#"{"models":[{"id":"gpt-6-sol"}]}"#).unwrap();
    let service = FingerprintService::new(manager.root().to_owned());
    service.enable(&profile.id, Some(&fake)).unwrap();
    let base = service.store.account_dir(&profile.id);
    let mut old = empty_report();
    old.status = ReportStatus::Partial;
    old.accepted = 3;
    save(&base.join("latest.json"), &old).unwrap();
    let original = fs::read(base.join("latest.json")).unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    assert!(
        service
            .run(&profile.id, false, Some(&fake), &cancel)
            .unwrap_err()
            .to_string()
            .contains("未收录 gpt-6.1-sol")
    );
    assert!(
        service
            .run_scheduled(&profile.id, &cancel)
            .unwrap()
            .is_none()
    );
    let snapshot = service.status(&profile.id).unwrap();
    assert!(
        snapshot
            .blocked_reason
            .unwrap()
            .contains("避免消耗订阅额度")
    );
    assert_eq!(fs::read(base.join("latest.json")).unwrap(), original);
    assert!(!base.join("runs").exists());
    assert!(!fake.with_file_name("fpd.started").exists());

    fs::write(&bank, "invalid bank").unwrap();
    assert!(
        service
            .run(&profile.id, false, Some(&fake), &cancel)
            .unwrap_err()
            .to_string()
            .contains("无法确认本地参考库")
    );
    assert!(!fake.with_file_name("fpd.started").exists());
    // Real upstream group label: the ID differs, but the explicit label covers MODEL.
    fs::write(
        &bank,
        json!({"models":[{"id":"gpt-6-astra","display_name":"gpt-6-astra/6.1-sol"}]}).to_string(),
    )
    .unwrap();
    assert!(
        service
            .status(&profile.id)
            .unwrap()
            .blocked_reason
            .is_none()
    );
    for label in ["gpt-6-astra/6.1-sol-preview", "gpt-6-astra/6.1-sol-pro"] {
        fs::write(
            &bank,
            json!({"models":[{"id":"gpt-6-astra","display_name":label}]}).to_string(),
        )
        .unwrap();
        assert!(
            service
                .status(&profile.id)
                .unwrap()
                .blocked_reason
                .is_some(),
            "unrelated variant {label} must not match by substring"
        );
    }
    fs::write(&bank, json!({"models":[{"id":MODEL}]}).to_string()).unwrap();
    assert!(
        service
            .status(&profile.id)
            .unwrap()
            .blocked_reason
            .is_none(),
        "bank changes must invalidate the cached preflight"
    );
}
fn empty_report() -> Report {
    Report {
        http_status: None,
        quota_resets_at: None,
        model: MODEL.into(),
        channel: "codex-subscription".into(),
        started_at: 0,
        finished_at: None,
        status: ReportStatus::Running,
        directory: PathBuf::new(),
        accepted: 0,
        scoring: None,
        reference_contains_model: false,
        reference_label: None,
        closest_candidate: None,
        confidence: None,
        message: String::new(),
    }
}
fn multiround_detection() -> Value {
    // Sanitized structure of the saved LMFPD 0.0.1792242250521 report:
    // one accepted sample per independent challenge condition, no top-level analysis.
    let rounds = [218, 297, 299]
        .into_iter()
        .enumerate()
        .map(|(i, count)| {
            json!({
                "index":i+1,
                "samples":[{"state":"complete","count":count,"expectedCount":count}],
                "analysis":{"used_outputs":1,"decision":"partial",
                    "probability_status":"unavailable","results":[{"model":"gpt-6-astra","display_name":"gpt-6-astra/6.1-sol"}]}
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schema":"fpd-detection-v1", "cancelled":false,
        "requested_rounds":3, "completed_rounds":3, "scored_rounds":3,
        "rounds":rounds
    })
}
#[test]
fn three_subscription_answers_form_one_strict_detection_round_with_completion_evidence() {
    let collection = sample_collection();
    let input = super::codex::detection_input(&collection).unwrap();
    assert_eq!(input["rounds"].as_array().unwrap().len(), 1);
    let round = &input["rounds"][0];
    for (i, sample) in collection["samples"].as_array().unwrap().iter().enumerate() {
        assert_eq!(round["outputs"][i]["text"], sample["text"]);
        assert_eq!(
            round["outputs"][i]["expected_count"],
            sample["expected_count"]
        );
        assert_eq!(round["samples"][i]["completion"], sample["completion"]);
        assert_eq!(round["samples"][i]["condition"], sample["condition"]);
    }
    for (field, value) in [
        ("completion", json!("truncated")),
        ("completion", json!("unknown")),
        ("response_model", json!("other-model")),
        ("expected_count", json!(0)),
    ] {
        let mut invalid = collection.clone();
        invalid["samples"][0][field] = value;
        assert!(
            super::codex::detection_input(&invalid).is_err(),
            "invalid {field} must not be promoted into a complete detection round"
        );
    }
    let mut duplicate = collection;
    duplicate["samples"][1]["challenge_id"] = duplicate["samples"][0]["challenge_id"].clone();
    assert!(super::codex::detection_input(&duplicate).is_err());
}
#[test]
fn only_calibrated_single_round_results_expose_confidence_and_candidate_labels() {
    let mut report = empty_report();
    let mut detection = json!({"schema":"fpd-detection-v1","analysis":{"used_outputs":3,
        "probability_status":"reference_calibrated", "results":[{
        "model":"gpt-6-astra","display_name":"gpt-6-astra/6.1-sol","probability":0.9983446737190644}]}});
    summarize(&mut report, &detection).unwrap();
    assert_eq!(report.status, ReportStatus::Completed);
    assert_eq!(
        report.closest_candidate.as_deref(),
        Some("gpt-6-astra/6.1-sol")
    );
    assert_eq!(report.confidence, Some(0.9983446737190644));
    detection["analysis"]["probability_status"] = json!("unavailable");
    summarize(&mut report, &detection).unwrap();
    assert_eq!(report.confidence, None);
    detection["analysis"]["probability_status"] = json!("reference_calibrated");
    detection["analysis"]["results"][0]["probability"] = json!(1.5);
    summarize(&mut report, &detection).unwrap();
    assert_eq!(report.confidence, None);
}
#[cfg(unix)]
#[test]
fn repairing_split_rounds_rescores_offline_without_sampling_or_changing_original_evidence() {
    use crate::codex_manager::tests::{Fixture, oauth_source};
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let mut manager = f.manager();
    let source = oauth_source(&f, "offline-score-workspace", "synthetic-refresh");
    let profile = manager.import_oauth(None, None, &source, None).unwrap();
    let fpd = test_fpd(&f.path);
    fs::write(&fpd, r#"#!/usr/bin/env python3
import json,sys,pathlib,os
args=sys.argv[1:]
assert args[0]=='--input' and '--strict' in args
assert os.environ['CODEX_HOME'].endswith('offline-home')
assert not os.environ.get('API_KEY')
data=json.loads(pathlib.Path(args[1]).read_text())
assert len(data['rounds'])==1 and len(data['rounds'][0]['outputs'])==3
print(json.dumps({'schema':'fpd-detection-v1','analysis':{'used_outputs':3,'probability_status':'reference_calibrated','results':[{'model':'gpt-6-astra','display_name':'gpt-6-astra/6.1-sol','probability':0.99}]}}))
"#).unwrap();
    fs::set_permissions(&fpd, fs::Permissions::from_mode(0o700)).unwrap();
    let service = FingerprintService::new(manager.root().to_owned());
    service.enable(&profile.id, Some(&fpd)).unwrap();
    let base = service.store.account_dir(&profile.id);
    let run = base.join("runs").join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(run.join("samples")).unwrap();
    let raw = multiround_detection().to_string();
    fs::write(run.join("detection.json"), &raw).unwrap();
    let collection = sample_collection().to_string();
    fs::write(run.join("samples/result.json"), &collection).unwrap();
    let mut report = empty_report();
    report.directory = run.clone();
    report.finished_at = Some(1234);
    summarize(&mut report, &multiround_detection()).unwrap();
    save(&run.join("report.json"), &report).unwrap();
    save(&base.join("latest.json"), &report).unwrap();
    let original = fs::read(run.join("report.json")).unwrap();
    let schedule = fs::read(base.join("schedule.json")).unwrap();
    assert_eq!(service.repair(&profile.id).unwrap(), 2);
    assert_eq!(service.repair(&profile.id).unwrap(), 0);
    let corrected = service.status(&profile.id).unwrap().latest.unwrap();
    assert_eq!(corrected.status, ReportStatus::Completed);
    assert_eq!(corrected.accepted, 3);
    assert_eq!(corrected.confidence, Some(0.99));
    assert_eq!(corrected.scoring.unwrap().rounds, 1);
    assert_eq!(corrected.finished_at, report.finished_at);
    assert!(run.join("detection-round.json").is_file());
    assert_eq!(fs::read_to_string(run.join("detection.json")).unwrap(), raw);
    assert_eq!(
        fs::read_to_string(run.join("samples/result.json")).unwrap(),
        collection
    );
    assert_eq!(
        fs::read(run.join("report.before-round-fix.json")).unwrap(),
        original
    );
    assert_eq!(
        fs::read(base.join("latest.before-round-fix.json")).unwrap(),
        original
    );
    assert_eq!(fs::read(base.join("schedule.json")).unwrap(), schedule);
}
#[test]
fn missing_requested_model_is_never_reported_as_identity_failure() {
    let mut r = empty_report();
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
fn multiround_samples_succeed_without_inventing_complete_scores_or_an_aggregate_winner() {
    let mut report = empty_report();
    summarize(&mut report, &multiround_detection()).unwrap();
    assert_eq!(report.accepted, 3);
    assert_eq!(report.status, ReportStatus::Partial);
    assert_eq!(
        report.scoring,
        Some(Scoring {
            rounds: 3,
            ranked_rounds: 3,
            complete_rounds: 0
        })
    );
    assert_eq!(report.closest_candidate, None);
    assert!(report.reference_contains_model);
    assert_eq!(
        report.reference_label.as_deref(),
        Some("gpt-6-astra/6.1-sol")
    );
    assert!(report.message.contains("采样 3/3 成功"));
    assert!(report.message.contains("不提供置信度"));
    assert!(!report.message.contains("尚未收录"));
}
#[test]
fn single_round_alias_is_counted_once_and_invalid_reports_are_rejected() {
    let mut report = empty_report();
    let analysis = json!({"used_outputs":3,"results":[{"model":MODEL}]});
    summarize(
        &mut report,
        &json!({"schema":"fpd-detection-v1", "analysis":analysis,
        "rounds":[{"analysis":analysis}]}),
    )
    .unwrap();
    assert_eq!(report.accepted, 3);
    assert_eq!(report.scoring.as_ref().unwrap().rounds, 1);
    assert_eq!(report.status, ReportStatus::Completed);
    assert_eq!(report.closest_candidate.as_deref(), Some(MODEL));
    let mut detection = multiround_detection();
    detection["rounds"][0]["analysis"]["used_outputs"] = json!(3);
    assert!(summarize(&mut empty_report(), &detection).is_err());
    detection = multiround_detection();
    detection["cancelled"] = json!(true);
    assert!(summarize(&mut empty_report(), &detection).is_err());
}
#[test]
fn unranked_samples_are_preserved_even_when_scoring_fails() {
    let mut report = empty_report();
    let value = json!({"schema":"fpd-detection-v1","rounds":[
        {"analysis":{"used_outputs":2,"decision":"unscorable","results":[]}}
    ]});
    assert!(summarize(&mut report, &value).is_err());
    assert_eq!(report.accepted, 2);
    assert_eq!(
        report.scoring,
        Some(Scoring {
            rounds: 1,
            ranked_rounds: 0,
            complete_rounds: 0
        })
    );
}

#[test]
fn offline_repair_backs_up_only_false_failures_and_preserves_evidence_and_schedule() {
    use crate::codex_manager::tests::{Fixture, oauth_source};
    let f = Fixture::new();
    let mut manager = f.manager();
    let source = oauth_source(&f, "repair-workspace", "synthetic-refresh");
    let profile = manager.import_oauth(None, None, &source, None).unwrap();
    let service = FingerprintService::new(manager.root().to_owned());
    // A separate reader models the running dashboard's cached history.
    let reader = FingerprintService::new(manager.root().to_owned());
    let base = service.store.account_dir(&profile.id);
    let run = base.join("runs").join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(run.join("samples")).unwrap();
    let detection = multiround_detection().to_string();
    fs::write(run.join("detection.json"), &detection).unwrap();
    fs::write(run.join("samples/result.json"), "synthetic raw evidence").unwrap();
    service
        .store
        .enable(&profile.id, PathBuf::from("/missing/fpd"))
        .unwrap();
    let schedule = fs::read(base.join("schedule.json")).unwrap();
    let mut report = empty_report();
    report.directory = run.clone();
    report.status = ReportStatus::Failed;
    report.finished_at = Some(1);
    report.message = "本轮有效样本不足，无法评分；原始记录已保留".into();
    save(&run.join("report.json"), &report).unwrap();
    save(&base.join("latest.json"), &report).unwrap();
    let original = fs::read(run.join("report.json")).unwrap();

    // Genuine upstream failure with a valid-looking detector report stays failed.
    let limited = base.join("runs").join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&limited).unwrap();
    let mut quota = report.clone();
    quota.directory = limited.clone();
    quota.http_status = Some(429);
    save(&limited.join("report.json"), &quota).unwrap();
    fs::write(limited.join("detection.json"), &detection).unwrap();

    assert!(
        reader
            .status(&profile.id)
            .unwrap()
            .history
            .iter()
            .all(|r| r.status == ReportStatus::Failed)
    );
    assert_eq!(service.repair(&profile.id).unwrap(), 2);
    assert_eq!(service.repair(&profile.id).unwrap(), 0);
    let repaired = service.status(&profile.id).unwrap().latest.unwrap();
    assert_eq!(repaired.status, ReportStatus::Partial);
    assert_eq!(repaired.accepted, 3);
    assert_eq!(repaired.finished_at, Some(1));
    assert_eq!(
        read_report(&run.join("report.json")).unwrap().unwrap(),
        repaired
    );
    assert!(
        reader
            .status(&profile.id)
            .unwrap()
            .history
            .iter()
            .any(|r| r.status == ReportStatus::Partial)
    );
    assert_eq!(
        fs::read(run.join("report.before-multiround-fix.json")).unwrap(),
        original
    );
    assert_eq!(
        fs::read(base.join("latest.before-multiround-fix.json")).unwrap(),
        original
    );
    assert_eq!(
        fs::read_to_string(run.join("detection.json")).unwrap(),
        detection
    );
    assert_eq!(
        fs::read_to_string(run.join("samples/result.json")).unwrap(),
        "synthetic raw evidence"
    );
    assert_eq!(fs::read(base.join("schedule.json")).unwrap(), schedule);
    assert_eq!(
        read_report(&limited.join("report.json")).unwrap().unwrap(),
        quota
    );
    assert!(!limited.join("report.before-multiround-fix.json").exists());

    // The subsequent ID-only parser misclassified existing partial reports too.
    let mut missing_reference = repaired.clone();
    missing_reference.reference_contains_model = false;
    missing_reference.reference_label = None;
    missing_reference.message = "参考库尚未收录 gpt-6.1-sol".into();
    save(&run.join("report.json"), &missing_reference).unwrap();
    save(&base.join("latest.json"), &missing_reference).unwrap();
    let before_reference_fix = fs::read(run.join("report.json")).unwrap();
    assert_eq!(service.repair(&profile.id).unwrap(), 2);
    assert_eq!(service.repair(&profile.id).unwrap(), 0);
    assert!(
        service
            .status(&profile.id)
            .unwrap()
            .latest
            .unwrap()
            .reference_contains_model
    );
    assert_eq!(
        fs::read(run.join("report.before-reference-fix.json")).unwrap(),
        before_reference_fix
    );
    assert_eq!(
        fs::read(base.join("latest.before-reference-fix.json")).unwrap(),
        before_reference_fix
    );
    assert_eq!(
        fs::read_to_string(run.join("detection.json")).unwrap(),
        detection
    );

    // A stale latest pointer to another account cannot repair that account's data.
    report.directory = f.path.join(uuid::Uuid::new_v4().to_string());
    save(&base.join("latest.json"), &report).unwrap();
    assert_eq!(service.repair(&profile.id).unwrap(), 0);
    let lock = fs::File::open(base.join("run.lock")).unwrap();
    lock.lock_exclusive().unwrap();
    assert!(service.repair(&profile.id).is_err());
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
fn official_sample_and_offline_score_obey_five_hour_failure_and_overlap_guards() {
    use crate::codex_manager::tests::{Fixture, oauth_source};
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let mut manager = f.manager();
    let source = oauth_source(&f, "fingerprint-workspace", "synthetic-refresh");
    let p = manager.import_oauth(None, None, &source, None).unwrap();
    let home = manager.home(&p.id, p.revision).unwrap();
    let config = home.join(format!("usage-r{}.config.toml", p.revision));
    let changed = "model = \"gpt-6-luna\"\nmodel_provider = \"daily-provider\"\n";
    fs::write(&config, changed).unwrap();
    let auth_before = fs::read(home.join("auth.json")).unwrap();
    let fake = test_fpd(&f.path);
    fs::write(&fake, r#"#!/usr/bin/env python3
import json,os,sys,pathlib
args=sys.argv[1:]
assert not os.environ.get('API_KEY')
assert not os.environ.get('BASE_URL')
if args[0]=='sample':
    assert 'homes' in os.environ['CODEX_HOME']
    assert args[args.index('--model')+1]=='gpt-6.1-sol'
    assert args[args.index('--response-model')+1]=='gpt-6.1-sol'
    assert args[args.index('--effort')+1]=='low'
    assert args[args.index('--channel')+1]=='codex-subscription'
    assert args[args.index('--count')+1]=='3'
    assert args[args.index('--max-attempts')+1]=='1'
    assert '--baseurl' not in args and '--apikey' not in args
    out=pathlib.Path(args[args.index('--output-dir')+1]);out.mkdir()
    (out/'result.json').write_text((pathlib.Path(__file__).parents[2]/'collection-fixture.json').read_text())
    print('{}')
else:
    assert '--input' in args and '--json' in args
    assert '--strict' in args
    assert os.environ['CODEX_HOME'].endswith('offline-home')
    saved=json.loads(pathlib.Path(args[args.index('--input')+1]).read_text())
    assert len(saved['rounds'])==1 and len(saved['rounds'][0]['outputs'])==3
    assert all(s['completion']=='complete' for s in saved['rounds'][0]['samples'])
    print(json.dumps({'schema':'fpd-detection-v1','analysis':{'used_outputs':3,'results':[{'model':'gpt-6-sol'}]}}))
"#).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let report = run(&mut manager, &p.id, true, Some(&fake))
        .unwrap()
        .unwrap();
    assert_eq!(report.status, ReportStatus::Completed);
    assert!(!report.reference_contains_model);
    assert!(report.directory.join("detection.json").is_file());
    assert_eq!(fs::read_to_string(&config).unwrap(), changed);
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), auth_before);
    assert!(
        run(&mut manager, &p.id, true, Some(&fake))
            .unwrap()
            .is_none()
    );
    let base = directory(&manager, &p.id);
    let mut recent = report.clone();
    recent.started_at -= 5 * 3600 - 60;
    save(&base.join("latest.json"), &recent).unwrap();
    assert!(
        run(&mut manager, &p.id, true, Some(&fake))
            .unwrap()
            .is_none(),
        "4h59m must not start another automatic test"
    );
    let mut aged = report.clone();
    aged.started_at -= 5 * 3600;
    save(&base.join("latest.json"), &aged).unwrap();
    fs::remove_file(&config).unwrap();
    let next = run(&mut manager, &p.id, true, Some(&fake))
        .unwrap()
        .unwrap();
    assert_eq!(next.status, ReportStatus::Completed);
    assert_ne!(next.directory, report.directory);
    assert!(
        !config.exists(),
        "sampling must not materialize configuration"
    );

    let other = oauth_source(&f, "different-workspace", "other-refresh");
    fs::copy(&other, home.join("auth.json")).unwrap();
    let mismatched = run(&mut manager, &p.id, false, Some(&fake))
        .unwrap()
        .unwrap();
    assert_eq!(mismatched.status, ReportStatus::Failed);
    assert!(mismatched.message.contains("OAuth 登录与所选账号不符"));
    assert!(!mismatched.directory.join("samples").exists());
    fs::write(home.join("auth.json"), &auth_before).unwrap();

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
