"""Exercise saved edits, navigation, resume identity and missing configs in a real terminal.

python3 tests/support/ui_workflows.py target/debug/usage
Uses synthetic accounts, isolated stores and a fake Codex; no model requests.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tomllib
import uuid


def plain(data):
    text = re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]", "", data.decode("utf-8", "replace"))
    return "".join(text.split())


class Terminal:
    def __init__(self, binary, args, env, cwd):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
            os.execve(binary, [binary, *args], env)
        self.data = bytearray()
        self.reaped = False

    def pump(self, duration=0.2):
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            if not select.select([self.fd], [], [], 0.05)[0]:
                continue
            try:
                data = os.read(self.fd, 131072)
            except OSError:
                break
            self.data.extend(data)
            if b"\x1b[6n" in data:
                os.write(self.fd, b"\x1b[1;1R")

    def send(self, data):
        self.data.clear()
        os.write(self.fd, data)
        self.pump()

    def until(self, text, timeout=8):
        deadline = time.monotonic() + timeout
        while text not in plain(self.data):
            assert time.monotonic() < deadline, f"Missing {text}: {plain(self.data)}"
            self.pump()

    def close(self):
        if self.reaped:
            return
        self.send(b"\x03")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            done, status = os.waitpid(self.pid, os.WNOHANG)
            if done:
                self.reaped = True
                os.close(self.fd)
                assert os.waitstatus_to_exitcode(status) == 0
                return
            self.pump()
        try:
            os.kill(self.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            done, _ = os.waitpid(self.pid, os.WNOHANG)
            if done:
                self.reaped = True
                break
            time.sleep(0.02)
        os.close(self.fd)
        raise AssertionError("Terminal did not exit cleanly")


def main():
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="usage-ui-workflows-") as temporary:
        root = Path(temporary).resolve()
        native, independent, project = (root / name for name in ["native", "independent", "project"])
        for path in [native, independent, project, root / "bin", root / "claude", root / "grok"]:
            path.mkdir()
        env = dict(os.environ)
        for key in ["CODEX_SQLITE_HOME", "USAGE_CONFIG", "OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_AUTH_TOKEN", "OPENAI_BASE_URL", "USAGE_CODEX_API_KEY", "USAGE_CODEX_RUN_CONTEXT", "USAGE_CODEX_BARK_CONTEXT", "OPENAI_ORG_ID", "OPENAI_ORGANIZATION", "OPENAI_PROJECT_ID", "HC_TERMINAL_ID", "HC_TERMINAL_SOCKET", "REX_SESSION", "REX_BIN_DIR", "REX_SERVER", "GHOSTTY_RESOURCES_DIR"]:
            env.pop(key, None)
        env.update(TERM="xterm-256color", TERM_PROGRAM="usage-test", LANG="en_US.UTF-8", USAGE_LOG="off", USAGE_GATEWAY="off",
                   USAGE_CODEX_DIR=str(root / "accounts"), USAGE_CODEX_BASE_HOME=str(native), CODEX_HOME=str(native),
                   XDG_DATA_HOME=str(root / "data"), XDG_CONFIG_HOME=str(root / "config"),
                   USAGE_PLUGIN_DIR=str(root / "plugins"), UI_WORKFLOW_KEY="synthetic-ui-key",
                   UI_WORKFLOW_RESULT=str(root / "launched.json"), PATH=str(root / "bin") + os.pathsep + env["PATH"],
                   HTTP_PROXY="http://127.0.0.1:9", HTTPS_PROXY="http://127.0.0.1:9", ALL_PROXY="http://127.0.0.1:9", NO_PROXY="")
        fake = root / "bin/codex"
        fake.write_text(f"#!{sys.executable}\n" + '''import json,os,pathlib,sys
if sys.argv[1:] == ['--help']:
    print('--no-daemon'); sys.exit(0)
home=pathlib.Path(os.environ['CODEX_HOME'])
session=sys.argv[sys.argv.index('resume')+1] if 'resume' in sys.argv else None
if session:
    assert any(session in p.name for p in (home/'sessions').rglob('*.jsonl')), 'resumed Home cannot see conversation'
pathlib.Path(os.environ['UI_WORKFLOW_RESULT']).write_text(json.dumps({'session':session,'home':str(home)}))
''')
        fake.chmod(0o700)

        def cli(*args):
            result = subprocess.run([binary, *args], env=env, cwd=project, capture_output=True, text=True, timeout=20)
            assert result.returncode == 0, result.stderr
            return result.stdout

        cli("codex", "add", "Fixture", "--base-url", "https://fixture.invalid/v1", "--model", "fixture-model", "--effort", "high", "--key-env", "UI_WORKFLOW_KEY")
        account = json.loads(cli("codex", "list", "--json"))[0]["id"]
        session = str(uuid.uuid4())
        log = independent / "sessions" / ("rollout-" + session + ".jsonl")
        log.parent.mkdir()
        log.write_text("\n".join(json.dumps(v) for v in [
            {"type": "session_meta", "payload": {"id": session, "cwd": str(project), "source": "cli"}},
            {"type": "event_msg", "payload": {"type": "user_message", "message": "Independent fixture"}},
        ]) + "\n")
        config = root / "usage.toml"

        def configure(home):
            config.write_text(f'''providers = ["grok"]
refresh_seconds = 0
[sessions]
resume = "current"
[paths]
claude = {json.dumps(str(root / "claude"))}
codex = {json.dumps(str(home))}
grok = {json.dumps(str(root / "grok"))}
[[resources]]
id = "custom-config-only"
service = "new_api_token"
base_url = "https://fixture.invalid"
credential_env = "UI_WORKFLOW_KEY"
profile_ids = [{json.dumps(account)}]
''')

        configure(independent)
        terminal = Terminal(binary, ["--config", str(config), "codex"], env, project)
        try:
            terminal.until("[F4Provider]")
            terminal.send(b"\x1b[B")
            terminal.send(b"e")
            terminal.until("APIKey")
            terminal.send(b"\t\t\t")
            terminal.send(b"\x15")
            terminal.send(b"\x13")
            terminal.until("已保存")
            exported = root / "export.toml"
            cli("codex", "export", "Fixture", "--output", str(exported))
            assert "model_reasoning_effort" not in tomllib.loads(exported.read_text())

            terminal.send(b"\x1bOQ")
            terminal.until("[F2历史]")
            terminal.send(b"\x1bOS")
            terminal.until("[F4Provider]")
            terminal.send(b"\x1bOR")
            terminal.until("Independentfixture")
            terminal.send(b"\r")
            terminal.send(b"P")
            terminal.until("独立目录")
            terminal.send(b"\x1b[B")
            terminal.send(b"\r")
            assert not (root / "launched.json").exists()
            terminal.send(b"\x1b")
            terminal.send(b"\x1bOP")
            terminal.until("[F1首页]")
            terminal.send(b"\x1bOS")
            terminal.until("[F4Provider]")
            terminal.send(b"\x1bOP")
            terminal.until("[F1首页]")
        finally:
            terminal.close()

        (native / "sessions").mkdir(exist_ok=True)
        (native / "sessions" / log.name).write_bytes(log.read_bytes())
        configure(native)
        terminal = Terminal(binary, ["--config", str(config)], env, project)
        try:
            terminal.until("[F1首页]")
            terminal.send(b"\x1bOR")
            terminal.until("Independentfixture")
            terminal.send(b"\r")
            terminal.send(b"P")
            terminal.until("Fixture")
            terminal.send(b"\x1b[B")
            terminal.send(b"\r")
            terminal.until("会话已退出")
            assert json.loads((root / "launched.json").read_text())["session"] == session
            runs = json.loads(cli("codex", "runs", "--json"))
            assert len(runs) == 1 and runs[0]["exit_code"] == 0, runs
            links = runs[0]["context"]["resources"]
            assert any(link["binding"]["account"] == "custom-config-only" for link in links), links
        finally:
            terminal.close()
        print("PASS: saved effort clear, F1–F4 navigation, unshared resume guard, shared resume and custom Usage config")

        def launch_selected_account(terminal, expected="Codex已退出："):
            terminal.send(b"\r")
            terminal.until("在哪个项目启动Fixture")
            terminal.send(b"\r")
            terminal.until(expected)

        def launch_from_home(terminal, expected="Codex已退出："):
            terminal.send(b"\x1bOS")
            terminal.until("[F4Provider]")
            terminal.send(b"\x1b[B")
            launch_selected_account(terminal, expected)

        default_config = root / "config/usage/config.toml"
        launched = root / "launched.json"
        for args, page in [
            (["--providers", "grok", "--refresh", "0"], "[F1首页]"),
            (["--providers", "grok", "history"], "[F2历史]"),
            (["codex"], "[F4Provider]"),
        ]:
            assert not default_config.exists()
            launched.unlink(missing_ok=True)
            terminal = Terminal(binary, args, env, project)
            try:
                terminal.until(page)
                if page == "[F4Provider]":
                    terminal.send(b"\x1bOP")
                    terminal.until("[F1首页]")
                launch_from_home(terminal)
                assert json.loads(launched.read_text())["session"] is None
                assert not default_config.exists(), "default config must remain optional across pages"
                # A second launch reuses the same dashboard without initializing a config.
                launched.unlink()
                terminal.send(b"\x1bOP")
                terminal.until("[F1首页]")
                launch_from_home(terminal)
                assert launched.exists()
            finally:
                terminal.close()
        print("PASS: absent default config supports repeated launches from dashboard, history and Provider navigation")

        # An explicit --config remains required even when it names the default path.
        default_config.parent.mkdir(parents=True, exist_ok=True)
        default_config.write_text(config.read_text())
        launched.unlink()
        terminal = Terminal(binary, ["--config", str(default_config)], env, project)
        try:
            terminal.until("[F1首页]")
            default_config.unlink()
            launch_from_home(terminal, "找不到配置文件:")
            assert not launched.exists(), "missing explicit config must not silently launch with defaults"
            cli("init-config", "--path", str(default_config))
            launch_selected_account(terminal)
            assert launched.exists()
        finally:
            terminal.close()
        print("PASS: missing explicit config blocks launch and init-config repairs it without restarting Usage")

        launched.unlink()
        terminal = Terminal(binary, ["--providers", "grok", "--refresh", "0"], env, project)
        try:
            terminal.until("[F1首页]")
            default_config.unlink()
            launch_from_home(terminal)
            assert launched.exists(), "implicit config stays optional when deleted after startup"
        finally:
            terminal.close()
        print("PASS: default config removed after startup falls back to defaults on launch")

        saved_config = config.read_text()
        terminal = Terminal(binary, ["--config", str(config), "codex"], env, project)
        try:
            terminal.until("[F4Provider]")
            terminal.send(b"\x1b[B")
            for damage, expected in [
                ("missing", "找不到配置文件:"),
                ("syntax", "配置不是合法的TOML:"),
                ("directory", "无法读取配置:"),
            ]:
                launched.unlink(missing_ok=True)
                config.unlink()
                if damage == "syntax":
                    config.write_text("providers = [")
                elif damage == "directory":
                    config.mkdir()
                launch_selected_account(terminal, expected)
                assert not launched.exists(), f"{damage} config must block the child"
                if damage == "directory":
                    config.rmdir()
                config.write_text(saved_config)
                # Retry in the same Provider page, with the same selected account.
                launch_selected_account(terminal)
                assert launched.exists(), f"{damage} config must recover without reopening Usage"
        finally:
            terminal.close()
        print("PASS: missing, malformed and unreadable configs keep Provider management open for repair and retry")


if __name__ == "__main__":
    main()
