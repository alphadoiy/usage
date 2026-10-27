#!/usr/bin/env python3
"""Single-factor checks for model discovery, usage, cleanup and keyboard changes.

All mutations run in disposable copies. No live provider requests or user data edits.
Passing mutants are evidence about the selected tests, not proof of equivalence.
"""
import argparse
import json
from pathlib import Path

from ablation.runner import (clone_target, manifest, prepare_workspace, replace,
                             restore, run_suite, save_diff, write_manifest)

SOURCE = Path(__file__).resolve().parents[1]

# label: (source path, exact anchor, replacement, test filter)
VARIANTS = {
    "remove_model_path_fallback": (
        "src/provider_catalog/api_models.rs", "for url in &self.urls {",
        "for url in self.urls.iter().take(1) {", "provider_catalog::api_models::tests"),
    "remove_dual_auth_retry": (
        "src/provider_catalog/api_models.rs",
        "matches!(auth, Auth::Both) && both_auth_refused(&response)",
        "false && matches!(auth, Auth::Both) && both_auth_refused(&response)",
        "provider_catalog::api_models::tests"),
    "remove_sub2api_windows": (
        "src/resources/provider_usage.rs", "let mut metrics = sub2api_windows(&value, unit)?;",
        "let mut metrics = Vec::new();", "resources::provider_usage::tests"),
    "ignore_conversion_identity": (
        "src/provider_catalog/usage.rs",
        "hash.update(reader.binding().cache_key(reader.service()));",
        "hash.update(reader.service());", "provider_catalog::usage::tests"),
    "ignore_stale_config": (
        "src/config.rs", "if current != original {", "if false && current != original {",
        "config::tests"),
    "remove_recent_write_guard": (
        "src/session_cleanup.rs", "if let Some(now) = now\n        && now",
        "if let Some(now) = now\n        && false && now", "session_cleanup::tests"),
    "remove_tombstone_filter": (
        "src/sessions.rs", "&& !hidden.contains(&record.id)",
        "&& (true || !hidden.contains(&record.id))", "session_cleanup::tests"),
    "ignore_search_input_ownership": (
        "src/ui/app/shortcuts.rs", ".is_some_and(|b| b.searching || b.launching)",
        ".is_some_and(|b| b.launching)", "ui::app::shortcuts::tests"),
    "remove_escape_menu_action": (
        "src/ui/app/shortcuts.rs",
        '        actions.push(Shortcut::new(KeyCode::Esc, "Esc", "返回 / 收起当前层级"));\n',
        "", "ui::app::shortcuts::tests"),
    "restore_project_session_clones": (
        "src/ui/session_browser.rs",
        """let keys: Vec<_> = self
            .projects
            .get(self.project_selected)
            .into_iter()
            .flat_map(|project| &project.sessions)
            .map(|&index| self.sessions[index].key())
            .collect();""",
        "let keys: Vec<_> = self.project_sessions().iter().map(Session::key).collect();",
        "ui::session_browser::tests"),
    "ignore_config_symlink_target": (
        "src/config.rs", "let resolved = std::fs::canonicalize(path)",
        "let resolved = std::path::absolute(path)", "config::tests"),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--variants", nargs="+", choices=["baseline", *VARIANTS],
                        default=["baseline", *VARIANTS])
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--check-anchors", action="store_true")
    args = parser.parse_args()
    output = args.output.resolve()
    snapshot, repo, original = prepare_workspace(SOURCE, output, args.resume)
    results_path = output / "results.json"
    results = json.loads(results_path.read_text()) if results_path.exists() else []
    try:
        for label in args.variants:
            restore(snapshot, repo)
            if label != "baseline":
                name, before, after, _ = VARIANTS[label]
                replace(repo, name, before, after)
            save_diff(snapshot, repo, output, label)
        if args.check_anchors:
            print(json.dumps(dict(anchors_valid=len(args.variants),
                                  source_unchanged=manifest(SOURCE) == original)))
            return
        clone_target(SOURCE, output)
        # Always establish a passing full baseline before interpreting any mutation.
        if not any(r.get("variant") == "baseline" and r["exit_code"] == 0 for r in results):
            restore(snapshot, repo)
            record = run_suite(repo, output, "baseline")
            record["variant"] = "baseline"
            results.append(record)
            results_path.write_text(json.dumps(results, indent=2, ensure_ascii=False))
            if record["exit_code"]:
                raise RuntimeError("baseline failed; mutants cannot be interpreted")
        for label in args.variants:
            if label == "baseline":
                continue
            restore(snapshot, repo)
            name, before, after, test_filter = VARIANTS[label]
            replace(repo, name, before, after)
            print(f"START {label}", flush=True)
            record = run_suite(repo, output, label, ("--lib", test_filter))
            record.update(variant=label, test_filter=test_filter)
            if not record["passed"] and not record["failed"]:
                raise RuntimeError(f"no selected tests: {label}")
            results.append(record)
            results_path.write_text(json.dumps(results, indent=2, ensure_ascii=False))
    finally:
        restore(snapshot, repo)
        write_manifest(SOURCE, output, original)


if __name__ == "__main__":
    main()
