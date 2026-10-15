// Appended only in the disposable experiment copy; reuses synthetic Fixture.
#[cfg(unix)]
#[test]
fn ablation_probe_nested_exec_resume_is_unresolved() {
    let f=Fixture::new(); let id=f.add(); f.configure(&id);
    ok(f.launch(&id,&["exec","resume","123e4567-e89b-12d3-a456-426614174001"]).output().unwrap());
    let runs=f.runs();
    assert_eq!(runs[0]["context"]["identity"]["state"],"unresolved","nested resume must not claim the saved launch identity");
}

#[cfg(unix)]
#[test]
fn ablation_probe_nested_exec_review_is_unresolved() {
    let f=Fixture::new(); let id=f.add(); f.configure(&id);
    ok(f.launch(&id,&["exec","review","review this change"]).output().unwrap());
    assert_eq!(f.runs()[0]["context"]["identity"]["state"],"unresolved","nested review must follow the same uncertainty rule as root review");
}

#[cfg(unix)]
#[test]
fn ablation_probe_command_names_inside_prompts_stay_resolved() {
    let f=Fixture::new(); let id=f.add(); f.configure(&id);
    ok(f.launch(&id,&["exec","resume the migration"]).output().unwrap());
    ok(f.launch(&id,&["--","resume"]).output().unwrap());
    assert!(f.runs().as_array().unwrap().iter().all(|r|r["context"]["identity"]["state"]=="resolved"));
}

#[cfg(unix)]
#[test]
fn ablation_probe_inference_configuration_changes_invalidate_the_link() {
    let f=Fixture::new(); let id=f.add(); let text=f.configure(&id);
    ok(f.launch(&id,&[]).output().unwrap()); let runs=f.runs(); let run=runs[0]["id"].as_str().unwrap();
    let mut config:toml::Value=toml::from_str(&text).unwrap();
    config["resources"].as_array_mut().unwrap().iter_mut().find(|c|c["id"].as_str()==Some("a")).unwrap()["inference"]["base_url"]="https://b.invalid/v1".into();
    fs::write(f.0.join("usage.toml"),toml::to_string(&config).unwrap()).unwrap();
    failure(f.resource_list(run).output().unwrap(),"资源配置已变更");
}

#[cfg(unix)]
#[test]
fn ablation_probe_removing_profile_membership_invalidates_the_link() {
    let f=Fixture::new(); let id=f.add(); let text=f.configure(&id);
    ok(f.launch(&id,&[]).output().unwrap()); let runs=f.runs(); let run=runs[0]["id"].as_str().unwrap();
    let mut config:toml::Value=toml::from_str(&text).unwrap();
    config["resources"].as_array_mut().unwrap().iter_mut().find(|c|c["id"].as_str()==Some("a")).unwrap()["profile_ids"]=toml::Value::Array(vec![]);
    fs::write(f.0.join("usage.toml"),toml::to_string(&config).unwrap()).unwrap();
    failure(f.resource_list(run).output().unwrap(),"资源配置已变更");
}

#[cfg(unix)]
#[test]
fn ablation_probe_unknown_resource_factory_cannot_be_marked_matched() {
    let f=Fixture::new(); let id=f.add(); let text=f.configure(&id);
    fs::write(f.0.join("usage.toml"),text.replacen("service=\"new_api_account\"","service=\"unknown-adapter\"",1)).unwrap();
    ok(f.launch(&id,&[]).output().unwrap());
    let runs=f.runs(); let links=runs[0]["context"]["resources"].as_array().unwrap();
    assert_eq!(links.iter().find(|l|l["binding"]["account"]=="a").unwrap()["state"],"invalid_resource_configuration");
}

#[cfg(unix)]
#[test]
fn ablation_probe_context_insert_failure_does_not_leave_an_orphan_run() {
    let f=Fixture::new(); let id=f.add(); f.configure(&id);
    let db=rusqlite::Connection::open(f.0.join("manager/manager.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_run_context BEFORE INSERT ON run_contexts BEGIN SELECT RAISE(ABORT,'synthetic insert failure'); END;").unwrap();
    let output=f.launch(&id,&[]).output().unwrap();
    assert_eq!(output.status.code(),Some(2));
    let count:i64=db.query_row("SELECT COUNT(*) FROM runs",[],|r|r.get(0)).unwrap();
    assert_eq!(count,0,"run and context must commit together");
}

#[cfg(unix)]
#[test]
fn ablation_probe_startup_manager_open_count_is_observable() {
    let f=Fixture::new(); let id=f.add(); f.configure(&id);
    let before:std::collections::BTreeSet<_>=fs::read_dir(f.0.join("logs")).unwrap().map(|e|e.unwrap().path()).collect();
    ok(f.launch(&id,&[]).output().unwrap());
    let events:Vec<Value>=fs::read_dir(f.0.join("logs")).unwrap().map(|e|e.unwrap().path()).filter(|p|!before.contains(p))
        .flat_map(|p|fs::read_to_string(p).unwrap().lines().map(|l|serde_json::from_str::<Value>(l).unwrap()).collect::<Vec<_>>()).collect();
    let opens=events.iter().filter(|e|e["fields"]["event"]=="operation_started" && e["span"]["operation"]=="codex.manager_open").count();
    assert!(opens>=2);
    eprintln!("ABLATION_METRIC manager_opens_per_cli_launch={opens}");
}
