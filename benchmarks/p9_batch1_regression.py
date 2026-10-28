#!/usr/bin/env python3
"""Offline P9 batch 1: real Shell output overflow, restart and tool ID continuity."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/release/kroma"


class Provider(BaseHTTPRequestHandler):
    requests = []
    scenario = "large"

    def log_message(self, *args):
        pass

    def do_POST(self):
        content = self.rfile.read(int(self.headers["Content-Length"]))
        request = json.loads(content)
        Provider.requests.append(request)
        number = len(Provider.requests)
        if Provider.scenario == "large" and number == 1:
            command = "python3 -c \"print('X'*500000)\""
            delta = {"tool_calls": [{
                "index": 0, "id": "huge-one", "type": "function",
                "function": {
                    "name": "shell", "arguments": json.dumps({"command": command})
                },
            }]}
            reason = "tool_calls"
        else:
            tool_calls = [
                message for message in request["messages"]
                if message["role"] == "tool"
            ]
            assert tool_calls, "second request must include a tool result"
            result = json.loads(tool_calls[-1]["content"])
            assert result["exit_code"] == 0, result
            assert result.get("stdout_file"), "full output must be file-backed"
            log = Path(result["stdout_file"])
            assert log.exists()
            assert len(log.read_bytes()) > 500000
            assert len(result["stdout"]) < 12000
            delta = {"content": "SUCCESS: resumed after oversized tool output"}
            reason = "stop"
        sse = ("data: " + json.dumps({
            "choices": [{"delta": delta, "finish_reason": reason}]
        }) + "\n\ndata: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(sse)))
        self.end_headers()
        self.wfile.write(sse)


def run():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="kroma-p9-batch1-") as temp:
            path = Path(temp)
            env = os.environ.copy()
            for key in ("KROMA_API_KEY", "KROMA_PROVIDER_PROFILE", "KROMA_DEEPSEEK_THINKING"):
                env.pop(key, None)
            env.update(
                HOME=temp,
                KROMA_MODEL="mock",
                KROMA_BASE_URL=f"http://127.0.0.1:{server.server_port}/v1",
                KROMA_WORKDIR=temp,
                KROMA_SESSION_FILE=str(path / "session.json"),
                KROMA_CONFIG_FILE=str(path / "missing-config.json"),
            )
            response = subprocess.run(
                [str(BINARY), "Run a command that prints a large log."],
                cwd=path, env=env, capture_output=True, text=True, timeout=30,
            )
            assert response.returncode == 0, response.stderr[-1500:]
            assert "SUCCESS: resumed" in response.stdout
            assert len(Provider.requests) == 2
            print("P9_BATCH1_LARGE_OUTPUT: PASS, 500K bytes retained, model received file preview")
            # Subsequent process startup must replay the saved history
            # without re-executing the previous Shell call.
            result = subprocess.run(
                [str(BINARY), "Confirm completion."],
                cwd=path, env=env, capture_output=True, text=True, timeout=30,
            )
            assert result.returncode == 0, result.stderr[-1500:]
            assert len(Provider.requests) == 3
            assert "SUCCESS: resumed" in result.stdout
            history = (path / "session.json.wal").read_text()
            assert "huge-one" in history
            print("P9_BATCH1_RESTART: PASS, saved tool call preserved, no duplicate invocation")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    run()
