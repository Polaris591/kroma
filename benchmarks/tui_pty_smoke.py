#!/usr/bin/env python3
"""Offline Unix PTY test for TUI input, model/tool events, themes and cleanup.

Run: cargo build --release --locked && python3 benchmarks/tui_pty_smoke.py
Uses a temporary HOME/workdir and a local mock HTTP server. No real API keys.
"""
import base64
import json
import re
import os
from pathlib import Path
import select
import struct
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BINARY = Path(os.environ.get("KROMA_TEST_BINARY", str(
    Path(__file__).resolve().parents[1] / "target/release/kroma"))).resolve()


def sse(delta, reason):
    return ("data: " + json.dumps({"choices": [{"delta": delta, "finish_reason": reason}]})
            + "\n\ndata: [DONE]\n\n").encode()


class Mock(BaseHTTPRequestHandler):
    calls = 0
    requests = []

    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"id": "mock", "context_window": 500000,
                           "max_output_tokens": 128000}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        Mock.requests.append(payload)
        Mock.calls += 1
        if Mock.calls == 1:
            body = sse({"reasoning_content": "TUI-REASONING-EVIDENCE",
                        "tool_calls": [{"index": 0, "id": "tui-tool-1", "type": "function",
                                        "function": {"name": "shell", "arguments": json.dumps({
                                            "command": "printf TUI_TOOL_OK"})}}]}, "tool_calls")
        else:
            body = sse({"content": "TUI-MOCK-DONE **bold**\n- item"}, "stop")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    if os.name != "posix":
        print("TUI_PTY_SKIPPED (POSIX-only smoke test)")
        return
    # Import POSIX-only modules after the platform check so this script can
    # at least be imported and skipped cleanly on Windows.
    import fcntl
    import pty
    import termios

    assert BINARY.is_file(), "cargo build --locked first"
    server = ThreadingHTTPServer(("127.0.0.1", 0), Mock)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="kroma-tui-pty-") as directory:
            root = Path(directory)
            home, workdir = root / "home", root / "workdir"
            home.mkdir()
            workdir.mkdir()
            config=home/".kroma/config.json"
            config.parent.mkdir(parents=True)
            config.write_text(json.dumps({"profiles":{"alternate":{
                "base_url":f"http://127.0.0.1:{server.server_port}/v1",
                "model":"alternate-mock","api_protocol":"chat","reasoning_effort":"low",
                "context_window":200000,"max_output_tokens":32000
            }}}))
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 110, 0, 0))
            before = termios.tcgetattr(slave)

            def terminal_setup():
                os.setsid()
                fcntl.ioctl(0, termios.TIOCSCTTY, 0)

            env = os.environ.copy()
            env.pop("KROMA_PROVIDER_PROFILE", None)
            env.pop("DISPLAY", None)
            env.pop("WAYLAND_DISPLAY", None)
            env.update(HOME=str(home), KROMA_WORKDIR=str(workdir),
                       KROMA_SESSION_FILE=str(workdir / "session.json"),
                       KROMA_BASE_URL=f"http://127.0.0.1:{server.server_port}/v1",
                       KROMA_MODEL="mock")
            child = subprocess.Popen([str(BINARY)], cwd=workdir, env=env,
                                     stdin=slave, stdout=slave, stderr=slave,
                                     preexec_fn=terminal_setup)
            captured = bytearray()

            def drain(seconds):
                until = time.monotonic() + seconds
                while time.monotonic() < until:
                    if select.select([master], [], [], 0.04)[0]:
                        try:
                            captured.extend(os.read(master, 65536))
                        except OSError:
                            break

            try:
                drain(0.35)
                initial_screen = re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]", "",
                                        captured.decode(errors="replace"))
                assert "Ctx — / 500k" in initial_screen, \
                    "Ctx must show the total window, not the 372k input budget"
                os.write(master,b"unfinished text")
                drain(0.08)
                os.write(master,b"\x03")  # Ctrl+C clears draft, not exit
                drain(0.12)
                assert child.poll() is None, "Ctrl+C must not exit with draft text"
                # Terminal bracketed paste becomes a compact log attachment.
                os.write(master,b"\x1b[200~" + b"long error output\n"*16 + b"\x1b[201~")
                drain(0.15)
                assert "288字符" in re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]", "",
                    captured.decode(errors="replace")), "pasted log should be folded inline"
                os.write(master,b"\x03")  # Clear attached log and keep TUI open
                drain(0.12)
                assert child.poll() is None
                os.write(master,b"\x1b[Z")  # Shift+Tab: auto -> off
                drain(0.15)
                os.write(master,b"\x1b[Z")  # off -> low
                drain(0.15)
                os.write(master, b"hello Kroma\r")
                limit = time.monotonic() + 8
                while Mock.calls < 2 and time.monotonic() < limit:
                    drain(0.10)
                assert Mock.calls == 2, "agent did not complete tool continuation"
                assert Mock.requests[0].get("reasoning_effort") == "low", \
                    "Shift+Tab must change the next provider request"
                drain(0.30)
                os.write(master, b"\x0f")  # Ctrl+O show tool output
                os.write(master, b"\x12")  # Ctrl+R show model reasoning
                drain(0.25)
                os.write(master, b"/theme light\r")
                drain(0.25)
                os.write(master, b"/history\r")
                drain(0.25)
                switch_start = len(captured)
                os.write(master,b"/model alternate\r")
                drain(0.40)
                switched_screen = re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]", "",
                                         captured[switch_start:].decode(errors="replace"))
                assert "200k" in switched_screen, \
                    "model switch must update Ctx to the new total window"
                os.write(master,b"/usage\r")
                drain(0.15)
                os.write(master,b"!printf LOCAL_SHELL_OK\r")
                drain(0.25)
                assert Mock.calls==2,"local shell and /usage must not call a model"
                # A cancelled manual Shell must not leave its process group
                # alive, or make the TUI unresponsive to later messages.
                started = workdir / "manual_started"
                survived = workdir / "manual_survived"
                os.write(master, (f"!printf started > {started}; sleep 1.5; "
                                  f"printf survived > {survived}\r").encode())
                deadline = time.monotonic() + 5
                while not started.exists() and time.monotonic() < deadline:
                    drain(0.05)
                assert started.exists(), "manual Shell never started"
                os.write(master, b"\x03")
                drain(0.20)
                assert child.poll() is None, "cancelled manual Shell killed the TUI"
                drain(1.55)
                assert not survived.exists(), "TUI Ctrl+C did not terminate Shell group"
                # Mixed attachments must remain ordered in the API request:
                # log -> image -> log -> image, not logs first / images last.
                image_file=workdir/"demo.png"
                image_file.write_bytes(base64.b64decode(
                    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9YqGTlkAAAAASUVORK5CYII="))
                second_image=workdir/"demo-second.png"
                second_image.write_bytes(image_file.read_bytes())
                os.write(master,b"\x1b[200~"+b"first failure\n"*14+b"\x1b[201~")
                drain(0.15)
                os.write(master,b"\x1bv")
                drain(0.15)
                os.write(master,str(image_file).encode()+b"\r")
                drain(0.15)
                os.write(master,b"\x1b[200~"+b"second failure\n"*17+b"\x1b[201~")
                drain(0.15)
                os.write(master,b"\x1bv")
                drain(0.15)
                os.write(master,str(second_image).encode()+b"\r")
                drain(0.15)
                os.write(master,b"Compare these four attachments in paste order\r")
                limit=time.monotonic()+8
                while Mock.calls<3 and time.monotonic()<limit:
                    drain(0.10)
                assert Mock.calls==3, "mixed attachment task did not reach provider"
                assert Mock.requests[2]["model"]=="alternate-mock", \
                    "/model must change the actual request model, not just UI label"
                user=next(m for m in reversed(Mock.requests[2]["messages"]) if m.get("role")=="user")
                blocks=user["content"]
                assert len(blocks)==7, f"incorrect multimodal layout: {len(blocks)} blocks"
                assert "Pasted text" in blocks[0]["text"]
                assert "Image demo.png, attachment #2/4" in blocks[1]["text"]
                assert blocks[2]["type"]=="image_url"
                assert "data:image/png;base64," in blocks[2]["image_url"]["url"]
                assert "Pasted text" in blocks[3]["text"]
                assert "Image demo-second.png, attachment #4/4" in blocks[4]["text"]
                assert blocks[5]["type"]=="image_url"
                assert "Compare these four" in blocks[6]["text"]
                display=re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]", "", captured.decode(errors="replace"))
                assert "demo.png" in display and "demo-second.png" in display
                drain(0.20)
                os.write(master,b"Summarize earlier messages\r")
                limit=time.monotonic()+8
                while Mock.calls<4 and time.monotonic()<limit:
                    drain(0.10)
                assert Mock.calls==4, "follow-up after image attachment failed"
                old_user=next(m for m in Mock.requests[3]["messages"] if
                    m.get("role")=="user" and isinstance(m.get("content"),list))
                assert "image_url" in json.dumps(old_user), "one immediate follow-up should retain its images"
                os.write(master,b"One more question about the project\r")
                limit=time.monotonic()+8
                while Mock.calls<5 and time.monotonic()<limit:
                    drain(0.10)
                assert Mock.calls==5
                older_user=next(m for m in Mock.requests[4]["messages"] if
                    m.get("role")=="user" and isinstance(m.get("content"),list))
                assert "image_url" not in json.dumps(older_user), "older images must become references"
                assert "omitted from model input" in json.dumps(older_user)
                drain(0.20)
                # Recall the mixed-media prompt using terminal history.
                # Visual labels without actual image data are a serious bug.
                for _ in range(3):
                    os.write(master,b"\x1b[A")
                    drain(0.10)
                os.write(master,b"\r")
                limit=time.monotonic()+8
                while Mock.calls<6 and time.monotonic()<limit:
                    drain(0.10)
                assert Mock.calls==6, "recalled media prompt did not reach provider"
                recalled=next(m for m in reversed(Mock.requests[5]["messages"])
                    if m.get("role")=="user")
                assert "image_url" in json.dumps(recalled["content"]), \
                    "recalled media must keep real image references, not just text labels"
                (workdir/"src").mkdir()
                (workdir/"src/agent.rs").write_text("source")
                os.write(master,b"Review @src/age")
                drain(0.10)
                os.write(master,b"\t\r")
                limit=time.monotonic()+8
                while Mock.calls<7 and time.monotonic()<limit:
                    drain(0.10)
                assert Mock.calls==7,"@file path completion was not submitted"
                assert "src/agent.rs" in json.dumps(Mock.requests[6]), \
                    "Tab must complete the @file path into model-visible reference"
                drain(0.20)
                os.write(master,b"/rename UI smoke test\r")
                drain(0.20)
                os.write(master,b"/export conversation.md\r")
                drain(0.20)
                os.write(master, b"\x03")  # Ctrl+C with no draft quits TUI
                deadline = time.monotonic() + 5
                while child.poll() is None and time.monotonic() < deadline:
                    drain(0.10)
                assert child.wait(timeout=2) == 0, "TUI process failed or did not exit"
                drain(0.05)
                after = termios.tcgetattr(slave)
                screen = bytes(captured)
                assert b"Kroma" in screen and b"TUI-MOCK-DONE" in screen
                assert b"Ctx" in screen and b"Think low" in screen
                assert b"printf TUI_TOOL_OK" in screen
                assert b"TUI-REASONING-EVIDENCE" in screen
                assert b"TUI_TOOL_OK" in screen
                assert b"checkpoint(s)" in screen, "history command should be readable in the transcript"
                assert b"\x1b[?1049h" in screen and b"\x1b[?1049l" in screen
                assert bool(before[3] & termios.ICANON) == bool(after[3] & termios.ICANON)
                theme_file = home / ".kroma/theme.toml"
                assert 'preset = "light"' in theme_file.read_text()
                wal = (workdir / "session.json.wal").read_text()
                assert "TUI-MOCK-DONE" in wal and "TUI_TOOL_OK" in wal
                assert "demo-second.png" in wal
                assert "LOCAL_SHELL_OK" in captured.decode(errors="replace")
                assert (workdir/"conversation.md").is_file()
                assert "UI smoke test" in (workdir/"session.names.json").read_text()
                print("TUI_PTY_SMOKE: PASS; Ctrl+C, recalled media, ordered mixed attachments, Alt+V image, folded logs, multimodal payload, streaming, Shell, theme, sessions, terminal restoration")
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
                os.close(master)
                os.close(slave)
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
