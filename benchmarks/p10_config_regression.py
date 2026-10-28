#!/usr/bin/env python3
"""Offline regression: environment-only startup must ignore unrelated profiles.

Run after cargo build --release --locked --offline.
All config files and the mock server are isolated; no real credentials used.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

BINARY = Path(__file__).resolve().parents[1] / "target/release/kroma"


class Model(BaseHTTPRequestHandler):
    requests = []

    def log_message(self, *_args):
        pass

    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.requests.append((self.headers.get("Authorization"), payload))
        data = (
            'data: {"choices":[{"delta":{"content":"env works"},'
            '"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def main():
    assert BINARY.is_file(), "build the release binary first"
    server = HTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="kroma-p10-config-") as tmp:
            root = Path(tmp)
            cfg = root / "config.json"
            cfg.write_text("invalid saved JSON")
            cfg.chmod(0o644)
            env = os.environ.copy()
            for key in ("KROMA_API_KEY", "KROMA_PROVIDER_PROFILE", "KROMA_DEEPSEEK_THINKING"):
                env.pop(key, None)
            env.update(
                HOME=tmp,
                KROMA_CONFIG_FILE=str(cfg),
                KROMA_MODEL="direct-model",
                KROMA_BASE_URL=f"http://127.0.0.1:{server.server_port}/v1",
                KROMA_WORKDIR=tmp,
                KROMA_SESSION_FILE=str(root / "session.json"),
            )

            def call(args, settings=env):
                return subprocess.run([str(BINARY), *args], cwd=root, env=settings,
                                      capture_output=True, text=True, timeout=15)

            first = call(["config", "show"])
            assert first.returncode == 0, first.stderr
            assert "model=direct-model" in first.stdout
            reply = call(["Verify a complete env-only configuration."])
            assert reply.returncode == 0, reply.stderr
            assert "env works" in reply.stdout
            assert len(Model.requests) == 1
            auth, request = Model.requests[0]
            assert auth is None, "must not inherit a saved API key"
            assert request["model"] == "direct-model"
            assert cfg.read_text() == "invalid saved JSON", "config must not be rewritten"
            print("P10_CONFIG full environment overrides ignore invalid, world-readable config: PASS")

            explicit = env.copy()
            explicit["KROMA_PROVIDER_PROFILE"] = "selected"
            result = call(["config", "show"], explicit)
            assert result.returncode != 0
            assert "invalid Kroma profile JSON" in result.stderr
            partial = env.copy()
            partial.pop("KROMA_BASE_URL")
            result = call(["config", "show"], partial)
            assert result.returncode != 0
            assert "invalid Kroma profile JSON" in result.stderr
            print("P10_CONFIG explicit profile and incomplete env still require valid profiles: PASS")

            cfg.unlink()
            cfg.symlink_to(root / "nonexistent.json")
            result = call(["config", "show"])
            assert result.returncode == 0, result.stderr
            print("P10_CONFIG complete env ignores broken saved symlink: PASS")

            blank = env.copy()
            blank["KROMA_MODEL"] = ""
            result = call(["config", "show"], blank)
            assert result.returncode != 0
            assert "KROMA_MODEL cannot be empty" in result.stderr
            print("P10_CONFIG explicit empty model is rejected, not silently replaced: PASS")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
