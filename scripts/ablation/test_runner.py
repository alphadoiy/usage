"""Contract tests for experiment isolation and trustworthy result accounting."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from runner import isolated_environment, manifest, prepare_workspace, restore, run_suite


class RunnerTests(unittest.TestCase):
    def test_environment_isolates_user_homes_and_inherited_notification_context(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            inherited = {
                "HOME": "/actual-user", "CODEX_HOME": "/actual-codex",
                "USAGE_CODEX_BARK_CONTEXT": "private-context", "USAGE_CODEX_RUN_CONTEXT": "parent-run",
                "CLAUDE_CONFIG_DIR": "/actual-claude", "GROK_HOME": "/actual-grok",
                "OPENAI_API_KEY": "private-key", "USAGE_CONFIG": "/actual-config",
                "CARGO_HOME": "/cached-cargo", "RUSTUP_HOME": "/cached-rustup",
            }
            with patch.dict(os.environ, inherited, clear=True):
                env = isolated_environment(root / "runtime", root / "target")
            self.assertEqual(env["HOME"], str(root / "runtime/home"))
            self.assertEqual(env["USAGE_CONFIG"], str(root / "runtime/config.toml"))
            self.assertEqual(env["CARGO_HOME"], "/cached-cargo")
            self.assertEqual(env["RUSTUP_HOME"], "/cached-rustup")
            for key in ("CODEX_HOME", "OPENAI_API_KEY", "USAGE_CODEX_BARK_CONTEXT",
                        "USAGE_CODEX_RUN_CONTEXT", "CLAUDE_CONFIG_DIR", "GROK_HOME"):
                self.assertNotIn(key, env)

    def test_nested_test_output_is_not_counted_twice_and_repeats_preserve_logs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = ("test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 249 filtered out\n"
                   "test result: FAILED. 248 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out\n"
                   "error: test failed, to rerun pass --lib\n")
            process = subprocess.CompletedProcess([], 101, log)
            with patch("runner.subprocess.run", return_value=process):
                first = run_suite(root, root, "probe")
                second = run_suite(root, root, "probe")
            self.assertEqual((first["passed"], first["failed"]), (248, 2))
            self.assertFalse(first["compile_error"])
            self.assertNotEqual(first["log"], second["log"])
            self.assertEqual(Path(first["log"]).read_text(), log)

    def test_invalid_build_and_timeout_are_not_reported_as_valid_ablations(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch("runner.subprocess.run", return_value=subprocess.CompletedProcess(
                [], 101, "error[E0425]: cannot find value\n")):
                with self.assertRaisesRegex(RuntimeError, "invalid experiment"):
                    run_suite(root, root, "build_failure")
            with patch("runner.subprocess.run", side_effect=subprocess.TimeoutExpired(
                "cargo", 240, output=b"partial output")):
                with self.assertRaisesRegex(RuntimeError, "invalid experiment"):
                    run_suite(root, root, "timeout")
            self.assertIn("ABLATION TIMEOUT", (root / "timeout.log").read_text())

    def test_restore_and_resume_reject_changed_source_without_overwriting_evidence(self):
        with tempfile.TemporaryDirectory(dir="/private/tmp") as directory:
            root = Path(directory)
            source, output = root / "source", root / "output"
            (source / "src").mkdir(parents=True)
            (source / "tests").mkdir()
            for name in ("Cargo.toml", "Cargo.lock", "src/lib.rs"):
                (source / name).write_text("original")
            snapshot, repo, before = prepare_workspace(source, output, False)
            restore(snapshot, repo)
            (repo / "src/lib.rs").write_text("mutant")
            (repo / "src/extra.rs").touch()
            restore(snapshot, repo)
            self.assertEqual(manifest(repo), before)
            evidence = output / "baseline.log"
            evidence.write_text("retained")
            (source / "src/lib.rs").write_text("changed source")
            with self.assertRaisesRegex(RuntimeError, "inputs differ"):
                prepare_workspace(source, output, True)
            self.assertEqual(evidence.read_text(), "retained")
            self.assertEqual(json.loads((output / "manifest.json").read_text())["inputs"], before)


if __name__ == "__main__":
    unittest.main()
