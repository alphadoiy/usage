"""Synthetic OAuth PTY regression: stdin, Ctrl+C, TUI restoration and cleanup.

Invoked by codex_cli.rs with the built Usage binary; no network or real credentials.
"""
import base64
import fcntl
import json
import os
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
from pathlib import Path

ANSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")
MOCK_CODEX = """#!/usr/bin/env python3
import os, sys, shutil
from pathlib import Path
if sys.argv[1:] == ['--version']:
    print('codex-cli 0.159.3')
    sys.exit(0)
if sys.argv[1:] and sys.argv[1] == 'login':
    print('SYNTHETIC_LOGIN_READY', flush=True)
    if sys.stdin.readline().strip() != 'confirm':
        sys.exit(2)
    shutil.copyfile(os.environ['SOURCE_AUTH'], Path(os.environ['CODEX_HOME']) / 'auth.json')
    sys.exit(0)
sys.exit(3)
"""


def check_terminal(program, env, ui, cancel):
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(env["HOME"])
        args = [program, "codex"]
        if not ui:
            args += ["login", "Synthetic"]
        os.execve(program, args, env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    data = bytearray()
    done = False

    def until(marker):
        needle = marker.replace(" ", "").encode()
        deadline = time.monotonic() + 8
        # Ratatui skips blank cells and may emit cursor positioning between glyphs.
        while needle not in ANSI.sub(b"", data).replace(b" ", b""):
            if time.monotonic() > deadline:
                text = ANSI.sub(b"", data).decode(errors="replace")
                raise AssertionError(f"timeout waiting for {marker}: {text[-3000:]}")
            if select.select([fd], [], [], 0.1)[0]:
                chunk = os.read(fd, 65536)
                assert chunk, f"early EOF waiting for {marker}"
                data.extend(chunk)

    try:
        if ui:
            until("Codex")
            os.write(fd, b"a")
            until("添加 Codex 账号")
            os.write(fd, b"o")
            until("授权中Ctrl+C取消")
            os.write(fd, b"2")
        until("SYNTHETIC_LOGIN_READY")
        os.write(fd, b"\x03" if cancel else b"confirm\n")
        until("登录已取消" if cancel else ("已保存 OAuth" if ui else "已保存 OAuth 账号"))
        if ui:
            # Wait for the second alternate-screen entry before sending the exit key.
            deadline = time.monotonic() + 5
            while data.count(b"\x1b[?1049h") < 2:
                assert time.monotonic() < deadline, "TUI did not restore its screen"
                if select.select([fd], [], [], 0.1)[0]:
                    data.extend(os.read(fd, 65536))
            os.write(fd, b"q")
        deadline = time.monotonic() + 5
        while True:
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited:
                done = True
                break
            assert time.monotonic() < deadline, "Usage did not exit"
            if select.select([fd], [], [], 0.05)[0]:
                try:
                    data.extend(os.read(fd, 65536))
                except OSError:
                    pass  # PTY EOF after the process exits.
        code = os.waitstatus_to_exitcode(status)
        assert code == (130 if cancel and not ui else 0), (code, data[-2000:])
        result = subprocess.run(
            [program, "codex", "list", "--json"], env=env, capture_output=True, check=True
        )
        profiles = json.loads(result.stdout)
        assert len(profiles) == (0 if cancel else 1)
        if not cancel:
            assert profiles[0]["name"] == ("synthetic@example.test" if ui else "Synthetic")
        assert not any((Path(env["USAGE_CODEX_DIR"]) / "login-staging").iterdir())
        assert b"panicked" not in data
        print(f"{'TUI' if ui else 'CLI'} {'cancellation' if cancel else 'login'}: passed")
    finally:
        if not done:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(fd)


def main():
    program = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="usage-auth-pty-") as temporary:
        root = Path(temporary).resolve()
        tools = root / "bin"
        tools.mkdir()
        claims = {"sub": "synthetic-user", "email": "synthetic@example.test"}
        payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
        jwt = f"e30.{payload}.fixture"
        source = root / "source.json"
        source.write_text(json.dumps({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": jwt, "access_token": jwt,
                "refresh_token": "synthetic-refresh", "account_id": "synthetic-account",
            },
        }))
        fake = tools / "codex"
        fake.write_text(MOCK_CODEX)
        fake.chmod(0o700)
        for ui in (False, True):
            for cancel in (False, True):
                case = root / ("tui" if ui else "cli") / ("cancel" if cancel else "success")
                case.mkdir(parents=True)
                env = dict(os.environ, HOME=str(case), XDG_DATA_HOME=str(case / "data"),
                           XDG_CONFIG_HOME=str(case / "config"),
                           USAGE_CODEX_DIR=str(case / "manager"), USAGE_LOG="off",
                           SOURCE_AUTH=str(source), PATH=str(tools) + os.pathsep + os.environ["PATH"],
                           TERM="xterm-256color")
                check_terminal(program, env, ui, cancel)


if __name__ == "__main__":
    main()
