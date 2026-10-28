#!/usr/bin/env python3
"""Offline TUI model/profile persistence regression across real restarts.

Uses a local Chat mock, POSIX PTY, isolated saved profiles and sessions.
Run with KROMA_TEST_BINARY=target/debug/kroma; no real API calls.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


BINARY = str(Path(os.environ.get("KROMA_TEST_BINARY", "target/debug/kroma")).resolve())


class Model(BaseHTTPRequestHandler):
    requests = []

    def log_message(self, *_args):
        pass

    def send(self, mime, content):
        content = content.encode()
        self.send_response(200)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(content)))
        self.end_headers()
        self.wfile.write(content)

    def do_GET(self):
        # Do not let metadata accidentally override the profile under test.
        self.send("application/json", json.dumps({"id": "mock"}))

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        Model.requests.append(request)
        self.send("text/event-stream", "data: " + json.dumps({
            "choices": [{"delta": {"content": "mock response"}, "finish_reason": "stop"}]
        }) + "\n\ndata: [DONE]\n\n")


def setup_env(home, workdir, extra=None):
    env = {key: value for key, value in os.environ.items()
           if not key.startswith("KROMA_")}
    env.update(HOME=str(home), KROMA_WORKDIR=str(workdir),
               KROMA_SESSION_FILE=str(workdir / "session.json"), TERM="xterm-256color")
    if extra:
        env.update(extra)
    return env


def launch(home, workdir, steps, extra=None):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 120, 0, 0))

    def setup_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    child = subprocess.Popen([BINARY], cwd=workdir, env=setup_env(home, workdir, extra),
                             stdin=slave, stdout=slave, stderr=slave,
                             preexec_fn=setup_terminal)
    captured = bytearray()

    def drain(duration):
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.04)[0]:
                try:
                    captured.extend(os.read(master, 65536))
                except OSError:
                    break

    try:
        drain(0.3)
        assert child.poll() is None, captured[-800:].decode(errors="replace")
        for step in steps:
            if step == "SHIFT_TAB":
                os.write(master, b"\x1b[Z")
                drain(0.1)
            else:
                previous = len(Model.requests)
                os.write(master, step.encode() + b"\r")
                if step.startswith("ASK "):
                    deadline = time.monotonic() + 8
                    while len(Model.requests) == previous and time.monotonic() < deadline:
                        drain(0.08)
                    assert len(Model.requests) > previous, (
                        f"no request for {step}: " + captured[-1400:].decode(errors="replace"))
                    drain(0.15)
                else:
                    drain(0.25)
        os.write(master, b"/exit\r")
        deadline = time.monotonic() + 5
        while child.poll() is None and time.monotonic() < deadline:
            drain(0.06)
        assert child.wait(timeout=2) == 0, captured[-1600:].decode(errors="replace")
        return bytes(captured)
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
        os.close(slave)


def main():
    assert Path(BINARY).is_file(), "build current debug Kroma binary first"
    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="kroma-astra-tui-profile-") as temp:
            home, workdir = Path(temp) / "home", Path(temp) / "project"
            config_dir = home / ".kroma"
            config_dir.mkdir(parents=True)
            workdir.mkdir()
            url = f"http://127.0.0.1:{server.server_port}/v1"

            def profile(model, reasoning, context, output, levels=None, thinking=None):
                result = {"base_url": url, "model": model, "api_protocol": "chat",
                          "reasoning_effort": reasoning, "context_window": context,
                          "max_output_tokens": output}
                if levels is not None:
                    result["thinking_levels"] = levels
                if thinking is not None:
                    result["deepseek_thinking"] = thinking
                return result

            profiles = {"active": "a", "profiles": {
                "a": profile("mock-a", "low", 32768, 8192, ["low"]),
                "b": profile("mock-b", "high", 500000, 128000, ["high", "xhigh"]),
                "c": profile("mock-c", "auto", 65536, 4096, thinking="enabled"),
            }}
            (config_dir / "config.json").write_text(json.dumps(profiles))

            launch(home, workdir, ["/model b", "SHIFT_TAB", "ASK first request"])
            first = Model.requests[-1]
            assert first["model"] == "mock-b"
            assert first["max_tokens"] == 128000
            assert first["reasoning_effort"] == "xhigh", first

            # Actual restart: stored profile and chosen tier must survive.
            launch(home, workdir, ["ASK restored model"])
            restored = Model.requests[-1]
            assert restored["model"] == "mock-b"
            assert restored["max_tokens"] == 128000
            assert restored["reasoning_effort"] == "xhigh"
            print("TUI_PROFILE_RESTART: PASS; model, output, effort restored")

            launch(home, workdir, ["/model a", "ASK output A", "/model b",
                                   "ASK output B", "/resume default", "ASK resumed output"],
                   {"KROMA_MAX_OUTPUT_TOKENS": "2048"})
            capped = Model.requests[-3:]
            assert [r["model"] for r in capped] == ["mock-a", "mock-b", "mock-b"]
            assert all(r["max_tokens"] == 2048 for r in capped), capped
            print("TUI_ENV_OUTPUT_OVERRIDE: PASS; /model and /resume respect process cap")

            launch(home, workdir, ["/model c", "ASK deepseek enabled"])
            assert Model.requests[-1]["thinking"]["type"] == "enabled"
            launch(home, workdir, ["ASK deepseek restored"])
            assert Model.requests[-1]["model"] == "mock-c"
            assert Model.requests[-1]["thinking"]["type"] == "enabled"

            # Simulate a historical file from before the saved effort fix.
            model_file = workdir / "session.models.json"
            old = json.loads(model_file.read_text())
            old["default"]["thinking"] = "enabled"
            model_file.write_text(json.dumps(old))
            launch(home, workdir, ["ASK legacy enabled restored"])
            assert Model.requests[-1]["model"] == "mock-c"
            assert Model.requests[-1]["thinking"]["type"] == "enabled"
            print("TUI_DEEPSEEK_RESTART: PASS; current and legacy enabled sessions")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
