"""Installed-Codex smoke test; synthetic homes and a loopback Responses stub.

Run: python3 tests/support/native_codex_context.py target/debug/usage /path/to/codex
"""
import json
import difflib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import fcntl
import pty
import re
import select
import selectors
import shlex
import signal
import sqlite3
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


def write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)


class ResponsesStub:
    """Serve deterministic text locally; never forward requests to a model service."""
    def __enter__(self):
        self.requests = []
        requests = self.requests

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                requests.append((self.path, self.headers.get("Authorization"), body))
                events = [
                    {"type": "response.created", "response": {"id": "response-fixture"}},
                    {"type": "response.output_item.done", "item": {
                        "type": "message", "role": "assistant", "id": "message-fixture",
                        "content": [{"type": "output_text", "text": "Synthetic fixture reply"}]}},
                    {"type": "response.completed", "response": {"id": "response-fixture", "usage": {
                        "input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}},
                ]
                payload = "".join(f'event: {event["type"]}\ndata: {json.dumps(event)}\n\n' for event in events).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.worker = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.worker.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        return self

    def __exit__(self, *_):
        self.server.shutdown()
        self.server.server_close()
        self.worker.join()


class Client:
    def __init__(self, command, env, cwd, error_log):
        self.error = error_log.open("w")
        self.process = subprocess.Popen(command, env=env, cwd=cwd, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.error, start_new_session=True)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.buffer = b""
        self.sequence = 0
        self.notifications = []
        try:
            self.rpc("initialize", {"clientInfo": {"name": "usage_context_test", "version": "1"},
                                    "capabilities": {"experimentalApi": True}})
            self.process.stdin.write(b'{"method":"initialized"}\n')
            self.process.stdin.flush()
        except BaseException:
            self.close()
            raise

    def rpc(self, method, params):
        self.sequence += 1
        request = {"id": self.sequence, "method": method, "params": params}
        self.process.stdin.write((json.dumps(request) + "\n").encode())
        self.process.stdin.flush()
        deadline = time.monotonic() + 25
        while time.monotonic() < deadline:
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                if not line:
                    continue
                message = json.loads(line)
                if message.get("id") == self.sequence:
                    assert "error" not in message, message
                    return message["result"]
                self.notifications.append(message)
            if self.selector.select(0.2):
                chunk = os.read(self.process.stdout.fileno(), 65536)
                if not chunk:
                    raise AssertionError(f"Codex exited during {method}; see {self.error.name}")
                self.buffer += chunk
        raise AssertionError(f"Timeout during {method}; see {self.error.name}")

    def complete_turn(self, thread_id, text):
        turn_id = self.rpc("turn/start", {"threadId": thread_id,
                                          "input": [{"type": "text", "text": text}]})["turn"]["id"]
        deadline = time.monotonic() + 25
        while time.monotonic() < deadline:
            for message in self.notifications:
                if message.get("method") == "turn/completed" and message["params"]["turn"]["id"] == turn_id:
                    assert message["params"]["turn"]["status"] == "completed", message
                    return
            # A read request also drains turn notifications from the same stream.
            self.rpc("thread/read", {"threadId": thread_id})
            time.sleep(0.05)
        raise AssertionError(f"Local fixture turn did not complete: {self.notifications[-5:]}")

    def close(self):
        try:
            try:
                self.process.stdin.close()
            except BrokenPipeError:
                pass
            for timeout, signal_number in [(3, signal.SIGTERM), (2, signal.SIGKILL)]:
                try:
                    self.process.wait(timeout=timeout)
                    return
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(self.process.pid, signal_number)
                    except ProcessLookupError:
                        pass
            self.process.wait(timeout=2)
        finally:
            self.selector.close()
            self.error.close()


class Terminal:
    """Exercise native TUI entry points against synthetic local fixtures."""
    def __init__(self, command, env, cwd):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
            os.execve(command[0], command, env)
        self.data = bytearray()
        self.hooks_dismissed = False

    def pump(self):
        if select.select([self.fd], [], [], 0.1)[0]:
            try:
                chunk = os.read(self.fd, 65536)
            except OSError:
                chunk = b""
            self.data.extend(chunk)
            if b"\x1b[6n" in chunk:
                os.write(self.fd, b"\x1b[1;1R")
            if not self.hooks_dismissed and "hooksareneworchanged" in self.text().lower().replace(" ", ""):
                # This smoke exercises native history, not Usage's notification hooks.
                # Select the native "continue without trusting" entry explicitly.
                # The native startup screen discards input immediately after its first draw.
                time.sleep(0.3)
                os.write(self.fd, b"3\r")
                self.hooks_dismissed = True

    def text(self, start=0):
        return re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", self.data[start:]).decode(errors="replace")

    def compact_text(self, start=0):
        # Ratatui may position adjacent words with cursor movements instead of spaces.
        return re.sub(r"\s+", "", self.text(start))

    def submit(self, text):
        os.write(self.fd, text.encode())
        # The native composer detects rapid input as a paste. Submit after that burst
        # settles so Enter is a key press, not part of the pasted text.
        deadline = time.monotonic() + 0.3
        while time.monotonic() < deadline:
            self.pump()
        os.write(self.fd, b"\r")

    def until(self, predicate, description):
        deadline = time.monotonic() + 25
        while not predicate():
            self.pump()
            assert time.monotonic() < deadline, f"Timeout waiting for {description}: {self.text()[-4000:]}"

    def close(self):
        def wait_for_exit(seconds):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                try:
                    if os.waitpid(self.pid, os.WNOHANG)[0]:
                        return True
                except ChildProcessError:
                    return True
                self.pump()
            return False

        try:
            for _ in range(2):
                try:
                    os.write(self.fd, b"\x03")
                except OSError:
                    pass
                if wait_for_exit(1):
                    return
            for signal_number in [signal.SIGTERM, signal.SIGKILL]:
                try:
                    os.killpg(self.pid, signal_number)
                except ProcessLookupError:
                    pass
                if wait_for_exit(2):
                    return
            raise AssertionError("Native fixture process did not exit after SIGKILL")
        finally:
            os.close(self.fd)


def main():
    usage = str(Path(sys.argv[1]).resolve())
    codex = str(Path(sys.argv[2]).resolve())
    with ResponsesStub() as stub, tempfile.TemporaryDirectory(prefix="usage-native-context-") as temporary:
        root = Path(temporary).resolve()
        base = root / "custom-codex"
        project = root / "project"
        project.mkdir()
        env = {"PATH": os.environ["PATH"], "TERM": "xterm-256color", "LANG": "en_US.UTF-8"}
        env.update(HOME=str(root / "user"), CODEX_HOME=str(base),
                   USAGE_CODEX_BASE_HOME=str(base), USAGE_CODEX_DIR=str(root / "manager"),
                   USAGE_PLUGIN_DIR=str(root / "plugins"), XDG_CONFIG_HOME=str(root / "config"),
                   XDG_DATA_HOME=str(root / "data"), XDG_CACHE_HOME=str(root / "cache"),
                   USAGE_GATEWAY="off", USAGE_LOG="off", USAGE_LOG_DIR=str(root / "logs"))
        # The native TUI must not leave a background daemon behind this temporary fixture.
        native = root / "native-codex"
        write(native, f'#!/bin/sh\nexec {shlex.quote(codex)} --no-daemon "$@"\n')
        native.chmod(0o700)
        (base / "shared-files").mkdir(parents=True)
        (base / "modules").mkdir()
        (base / "tools").mkdir()
        (base / "tools/node").symlink_to("/usr/bin/true")
        write(base / "config.toml", 'model="gpt-5.4"\nmodel_provider="global-only"\n'
              'check_for_update_on_startup=false\n'
              'cli_auth_credentials_store="file"\njs_repl_node_path="tools/node"\njs_repl_node_module_dirs=["modules"]\n'
              f'[model_providers.global-only]\nname="Global fixture"\nbase_url="{stub.url}/default/v1"\nwire_api="responses"\n'
              '[analytics]\nenabled=false\n[tui]\nnotifications=false\nscreen_reader_detection_done=true\n'
              '[mcp_servers.fixture]\ncommand="true"\nenabled=false\n'
              '[sandbox_workspace_write]\nwritable_roots=["shared-files"]\n'
              f'[projects.{json.dumps(str(project))}]\ntrust_level="trusted"\n')
        write(base / "AGENTS.md", "Usage native context fixture instructions.\n")
        write(base / "rules/default.rules", 'prefix_rule(pattern=["fixture-command"], decision="allow")\n')
        for path, name in [(base / "skills" / "legacy", "fixture-legacy"),
                           (root / "user/.agents/skills/global", "fixture-global")]:
            write(path / "SKILL.md", f"---\nname: {name}\ndescription: Context inheritance test.\n---\nFixture skill.\n")

        def cli(args, content=None):
            result = subprocess.run([usage, "codex", *args], env=env, cwd=project,
                                    input=content, text=True, capture_output=True, timeout=30)
            assert result.returncode == 0, result.stderr
            return result.stdout

        def picker_params(provider):
            return {"cwd": str(project), "modelProviders": [provider], "sourceKinds": ["cli", "vscode"],
                    "archived": False, "sortKey": "updated_at", "limit": 25}

        cli(["add", "A", "--base-url", f"{stub.url}/a/v1", "--model", "gpt-5.4", "--key-stdin"], "synthetic-key-A\n")
        cli(["add", "B", "--base-url", f"{stub.url}/b/v1", "--model", "gpt-5.4", "--key-stdin"], "synthetic-key-B\n")
        provider_b = root / "provider-b.toml"
        write(provider_b, 'model="gpt-5.4"\nmodel_provider="second-provider"\n'
              f'[model_providers.second-provider]\nname="Second fixture"\nbase_url="{stub.url}/c/v1"\nwire_api="responses"\n')
        cli(["add", "C", "--config", str(provider_b), "--key-stdin"], "synthetic-key-C\n")
        accounts = {p["name"]: p["id"] for p in json.loads(cli(["list", "--json"]))}
        baseline = Client([codex, "app-server", "--listen", "stdio://"], env, project, root / "native.stderr")
        try:
            native_config = baseline.rpc("config/read", {"cwd": str(project)})["config"]
            entries = baseline.rpc("skills/list", {"cwds": [str(project)]})["data"]
            native_skills = {skill["name"] for entry in entries for skill in entry["skills"]}
        finally:
            baseline.close()
        thread_id = None
        for name in ["A", "B", "C"]:
            provider = "second-provider" if name == "C" else "usage_byok"
            command = [usage, "codex", "run", accounts[name], "--cwd", str(project),
                       "--binary", codex, "--", "app-server", "--listen", "stdio://"]
            client = Client(command, env, project, root / f"{name}.stderr")
            try:
                config = client.rpc("config/read", {"cwd": str(project)})["config"]
                assert config["model_provider"] == provider, config
                assert config["mcp_servers"]["fixture"]["enabled"] is False, config
                for field in ["sandbox_workspace_write", "js_repl_node_path", "js_repl_node_module_dirs"]:
                    assert config[field] == native_config[field], (field, config[field], native_config[field])
                entries = client.rpc("skills/list", {"cwds": [str(project)], "forceReload": True})["data"]
                names = {skill["name"] for entry in entries for skill in entry["skills"]}
                assert {"fixture-legacy", "fixture-global"} <= names, names
                assert names == native_skills, (names, native_skills)
                if thread_id is None:
                    thread_id = client.rpc("thread/start", {"cwd": str(project), "model": "gpt-5.4"})["thread"]["id"]
                    # A real native turn creates the preview/history the picker requires.
                    # Every request is answered by the local stub using synthetic credentials.
                    client.complete_turn(thread_id, "Shared A fixture history")
                    assert any(path == "/a/v1/responses" and auth == "Bearer synthetic-key-A"
                               for path, auth, _ in stub.requests), stub.requests
                    # A still owns the live writer lease here. B must be able to start and
                    # list its completed conversation before A exits; this does not attempt
                    # to steal A's active writer by resuming it concurrently.
                    concurrent_b = Client([usage, "codex", "run", accounts["B"], "--cwd", str(project),
                                           "--binary", codex, "--", "app-server", "--listen", "stdio://"],
                                          env, project, root / "B-concurrent.stderr")
                    try:
                        visible = concurrent_b.rpc("thread/list", picker_params("usage_byok"))["data"]
                        assert thread_id in {thread["id"] for thread in visible}, visible
                        assert client.process.poll() is None, "A must still be running for this visibility check"
                    finally:
                        concurrent_b.close()
                    # Archiving/unarchiving flushes the native rollout and index.
                    client.rpc("thread/archive", {"threadId": thread_id})
                    client.rpc("thread/unarchive", {"threadId": thread_id})
                    client.rpc("thread/name/set", {"threadId": thread_id, "name": "Shared A fixture"})
                else:
                    # Match the real /resume picker's provider and cwd filters. An empty
                    # modelProviders array is a separate all-provider API, not the picker.
                    visible = client.rpc("thread/list", picker_params(provider))["data"]
                    assert (thread_id in {thread["id"] for thread in visible}) == (name == "B"), visible
                    all_providers = client.rpc("thread/list", {"cwd": str(project), "modelProviders": []})["data"]
                    assert thread_id in {thread["id"] for thread in all_providers}, all_providers
                    thread = client.rpc("thread/read", {"threadId": thread_id})["thread"]
                    assert thread["id"] == thread_id, thread
                    resumed = client.rpc("thread/resume", {"threadId": thread_id, "modelProvider": provider, "model": "gpt-5.4"})
                    assert resumed["thread"]["id"] == thread_id, resumed
                    assert resumed["modelProvider"] == provider, resumed
                account_home = root / "manager/homes" / accounts[name] / "r1"
                assert (account_home / "AGENTS.md").read_text() == (base / "AGENTS.md").read_text()
                assert (account_home / "rules/default.rules").read_text() == (base / "rules/default.rules").read_text()
                if name == "C":
                    client.rpc("config/value/write", {"keyPath": "tui.notifications", "value": True,
                                                       "mergeStrategy": "replace"})
            finally:
                client.close()
            if name == "B":
                terminal = Terminal([usage, "codex", "run", accounts[name], "--cwd", str(project),
                                     "--binary", codex], env, project)
                try:
                    terminal.until(lambda: "gpt-5.4" in terminal.text(), "native B prompt")
                    terminal.submit("/resume")
                    terminal.until(lambda: "Shared A fixture" in terminal.text(), "A in B's native /resume picker")
                    resume_start = len(terminal.data)
                    time.sleep(0.3)
                    os.write(terminal.fd, b"\r")
                    terminal.until(lambda: "Syntheticfixturereply" in terminal.compact_text(resume_start), "A history resumed by B")
                    reply_start = len(terminal.data)
                    terminal.submit("Verify B fixture route")
                    terminal.until(lambda: any(path == "/b/v1/responses" and auth == "Bearer synthetic-key-B"
                                              for path, auth, _ in stub.requests), "B's own route and credential")
                    assert any(path == "/b/v1/responses" and auth == "Bearer synthetic-key-B"
                               and b"Shared A fixture history" in body and b"Verify B fixture route" in body
                               for path, auth, body in stub.requests), "B must send A's history with its new input"
                    terminal.until(lambda: "Syntheticfixturereply" in terminal.compact_text(reply_start), "B fixture turn completion")
                    print("Native B /resume: A visible, history restored, B route and credential confirmed", flush=True)
                finally:
                    terminal.close()
        nested_env = dict(env)
        nested_env.pop("USAGE_CODEX_BASE_HOME")
        nested_env["CODEX_HOME"] = str(root / "manager/homes" / accounts["A"] / "r1")
        client = Client(command, nested_env, project, root / "C-relaunch.stderr")
        try:
            config = client.rpc("config/read", {"cwd": str(project)})["config"]
            assert config["tui"]["notifications"] is True, config
        finally:
            client.close()
        # Exercise the exact ordinary Enter/clipboard launcher, not a hand-written RPC
        # override. Successful native startup writes its selected provider to the index.
        database = next(base.glob("state_*.sqlite"))
        def current_model_settings():
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
                return connection.execute("SELECT model_provider, model, reasoning_effort FROM threads WHERE id=?",
                                          (thread_id,)).fetchone()
        assert current_model_settings()[0] == "second-provider"
        home_config_before = (base / "config.toml").read_bytes()
        local_command = [usage, "codex", "resume-local", "--home", str(base), "--cwd", str(project),
                         "--session", thread_id, "--binary", str(native)]
        terminal = Terminal(local_command, env, project)
        try:
            terminal.until(lambda: current_model_settings()[0] == "global-only", "ordinary Enter's default provider")
            assert "Model provider `second-provider` not found" not in terminal.text()
        finally:
            terminal.close()
        assert (base / "config.toml").read_bytes() == home_config_before, "".join(difflib.unified_diff(
            home_config_before.decode().splitlines(True), (base / "config.toml").read_text().splitlines(True)))
        project_config = project / ".codex/config.toml"
        # Native 0.165 excludes provider/routing keys from project-local layers even
        # when trusted, while honoring project model/effort. Keep conflicting routing
        # here to verify the launcher follows the native loader's effective result.
        write(project_config, 'model_provider="project-default"\nmodel="gpt-6.1-sol"\nmodel_reasoning_effort="high"\n'
              f'[model_providers.project-default]\nname="Project fixture"\nbase_url="{stub.url}/project/v1"\nwire_api="responses"\n')
        original_configs = {path: path.read_bytes() for path in [base / "config.toml", project_config]}
        terminal = Terminal(local_command, env, project)
        try:
            terminal.until(lambda: "gpt-6.1-solhigh" in terminal.compact_text().lower(),
                           "ordinary Enter's trusted project defaults")
            reply_start = len(terminal.data)
            terminal.submit("Verify trusted project defaults")
            # The index's model fields can still describe the previous turn at resume.
            # The actual request is the source of truth for this invocation's settings.
            terminal.until(lambda: any(path == "/default/v1/responses"
                                      and json.loads(body).get("model") == "gpt-6.1-sol"
                                      and json.loads(body).get("reasoning", {}).get("effort") == "high"
                                      for path, _, body in stub.requests),
                           "native effective Home route with trusted project model and effort")
            assert not any(path.startswith("/project/") for path, _, _ in stub.requests), "Native project routing restrictions must be preserved"
            terminal.until(lambda: "Syntheticfixturereply" in terminal.compact_text(reply_start),
                           "trusted project fixture turn completion")
        finally:
            terminal.close()
        assert all(path.read_bytes() == original for path, original in original_configs.items()), "Local resume must not rewrite configuration"
        print("Native Codex: context/path parity, A visible in B's /resume, explicit cross-provider resume, local/project defaults, and preference relaunch passed")


if __name__ == "__main__":
    main()
