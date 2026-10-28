#!/usr/bin/env python3
"""Offline failure-boundary checks for legacy oversized session histories.

Simulates a previously saved conversation containing inline tool output
that predates P9's file-backed Shell threshold. No external model or
credentials are required. The original raw snapshot must not be modified.
"""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/release/kroma"


def message(role, content=None, **kwargs):
    return {"role": role, "content": content, **kwargs}


def legacy_history(workdir, sizes):
    calls = [
        {"id": f"legacy-{i}", "type": "function",
         "function": {"name": "shell", "arguments": '{"command":"echo legacy"}'}}
        for i in range(len(sizes))
    ]
    raw = [
        message("system", "You are Kroma. Only tool: shell."),
        message("user", "Run legacy checks."),
        message("assistant", None, tool_calls=calls),
    ]
    for i, size in enumerate(sizes):
        result = json.dumps({
            "exit_code": i % 2,
            "stdout": "K" * size,
            "stderr": f"legacy failure evidence {i}" if i % 2 else "",
        })
        raw.append(message("tool", result, tool_call_id=f"legacy-{i}"))
    return {
        "version": 3,
        "workdir": str(workdir),
        "generation": 3001,
        "messages": raw,
        "compression": {
            "summaries": [], "covered_until": 1, "consumed_until": 1,
            "cleared_tools": [], "revision": 0,
        },
    }


class Server(BaseHTTPRequestHandler):
    requests = []

    def log_message(self, *_):
        pass

    def do_POST(self):
        data = self.rfile.read(int(self.headers["Content-Length"]))
        self.__class__.requests.append(json.loads(data))
        response = {
            "choices": [{
                "delta": {"content": "Resumed legacy session successfully"},
                "finish_reason": "stop",
            }]
        }
        body = ("data: " + json.dumps(response) + "\n\ndata: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def check(sizes, case):
    Server.requests = []
    server = ThreadingHTTPServer(("127.0.0.1", 0), Server)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix=f"kroma-p9-{case}-") as directory:
            directory = Path(directory)
            history = legacy_history(directory, sizes)
            snapshot = directory / "history.json"
            snapshot.write_text(json.dumps(history))
            env = os.environ.copy()
            for key in ("KROMA_API_KEY", "KROMA_PROVIDER_PROFILE", "KROMA_DEEPSEEK_THINKING"):
                env.pop(key, None)
            env.update(
                HOME=str(directory),
                KROMA_MODEL="local-mock",
                KROMA_BASE_URL=f"http://127.0.0.1:{server.server_port}/v1",
                KROMA_WORKDIR=str(directory),
                KROMA_SESSION_FILE=str(snapshot),
                KROMA_CONFIG_FILE=str(directory / "no-config.json"),
            )
            agent = subprocess.run(
                [str(BINARY), "Continue and inspect the previous tool results."],
                cwd=directory, env=env,
                capture_output=True, text=True, timeout=30,
            )
            assert agent.returncode == 0, agent.stderr[-2000:]
            assert len(Server.requests) == 1, Server.requests
            projected = [
                json.loads(item["content"])
                for item in Server.requests[0]["messages"]
                if item["role"] == "tool"
            ]
            overflow = [r for r in projected if "kroma_context_overflow" in r]
            assert overflow, "must project overlarge legacy output for this request"
            assert 0 < len(overflow) <= len(sizes)
            for item in overflow:
                idx = item["archive_message_index"]
                assert item["archive_file"] == str(snapshot)
                assert item["exit_code"] == int(history["messages"][idx]["tool_call_id"].split("-")[1]) % 2
            persisted = json.loads(snapshot.read_text())
            assert persisted["messages"] == history["messages"], "original raw evidence must survive"
            assert len(persisted["messages"]) == 3 + len(sizes)
            assert (directory / "history.json.wal").is_file()
            print(f"P9_LEGACY_{case.upper()}: PASS, {len(overflow)}/{len(sizes)} raw results projected; archive untouched", flush=True)
    finally:
        server.shutdown()
        server.server_close()


def main():
    check([500_000], "large")
    check([72_000] * 6, "accumulated")


if __name__ == "__main__":
    main()
