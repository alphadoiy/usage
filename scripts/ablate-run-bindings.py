#!/usr/bin/env python3
"""Single-factor ablations of launch identity, in disposable source copies only."""
import argparse
import json
from pathlib import Path

from ablation.runner import (clone_target, manifest, prepare_workspace, replace, restore,
                             run_suite, save_diff, write_manifest)

SOURCE = Path(__file__).resolve().parents[1]


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
    "remove_inference_from_revision": [("src/resources/connections.rs", "            inference,", "")],
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
    "remove_child_env_pinning": [("src/codex_manager/launch.rs", "            prepared.command.env(env, key);", "            let _ = (env, key);")],
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
    "remove_run_transaction": [("src/codex_manager/launch.rs", """            let tx = self.conn.transaction()?;
            tx.execute("INSERT INTO runs""", """            let tx = &self.conn;
            tx.execute("INSERT INTO runs"""), ("src/codex_manager/launch.rs", """            runtime::record_started(&tx, &run_id)?;
            tx.commit()?;""", """            runtime::record_started(tx, &run_id)?;""")],
    "remove_redundant_service_guard": [("src/runs.rs", """if config.service != link.service
                || config.binding() != link.binding""", "if config.binding() != link.binding")],
    "remove_select_identity_guard": [("src/runs.rs", "if self.context.identity.state != IdentityState::Resolved {", "if false && self.context.identity.state != IdentityState::Resolved {")],
    "duplicate_context_read": [("src/codex_manager/run_store.rs", """        let (profile_id, text): (String, Option<String>) = self
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
        ).optional()?;"""), ("src/codex_manager/run_store.rs", '.select(id, &profile_id, configs, ids, credentials)', '.select(id, &run.profile_id, configs, ids, credentials)')],
    "eager_resource_service": [
        ("src/codex_manager/launch.rs", "let resources = std::cell::OnceCell::new();", "let resources = crate::resources::connections::ResourceService::from_environment(crate::hub::shared().resources.clone());"),
        ("src/codex_manager/launch.rs", """            &|config| {
                resources
                    .get_or_init(|| {
                        crate::resources::connections::ResourceService::from_environment(
                            crate::hub::shared().resources.clone(),
                        )
                    })
                    .prepare(config)
                    .ok()
                    .map(|reader| reader.scope())
            },""", "&|config| resources.prepare(config).ok().map(|reader| reader.scope()),"),
    ],
    "ignore_nested_exec_subcommands": [("src/codex_manager/effective.rs", "CommandPosition::Exec\n                    }", "CommandPosition::Prompt\n                    }")],
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output",type=Path,required=True)
    parser.add_argument("--variants",nargs="+",choices=VARIANTS,default=list(VARIANTS))
    parser.add_argument("--resume",action="store_true")
    parser.add_argument("--audit",action="store_true",help="also run the focused run_binding_ production regressions")
    parser.add_argument("--check-anchors",action="store_true")
    args=parser.parse_args()
    output=args.output.resolve()
    snapshot,repo,original=prepare_workspace(SOURCE,output,args.resume)
    try:
        # Verify every mutation before starting any expensive build.
        for label in args.variants:
            restore(snapshot,repo)
            for name,before,after in VARIANTS[label]: replace(repo,name,before,after)
            save_diff(snapshot,repo,output,label)
        if args.check_anchors:
            print(json.dumps(dict(anchors_valid=len(args.variants),source_unchanged=manifest(SOURCE)==original)),flush=True)
            return
        clone_target(SOURCE,output)
        results=json.loads((output/"results.json").read_text()) if (output/"results.json").exists() else []
        for label in args.variants:
            restore(snapshot,repo)
            for name,before,after in VARIANTS[label]: replace(repo,name,before,after)
            print(f"START {label}",flush=True)
            record=run_suite(repo,output,label)
            record["variant"]=label
            results.append(record)
            (output/"results.json").write_text(json.dumps(results,indent=2,ensure_ascii=False))
            if label=="baseline" and record["exit_code"]:
                raise RuntimeError("baseline failed; do not interpret mutants")
            if args.audit:
                record=run_suite(repo,output,label+"_audit",("run_binding_",))
                record["variant"]=label; record["kind"]="audit"
                results.append(record)
                (output/"results.json").write_text(json.dumps(results,indent=2,ensure_ascii=False))
    finally:
        restore(snapshot,repo)
        write_manifest(SOURCE,output,original)
        print(json.dumps(dict(output=str(output),source_unchanged=True)),flush=True)


if __name__=="__main__": main()
