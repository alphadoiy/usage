#!/usr/bin/env python3
"""Single-factor, whole-suite ablation in disposable source copies.

Requires cached Cargo dependencies and loopback networking for existing HTTP tests.
Never edits production source or reads the user's account/notification configuration.
"""
import argparse
import json
from pathlib import Path
import subprocess

from ablation.runner import (clone_target, isolated_environment, manifest, prepare_workspace,
                             replace, restore as restore_inputs, run_suite, save_diff, write_manifest)

SOURCE = Path(__file__).resolve().parents[1]


def function(repo, name, signature, replacement):
    path = repo / name
    text = path.read_text()
    start = text.index(signature)
    end = text.index("\n}", start) + 2
    path.write_text(text[:start] + replacement + text[end:])


def bypass_hub(repo):
    function(repo, "src/codex_manager/managed.rs", "pub fn start(", """
pub fn start(root: &std::path::Path, account: &str, revision: Option<i64>, request: &SpawnRequest) -> Result<Box<dyn RunningAgent>, String> {
    crate::codex_manager::Manager::open(root.to_path_buf()).and_then(|mut manager|
        manager.start_recorded_with_config(account, &request.cwd, revision, request.resume.as_deref(), &request.program, &request.args, request.usage_config.as_deref()))
        .map(|handle| Box::new(handle) as Box<dyn RunningAgent>).map_err(|error| error.to_string())
}
""".strip())


def flatten_legacy(repo):
    function(repo, "src/providers/mod.rs", "pub fn builtin_factory()", """
pub fn builtin_factory() -> ProviderFactory {
    let mut factory = ProviderFactory::new();
    for (id, service) in [("claude", "claude_subscription"), ("codex", "chatgpt_subscription"), ("grok", "grok_subscription")] {
        factory.register(id, move |ctx| {
            let home = match id { "claude" => &ctx.claude_dir, "codex" => &ctx.codex_dir, _ => &ctx.grok_dir };
            let context = crate::resources::local_context(service, home, ctx.clone());
            let key = context.binding.cache_key(service);
            let usage: Box<dyn UsageProvider> = match id {
                "claude" => Box::new(ClaudeProvider::new(ctx)), "codex" => Box::new(CodexProvider::new(ctx)), _ => Box::new(GrokProvider::new(ctx)),
            };
            Box::new(ScopedAblation { usage, key })
        }).unwrap();
    }
    factory
}
struct ScopedAblation { usage: Box<dyn UsageProvider>, key: String }
#[async_trait]
impl UsageProvider for ScopedAblation {
    fn id(&self) -> &'static str { self.usage.id() }
    fn display_name(&self) -> &'static str { self.usage.display_name() }
    fn cache_key(&self) -> String { self.key.clone() }
    fn reset(&self) -> Option<&dyn UsageReset> { self.usage.reset() }
    async fn query(&self) -> UsageSnapshot { self.usage.query().await }
}
""".strip())


def hardcode_sessions(repo):
    function(repo, "src/sessions.rs", "pub fn builtin_registry()", """
pub fn builtin_registry() -> SessionRegistry {
    let mut registry = SessionRegistry::new();
    for (kind, load) in [(AgentKind::CLAUDE, load_claude as fn(&Path) -> Result<Vec<SessionRecord>, String>), (AgentKind::CODEX, load_codex), (AgentKind::GROK, load_grok)] {
        registry.register(SessionSource { kind, load }).unwrap();
    }
    registry
}
""".strip())


def allow_duplicate(repo):
    replace(repo, "src/registry.rs", """        if self.contains(id) {
            return Err(RegistryError::Duplicate(id.into()));
        }""", """        if let Some(index) = self.entries.iter().position(|(key, _)| key == id) {
            self.entries[index].1 = Arc::new(factory);
            return Ok(());
        }""")


def eager_manager_open(repo):
    replace(repo, "src/codex_manager/cli.rs", "    let command = match command {",
            "    if matches!(&command, CodexCommand::Run { .. }) {\n"
            "        let _eager_manager = Manager::open(super::default_root())?;\n"
            "    }\n    let command = match command {")


VARIANTS = {
    "baseline": lambda repo: None,
    "bypass_hub_launch": bypass_hub,
    "eager_manager_open": eager_manager_open,
    "flatten_legacy_bridge": flatten_legacy,
    "hardcode_builtin_sessions": hardcode_sessions,
    "allow_duplicate_registration": allow_duplicate,
    "collapse_cache_scope": lambda repo: replace(repo, "src/resources.rs",
        "self.reader.binding().cache_key(self.reader.service())", "self.id().into()"),
    "force_uuid_session_ids": lambda repo: replace(repo, "src/agents.rs",
        "(self.valid_session_id)(id)", "uuid::Uuid::parse_str(id).is_ok()"),
    "remove_query_deadline": lambda repo: replace(repo, "src/providers/mod.rs",
        "tokio::time::timeout(timeout, provider.query()).await",
        "Ok::<_, tokio::time::error::Elapsed>(provider.query().await)"),
    "ignore_ui_generation": lambda repo: replace(repo, "src/ui/app.rs",
        "if event.generation != self.generation {", "if false && event.generation != self.generation {"),
    "ignore_bark_source_gate": lambda repo: replace(repo, "src/codex_manager/bark.rs",
        "if origin != source::Origin::User {", "if false && origin != source::Origin::User {"),
    "disable_history_incremental": lambda repo: replace(repo, "src/history.rs",
        "let incremental = !compressed", "let incremental = false && !compressed"),
}


def restore(snapshot, repo):
    restore_inputs(snapshot, repo, [(SOURCE / "scripts/ablation/provider_contracts.rs", "stage1_probe.rs")])


def launch_probe(repo, output, label):
    """Count manager opens from synthetic CLI logs, no wall-time performance claim."""
    root = output / "launch-probe" / label
    root.mkdir(parents=True)
    binary = output / "target/debug/usage"
    env = isolated_environment(root, output / "target")
    env["USAGE_LOG"] = "debug"
    def run(args, **kw):
        return subprocess.run([str(binary), "codex", *args], cwd=repo, env=env,
                              text=True, capture_output=True, check=True, timeout=20, **kw)
    run(["add", "probe", "--base-url", "https://example.invalid/v1", "--model", "synthetic", "--key-stdin"], input="synthetic-not-a-real-key\n")
    account = json.loads(run(["list", "--json"]).stdout)[0]["id"]
    before = set((root / "logs").glob("*"))
    script = root / "synthetic-codex"
    script.write_text('#!/bin/sh\nif [ "$1" = "--help" ]; then echo --no-daemon; fi\nexit 0\n')
    script.chmod(0o700)
    run(["run", account, "--cwd", str(root), "--binary", str(script)])
    events = [json.loads(line) for path in set((root / "logs").glob("*")) - before
              for line in path.read_text().splitlines()]
    return {"manager_opens_per_cli_run": sum(e.get("fields", {}).get("event") == "operation_started"
            and e.get("span", {}).get("operation") == "codex.manager_open" for e in events)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--variants", nargs="+", choices=VARIANTS, default=list(VARIANTS))
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--check-anchors", action="store_true")
    args = parser.parse_args()
    output = args.output.resolve()
    snapshot, repo, original = prepare_workspace(SOURCE, output, args.resume)
    results = json.loads((output / "results.json").read_text()) if (output / "results.json").exists() else []
    try:
        for label in args.variants:
            restore(snapshot, repo)
            VARIANTS[label](repo)
            save_diff(snapshot, repo, output, label)
        if args.check_anchors:
            print(json.dumps(dict(anchors_valid=len(args.variants), source_unchanged=manifest(SOURCE) == original)), flush=True)
            return
        clone_target(SOURCE, output)
        for label in args.variants:
            print(f"START {label}", flush=True)
            restore(snapshot, repo)
            VARIANTS[label](repo)
            record = run_suite(repo, output, label)
            record["variant"] = label
            if label in ("baseline", "bypass_hub_launch", "eager_manager_open") and record["exit_code"] == 0:
                record.update(launch_probe(repo, output, record["label"]))
            results.append(record)
            (output / "results.json").write_text(json.dumps(results, indent=2, ensure_ascii=False))
            if label == "baseline" and record["exit_code"]:
                raise RuntimeError("baseline failed: do not interpret mutants")
        restore(snapshot, repo)
        results.append(run_suite(repo, output, "boundary_audit", ("regression_tests",)))
        (output / "results.json").write_text(json.dumps(results, indent=2, ensure_ascii=False))
    finally:
        restore(snapshot, repo)
        write_manifest(SOURCE, output, original)
        print(json.dumps(dict(output=str(output), source_unchanged=True)), flush=True)


if __name__ == "__main__":
    main()
