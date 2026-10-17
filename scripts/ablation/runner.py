"""Shared source isolation, execution and evidence handling for ablation scripts."""
import difflib
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time

INPUTS = ("Cargo.toml", "Cargo.lock", "src", "tests")


def replace(repo, name, before, after):
    path = repo / name
    text = path.read_text()
    if text.count(before) != 1:
        raise RuntimeError(f"anchor changed ({text.count(before)}): {name}: {before[:70]!r}")
    path.write_text(text.replace(before, after, 1))


def manifest(root):
    return {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for item in INPUTS
            for path in ([root / item] if (root / item).is_file() else sorted((root / item).rglob("*")))
            if path.is_file()}


def restore(snapshot, repo, extra_tests=()):
    for item in INPUTS:
        src, dst = snapshot / item, repo / item
        if src.is_dir():
            if dst.exists():
                shutil.rmtree(dst)
            shutil.copytree(src, dst)
        else:
            shutil.copy2(src, dst)
    for source, name in extra_tests:
        shutil.copy2(source, repo / "tests" / name)


def prepare_workspace(source, output, resume):
    if not output.is_relative_to(Path("/private/tmp")) or (output.exists() and not resume):
        raise RuntimeError("output must be a new /private/tmp directory, or use --resume")
    original = manifest(source)
    snapshot, repo = output / "snapshot", output / "repo"
    if resume:
        if json.loads((output / "manifest.json").read_text())["inputs"] != original:
            raise RuntimeError("production inputs differ from snapshot")
    else:
        output.mkdir(parents=True, mode=0o700)
        snapshot.mkdir()
        repo.mkdir()
        restore(source, snapshot)
        write_manifest(source, output, original)
    return snapshot, repo, original


def write_manifest(source, output, original):
    unchanged = manifest(source) == original
    (output / "manifest.json").write_text(json.dumps(
        dict(inputs=original, source_unchanged=unchanged), indent=2))
    if not unchanged:
        raise RuntimeError("source changed during experiment; snapshot remains available")


def clone_target(source, output):
    if not (output / "target").exists() and (source / "target").exists():
        subprocess.run(["cp", "-cR", str(source / "target"), str(output / "target")], check=True)


def save_diff(snapshot, repo, output, label):
    changes = []
    for path in sorted((snapshot / "src").rglob("*.rs")):
        relative = path.relative_to(snapshot)
        other = repo / relative
        changes.extend(difflib.unified_diff(
            path.read_text().splitlines(keepends=True),
            other.read_text().splitlines(keepends=True) if other.exists() else [],
            fromfile=str(relative), tofile=str(relative)))
    (output / f"{label}.diff").write_text("".join(changes))


def isolated_environment(runtime, target):
    runtime.mkdir(parents=True, exist_ok=True)
    for name in ("home", "config", "data", "state", "cache"):
        (runtime / name).mkdir(exist_ok=True)
    (runtime / "config.toml").write_text("")
    env = os.environ.copy()
    original_home = Path.home()
    for name in list(env):
        if name.startswith(("RELAY_", "FIXTURE_", "OPENAI_", "CODEX_", "USAGE_CODEX_", "BARK_")) or name in (
            "CLAUDE_CONFIG_DIR", "GROK_HOME", "USAGE_LOCAL_BARK_TEST", "USAGE_MANAGER_TEST"
        ):
            env.pop(name)
    env.update(
        HOME=str(runtime / "home"), XDG_CONFIG_HOME=str(runtime / "config"),
        XDG_DATA_HOME=str(runtime / "data"), XDG_STATE_HOME=str(runtime / "state"),
        XDG_CACHE_HOME=str(runtime / "cache"), USAGE_CODEX_DIR=str(runtime / "manager"),
        USAGE_CONFIG=str(runtime / "config.toml"), USAGE_LOG_DIR=str(runtime / "logs"),
        USAGE_LOG="off", RUST_BACKTRACE="0", CARGO_TARGET_DIR=str(target),
        CARGO_HOME=env.get("CARGO_HOME", str(original_home / ".cargo")),
        RUSTUP_HOME=env.get("RUSTUP_HOME", str(original_home / ".rustup")))
    return env


def run_suite(repo, output, label, extra=()):
    # A resumed experiment preserves earlier logs and labels.
    base, attempt = label, 1
    while (output / f"{label}.log").exists():
        label = f"{base}_repeat{attempt}"
        attempt += 1
    env = isolated_environment(output / "runtime" / label, output / "target")
    command = ["cargo", "test", "--offline", "--quiet", "--no-fail-fast", *extra,
               "--", "--test-threads=1"]
    started = time.monotonic()
    try:
        process = subprocess.run(command, cwd=repo, env=env, text=True,
                                 stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=240)
        log, code = process.stdout, process.returncode
    except subprocess.TimeoutExpired as error:
        log = error.stdout or b""
        log = log.decode(errors="replace") if isinstance(log, bytes) else log
        log += "\nABLATION TIMEOUT\n"
        code = 124
    path = output / f"{label}.log"
    path.write_text(log)
    summaries = re.findall(
        r"^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; \d+ measured; (\d+) filtered out",
        log, re.M)
    if not extra:
        # Exact child tests printed on a parent failure must not be counted twice.
        summaries = [summary for summary in summaries if summary[4] == "0"]
    compile_error = bool(re.search(r"^error(?:\[|:)", log, re.M)) and not summaries
    record = dict(label=label, exit_code=code,
                  passed=sum(int(s[1]) for s in summaries), failed=sum(int(s[2]) for s in summaries),
                  ignored=sum(int(s[3]) for s in summaries),
                  failures=sorted(set(re.findall(r"^---- (\S+) stdout ----$", log, re.M))),
                  compile_error=compile_error, elapsed_seconds=round(time.monotonic() - started, 3),
                  log=str(path))
    print(json.dumps(record, ensure_ascii=False), flush=True)
    if compile_error or code == 124:
        raise RuntimeError(f"invalid experiment {label}; inspect log")
    return record
