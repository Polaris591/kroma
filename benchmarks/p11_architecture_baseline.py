#!/usr/bin/env python3
"""Frozen behavior contract for the P0-P4 architecture-only refactor.

No external model/API. Run after cargo build --release --locked --offline.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BINARY = Path(__file__).resolve().parents[1] / "target/release/kroma"
GUIDANCE = (
    "For coding tasks, focus inspections on relevant files; once the needed "
    "evidence is found, edit rather than re-inspecting unchanged files."
)


def sse(delta, reason):
    frame = {"choices": [{"delta": delta, "finish_reason": reason}]}
    return ("data: " + json.dumps(frame) + "\n\ndata: [DONE]\n\n").encode()


class Provider(BaseHTTPRequestHandler):
    requests = []

    def log_message(self, *_args):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.requests.append(request)
        index = len(self.requests)
        if index == 1:
            calls = [
                {"index": i, "id": f"tool-{i}", "type": "function",
                 "function": {"name": "shell", "arguments": json.dumps({
                     "command": f"printf ordered-{i}"
                 })}}
                for i in range(2)
            ]
            body = sse({"reasoning_content": "private-model-reasoning",
                        "tool_calls": calls}, "tool_calls")
        elif index in (2, 3):
            body = sse({"content": f"completed-{index}"}, "stop")
        else:
            raise AssertionError(f"Unexpected model request {index}")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    assert BINARY.is_file(), "release build required"
    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="kroma-p11-baseline-") as tmp:
            root = Path(tmp)
            env = os.environ.copy()
            for key in ("KROMA_PROVIDER_PROFILE", "KROMA_API_KEY"):
                env.pop(key, None)
            env.update(
                HOME=tmp, KROMA_WORKDIR=tmp,
                KROMA_CONFIG_FILE=str(root / "absent.json"),
                KROMA_MODEL="mock", KROMA_BASE_URL=f"http://127.0.0.1:{server.server_port}/v1",
                KROMA_SESSION_FILE=str(root / "session.json"),
                KROMA_DEEPSEEK_THINKING="high",
            )
            def run(user_message):
                return subprocess.run(
                    [str(BINARY), user_message], env=env, cwd=root,
                    capture_output=True, text=True, timeout=20
                )

            first = run("First task")
            assert first.returncode == 0, first.stderr
            assert "completed-2" in first.stdout, first.stdout
            assert "private-model-reasoning" not in first.stdout
            assert len(Provider.requests) == 2
            first_request, followup = Provider.requests[:2]
            expected_commands = ["printf ordered-0", "printf ordered-1"]
            tools = first_request["tools"]
            assert len(tools) == 1, tools
            schema = tools[0]["function"]
            assert schema["name"] == "shell"
            assert schema["parameters"]["required"] == ["command"]
            assert schema["parameters"]["additionalProperties"] is False
            assert first_request["thinking"] == {"type": "enabled"}
            assert first_request["reasoning_effort"] == "high"
            assert first_request["stream_options"] == {"include_usage": True}
            assert "tool_choice" not in first_request
            assert first_request["messages"][0]["role"] == "system"
            assert first_request["messages"][0]["content"].count(GUIDANCE) == 1
            assert str(root) in first_request["messages"][0]["content"]
            assert first_request["messages"][1]["content"] == "First task"
            assistant = [m for m in followup["messages"] if m["role"] == "assistant" and m.get("tool_calls")]
            assert len(assistant) == 1
            assert assistant[0].get("reasoning_content") == "private-model-reasoning"
            assert [json.loads(c["function"]["arguments"])["command"] for c in assistant[0]["tool_calls"]] == expected_commands
            results = [m for m in followup["messages"] if m["role"] == "tool"]
            assert [m["tool_call_id"] for m in results] == ["tool-0", "tool-1"]
            assert [json.loads(m["content"])["stdout"] for m in results] == ["ordered-0", "ordered-1"]
            assert followup["messages"][0]["content"].count(GUIDANCE) == 1

            second = run("Second task, resume the same session")
            assert second.returncode == 0, second.stderr
            assert "completed-3" in second.stdout
            assert len(Provider.requests) == 3
            resumed = Provider.requests[-1]["messages"]
            assert resumed[0]["content"].count(GUIDANCE) == 1
            assert any(m.get("content") == "First task" for m in resumed)
            assert any(m.get("content") == "Second task, resume the same session" for m in resumed)
            assert [m["tool_call_id"] for m in resumed if m["role"] == "tool"] == ["tool-0", "tool-1"]
            assert (root / "session.json").is_file()
            assert (root / "session.json.wal").is_file()
            print("P11_BASELINE exact tool schema, order, thinking continuity, prompt and resumed WAL: PASS")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
