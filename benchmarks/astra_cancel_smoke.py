#!/usr/bin/env python3
"""Offline SIGINT regression for JSON and manual CLI Shell process groups.

Uses temporary HOME, a local mock endpoint, and only isolated child processes.
No real credentials or changes to the developer's Kroma sessions.

Run: KROMA_TEST_BINARY=target/debug/kroma python3 benchmarks/astra_cancel_smoke.py
"""
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


BINARY = str(Path(os.environ.get("KROMA_TEST_BINARY", "target/debug/kroma")).resolve())


class Provider(BaseHTTPRequestHandler):
    command = None
    tool_sent = False
    posted = 0

    def log_message(self, *_args):
        pass

    def reply(self, mime, content):
        content = content.encode()
        self.send_response(200)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(content)))
        self.end_headers()
        self.wfile.write(content)

    def do_GET(self):
        self.reply("application/json", json.dumps({
            "id": "mock", "context_window": 128000, "max_output_tokens": 4096,
        }))

    def do_POST(self):
        _ = self.rfile.read(int(self.headers["Content-Length"]))
        Provider.posted += 1
        if Provider.command is not None and not Provider.tool_sent:
            Provider.tool_sent = True
            delta = {"tool_calls": [{"index": 0, "id": "astra-cancel-call",
                "type": "function", "function": {"name": "shell",
                    "arguments": json.dumps({"command": Provider.command})}}]}
            reason = "tool_calls"
        else:
            delta = {"content": "confirmed after restart"}
            reason = "stop"
        self.reply("text/event-stream", "data: " + json.dumps({
            "choices": [{"delta": delta, "finish_reason": reason}],
        }) + "\n\ndata: [DONE]\n\n")


def wait_for_marker(marker, child):
    until = time.monotonic() + 8
    while time.monotonic() < until:
        if marker.exists():
            return
        if child.poll() is not None:
            raise AssertionError(f"Kroma exited before Shell started ({child.returncode})")
        time.sleep(0.025)
    raise AssertionError("Shell never reached the start marker")


def command_for(path):
    # The post-sleep marker proves whether cancellation killed the child group.
    start, late = path / "started", path / "survived"
    return (f"printf started > {shlex.quote(str(start))}; "
            f"sleep 1.5; printf survived > {shlex.quote(str(late))}")


def base_env(home, workdir, port):
    env = {key: value for key, value in os.environ.items()
           if not key.startswith("KROMA_")}
    env.update(HOME=str(home), KROMA_MODEL="mock",
               KROMA_BASE_URL=f"http://127.0.0.1:{port}/v1",
               KROMA_WORKDIR=str(workdir),
               KROMA_SESSION_FILE=str(workdir / "session.json"))
    env.pop("KROMA_API_KEY", None)
    return env


def main():
    assert Path(BINARY).is_file(), "build current debug Kroma binary first"
    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="kroma-astra-cancel-") as temp:
            base = Path(temp)
            home, json_dir, cli_dir = (base / "home", base / "json", base / "cli")
            for folder in (home, json_dir, cli_dir):
                folder.mkdir()
            env = base_env(home, json_dir, server.server_port)

            Provider.command = command_for(json_dir)
            Provider.tool_sent = False
            child = subprocess.Popen([BINARY, "--json", "run cancellable shell"],
                                     cwd=json_dir, env=env, stdin=subprocess.DEVNULL,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     text=True)
            try:
                wait_for_marker(json_dir / "started", child)
                child.send_signal(signal.SIGINT)
                stdout, stderr = child.communicate(timeout=5)
                time.sleep(1.65)
                assert not (json_dir / "survived").exists(), (
                    "JSON cancellation left a Shell child executing")
                events = [json.loads(line) for line in stdout.splitlines()]
                assert any(e.get("type") == "task_finished" and
                           e.get("status") == "cancelled" for e in events), events
                assert "task cancelled" in stderr.lower()
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()

            # Replaying WAL must report uncertainty without re-executing the
            # interrupted tool command after a process restart.
            request_count = Provider.posted
            restarted = subprocess.run([BINARY, "--json", "continue"], env=env,
                                       cwd=json_dir, stdin=subprocess.DEVNULL,
                                       capture_output=True, text=True, timeout=8)
            assert restarted.returncode == 0, restarted.stderr
            assert Provider.posted == request_count + 1
            assert "confirmed after restart" in restarted.stdout
            assert not (json_dir / "survived").exists()
            print("JSON_CANCEL: PASS; child stopped, cancellation durable, restart no replay")

            Provider.command = None
            cli_env = base_env(home, cli_dir, server.server_port)
            child = subprocess.Popen([BINARY, "--cli"], cwd=cli_dir, env=cli_env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True, bufsize=1)
            try:
                child.stdin.write("!" + command_for(cli_dir) + "\n")
                child.stdin.flush()
                wait_for_marker(cli_dir / "started", child)
                child.send_signal(signal.SIGINT)
                time.sleep(0.12)
                assert child.poll() is None, "manual Shell SIGINT exited Kroma"
                stdout, stderr = child.communicate("/exit\n", timeout=6)
                time.sleep(1.65)
                assert child.returncode == 0, (stdout, stderr)
                assert "[shell] Cancelled" in stderr
                assert not (cli_dir / "survived").exists(), (
                    "manual CLI Shell continued after Ctrl+C")
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
            print("CLI_MANUAL_SHELL_CANCEL: PASS; SIGINT stops child, CLI remains usable")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
