#!/usr/bin/env python3
"""Compare two frozen Kroma CLIs against deterministic local HTTP fixtures.

No API credentials or external network access are used. Each case has its
own temporary isolated session and workspace. This is a protocol/latency
verification, not an estimate of actual LLM task-quality improvements.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
import time


class Server(ThreadingHTTPServer):
    daemon_threads = True
    def __init__(self, mode: str, get_delay=0.0, summary_delay=0.0):
        super().__init__(("127.0.0.1", 0), Handler)
        self.mode = mode
        self.get_delay = get_delay
        self.summary_delay = summary_delay
        self.requests: list[dict] = []
        self.gets = 0
        self.lock = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, body: bytes, mimetype: str):
        self.send_response(200)
        self.send_header("Content-Type", mimetype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        server: Server = self.server
        with server.lock:
            server.gets += 1
        time.sleep(server.get_delay)
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self):
        server: Server = self.server
        size = int(self.headers["Content-Length"])
        body = json.loads(self.rfile.read(size))
        with server.lock:
            server.requests.append(body)
            n = sum(x.get("stream", True) for x in server.requests)
        if body.get("stream") is False:
            time.sleep(server.summary_delay)
            result = {
                "choices": [{"finish_reason": "stop", "message": {
                    "role": "assistant", "content": "Original task context summarized."}}],
                "usage": {"prompt_tokens": 120, "completion_tokens": 20, "total_tokens": 140},
            }
            return self.reply(json.dumps(result).encode(), "application/json")
        if server.mode == "log" and n == 1:
            result = {"choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_probe", "type": "function",
                "function": {"name": "shell", "arguments":
                             json.dumps({"command": "python3 emit.py"})},
            }]}, "finish_reason": "tool_calls"}]}
        else:
            result = {"choices": [{"delta": {
                "reasoning_content": "MODEL_REASONING_FROM_THIS_TURN" if server.mode == "replay" else "",
                "content": "OK"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 400, "completion_tokens": 11}}
        data = f"data: {json.dumps(result)}\n\ndata: [DONE]\n\n".encode()
        self.reply(data, "text/event-stream")


def environment(workdir, server, session):
    env = {k: v for k, v in os.environ.items() if not k.startswith("KROMA_")}
    env.update(
        KROMA_BASE_URL=f"http://127.0.0.1:{server.server_port}/v1",
        KROMA_MODEL="deepseek",
        KROMA_API_PROTOCOL="chat",
        KROMA_REASONING_EFFORT="high",
        KROMA_DEEPSEEK_THINKING="off",
        KROMA_CONTEXT_WINDOW="16000",
        KROMA_OUTPUT_RESERVE_TOKENS="1024",
        KROMA_WORKDIR=str(workdir),
        KROMA_SESSION_FILE=str(session),
        KROMA_PROFILE="1",
    )
    return env


def call(binary, workdir, env, args, stdin=None):
    start = time.monotonic()
    result = subprocess.run(
        [str(binary), *args], cwd=workdir, env=env,
        input=stdin, stdin=subprocess.DEVNULL if stdin is None else None,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        timeout=9, check=False,
    )
    return {
        "elapsed_s": round(time.monotonic() - start, 3),
        "exit": result.returncode,
        "stdout": result.stdout.decode(errors="replace"),
        "stderr": result.stderr.decode(errors="replace"),
    }


def run_one(binary, scenario, root):
    work = root / scenario
    work.mkdir(parents=True, exist_ok=True)
    server = Server(
        scenario,
        get_delay=2.0 if scenario == "startup" else 0.0,
        summary_delay=2.0 if scenario == "apec" else 0.0,
    )
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        env = environment(work, server, work / "session.json")
        if scenario == "startup":
            run = call(binary, work, env, ["--print", "Say OK"])
            outcome = {
                "seconds": run["elapsed_s"],
                "get_requests": server.gets,
                "post_requests": len(server.requests),
                "pass": run["exit"] == 0 and "OK" in run["stdout"],
            }
        elif scenario == "replay":
            first = call(binary, work, env, ["--print", "First user turn"])
            second = call(binary, work, env, ["--print", "Second user turn"])
            history = server.requests[-1].get("messages", [])
            assistants = [x for x in history if x.get("role") == "assistant"]
            replay = any("MODEL_REASONING_FROM_THIS_TURN"
                         in x.get("reasoning_content", "") for x in assistants)
            outcome = {
                "first_s": first["elapsed_s"], "second_s": second["elapsed_s"],
                "replayed_prior_assistant_reasoning": replay,
                "assistant_history_messages": len(assistants),
                "pass": first["exit"] == 0 and second["exit"] == 0,
            }
        elif scenario == "log":
            # Avoid Kroma's independent emergency context overflow projection:
            # this case isolates Shell's normal first-entry output policy.
            env["KROMA_CONTEXT_WINDOW"] = "128000"
            (work / "emit.py").write_text(
                "for i in range(5000):\n"
                "    print('error: SIGNATURE_MIDDLE_2500' if i == 2500 else 'samplelog')\n"
            )
            run = call(binary, work, env, ["--print", "Run the test"])
            msg = next(m for m in server.requests[-1]["messages"] if m["role"] == "tool")
            payload = json.loads(msg["content"])
            stdout = payload.get("stdout", "")
            archive = payload.get("stdout_file")
            outcome = {
                "seconds": run["elapsed_s"],
                "tool_context_bytes": len(msg["content"].encode()),
                "tool_visible_lines": stdout.count("\n"),
                "tool_exit_code": payload.get("exit_code"),
                "stderr_preview": str(payload.get("stderr", ""))[:160],
                "middle_error_visible": "SIGNATURE_MIDDLE_2500" in stdout,
                "archived_complete": (bool(archive) and Path(archive).is_file()
                                      and "SIGNATURE_MIDDLE_2500"
                                      in Path(archive).read_text()),
                "pass": run["exit"] == 0,
            }
        elif scenario == "apec":
            # Same multi-turn fixture used in the preceding context audit.
            prompt = "\n".join("Question " + str(n) + " " + "q" * 4800 for n in range(7)) + "\n"
            run = call(binary, work, env, ["--cli"], stdin=prompt.encode())
            summaries = [p for p in server.requests if p.get("stream") is False]
            waits = [float(v) for v in
                     re.findall(r"apec_finish_ms=([0-9.]+)", run["stderr"])]
            outcome = {
                "seconds": run["elapsed_s"],
                "model_requests": sum(p.get("stream", True) for p in server.requests),
                "apec_requests": len(summaries),
                "apec_efforts": [p.get("reasoning_effort") for p in summaries],
                "task_finish_wait_ms": round(sum(waits), 1),
                "summary_committed": "[apec] committed" in run["stderr"],
                "pass": run["exit"] == 0,
            }
        else:
            raise ValueError(scenario)
        return outcome
    finally:
        server.shutdown()
        server.server_close()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--before", type=Path, required=True)
    p.add_argument("--after", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    args = p.parse_args()
    result = {}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="kroma-fix2-targeted-") as tmp:
        for scenario in ("startup", "replay", "log", "apec"):
            for arm, binary in (("before", args.before), ("after", args.after)):
                print("START", scenario, arm, flush=True)
                try:
                    outcome = run_one(binary, scenario, Path(tmp) / arm)
                except Exception as error:
                    outcome = {"pass": False, "error": repr(error)}
                result[f"{scenario}.{arm}"] = outcome
                args.out.write_text(json.dumps(result, indent=2) + "\n")
                print("END", scenario, arm, json.dumps(outcome), flush=True)
    print("REPORT", args.out, flush=True)


if __name__ == "__main__":
    main()
