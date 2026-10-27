"""Drive the real Usage TUI in a PTY using synthetic accounts and conversations.

python3 tests/support/tui_e2e.py --binary target/debug/usage --output /tmp/usage-tui-e2e
Requires pyte (may be installed into --deps rather than the project's environment).
No tool is launched, and deletion is cancelled before any mutation.
"""
import argparse
import codecs
import fcntl
import hashlib
import html
import http.server
import json
import os
import pty
import select
import signal
import struct
import sys
import termios
import threading
import time
import uuid
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--deps", default="/private/tmp/usage-tui-e2e-deps")
    args = parser.parse_args()
    sys.path.insert(0, args.deps)
    import pyte
    from wcwidth import wcswidth

    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    fixture = output / ("fixture-" + uuid.uuid4().hex[:8])
    fixture.mkdir()
    project = fixture / "SampleProject"
    project.mkdir()
    homes = {name: fixture / name for name in ["claude", "codex", "grok"]}
    for home in homes.values():
        home.mkdir()

    def rows(path, values):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("".join(json.dumps(v, ensure_ascii=False) + "\n" for v in values))
        return path

    logs = []
    sid = str(uuid.uuid4())
    logs.append(rows(homes["claude"] / "projects" / "fixture" / (sid + ".jsonl"), [
        {"type": "user", "sessionId": sid, "cwd": str(project), "message": {"content": "Claude layout request"}},
        {"type": "assistant", "message": {"content": [{"type": "text", "text": "Claude layout reply"}]}},
    ]))
    sid = str(uuid.uuid4())
    logs.append(rows(homes["codex"] / "sessions" / "2026" / ("rollout-" + sid + ".jsonl"), [
        {"type": "session_meta", "payload": {"id": sid, "cwd": str(project), "source": "cli"}},
        {"type": "event_msg", "payload": {"type": "user_message", "message": "Codex preview request"}},
        {"type": "event_msg", "payload": {"type": "agent_message", "message": "Codex preview reply"}},
    ]))
    sid = str(uuid.uuid4())
    grok = homes["grok"] / "sessions" / "fixture" / sid
    rows(grok / "summary.json", [{"info": {"id": sid, "cwd": str(project)}, "generated_title": "Grok preview request"}])
    logs.append(rows(grok / "updates.jsonl", [
        {"params": {"update": {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Grok preview request"}}}},
        {"params": {"update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Grok preview reply"}}}},
    ]))
    hashes = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in logs}

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_CONNECT(self):
            self.send_error(502, "Synthetic test blocks external traffic")

        def do_GET(self):
            time.sleep(0.2)
            if self.path.endswith("/models"):
                value = {"data": [{"id": "fixture-model-" + str(i)} for i in range(6)]}
            elif "/api/usage/token" in self.path:
                value = {"code": True, "data": {"object": "token_usage", "total_used": 1500000, "total_available": 5000000, "unlimited_quota": False}}
            else:
                self.send_error(404)
                return
            body = json.dumps(value).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = "http://127.0.0.1:" + str(server.server_port)
    config = fixture / "config.toml"
    config.write_text(f'''providers = ["grok"]
refresh_seconds = 0
[paths]
claude = {json.dumps(str(homes["claude"]))}
codex = {json.dumps(str(homes["codex"]))}
grok = {json.dumps(str(homes["grok"]))}
[[resources]]
id = "fixture"
label = "E2E Provider"
service = "new_api_token"
base_url = "{base}"
credential_env = "USAGE_E2E_KEY"
options = {{ quota_per_unit = "500000", currency = "USD" }}
[resources.inference]
base_url = "{base}/v1"
credential_env = "USAGE_E2E_KEY"
''')
    env = dict(os.environ)
    env.update(TERM="xterm-256color", COLORTERM="truecolor", LANG="en_US.UTF-8",
               USAGE_LOG="off", USAGE_E2E_KEY="synthetic-only",
               USAGE_CODEX_DIR=str(fixture / "managed"),
               XDG_DATA_HOME=str(fixture / "data"), XDG_CONFIG_HOME=str(fixture / "config"),
               HTTP_PROXY=base, HTTPS_PROXY=base, ALL_PROXY=base, NO_PROXY="127.0.0.1,localhost")
    pid, fd = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 120, 0, 0))
        os.execve(str(args.binary.resolve()), [str(args.binary.resolve()), "--config", str(config)], env)

    class Screen(pyte.Screen):
        def set_mode(self, *modes, **kwargs):
            # pyte intentionally has no alternate-buffer emulation. Each nested TUI
            # enters a fresh alternate screen, which must not retain its parent's cells.
            if kwargs.get("private") and any(mode in (47, 1047, 1049) for mode in modes):
                self.reset()
            super().set_mode(*modes, **kwargs)

        @property
        def display(self):
            # Ratatui erases individual wide-glyph cells during diff updates. An orphaned
            # continuation cell must be blank; pyte 0.8's display helper indexes it as text.
            return ["".join(self.cell_text(x, y) for x in range(self.columns)) for y in range(self.lines)]

        def cell_text(self, x, y):
            data = self.buffer[y][x].data
            if data:
                return data
            return "" if x and wcswidth(self.buffer[y][x - 1].data) == 2 else " "

    screen = Screen(120, 34)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    transcript = bytearray()
    snapshots = []
    checks = []
    child_reaped = False

    def pump(seconds=0.15):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if not select.select([fd], [], [], min(0.08, max(0, deadline - time.monotonic())))[0]:
                continue
            try:
                data = os.read(fd, 131072)
            except OSError:
                return
            if not data:
                return
            transcript.extend(data)
            if b"\x1b[6n" in data:
                os.write(fd, b"\x1b[1;1R")
            stream.feed(decoder.decode(data))

    def text():
        return "\n".join(screen.display)

    def until(needle, timeout=8):
        deadline = time.monotonic() + timeout
        while needle not in text():
            if time.monotonic() > deadline:
                capture("failure")
                raise AssertionError(f"timeout waiting for {needle!r}\n{text()}")
            pump()
        pump(0.1)

    def send(value):
        os.write(fd, value.encode() if isinstance(value, str) else value)
        pump(0.25)

    def capture(name):
        (output / (name + ".txt")).write_text(text())
        runs = []
        for y in range(screen.lines):
            row = []
            for x in range(screen.columns):
                cell = screen.buffer[y][x]
                data = screen.cell_text(x, y)
                if not data:
                    continue
                def color(value, default):
                    if len(value) == 6 and all(c in "0123456789abcdef" for c in value.lower()):
                        return "#" + value
                    return {"default": default, "brown": "#e5b567", "brightblack": "#777777"}.get(value, value)
                style = f"color:{color(cell.fg, '#e6e6e6')};background:{color(cell.bg, '#161618')}"
                if cell.bold:
                    style += ";font-weight:bold"
                row.append(f'<span style="{style}">{html.escape(data)}</span>')
            runs.append("".join(row))
        snapshots.append((name, "\n".join(runs)))

    def check(label, condition):
        assert condition, label + "\n" + text()
        checks.append(label)

    def resize(width, height):
        screen.resize(lines=height, columns=width)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        os.kill(pid, signal.SIGWINCH)
        pump(0.4)

    try:
        until("E2E Provider")
        until("USD")
        capture("01-home-wide")
        row_before = next(i for i, line in enumerate(screen.display) if "E2E Provider" in line)
        send("r")
        until("USD")
        check("provider refresh preserves content row", next(i for i, line in enumerate(screen.display) if "E2E Provider" in line) == row_before)
        send("\t\r")
        until("最近查询")
        capture("02-status-details")
        send(b"\x1b")
        send(b"\x1b[Z")
        send("?")
        send("重置")
        until("此 Provider 不支持主动重置")
        send("\r")
        check("disabled action does not execute", "搜索操作" in text())
        title_row = next(y for y, line in enumerate(screen.display) if "Provider · 操作" in line)
        left = next(x for x in range(screen.columns) if screen.buffer[title_row][x].data == "┌")
        bottom = next(y for y in range(title_row + 1, screen.lines) if screen.buffer[y][left].data == "└")
        check("modal border survives underlying Chinese glyphs", all(screen.buffer[y][left].data == "│" for y in range(title_row + 1, bottom)))
        capture("03-action-search-disabled")
        send(b"\x1b")
        send(b"\x1b")
        send(b"\x1bOQ")
        until("[F2 历史]")
        capture("04-history")
        send(b"\x1bOR")
        until("Claude layout reply")
        capture("05-sessions-claude")
        send(b"\x1b[C\x1b[B")
        until("Codex preview reply")
        capture("06-sessions-codex")
        send(b"\x1b[B")
        until("Grok preview reply")
        capture("07-sessions-grok")
        send("?")
        send("排序")
        until("切换最近活动")
        send("\r")
        until("Token 排序")
        check("searchable command executes selected operation", "搜索操作" not in text())
        send("/Claude")
        until("Claude layout")
        send(b"\x1b")
        send("a")
        until("已选 3")
        send("d")
        until("移入回收站")
        capture("08-delete-confirmation")
        send(b"\x1b")
        check("delete cancellation keeps all files", all(p.exists() for p in logs))
        send("b")
        until("回收站为空")
        capture("09-trash")
        send(b"\x1b")
        send(b"\x1bOS")
        until("Provider 管理")
        send("?")
        send("导入")
        until("从 CC Switch 导入")
        capture("10-provider-actions")
        send(b"\x1b")
        send(b"\x1b")
        send(b"\x1b")
        send(b"\x1bOP")
        until("E2E Provider")
        for width, height in [(80, 24), (44, 20), (28, 14)]:
            resize(width, height)
            check(f"navigation survives {width}x{height}", all(key in screen.display[0] for key in ["F1", "F2", "F3", "F4"]))
            check(f"balance remains visible at {width}x{height}", "余额" in text() and "10 USD" in text())
            capture(f"11-home-{width}x{height}")
        resize(44, 24)
        send(b"\x1bOR")
        send(b"\x1b[D")
        until("SampleProject")
        capture("12-narrow-projects")
        send(b"\x1b[C")
        until("恢复预览")
        capture("13-narrow-sessions")
        resize(120, 34)
        until("请求")
        check("conversation content remains after resizing", "reply" in text())
        send(b"\x1bOP")
        capture("14-home-final")
        check("conversation fixtures are unchanged", all(hashlib.sha256(p.read_bytes()).hexdigest() == hashes[str(p)] for p in logs))
        send("q")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                child_reaped = True
                check("clean terminal exit", os.waitstatus_to_exitcode(status) == 0)
                break
            pump()
        assert child_reaped, "Usage did not exit"
    finally:
        (output / "terminal.ansi").write_bytes(transcript)
        sections = "".join(f"<h2>{html.escape(name)}</h2><pre>{body}</pre>" for name, body in snapshots)
        (output / "screens.html").write_text('<!doctype html><meta charset="utf-8"><title>Usage terminal E2E</title><style>body{background:#101012;color:#eee;padding:24px}pre{font:14px/1.5 Menlo,Consolas,monospace;white-space:pre}h2{font:18px system-ui;margin-top:32px}</style>' + sections)
        (output / "results.json").write_text(json.dumps({"checks": checks, "snapshots": [name for name, _ in snapshots], "fixture": str(fixture)}, ensure_ascii=False, indent=2))
        if not child_reaped:
            os.kill(pid, signal.SIGTERM)
            os.waitpid(pid, 0)
        os.close(fd)
        server.shutdown()
    print(json.dumps({"passed": len(checks), "screens": len(snapshots), "output": str(output)}, ensure_ascii=False))


if __name__ == "__main__":
    main()
