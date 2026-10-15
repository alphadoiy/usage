#!/usr/bin/env python3
"""Single-factor, whole-suite ablation in disposable source copies.

Requires cached Cargo dependencies and loopback networking for existing HTTP tests.
Never edits production source or reads the user's account/notification configuration.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time


SOURCE = Path(__file__).resolve().parents[1]
INPUTS = ("Cargo.toml", "Cargo.lock", "src", "tests")


def replace(repo, name, before, after):
    path = repo / name
    text = path.read_text()
    if text.count(before) != 1:
        raise RuntimeError(f"mutation anchor changed: {name}: {before[:65]!r}")
    path.write_text(text.replace(before, after, 1))


def function(repo, name, signature, replacement):
    path = repo / name
    text = path.read_text()
    start = text.index(signature)
    end = text.index("\n}", start) + 2
    path.write_text(text[:start] + replacement + text[end:])


def bypass_hub(repo):
    function(repo, "src/hub.rs", "pub fn spawn_managed_codex(", """
pub fn spawn_managed_codex(root: &std::path::Path, account: &str, revision: Option<i64>, request: &SpawnRequest) -> Result<SpawnOutcome, String> {
    crate::codex_manager::Manager::open(root.to_path_buf()).and_then(|mut manager|
        manager.run_recorded(account, &request.cwd, revision, request.resume.as_deref(), &request.program, &request.args))
        .map(|(status, run)| SpawnOutcome { status, run: Some(run) }).map_err(|error| error.to_string())
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


def reuse_open_manager(repo):
    path = repo / "src/codex_manager/cli.rs"
    text = path.read_text()
    start = text.index("                let outcome = crate::hub::spawn_managed_codex(")
    end = text.index(".map_err(Error::message)?;", start) + len(".map_err(Error::message)?;")
    path.write_text(text[:start] + """                let outcome = manager.run_recorded(&profile, &cwd, revision, resume.as_deref(), &binary, &args)
                    .map(|(status, run)| crate::agents::SpawnOutcome { status, run: Some(run) })?;""" + text[end:])


def save_diff(snapshot, repo, output, label):
    import difflib
    changes = []
    for source in sorted((snapshot / "src").rglob("*.rs")):
        relative = source.relative_to(snapshot)
        changes.extend(difflib.unified_diff(source.read_text().splitlines(keepends=True),
            (repo / relative).read_text().splitlines(keepends=True),
            fromfile=str(relative), tofile=str(relative)))
    (output / f"{label}.diff").write_text("".join(changes))


VARIANTS = {
    "baseline": lambda repo: None,
    "bypass_hub_launch": bypass_hub,
    "reuse_open_manager": reuse_open_manager,
    "flatten_legacy_bridge": flatten_legacy,
    "hardcode_builtin_sessions": hardcode_sessions,
    "allow_duplicate_registration": allow_duplicate,
    "collapse_cache_scope": lambda repo: replace(repo, "src/resources.rs",
        "self.reader.binding().cache_key(self.reader.service())", "self.id().into()"),
    "force_uuid_session_ids": lambda repo: replace(repo, "src/agents.rs",
        "(self.valid_session_id)(id)", "uuid::Uuid::parse_str(id).is_ok()"),
    "remove_query_deadline": lambda repo: replace(repo, "src/providers/mod.rs",
        "let result = tokio::time::timeout(timeout, provider.query()).await;",
        "let result: Result<_, tokio::time::error::Elapsed> = Ok(provider.query().await);"),
    "ignore_ui_generation": lambda repo: replace(repo, "src/ui/app.rs",
        "if event.generation != self.generation {", "if false && event.generation != self.generation {"),
    "ignore_bark_source_gate": lambda repo: replace(repo, "src/codex_manager/bark.rs",
        "if origin != source::Origin::User {", "if false && origin != source::Origin::User {"),
    "disable_history_incremental": lambda repo: replace(repo, "src/history.rs",
        "let incremental = entry.offset <= metadata.len()", "let incremental = false && entry.offset <= metadata.len()"),
}


def manifest(root):
    return {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for item in INPUTS for path in ([root / item] if (root / item).is_file()
                else sorted((root / item).rglob("*"))) if path.is_file()}


def restore(snapshot, repo):
    for item in INPUTS:
        src, dst = snapshot / item, repo / item
        if src.is_dir():
            if dst.exists():
                shutil.rmtree(dst)
            shutil.copytree(src, dst)
        else:
            shutil.copy2(src, dst)
    shutil.copy2(SOURCE / "scripts/ablation/provider_contracts.rs", repo / "tests/stage1_probe.rs")


def run_suite(repo, output, label, extra=()):
    env = os.environ.copy()
    env.update(USAGE_CODEX_DIR=str(output / "runtime" / label),
               USAGE_LOG_DIR=str(output / "logs" / label), USAGE_LOG="off", RUST_BACKTRACE="0",
               CARGO_TARGET_DIR=str(output / "target"))
    cmd = ["cargo", "test", "--offline", "--quiet", "--no-fail-fast", *extra, "--", "--test-threads=1"]
    started = time.monotonic()
    try:
        process = subprocess.run(cmd, cwd=repo, env=env, text=True, stdout=subprocess.PIPE,
                                 stderr=subprocess.STDOUT, timeout=180)
        log, code = process.stdout, process.returncode
    except subprocess.TimeoutExpired as error:
        raw = error.stdout or b""
        log = raw.decode(errors="replace") if isinstance(raw, bytes) else raw
        log += "\nABLATION TIMEOUT\n"
        code = 124
    (output / f"{label}.log").write_text(log)
    summaries = re.findall(r"^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; \d+ measured; (\d+) filtered out", log, re.M)
    # A Bark test runs an exact child test and prints its output on failure.
    # Do not count that nested test a second time in whole-suite totals.
    if not extra:
        summaries = [summary for summary in summaries if summary[4] == "0"]
    failed = sorted(set(re.findall(r"^---- (\S+) stdout ----$", log, re.M)))
    compile_error = bool(re.search(r"^error(?:\[|:)", log, re.M)) and not summaries
    record = dict(label=label, exit_code=code, elapsed_seconds=round(time.monotonic() - started, 3),
                  passed=sum(int(s[1]) for s in summaries), failed=sum(int(s[2]) for s in summaries),
                  ignored=sum(int(s[3]) for s in summaries), failures=failed, compile_error=compile_error,
                  log=str(output / f"{label}.log"))
    print(json.dumps(record, ensure_ascii=False), flush=True)
    return record


def launch_probe(repo, output, label):
    """Count manager opens from synthetic CLI logs, no wall-time performance claim."""
    root = output / "launch-probe" / label
    root.mkdir(parents=True)
    binary = output / "target/debug/usage"
    env = os.environ.copy()
    env.update(USAGE_CODEX_DIR=str(root / "manager"), USAGE_LOG_DIR=str(root / "logs"), USAGE_LOG="debug")
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


def unique_tag(label, results):
    existing = {record["label"] for record in results}
    if label not in existing:
        return label
    tag, attempt = label + "_repeat", 2
    while tag in existing:
        tag = f"{label}_repeat{attempt}"
        attempt += 1
    return tag


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--variants", nargs="+", choices=VARIANTS, default=list(VARIANTS))
    parser.add_argument("--resume", action="store_true", help="reuse a completed experiment's unchanged snapshot and target cache")
    args = parser.parse_args()
    output = args.output.resolve()
    if not output.is_relative_to(Path("/private/tmp")) or (output.exists() and not args.resume):
        parser.error("--output must be a new directory under /private/tmp (or use --resume)")
    original = manifest(SOURCE)
    snapshot, repo = output / "snapshot", output / "repo"
    if args.resume:
        saved = json.loads((output / "manifest.json").read_text())
        if saved["inputs"] != original:
            parser.error("workspace differs from the completed snapshot; use a new output directory")
    else:
        output.mkdir(mode=0o700, parents=True)
        snapshot.mkdir()
        repo.mkdir()
        for item in INPUTS:
            src, dst = SOURCE / item, snapshot / item
            shutil.copytree(src, dst) if src.is_dir() else shutil.copy2(src, dst)
    if not args.resume and (SOURCE / "target").exists():
        # APFS clone: retain dependency cache without mutating workspace artifacts.
        clone = subprocess.run(["cp", "-cR", str(SOURCE / "target"), str(output / "target")], capture_output=True)
        if clone.returncode:
            raise RuntimeError("APFS target clone failed; original workspace is unchanged")
    results = json.loads((output / "results.json").read_text()) if args.resume else []
    for label in args.variants:
        print(f"START {label}", flush=True)
        restore(snapshot, repo)
        VARIANTS[label](repo)
        save_diff(snapshot, repo, output, label)
        tag = unique_tag(label, results)
        record = run_suite(repo, output, tag)
        record["variant"] = label
        if record["compile_error"] or record["exit_code"] == 124:
            raise RuntimeError(f"{label}: invalid experiment, inspect log")
        if label in ("baseline", "bypass_hub_launch", "reuse_open_manager") and record["exit_code"] == 0:
            record.update(launch_probe(repo, output, tag))
        results.append(record)
        (output / "results.json").write_text(json.dumps(results, indent=2, ensure_ascii=False))
        if label == "baseline" and record["exit_code"]:
            raise RuntimeError("baseline failed: do not interpret mutants")
    restore(snapshot, repo)
    # The formerly failing probes now live in normal Rust regression modules.
    # Keep the original probe sources as evidence, without injecting a second copy.
    audit = unique_tag("boundary_audit", results)
    results.append(run_suite(repo, output, audit, ("regression_tests",)))
    (output / "results.json").write_text(json.dumps(results, indent=2, ensure_ascii=False))
    unchanged = manifest(SOURCE) == original
    (output / "manifest.json").write_text(json.dumps({"source_unchanged": unchanged, "inputs": original}, indent=2))
    print(json.dumps({"output": str(output), "source_unchanged": unchanged}), flush=True)
    if not unchanged:
        raise RuntimeError("source changed during experiment; snapshot remains available")


if __name__ == "__main__":
    main()
