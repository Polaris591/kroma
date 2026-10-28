#!/usr/bin/env python3
"""Offline acceptance test for --print, --json, piped stdin and file references.

Uses an isolated HOME/workdir and a local fake Chat Completions endpoint.
No provider credentials and no changes to a real conversation.
"""
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


BINARY = Path(os.environ.get("KROMA_TEST_BINARY", str(
    Path(__file__).resolve().parents[1] / "target/release/kroma"))).resolve()


class Model(BaseHTTPRequestHandler):
    received = []

    def log_message(self, *_args):
        pass

    def reply(self, content_type, body):
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.reply("application/json", json.dumps({
            "id": "mock", "context_window": 128000, "max_output_tokens": 4096,
        }).encode())

    def do_POST(self):
        message = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.received.append(message)
        body = ("data: " + json.dumps({"choices": [{"delta": {
            "content": "OFFLINE-RESULT"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 20, "completion_tokens": 4}}) +
            "\n\ndata: [DONE]\n\n").encode()
        self.reply("text/event-stream", body)


def main():
    assert BINARY.is_file(), "build the release executable first"
    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="kroma-cli-acceptance-") as temp:
            base = Path(temp)
            home = base / "home"
            repo = base / "repo"
            home.mkdir()
            (repo / "src").mkdir(parents=True)
            (repo / "src" / "module.rs").write_text("pub fn example() {}")
            (repo / "src" / "AGENTS.md").write_text("NESTED-INSTRUCTIONS-TEST")
            env = dict(os.environ, HOME=str(home), KROMA_MODEL="mock",
                       KROMA_WORKDIR=str(repo), KROMA_BASE_URL=(
                           f"http://127.0.0.1:{server.server_port}/v1"))
            env.pop("KROMA_API_KEY", None)

            version = subprocess.run([str(BINARY), "--version"], env=env,
                                     capture_output=True, text=True, timeout=10)
            assert version.returncode == 0 and version.stdout.startswith("kroma ")

            plain = subprocess.run([str(BINARY), "--print",
                                    "Inspect @src/module.rs"],
                                   input="small test log\n", env=env,
                                   cwd=repo, capture_output=True, text=True, timeout=20)
            assert plain.returncode == 0, plain.stderr
            assert "OFFLINE-RESULT" in plain.stdout
            assert "data: " not in plain.stdout, "plain mode must not leak JSONL"
            assert "NESTED-INSTRUCTIONS-TEST" in json.dumps(Model.received[-1])
            assert "module.rs" in json.dumps(Model.received[-1])
            assert "small test log" in json.dumps(Model.received[-1])

            structured = subprocess.run([str(BINARY), "--json", "Summarize"],
                                        input="another log\n", env=env, cwd=repo,
                                        capture_output=True, text=True, timeout=20)
            assert structured.returncode == 0, structured.stderr
            events = [json.loads(line) for line in structured.stdout.splitlines()]
            types = [e["type"] for e in events]
            for event in ["task_started", "model_started", "text_delta",
                          "model_finished", "task_finished", "result"]:
                assert event in types, (event, types)
            assert events[-1] == {"type": "result", "text": "OFFLINE-RESULT"}
            assert all(line.startswith("{") for line in structured.stdout.splitlines()), (
                "stdout must consist only of JSONL objects")
            assert "another log" in json.dumps(Model.received[-1])

            large = "X" * 20_000
            piped = subprocess.run([str(BINARY), "--print", "Analyze piped log"],
                                   input=large, env=env, cwd=repo,
                                   capture_output=True, text=True, timeout=20)
            assert piped.returncode == 0, piped.stderr
            sent = json.dumps(Model.received[-1])
            assert "pipe-inputs" in sent and large not in sent, (
                "long logs should be referenced, not injected")
            private_dir = home / ".kroma" / "pipe-inputs"
            captures = list(private_dir.glob("*.log"))
            assert captures
            if os.name == "posix":
                assert stat.S_IMODE(private_dir.stat().st_mode) == 0o700
                assert all(stat.S_IMODE(path.stat().st_mode) == 0o600 for path in captures)
            else:
                print("Windows: POSIX mode checks not applicable; ACLs not tested")
            print("CLI_AUTOMATION_SMOKE: PASS; --version, --print, stdin, JSONL, nested instructions, long-log references")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
