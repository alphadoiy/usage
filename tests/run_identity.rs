//! Synthetic launch identities and resource credentials only.
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("usage-run-identity-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("usage.toml"), "").unwrap();
        Self(root.canonicalize().unwrap())
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_usage"));
        command
            .args(args)
            .env("USAGE_CODEX_DIR", self.0.join("manager"))
            .env("USAGE_CONFIG", self.0.join("usage.toml"))
            .env("USAGE_LOG_DIR", self.0.join("logs"))
            .env("USAGE_LOG", "debug")
            .env("USAGE_GATEWAY", "off")
            .env("FIXTURE_INFERENCE_A", "synthetic-inference-a")
            .env("FIXTURE_INFERENCE_B", "synthetic-inference-b")
            .env("FIXTURE_RESOURCE_A", "synthetic-query-a")
            .env("FIXTURE_RESOURCE_B", "synthetic-query-b");
        command
    }
    fn add(&self) -> String {
        let mut child = self
            .command(&[
                "codex",
                "add",
                "Work",
                "--base-url",
                "https://a.invalid/v1",
                "--model",
                "base-model",
                "--key-stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"synthetic-inference-a\n")
            .unwrap();
        ok(child.wait_with_output().unwrap());
        let profiles: Value = serde_json::from_str(&ok(self
            .command(&["codex", "list", "--json"])
            .output()
            .unwrap()))
        .unwrap();
        profiles[0]["id"].as_str().unwrap().into()
    }
    fn configure(&self, profile: &str) -> String {
        let text = format!(
            r#"
[[resources]]
id="a"
service="new_api_account"
base_url="https://a.invalid"
credential_env="FIXTURE_RESOURCE_A"
profile_ids=["{profile}"]
options={{user_id=1}}
inference={{base_url="https://a.invalid/v1",credential_env="FIXTURE_INFERENCE_A"}}
[[resources]]
id="b"
service="new_api_account"
base_url="https://b.invalid"
credential_env="FIXTURE_RESOURCE_B"
profile_ids=["{profile}"]
options={{user_id=2}}
inference={{base_url="https://b.invalid/v1",credential_env="FIXTURE_INFERENCE_B"}}
[[resources]]
id="association-only"
service="new_api_token"
base_url="https://a.invalid"
credential_env="FIXTURE_RESOURCE_A"
profile_ids=["{profile}"]
[[resources]]
id="wrong-token"
service="new_api_token"
base_url="https://a.invalid"
credential_env="FIXTURE_RESOURCE_A"
profile_ids=["{profile}"]
inference={{base_url="https://a.invalid/v1",credential_env="FIXTURE_INFERENCE_A"}}
"#
        );
        fs::write(self.0.join("usage.toml"), &text).unwrap();
        text
    }
    #[cfg(unix)]
    fn binary(&self) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let binary = self.0.join("fake-codex");
        if binary.exists() {
            return binary;
        }
        fs::write(
            &binary,
            "#!/bin/sh\nif [ \"$1\" = --help ]; then echo --no-daemon; fi\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        binary
    }
    #[cfg(unix)]
    fn launch(&self, profile: &str, args: &[&str]) -> Command {
        let binary = self.binary();
        let mut command = self.command(&[
            "codex",
            "run",
            profile,
            "--cwd",
            self.0.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
            "--",
        ]);
        command.args(args);
        command
    }
    fn runs(&self) -> Value {
        serde_json::from_str(&ok(self
            .command(&["codex", "runs", "--json"])
            .output()
            .unwrap()))
        .unwrap()
    }
    fn resource_list(&self, run: &str) -> Command {
        self.command(&["resources", "--run", run, "--list", "--json"])
    }
    fn dump(&self) -> Vec<u8> {
        fs::read(self.0.join("manager/manager.sqlite3")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn ok(output: Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}
fn failure(output: Output, reason: &str) {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(reason),
        "{output:?}"
    );
}

#[cfg(unix)]
#[test]
fn persisted_model_choice_is_captured_and_cli_overrides_still_take_precedence() {
    let fixture = Fixture::new();
    let id = fixture.add();
    fixture.configure(&id);
    let manager = usage::codex_manager::Manager::open(fixture.0.join("manager")).unwrap();
    let saved = manager.revision(&id, 1).unwrap().config;
    let home = manager.materialize(&id, 1).unwrap();
    let runtime = saved.replace("base-model", "selected-model");
    fs::write(home.join("config.toml"), &runtime).unwrap();
    drop(manager);

    ok(fixture.launch(&id, &[]).output().unwrap());
    ok(fixture
        .launch(&id, &["--model", "override-model"])
        .output()
        .unwrap());
    let runs = fixture.runs();
    let mut models: Vec<_> = runs
        .as_array()
        .unwrap()
        .iter()
        .map(|run| {
            assert_eq!(run["context"]["identity"]["state"], "resolved");
            assert_eq!(
                run["context"]["identity"]["endpoint"],
                "https://a.invalid/v1"
            );
            run["context"]["identity"]["model"].as_str().unwrap()
        })
        .collect();
    models.sort_unstable();
    assert_eq!(models, ["override-model", "selected-model"]);
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        runtime
    );
    let manager = usage::codex_manager::Manager::open(fixture.0.join("manager")).unwrap();
    assert_eq!(manager.revision(&id, 1).unwrap().config, saved);
}

#[cfg(unix)]
#[test]
fn concurrent_runs_of_one_profile_pin_different_effective_endpoints_and_credentials() {
    let fixture = Fixture::new();
    let id = fixture.add();
    fixture.configure(&id);
    let a = fixture
        .launch(&id, &["--model", "model-a"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let b = fixture
        .launch(
            &id,
            &[
                "exec",
                "-c",
                "model_providers.usage_byok.base_url='https://b.invalid/v1'",
                "-c",
                "model_providers.usage_byok.env_key='FIXTURE_INFERENCE_B'",
                "-c",
                "model='config-model'",
                "-m",
                "model-b",
            ],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    ok(a.wait_with_output().unwrap());
    ok(b.wait_with_output().unwrap());
    let runs = fixture.runs();
    assert_eq!(runs.as_array().unwrap().len(), 2);
    let by_model = |name: &str| {
        runs.as_array()
            .unwrap()
            .iter()
            .find(|r| r["context"]["identity"]["model"] == name)
            .unwrap()
    };
    let a = by_model("model-a");
    let b = by_model("model-b");
    assert_ne!(a["id"], b["id"]);
    assert_eq!(
        a["context"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["binding"]["account"] == "wrong-token")
            .unwrap()["state"],
        "credential_mismatch"
    );
    for (run, resource, endpoint, source) in [
        (a, "a", "https://a.invalid/v1", "managed_revision"),
        (b, "b", "https://b.invalid/v1", "environment"),
    ] {
        let context = &run["context"];
        assert_eq!(context["identity"]["state"], "resolved");
        assert_eq!(context["identity"]["endpoint"], endpoint);
        assert_eq!(context["identity"]["credential_source"], source);
        let links = context["resources"].as_array().unwrap();
        let matched: Vec<_> = links
            .iter()
            .filter(|l| l["state"] == "matched")
            .map(|l| l["binding"]["account"].clone())
            .collect();
        assert_eq!(matched, vec![json!(resource)]);
        assert_eq!(
            links
                .iter()
                .find(|l| l["binding"]["account"] == "association-only")
                .unwrap()["state"],
            "missing_inference_binding"
        );
        let list: Value = serde_json::from_str(&ok(fixture
            .resource_list(run["id"].as_str().unwrap())
            .output()
            .unwrap()))
        .unwrap();
        assert_eq!(list[0]["id"], resource);
        assert_eq!(list.as_array().unwrap().len(), 1);
    }
    assert_eq!(fixture.runs()[0]["revision"], 1);
    let logs: String = fs::read_dir(fixture.0.join("logs"))
        .unwrap()
        .map(|f| fs::read_to_string(f.unwrap().path()).unwrap())
        .collect();
    assert!(logs.contains("run_identity_captured"));
    for secret in [
        "synthetic-inference-a",
        "synthetic-inference-b",
        "synthetic-query-a",
        "synthetic-query-b",
    ] {
        assert!(!runs.to_string().contains(secret));
        assert!(!logs.contains(secret));
        assert!(
            !fixture
                .dump()
                .windows(secret.len())
                .any(|s| s == secret.as_bytes())
        );
    }
    assert!(!runs.to_string().contains("proofs"));
}

#[cfg(unix)]
#[test]
fn run_selection_rejects_config_changes_secret_rotation_and_unbound_requests() {
    let fixture = Fixture::new();
    let id = fixture.add();
    let config = fixture.configure(&id);
    ok(fixture.launch(&id, &[]).output().unwrap());
    let runs = fixture.runs();
    let run = runs[0]["id"].as_str().unwrap();
    failure(
        fixture
            .command(&["resources", "b", "--run", run, "--list"])
            .output()
            .unwrap(),
        "未在此运行启动时匹配",
    );
    failure(
        fixture
            .resource_list(run)
            .env("FIXTURE_RESOURCE_A", "rotated-query")
            .output()
            .unwrap(),
        "凭据已轮换",
    );
    failure(
        fixture
            .resource_list(run)
            .env("FIXTURE_INFERENCE_A", "rotated-inference")
            .output()
            .unwrap(),
        "凭据已轮换",
    );
    fs::write(
        fixture.0.join("usage.toml"),
        config.replace("user_id=1", "user_id=3"),
    )
    .unwrap();
    failure(
        fixture.resource_list(run).output().unwrap(),
        "资源配置已变更",
    );
    fs::write(fixture.0.join("usage.toml"), &config).unwrap();
    let list: Value =
        serde_json::from_str(&ok(fixture.resource_list(run).output().unwrap())).unwrap();
    assert_eq!(list[0]["id"], "a");
    failure(
        fixture
            .command(&["resources", "--run", run, "--profile", "Work"])
            .output()
            .unwrap(),
        "cannot be used",
    );
}

#[cfg(unix)]
#[test]
fn endpoint_override_without_matching_inference_key_cannot_bind_an_account_balance() {
    let fixture = Fixture::new();
    let id = fixture.add();
    fixture.configure(&id);
    ok(fixture
        .launch(
            &id,
            &[
                "-c",
                "model_providers.usage_byok.base_url='https://b.invalid/v1'",
            ],
        )
        .output()
        .unwrap());
    let runs = fixture.runs();
    let run = &runs[0];
    let links = run["context"]["resources"].as_array().unwrap();
    assert_eq!(
        links
            .iter()
            .find(|l| l["binding"]["account"] == "a")
            .unwrap()["state"],
        "endpoint_mismatch"
    );
    assert_eq!(
        links
            .iter()
            .find(|l| l["binding"]["account"] == "b")
            .unwrap()["state"],
        "credential_mismatch"
    );
    failure(
        fixture
            .resource_list(run["id"].as_str().unwrap())
            .output()
            .unwrap(),
        "没有已匹配的资源",
    );
}

#[cfg(unix)]
#[test]
fn unresolved_sources_are_explicit_and_never_fall_back_to_profile_associations() {
    let fixture = Fixture::new();
    let id = fixture.add();
    fixture.configure(&id);
    for args in [
        vec!["--profile", "native-profile"],
        vec![
            "-c",
            "model_providers.usage_byok.experimental_bearer_token='inline-secret-fixture'",
        ],
        vec!["--unknown-routing-option"],
    ] {
        ok(fixture.launch(&id, &args).output().unwrap());
    }
    fs::create_dir(fixture.0.join(".codex")).unwrap();
    fs::write(
        fixture.0.join(".codex/config.toml"),
        "model='project-model'",
    )
    .unwrap();
    ok(fixture.launch(&id, &[]).output().unwrap());
    for run in fixture.runs().as_array().unwrap() {
        assert_eq!(run["context"]["identity"]["state"], "unresolved");
        assert!(run["context"]["identity"]["reason"].is_string());
        failure(
            fixture
                .resource_list(run["id"].as_str().unwrap())
                .output()
                .unwrap(),
            "实际配置未解析",
        );
    }
    assert!(
        !fixture
            .dump()
            .windows(b"inline-secret-fixture".len())
            .any(|s| s == b"inline-secret-fixture")
    );
}

#[cfg(unix)]
#[test]
fn explicit_config_path_is_carried_to_launch_and_old_runs_remain_readable() {
    let fixture = Fixture::new();
    let id = fixture.add();
    fixture.configure(&id);
    let path = fixture.0.join("explicit usage.toml");
    fs::rename(fixture.0.join("usage.toml"), &path).unwrap();
    fs::write(fixture.0.join("usage.toml"), "").unwrap();
    // An explicit global config must win over the inherited USAGE_CONFIG.
    // Put the global option before the subcommand, outside the native argument tail.
    let binary = fixture.binary();
    ok(fixture
        .command(&[
            "--config",
            path.to_str().unwrap(),
            "codex",
            "run",
            &id,
            "--cwd",
            fixture.0.to_str().unwrap(),
            "--binary",
            binary.to_str().unwrap(),
        ])
        .output()
        .unwrap());
    let runs = fixture.runs();
    let run = runs[0]["id"].as_str().unwrap();
    let list: Value = serde_json::from_str(&ok(fixture
        .command(&[
            "--config",
            path.to_str().unwrap(),
            "resources",
            "--run",
            run,
            "--list",
            "--json",
        ])
        .output()
        .unwrap()))
    .unwrap();
    assert_eq!(list[0]["id"], "a");
    let db = rusqlite::Connection::open(fixture.0.join("manager/manager.sqlite3")).unwrap();
    db.execute("DROP TABLE run_contexts", []).unwrap();
    assert_eq!(fixture.runs()[0]["context"], Value::Null);
    failure(
        fixture.resource_list(run).output().unwrap(),
        "旧运行未记录实际配置",
    );
}

#[cfg(unix)]
#[test]
fn run_binding_nested_session_commands_are_unresolved() {
    let f = Fixture::new();
    let id = f.add();
    f.configure(&id);
    for exec in ["exec", "e"] {
        for command in ["resume", "fork"] {
            ok(f.launch(
                &id,
                &[
                    "--model",
                    "review",
                    exec,
                    "--json",
                    command,
                    "123e4567-e89b-12d3-a456-426614174001",
                ],
            )
            .output()
            .unwrap());
        }
    }
    for run in f.runs().as_array().unwrap() {
        assert_eq!(
            run["context"]["identity"]["state"], "unresolved",
            "nested session commands must not claim the saved launch identity"
        );
        assert!(
            run["context"]["resources"]
                .as_array()
                .unwrap()
                .iter()
                .all(|link| link["state"] == "unresolved_identity")
        );
        failure(
            f.resource_list(run["id"].as_str().unwrap())
                .output()
                .unwrap(),
            "实际配置未解析",
        );
    }
}

#[cfg(unix)]
#[test]
fn run_binding_nested_exec_review_is_unresolved() {
    let f = Fixture::new();
    let id = f.add();
    f.configure(&id);
    for exec in ["exec", "e"] {
        ok(f.launch(
            &id,
            &[exec, "--model", "resume", "review", "review this change"],
        )
        .output()
        .unwrap());
    }
    for run in f.runs().as_array().unwrap() {
        assert_eq!(
            run["context"]["identity"]["state"], "unresolved",
            "nested review must follow the same uncertainty rule as root review"
        );
        failure(
            f.resource_list(run["id"].as_str().unwrap())
                .output()
                .unwrap(),
            "实际配置未解析",
        );
    }
}

#[cfg(unix)]
#[test]
fn run_binding_command_names_inside_prompts_stay_resolved() {
    let f = Fixture::new();
    let id = f.add();
    f.configure(&id);
    for args in [
        vec!["exec", "resume the migration"],
        vec!["e", "review the migration"],
        vec!["exec", "login"],
        vec!["exec", "exec"],
        vec!["exec", "--", "resume"],
        vec!["--", "resume"],
        vec!["--model", "resume", "exec", "ordinary prompt"],
        vec!["exec", "--output-last-message", "review", "ordinary prompt"],
    ] {
        ok(f.launch(&id, &args).output().unwrap());
    }
    assert!(
        f.runs()
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["context"]["identity"]["state"] == "resolved")
    );
}

#[cfg(unix)]
#[test]
fn run_binding_inference_configuration_changes_invalidate_the_link() {
    let f = Fixture::new();
    let id = f.add();
    let text = f.configure(&id);
    ok(f.launch(&id, &[]).output().unwrap());
    let runs = f.runs();
    let run = runs[0]["id"].as_str().unwrap();
    let mut config: toml::Value = toml::from_str(&text).unwrap();
    config["resources"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|c| c["id"].as_str() == Some("a"))
        .unwrap()["inference"]["base_url"] = "https://b.invalid/v1".into();
    fs::write(f.0.join("usage.toml"), toml::to_string(&config).unwrap()).unwrap();
    failure(f.resource_list(run).output().unwrap(), "资源配置已变更");
}

#[cfg(unix)]
#[test]
fn run_binding_removing_profile_membership_invalidates_the_link() {
    let f = Fixture::new();
    let id = f.add();
    let text = f.configure(&id);
    ok(f.launch(&id, &[]).output().unwrap());
    let runs = f.runs();
    let run = runs[0]["id"].as_str().unwrap();
    let mut config: toml::Value = toml::from_str(&text).unwrap();
    config["resources"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|c| c["id"].as_str() == Some("a"))
        .unwrap()["profile_ids"] = toml::Value::Array(vec![]);
    fs::write(f.0.join("usage.toml"), toml::to_string(&config).unwrap()).unwrap();
    failure(f.resource_list(run).output().unwrap(), "资源配置已变更");
}

#[cfg(unix)]
#[test]
fn run_binding_unknown_resource_factory_cannot_be_marked_matched() {
    let f = Fixture::new();
    let id = f.add();
    let text = f.configure(&id);
    fs::write(
        f.0.join("usage.toml"),
        text.replacen(
            "service=\"new_api_account\"",
            "service=\"unknown-adapter\"",
            1,
        ),
    )
    .unwrap();
    ok(f.launch(&id, &[]).output().unwrap());
    let runs = f.runs();
    let links = runs[0]["context"]["resources"].as_array().unwrap();
    assert_eq!(
        links
            .iter()
            .find(|l| l["binding"]["account"] == "a")
            .unwrap()["state"],
        "invalid_resource_configuration"
    );
}

#[cfg(unix)]
#[test]
fn run_binding_context_insert_failure_does_not_leave_an_orphan_run() {
    let f = Fixture::new();
    let id = f.add();
    f.configure(&id);
    let db = rusqlite::Connection::open(f.0.join("manager/manager.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_run_context BEFORE INSERT ON run_contexts BEGIN SELECT RAISE(ABORT,'synthetic insert failure'); END;").unwrap();
    let output = f.launch(&id, &[]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "run and context must commit together");
}
