#!/usr/bin/env python3
"""Offline Pi/Kroma payload comparison with dummy credentials, no model traffic.

A local HTTP SSE stub receives both requests and prints only lengths, tool
names and request parameters. Never writes captured prompt/tool content.
"""
from __future__ import annotations
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL = "deepseek-v4.1-flash"
PI = Path.home() / ".pi" / "agent" / "bin" / "pi.cmd"
KROMA = Path("D:/Kroma/target-kroma-check/release/kroma.exe")
PROMPT = "Reply with exactly: OK"


def summarize(request: dict) -> dict:
    messages = request.get("messages") or []
    tools = request.get("tools") or []
    system = [msg for msg in messages if msg.get("role") == "system"]
    return {
        "request_json_bytes": len(json.dumps(request, separators=(",", ":"), ensure_ascii=False).encode()),
        "messages_json_bytes": len(json.dumps(messages, separators=(",", ":"), ensure_ascii=False).encode()),
        "system_chars": sum(len(str(msg.get("content") or "")) for msg in system),
        "tools_json_bytes": len(json.dumps(tools, separators=(",", ":"), ensure_ascii=False).encode()),
        "tool_names": [t.get("function", {}).get("name") for t in tools],
        "tool_descriptions_chars": sum(len(str(t.get("function", {}).get("description") or "")) for t in tools),
        "reasoning_effort": request.get("reasoning_effort"),
        "thinking": request.get("thinking"),
        "max_tokens": request.get("max_tokens"),
        "stream_options": request.get("stream_options"),
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        pass

    def _send(self, status: int, kind: str, data: bytes):
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
        self.wfile.flush()

    def do_GET(self):
        self._send(200, "application/json", json.dumps({
            "object":"list", "data":[{"id":MODEL,"object":"model","context_window":500000}]
        }).encode())

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        data = self.rfile.read(length)
        try:
            request = json.loads(data)
            self.server.observed.append(summarize(request))
        except Exception:
            self._send(400, "application/json", b'{"error":"bad request"}')
            return
        if not request.get("stream", True):
            response = {"id":"stub","object":"chat.completion","model":MODEL,
                        "choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}
            self._send(200, "application/json", json.dumps(response).encode())
            return
        events = [
            {"id":"stub","object":"chat.completion.chunk","model":MODEL,
             "choices":[{"index":0,"delta":{"role":"assistant","content":"OK"},"finish_reason":None}]},
            {"id":"stub","object":"chat.completion.chunk","model":MODEL,
             "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
             "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}},
        ]
        body = "".join("data: " + json.dumps(event) + "\n\n" for event in events)
        body += "data: [DONE]\n\n"
        self._send(200, "text/event-stream", body.encode())


def main():
    with tempfile.TemporaryDirectory(prefix="kroma-pi-capture-") as temporary:
        root = Path(temporary)
        project = root / "project"
        project.mkdir()
        with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            server.observed = []
            server_thread = threading.Thread(target=server.serve_forever, daemon=True)
            server_thread.start()
            url = f"http://127.0.0.1:{server.server_address[1]}/v1"
            pi_dir = root / "pi"
            pi_dir.mkdir()
            (pi_dir / "models.json").write_text(json.dumps({"providers":{
                "capture":{"baseUrl":url, "apiKey":"dummy-only", "api":"openai-completions",
                           "models":[{"id":MODEL,"name":"DeepSeek Test","api":"openai-completions",
                                      "reasoning":True,"input":["text"],
                                      "contextWindow":500000,"maxTokens":393216,
                                      "thinkingLevelMap":{"high":"high"},
                                      "compat":{"supportsReasoningEffort":True,"supportsDeveloperRole":False}}]}
            }}), encoding="utf-8")
            env = {key:value for key,value in os.environ.items() if not key.startswith("KROMA_")}
            env["PI_CODING_AGENT_DIR"] = str(pi_dir)
            args = [
                str(PI), "--offline", "--no-extensions", "--no-skills",
                "--no-mcp", "--no-context-files", "--no-approve",
                "--provider", "capture", "--model", MODEL, "--thinking", "high",
                "--mode", "json", "--print", "--no-session", PROMPT
            ]
            pi = subprocess.run(args, cwd=project, env=env, capture_output=True,
                                text=True, timeout=55)
            if pi.returncode or not server.observed:
                raise RuntimeError("local Pi capture failed (no live model traffic): "
                                   + pi.stderr[-350:])
            result = {"pi":server.observed[-1]}
            server.observed.clear()
            env.pop("PI_CODING_AGENT_DIR", None)
            env.update({
                "KROMA_BASE_URL":url, "KROMA_MODEL":MODEL, "KROMA_API_KEY":"dummy-only",
                "KROMA_API_PROTOCOL":"chat", "KROMA_REASONING_EFFORT":"high",
                "KROMA_DEEPSEEK_THINKING":"off", "KROMA_WORKDIR":str(project),
                "KROMA_PLUGIN_DIR":str(root / "disabled-global-plugins" / "plugins"),
                "KROMA_SESSION_FILE":str(root / "kroma-session.json"),
            })
            kroma = subprocess.run([str(KROMA), "--print", "--no-mcp", PROMPT],
                                   cwd=project, env=env, capture_output=True,
                                   text=True, timeout=55)
            if kroma.returncode or not server.observed:
                raise RuntimeError("local Kroma capture failed: " + kroma.stderr[-350:])
            result["kroma"] = server.observed[-1]
            print(json.dumps(result, indent=2, ensure_ascii=False), flush=True)
            server.shutdown()


if __name__ == "__main__":
    main()
