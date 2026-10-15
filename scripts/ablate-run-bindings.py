#!/usr/bin/env python3
"""Single-factor ablations of launch identity, in disposable source copies only."""
import argparse
import difflib
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
        raise RuntimeError(f"anchor changed ({text.count(before)}): {name}: {before[:70]!r}")
    path.write_text(text.replace(before, after, 1))


VARIANTS = {
    "baseline": [],
    "remove_endpoint_match": [("src/runs.rs", """if endpoint(&inference.base_url).is_none()
                    || captured
                        .context
                        .identity
                        .endpoint
                        .as_deref()
                        .and_then(endpoint)
                        != endpoint(&inference.base_url)""", "if false")],
    "remove_inference_key_match": [("src/runs.rs", "if key == expected", "if true")],
    "remove_token_key_match": [("src/runs.rs", """&& (scope != Some(crate::resources::ResourceScope::Token)
                                    || query == key)""", "")],
    "remove_revision_check": [("src/runs.rs", "|| config.binding() != link.binding", "")],
    "remove_inference_from_revision": [("src/resources/connections.rs", "            self.inference,", "")],
    "remove_profile_recheck": [("src/runs.rs", "|| !config.profile_ids.iter().any(|id| id == profile_id)", "")],
    "remove_query_credential_proof": [("src/runs.rs", '|| proof(run_id, "query", &query_key) != expected.query', "")],
    "remove_inference_credential_proof": [("src/runs.rs", """if proof(run_id, "inference", &inference_key) != expected.inference
                || proof(run_id, "query", &query_key) != expected.query""", 'if proof(run_id, "query", &query_key) != expected.query')],
    "remove_credential_freezing": [("src/runs.rs", """let credentials = self.credentials.clone();
        std::sync::Arc::new(move |reference| {
            credentials
                .get(reference)
                .cloned()
                .ok_or_else(|| "运行未绑定此凭据".into())
        })""", """std::sync::Arc::new(move |reference| {
            std::env::var(reference.strip_prefix("env:").ok_or("invalid ref")?).map_err(|_| "missing env".into())
        })""")],
    "remove_child_env_pinning": [("src/codex_manager/mod.rs", "                    command.env(env, key);", "                    let _ = (env, key);")],
    "reverse_model_precedence": [("src/codex_manager/effective.rs", """let selected_model = model
            .or_else(|| {
                document
                    .get("model")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string)
            })""", """let selected_model = document.get("model").and_then(toml::Value::as_str).map(str::to_string).or(model)""")],
    "merge_assigned_tables": [("src/codex_manager/effective.rs", """target
        .as_table_mut()
        .ok_or("配置覆盖路径不是表")?
        .insert(path[path.len() - 1].clone(), value);""", """let table = target.as_table_mut().ok_or("配置覆盖路径不是表")?;
    let key = path[path.len()-1].clone();
    if let (Some(existing), Some(overlay)) = (table.get_mut(&key).and_then(toml::Value::as_table_mut), value.as_table()) {
        existing.extend(overlay.clone());
    } else { table.insert(key, value); }""")],
    "ignore_unknown_flags": [("src/codex_manager/effective.rs", 'return Err("存在尚未识别的启动选项");', 'let _ = flag;')],
    "ignore_project_layer": [("src/codex_manager/effective.rs", 'if directory.join(".codex/config.toml").exists() {', 'if false && directory.join(".codex/config.toml").exists() {')],
    "ignore_resume_gate": [("src/codex_manager/effective.rs", "        if resume {", "        if false && resume {")],
    "ignore_resource_factory_validation": [("src/runs.rs", "} else if scope.is_none() {", "} else if false && scope.is_none() {")],
    "remove_run_transaction": [("src/codex_manager/mod.rs", """let recorded = (|| -> Result<()> {
                let tx = self.conn.transaction()?;""", """let recorded = (|| -> Result<()> {
                let tx = &self.conn;"""), ("src/codex_manager/mod.rs", """                tx.commit()?;
                Ok(())
            })();""", """                Ok(())
            })();""")],
    "remove_redundant_service_guard": [("src/runs.rs", """if config.service != link.service
                || config.binding() != link.binding""", "if config.binding() != link.binding")],
    "remove_select_identity_guard": [("src/runs.rs", "if self.context.identity.state != IdentityState::Resolved {", "if false && self.context.identity.state != IdentityState::Resolved {")],
    "duplicate_context_read": [("src/codex_manager/mod.rs", """        let (profile_id, text): (String, Option<String>) = self
            .conn
            .query_row(
                "SELECT r.profile_id,c.context_json FROM runs r
                 LEFT JOIN run_contexts c ON c.run_id=r.id WHERE r.id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| Error::message("找不到运行记录"))?;""", """        let run = self.run_record(id)?.ok_or_else(|| Error::message("找不到运行记录"))?;
        let text: Option<String> = self.conn.query_row(
            "SELECT context_json FROM run_contexts WHERE run_id=?1", [id], |row| row.get(0),
        ).optional()?;"""), ("src/codex_manager/mod.rs", '.select(id, &profile_id, configs, ids, &|name| {', '.select(id, &run.profile_id, configs, ids, &|name| {')],
    "eager_resource_service": [("src/codex_manager/mod.rs", "let resources = std::cell::OnceCell::new();", "let resources = crate::resources::connections::ResourceService::from_environment(crate::resources::builtin_registry());"), ("src/codex_manager/mod.rs", "&|config| resources.get_or_init(|| crate::resources::connections::ResourceService::from_environment(crate::resources::builtin_registry())).prepare(config).ok().map(|reader| reader.scope()),", "&|config| resources.prepare(config).ok().map(|reader| reader.scope()),")],
    "ignore_nested_exec_subcommands": [("src/codex_manager/effective.rs", "CommandPosition::Exec\n                    }", "CommandPosition::Prompt\n                    }")],
}


def manifest(root):
    return {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for item in INPUTS for path in ([root / item] if (root / item).is_file() else sorted((root / item).rglob("*")))
            if path.is_file()}


def restore(snapshot, repo):
    for item in INPUTS:
        src, dst = snapshot / item, repo / item
        if src.is_dir():
            if dst.exists():
                shutil.rmtree(dst)
            shutil.copytree(src, dst)
        else:
            shutil.copy2(src, dst)


def run(repo, output, label, audit=False):
    # Preserve pilot/retry evidence rather than replacing earlier logs.
    base, attempt = label, 1
    while (output/f"{label}.log").exists():
        label=f"{base}_repeat{attempt}"; attempt+=1
    runtime = output / "runtime" / label
    runtime.mkdir(parents=True, exist_ok=True)
    (runtime / "config.toml").write_text("")
    env = os.environ.copy()
    original_home = Path.home()
    env.update(USAGE_CODEX_DIR=str(runtime / "manager"), USAGE_CONFIG=str(runtime / "config.toml"),
               USAGE_LOG_DIR=str(runtime / "logs"), USAGE_LOG="off", RUST_BACKTRACE="0",
               HOME=str(runtime / "home"), XDG_CONFIG_HOME=str(runtime / "config"), XDG_DATA_HOME=str(runtime / "data"),
               CARGO_HOME=env.get("CARGO_HOME", str(original_home / ".cargo")),
               RUSTUP_HOME=env.get("RUSTUP_HOME", str(original_home / ".rustup")),
               CARGO_TARGET_DIR=str(output / "target"))
    # No real notification or resource configuration is read by the experiment.
    for name in list(env):
        if name.startswith(("RELAY_", "FIXTURE_")) or name in ("OPENAI_API_KEY", "CODEX_API_KEY", "USAGE_CODEX_API_KEY"):
            env.pop(name)
    command = ["cargo", "test", "--offline", "--quiet", "--no-fail-fast"]
    if audit:
        command += ["run_binding_"]
    command += ["--", "--test-threads=1"]
    started = time.monotonic()
    try:
        process = subprocess.run(command, cwd=repo, env=env, text=True, stdout=subprocess.PIPE,
                                 stderr=subprocess.STDOUT, timeout=240)
        log, code = process.stdout, process.returncode
    except subprocess.TimeoutExpired as error:
        log = error.stdout or b""
        log = log.decode(errors="replace") if isinstance(log, bytes) else log
        log += "\nABLATION TIMEOUT\n"
        code = 124
    (output / f"{label}.log").write_text(log)
    summaries = re.findall(r"^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; \d+ measured; (\d+) filtered out", log, re.M)
    if not audit:
        summaries = [s for s in summaries if s[4] == "0"]
    errors = bool(re.search(r"^error(?:\[|:)",log,re.M)) and not summaries
    record = dict(label=label, exit_code=code, passed=sum(int(s[1]) for s in summaries),
                  failed=sum(int(s[2]) for s in summaries), failures=sorted(set(re.findall(r"^---- (\S+) stdout ----$",log,re.M))),
                  compile_error=errors, elapsed_seconds=round(time.monotonic()-started,3), log=str(output/f"{label}.log"))
    print(json.dumps(record,ensure_ascii=False),flush=True)
    if errors or code == 124:
        raise RuntimeError(f"invalid experiment {label}; inspect log")
    return record


def diff(snapshot, repo, output, label):
    changes = []
    for path in sorted((snapshot / "src").rglob("*.rs")):
        relative = path.relative_to(snapshot)
        changes += difflib.unified_diff(path.read_text().splitlines(keepends=True),(repo/relative).read_text().splitlines(keepends=True),fromfile=str(relative),tofile=str(relative))
    (output/f"{label}.diff").write_text("".join(changes))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output",type=Path,required=True)
    parser.add_argument("--variants",nargs="+",choices=VARIANTS,default=list(VARIANTS))
    parser.add_argument("--resume",action="store_true")
    parser.add_argument("--audit",action="store_true",help="also run the focused run_binding_ production regressions")
    parser.add_argument("--check-anchors",action="store_true")
    args=parser.parse_args()
    output=args.output.resolve()
    if not output.is_relative_to(Path("/private/tmp")) or (output.exists() and not args.resume):
        parser.error("output must be a new /private/tmp directory, or use --resume")
    original=manifest(SOURCE)
    snapshot,repo=output/"snapshot",output/"repo"
    if args.resume:
        if json.loads((output/"manifest.json").read_text())["inputs"]!=original:
            parser.error("production inputs differ from snapshot")
    else:
        output.mkdir(parents=True,mode=0o700); snapshot.mkdir(); repo.mkdir()
        restore(SOURCE,snapshot)
        (output/"manifest.json").write_text(json.dumps(dict(inputs=original,source_unchanged=True),indent=2))
    # Verify every mutation before starting any expensive build.
    for label in args.variants:
        restore(snapshot,repo)
        for name,before,after in VARIANTS[label]: replace(repo,name,before,after)
        diff(snapshot,repo,output,label)
    if args.check_anchors:
        print(json.dumps(dict(anchors_valid=len(args.variants),source_unchanged=manifest(SOURCE)==original)),flush=True)
        return
    if not (output/"target").exists() and (SOURCE/"target").exists():
        subprocess.run(["cp","-cR",str(SOURCE/"target"),str(output/"target")],check=True)
    results=json.loads((output/"results.json").read_text()) if (output/"results.json").exists() else []
    try:
        for label in args.variants:
            restore(snapshot,repo)
            for name,before,after in VARIANTS[label]: replace(repo,name,before,after)
            print(f"START {label}",flush=True)
            record=run(repo,output,label)
            record["variant"]=label
            results.append(record)
            (output/"results.json").write_text(json.dumps(results,indent=2,ensure_ascii=False))
            if label=="baseline" and record["exit_code"]:
                raise RuntimeError("baseline failed; do not interpret mutants")
            if args.audit:
                record=run(repo,output,label+"_audit",True)
                record["variant"]=label; record["kind"]="audit"
                results.append(record)
                (output/"results.json").write_text(json.dumps(results,indent=2,ensure_ascii=False))
    finally:
        unchanged=manifest(SOURCE)==original
        (output/"manifest.json").write_text(json.dumps(dict(inputs=original,source_unchanged=unchanged),indent=2))
        print(json.dumps(dict(output=str(output),source_unchanged=unchanged)),flush=True)


if __name__=="__main__": main()
