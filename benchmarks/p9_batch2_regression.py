#!/usr/bin/env python3
"""Offline end-to-end P9 batch-2 retry, interruption and usage regressions."""
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/release/kroma"


def sse(delta, finish):
    events = [{"choices": [{"delta": delta, "finish_reason": finish}]}]
    return ("".join("data: " + json.dumps(item) + "\n\n" for item in events)
            + "data: [DONE]\n\n").encode()


class MockProvider(BaseHTTPRequestHandler):
    scenario = ""
    requests = []
    response_sent = threading.Event()

    def log_message(self, *_args):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.requests.append(request)
        i = len(self.requests)
        scenario = self.scenario
        if scenario == "transient" and i == 1:
            return self.reply(429, b"retry later", {"Retry-After": "0"})
        if scenario == "transient" and i == 2:
            return self.reply(503, b"temporarily unavailable")
        if scenario == "unauthorized":
            return self.reply(401, b"wrong key")
        if scenario == "long_retry_after":
            return self.reply(429, b"wait a minute", {"Retry-After": "60"})
        if scenario == "tool_then_error" and i == 1:
            command = 'python3 -c "print(123)"'
            return self.reply(200, sse({"tool_calls": [{
                "index": 0, "id": "completed-tool", "type": "function",
                "function": {"name": "shell", "arguments": json.dumps({"command": command})},
            }]}, "tool_calls"))
        if scenario == "tool_then_error":
            assert any(m.get("tool_call_id") == "completed-tool"
                       for m in request["messages"])
            return self.reply(401, b"cannot finish")
        if scenario == "partial_tool":
            # Deliberately missing finish reason and incomplete JSON args.
            event = {"choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "half-call", "type": "function",
                "function": {"name": "shell", "arguments": '{"command":"touch accidental'},
            }]}, "finish_reason": None}]}
            return self.reply(200, ("data: " + json.dumps(event) + "\n\n").encode())
        if scenario == "cancelled":
            event = {"choices": [{"delta": {"content": "partial"}, "finish_reason": None}]}
            data = ("data: " + json.dumps(event) + "\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(data)
            self.wfile.flush()
            self.response_sent.set()
            time.sleep(2)
            return
        assert scenario == "transient" and i == 3, (scenario, i)
        return self.reply(200, sse({"content": "recovered"}, "stop"))

    def reply(self, status, data, headers=None):
        self.send_response(status)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)


def run_one(scenario):
    handler = type("ScenarioHandler", (MockProvider,), {
        "scenario": scenario, "requests": [],
        "response_sent": threading.Event(),
    })
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="kroma-p9b2-" + scenario + "-") as d:
            env = os.environ.copy()
            for key in ("KROMA_API_KEY", "KROMA_PROVIDER_PROFILE", "KROMA_DEEPSEEK_THINKING"):
                env.pop(key, None)
            env.update(
                HOME=d, KROMA_MODEL="mock", KROMA_WORKDIR=d,
                KROMA_SESSION_FILE=str(Path(d) / "session.json"),
                KROMA_CONFIG_FILE=str(Path(d) / "none.json"),
                KROMA_BASE_URL=f"http://127.0.0.1:{server.server_port}/v1",
                KROMA_PROFILE="1",
            )
            cmd = [str(BINARY), "Inspect the project and report."]
            if scenario == "cancelled":
                p = subprocess.Popen(cmd, env=env, cwd=d, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True)
                assert handler.response_sent.wait(5), "server never started streaming"
                p.send_signal(signal.SIGINT)
                stdout, stderr = p.communicate(timeout=10)
                assert p.returncode != 0, (stdout, stderr)
                assert "[usage] task_status=cancelled" in stderr, stderr
                assert "[task] Cancelled" in stderr, stderr
                assert len(handler.requests) == 1
            else:
                p = subprocess.run(cmd, env=env, cwd=d, capture_output=True, text=True,
                                   timeout=15)
                stdout, stderr = p.stdout, p.stderr
                if scenario == "transient":
                    assert p.returncode == 0, stderr
                    assert stdout.strip() == "recovered", stdout
                    assert len(handler.requests) == 3
                    assert "[usage] task_status=completed" in stderr
                else:
                    assert p.returncode != 0, (scenario, stderr)
                    assert "[usage] task_status=failed" in stderr, stderr
                    count = 2 if scenario == "tool_then_error" else 1
                    assert len(handler.requests) == count, (scenario, len(handler.requests), stderr)
                    if scenario == "tool_then_error":
                        assert "completed_model_requests=1 | completed_tool_calls=1" in stderr
                    if scenario == "partial_tool":
                        assert not (Path(d) / "accidental").exists()
                        assert "Kroma interrupted" in (Path(d) / "session.json.wal").read_text()
            print("P9_BATCH2", scenario, "PASS", "requests", len(handler.requests), flush=True)
    finally:
        server.shutdown()
        server.server_close()


def main():
    assert BINARY.is_file(), "build release binary first"
    for scenario in ("transient", "unauthorized", "long_retry_after", "tool_then_error",
                     "partial_tool", "cancelled"):
        run_one(scenario)


if __name__ == "__main__":
    main()
