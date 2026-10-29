use super::*;
use crate::codex_manager::tests::{Fixture, native, oauth_source};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
fn read(path: &Path) -> toml::Value {
    fs::read_to_string(path).unwrap().parse().unwrap()
}

#[test]
fn global_context_is_inherited_without_replacing_account_credentials() {
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let base = base_home(manager.root());
    let global = r#"
model = 'global-model'
model_provider = 'global-provider'
chatgpt_base_url = 'https://global.invalid'
model_instructions_file = 'instructions.md'
experimental_compact_prompt_file = 'compact.md'
[model_providers.global-provider]
experimental_bearer_token = 'global-secret'
[features]
shell_snapshot = true
[agents.reviewer]
config_file = 'roles/reviewer.toml'
[mcp_servers.docs]
command = 'fixture-mcp'
cwd = 'mcp-work'
[tui]
notifications = false
"#;
    write(&base.join("config.toml"), global);
    for name in [
        "AGENTS.md",
        "rules/default.rules",
        "skills/custom/SKILL.md",
        "plugins/config.json",
        "roles/reviewer.toml",
        ".credentials.json",
    ] {
        write(&base.join(name), "fixture context");
    }
    write(&base.join("auth.json"), "global-auth-must-stay-private");
    let account = manager
        .add(
            "A",
            &native("https://a.invalid/v1", "account-model"),
            "key-a",
        )
        .unwrap();
    let home = manager.materialize(&account.id, 1).unwrap();
    let config = read(&home.join("config.toml"));
    assert_eq!(config["model"].as_str(), Some("account-model"));
    assert_eq!(config["model_provider"].as_str(), Some("usage_byok"));
    assert_eq!(config["features"]["shell_snapshot"].as_bool(), Some(true));
    assert_eq!(config["tui"]["notifications"].as_bool(), Some(false));
    assert_eq!(
        config["agents"]["reviewer"]["config_file"].as_str(),
        base.join("roles/reviewer.toml").to_str()
    );
    assert_eq!(
        config["mcp_servers"]["docs"]["cwd"].as_str(),
        base.join("mcp-work").to_str()
    );
    assert_eq!(
        config["model_instructions_file"].as_str(),
        base.join("instructions.md").to_str()
    );
    assert!(!config.to_string().contains("global-secret"));
    assert!(config.get("chatgpt_base_url").is_none());
    assert!(!home.join("auth.json").exists());
    assert_eq!(
        fs::read_to_string(home.join("skills/custom/SKILL.md")).unwrap(),
        "fixture context"
    );
    assert_eq!(
        fs::read_to_string(home.join(".credentials.json")).unwrap(),
        "fixture context"
    );
    write(&base.join("AGENTS.md"), "updated instructions");
    assert_eq!(
        fs::read_to_string(home.join("AGENTS.md")).unwrap(),
        "updated instructions"
    );
    assert_eq!(
        fs::read_to_string(base.join("config.toml")).unwrap(),
        global
    );

    let source = oauth_source(&fixture, "workspace", "refresh-fixture");
    let oauth = manager
        .import_oauth(Some("OAuth"), None, &source, None)
        .unwrap();
    let oauth_home = manager.materialize(&oauth.id, 1).unwrap();
    let oauth_config = read(&oauth_home.join("usage-r1.config.toml"));
    assert_eq!(oauth_config["model_provider"].as_str(), Some("openai"));
    assert_eq!(
        oauth_config["mcp_servers"]["docs"]["command"].as_str(),
        Some("fixture-mcp")
    );
    assert!(
        fs::read_to_string(oauth_home.join("auth.json"))
            .unwrap()
            .contains("refresh-fixture")
    );
    assert!(!oauth_home.join("auth.json").is_symlink());
}

#[test]
fn global_updates_and_native_preference_edits_both_survive_relaunch() {
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let base = base_home(manager.root());
    write(
        &base.join("config.toml"),
        "personality='friendly'\n[tui]\nnotifications=false\nstatus_line=['model-name']\n",
    );
    let account = manager
        .add("A", &native("https://a.invalid/v1", "model"), "key-a")
        .unwrap();
    let home = manager.materialize(&account.id, 1).unwrap();
    let mut config = read(&home.join("config.toml"));
    config["model"] = "native-choice".into();
    config["tui"]["notifications"] = true.into();
    config.as_table_mut().unwrap().remove("personality");
    write(
        &home.join("config.toml"),
        &toml::to_string(&config).unwrap(),
    );
    write(
        &base.join("config.toml"),
        "personality='pragmatic'\n[tui]\nnotifications=false\nstatus_line=['context-remaining']\n[mcp_servers.new]\ncommand='new-mcp'\n",
    );
    write(&base.join("skills/new/SKILL.md"), "new skill");
    manager.materialize(&account.id, 1).unwrap();
    let config = read(&home.join("config.toml"));
    assert_eq!(config["model"].as_str(), Some("native-choice"));
    assert_eq!(config["tui"]["notifications"].as_bool(), Some(true));
    assert_eq!(
        config["tui"]["status_line"][0].as_str(),
        Some("context-remaining")
    );
    assert!(config.get("personality").is_none());
    assert_eq!(
        config["mcp_servers"]["new"]["command"].as_str(),
        Some("new-mcp")
    );
    assert!(home.join("skills/new/SKILL.md").exists());
    let before = fs::read(home.join("config.toml")).unwrap();
    manager.materialize(&account.id, 1).unwrap();
    assert_eq!(fs::read(home.join("config.toml")).unwrap(), before);
    assert!(
        manager
            .revision(&account.id, 1)
            .unwrap()
            .config
            .contains("model = \"model\"")
    );
}

#[test]
fn default_profile_preferences_and_relative_paths_are_inherited() {
    let text = "profile='work'\n[profiles.work]\nmodel_provider='private'\nmodel_instructions_file='work.md'\n[profiles.work.features]\nshell_snapshot=true\n[projects.fixture]\nmodel_provider='private'\ntrust_level='trusted'\n";
    let preferences: toml::Value = preferences(text, Path::new("/fixture/codex"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(preferences.get("profile").is_none());
    assert_eq!(
        preferences["model_instructions_file"].as_str(),
        Some("/fixture/codex/work.md")
    );
    assert_eq!(
        preferences["features"]["shell_snapshot"].as_bool(),
        Some(true)
    );
    assert_eq!(
        preferences["projects"]["fixture"]["trust_level"].as_str(),
        Some("trusted")
    );
    assert!(
        config::account_settings(&preferences)
            .as_table()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn native_path_fields_keep_the_original_base_in_tables_and_inline_values() {
    let base = Path::new("/fixture/native");
    let text = r#"
js_repl_node_path = 'tools/node'
js_repl_node_module_dirs = ['modules', '/opt/shared/modules', '~']
[sandbox_workspace_write]
writable_roots = ['shared-files', '/opt/shared/files']
[skills]
config = [{ path = 'skills/disabled', enabled = false }]
[otel]
exporter = { otlp-http = { endpoint = 'http://localhost:9', protocol = 'binary', tls = { ca-certificate = 'certs/ca.pem', client-certificate = 'certs/client.pem', client-private-key = 'certs/client.key' } } }
[otel.trace_exporter.otlp-grpc.tls]
ca-certificate = 'trace/ca.pem'
[otel.metrics_exporter.otlp-http.tls]
client-private-key = 'metrics/client.key'
[mcp_servers.local]
command = './server'
cwd = 'mcp'
[permissions.custom.filesystem]
'./project-files' = 'write'
[future_extension]
path = 'leave-unknown-fields-alone'
[profiles.work]
js_repl_node_path = 'tools/work-node'
js_repl_node_module_dirs = ['work-modules']
[[profiles.work.skills.config]]
path = 'skills/work'
enabled = false
"#;
    let config: toml::Value = preferences(text, base).unwrap().parse().unwrap();
    assert_eq!(
        config["js_repl_node_path"].as_str(),
        base.join("tools/node").to_str()
    );
    assert_eq!(
        config["js_repl_node_module_dirs"][0].as_str(),
        base.join("modules").to_str()
    );
    assert_eq!(
        config["js_repl_node_module_dirs"][1].as_str(),
        Some("/opt/shared/modules")
    );
    assert_eq!(
        config["js_repl_node_module_dirs"][2].as_str(),
        crate::config::home_dir().to_str()
    );
    assert_eq!(
        config["sandbox_workspace_write"]["writable_roots"][0].as_str(),
        base.join("shared-files").to_str()
    );
    assert_eq!(
        config["sandbox_workspace_write"]["writable_roots"][1].as_str(),
        Some("/opt/shared/files")
    );
    assert_eq!(
        config["skills"]["config"][0]["path"].as_str(),
        base.join("skills/disabled").to_str()
    );
    for (field, path) in [
        ("ca-certificate", "certs/ca.pem"),
        ("client-certificate", "certs/client.pem"),
        ("client-private-key", "certs/client.key"),
    ] {
        assert_eq!(
            config["otel"]["exporter"]["otlp-http"]["tls"][field].as_str(),
            base.join(path).to_str()
        );
    }
    assert_eq!(
        config["otel"]["trace_exporter"]["otlp-grpc"]["tls"]["ca-certificate"].as_str(),
        base.join("trace/ca.pem").to_str()
    );
    assert_eq!(
        config["otel"]["metrics_exporter"]["otlp-http"]["tls"]["client-private-key"].as_str(),
        base.join("metrics/client.key").to_str()
    );
    assert_eq!(
        config["profiles"]["work"]["js_repl_node_path"].as_str(),
        base.join("tools/work-node").to_str()
    );
    assert_eq!(
        config["profiles"]["work"]["js_repl_node_module_dirs"][0].as_str(),
        base.join("work-modules").to_str()
    );
    assert_eq!(
        config["profiles"]["work"]["skills"]["config"][0]["path"].as_str(),
        base.join("skills/work").to_str()
    );
    assert_eq!(
        config["mcp_servers"]["local"]["command"].as_str(),
        Some("./server")
    );
    assert_eq!(
        config["mcp_servers"]["local"]["cwd"].as_str(),
        base.join("mcp").to_str()
    );
    assert_eq!(
        config["permissions"]["custom"]["filesystem"]["./project-files"].as_str(),
        Some("write")
    );
    assert_eq!(
        config["future_extension"]["path"].as_str(),
        Some("leave-unknown-fields-alone")
    );
}

#[test]
fn existing_instance_context_is_kept_and_new_global_entries_are_added() {
    let fixture = Fixture::new();
    let mut manager = fixture.manager();
    let base = base_home(manager.root());
    let account = manager
        .add("A", &native("https://a.invalid/v1", "model"), "key-a")
        .unwrap();
    let home = manager.materialize(&account.id, 1).unwrap();
    write(&home.join("AGENTS.md"), "local instructions");
    write(&home.join("skills/local/SKILL.md"), "local skill");
    write(&base.join("AGENTS.md"), "global instructions");
    write(&base.join("skills/global/SKILL.md"), "global skill");
    manager.materialize(&account.id, 1).unwrap();
    assert_eq!(
        fs::read_to_string(home.join("AGENTS.md")).unwrap(),
        "local instructions"
    );
    assert!(home.join("skills/local/SKILL.md").exists());
    assert!(home.join("skills/global/SKILL.md").exists());
}
