#!/usr/bin/env python3
"""Offline end-to-end regression for usage/profiling and DeepSeek tool replay.

Run after cargo build --release: python3 benchmarks/mock_profile_smoke.py
Does not require credentials, provider access, or third-party Python packages.
"""

import json
import os
import pathlib
import re
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer


class MockProvider(BaseHTTPRequestHandler):
    requests = []

    def log_message(self, *_args):
        pass

    def do_POST(self):
        assert self.path == "/v1/chat/completions", self.path
        length = int(self.headers["Content-Length"])
        payload = json.loads(self.rfile.read(length))
        MockProvider.requests.append(payload)
        number = len(MockProvider.requests)
        if number == 1:
            events = [
                {"choices": [{"delta": {"reasoning_content": "Inspect working directory"},
                              "finish_reason": None}]},
                {"choices": [{
                    "delta": {"tool_calls": [{
                        "index": 0, "id": "mock-call-1", "type": "function",
                        "function": {"name": "shell", "arguments": '{"command":"printf ok"}'},
                    }]},
                    "finish_reason": "tool_calls",
                }]},
                {"choices": [], "usage": {
                    "prompt_tokens": 100, "completion_tokens": 12,
                    "prompt_cache_hit_tokens": 20,
                }},
            ]
        else:
            assert number == 2, "unexpected extra model round"
            messages = payload["messages"]
            assistant = next(m for m in messages if m["role"] == "assistant"
                             and m.get("tool_calls"))
            assert assistant["reasoning_content"] == "Inspect working directory"
            assert assistant["tool_calls"][0]["id"] == "mock-call-1"
            tool = next(m for m in messages if m["role"] == "tool")
            assert tool["tool_call_id"] == "mock-call-1"
            assert json.loads(tool["content"])["stdout"] == "ok"
            events = [
                {"choices": [{"delta": {"content": "Completed mock task."},
                              "finish_reason": "stop"}]},
                {"choices": [], "usage": {
                    "prompt_tokens": 120, "completion_tokens": 10,
                    "prompt_cache_hit_tokens": 30,
                }},
            ]
        wire = "".join(f"data: {json.dumps(event)}\n\n" for event in events)
        wire += "data: [DONE]\n\n"
        body = wire.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)


def run():
    binary = pathlib.Path(os.environ.get("KROMA_TEST_BINARY", "target/release/kroma")).absolute()
    assert binary.exists(), "Build the Release binary before running the smoke test"
    MockProvider.requests.clear()
    server = HTTPServer(("127.0.0.1", 0), MockProvider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="kroma-profile-smoke-") as temp:
            home = pathlib.Path(temp)
            history = home / "session.json"
            env = os.environ.copy()
            env.update({
                "HOME": str(home),
                "KROMA_MODEL": "mock-thinking",
                "KROMA_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
                "KROMA_WORKDIR": str(home),
                "KROMA_SESSION_FILE": str(history),
                "KROMA_DEEPSEEK_THINKING": "high",
                "KROMA_PROFILE": "1",
            })
            env.pop("KROMA_API_KEY", None)
            outcome = subprocess.run(
                [str(binary), "Inspect and report."], env=env, cwd=home,
                capture_output=True, text=True, timeout=20, check=True,
            )
            assert "Completed mock task." in outcome.stdout
            assert "Inspect working directory" not in outcome.stdout
            assert re.search(r"\[profile\].*model_requests=2.*tool_calls=1", outcome.stderr)
            assert "input: 220 | output: 22 | cached: 50" in outcome.stderr
            assert len(MockProvider.requests) == 2
            assert all(req.get("thinking") == {"type": "enabled"}
                       for req in MockProvider.requests)
            assert all(req.get("reasoning_effort") == "high"
                       for req in MockProvider.requests)
            raw = history.with_suffix(".json.wal").read_text()
            assert "Inspect working directory" in raw
            assert "mock-call-1" in raw
            line = next(line for line in outcome.stderr.splitlines()
                        if line.startswith("[profile]"))
            print("mock_e2e: two requests, one tool, thinking replay, WAL, usage OK")
            print(line)
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    run()
