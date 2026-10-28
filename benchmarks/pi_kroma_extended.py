#!/usr/bin/env python3
"""Live, isolated Pi vs Kroma tests for restart memory and Rust repair.

No real checkout changes. Both runners use preconfigured WorkBuddy/DeepSeek
High. Test output and private logs stay outside the repo. Each stage is a new
process; repeated stages use the same runner-specific durable session.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time
import uuid


ROOT = Path(__file__).resolve().parents[1]
KROMA = ROOT / "target/debug/kroma"
LAB = Path("/tmp") / ("kroma-pi-extended-" + uuid.uuid4().hex[:8])
PI = [
    "pi", "--offline", "--no-extensions", "--no-skills", "--no-mcp",
    "--no-context-files", "--no-approve", "--provider", "workbuddy",
    "--model", "deepseek", "--thinking", "high", "--mode", "json", "--print",
]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / "benchmarks" / path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


previous = load("pi_kroma_usage", "pi_kroma_full_compare.py")
engineering = load("kroma_engineering", "engineering_compare.py")


def parse_line(raw):
    try:
        return json.loads(raw)
    except (ValueError, TypeError):
        return {}


def invoke(runner, work, session, prompt, step, timeout):
    env = {k: v for k, v in os.environ.items() if not k.startswith("KROMA_")}
    env["CARGO_TARGET_DIR"] = str(ROOT / "target")
    if runner == "pi":
        command = PI + ["--session", str(session), prompt]
    else:
        env.update(
            KROMA_PROVIDER_PROFILE="workbuddy",
            KROMA_PROFILE="1",
            KROMA_WORKDIR=str(work),
            KROMA_SESSION_FILE=str(session),
        )
        command = [str(KROMA), prompt]
    logs = work.parent / "logs" / f"{step:02d}"
    logs.mkdir(parents=True, mode=0o700, exist_ok=False)
    started = time.monotonic()
    proc = subprocess.Popen(
        command, cwd=work, env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
    )
    timed_out = False
    try:
        out, err = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        out, err = proc.communicate(timeout=15)
    elapsed = round(time.monotonic() - started, 3)
    (logs / "stdout.log").write_bytes(out)
    (logs / "stderr.log").write_bytes(err)
    os.chmod(logs / "stdout.log", 0o600)
    os.chmod(logs / "stderr.log", 0o600)
    stdout = out.decode(errors="replace")
    stderr = err.decode(errors="replace")
    if runner == "pi":
        metrics = previous.usage_pi(stdout)
        answers = [
            "".join(item.get("text", "") for item in event["message"].get("content", [])
                    if item.get("type") == "text")
            for line in stdout.splitlines()
            for event in [parse_line(line)]
            if event.get("type") == "message_end"
            and event.get("message", {}).get("role") == "assistant"
        ]
        answer = answers[-1] if answers else ""
    else:
        metrics = previous.usage_kroma(stderr)
        answer = stdout
    return {
        "runner": runner, "step": step, "elapsed_s": elapsed,
        "exit": proc.returncode, "timeout": timed_out, "metrics": metrics,
        "answer": answer.strip()[-500:], "logs": str(logs),
    }


def save(report):
    path = LAB / "results.json"
    temp = path.with_suffix(".tmp")
    temp.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    temp.replace(path)


def memory_test(runner, report, timeout):
    work = LAB / "memory_restart" / runner / "source"
    work.mkdir(parents=True, mode=0o700)
    key = "RESTART_FIXED_C8M7P2"
    file = work / "note.txt"
    file.write_text(f"MARKER={key}\n")
    session = work.parent / ("session.jsonl" if runner == "pi" else "session.json")
    first = invoke(runner, work, session,
                   "Read note.txt with a tool. Reply with the exact MARKER value and remember "
                   "it for the next message after the process restarts. Do not modify files.",
                   1, timeout)
    passed_first = first["exit"] == 0 and key in first["answer"]
    file.unlink()
    second = invoke(runner, work, session,
                    "The process restarted and note.txt was deleted. From the earlier "
                    "conversation ONLY, reply with the exact MARKER value. "
                    "Do not use tools, read files, or guess. Reply with the value alone.",
                    2, timeout)
    passed_second = second["exit"] == 0 and key in second["answer"]
    evidence = {
        "scenario": "restart_memory", "runner": runner,
        "passed": passed_first and passed_second,
        "first_passed": passed_first, "after_restart_passed": passed_second,
        "elapsed_s": round(first["elapsed_s"] + second["elapsed_s"], 3),
        "steps": [first, second],
    }
    report["cases"].append(evidence)
    save(report)
    print("EXTENDED", "restart_memory", runner,
          "pass", evidence["passed"], "steps", [passed_first, passed_second],
          "elapsed", evidence["elapsed_s"], flush=True)


def rust_test(runner, report, timeout):
    work = LAB / "rust_repair" / runner / "source"
    engineering.make_copy(work, "A", 1)
    session = work.parent / ("session.jsonl" if runner == "pi" else "session.json")
    prompt = engineering.A_PROMPT + "\n\nWork only in this isolated source checkout. " \
             "Do not read credentials, modify the original Kroma repo, make network requests " \
             "or commit. The build's CARGO_TARGET_DIR is already configured."
    result = invoke(runner, work, session, prompt, 1, timeout)
    try:
        passed_external = engineering.verify(work, "A", 1)
    except Exception:
        passed_external = False
    original = engineering.A_PROMPT
    acceptance = {
        "scenario": "rust_regression", "runner": runner,
        "passed": result["exit"] == 0 and not result["timeout"] and passed_external,
        "external_test_passed": passed_external,
        "elapsed_s": result["elapsed_s"], "steps": [result],
    }
    report["cases"].append(acceptance)
    save(report)
    print("EXTENDED", "rust_regression", runner,
          "pass", acceptance["passed"], "external", passed_external,
          "elapsed", result["elapsed_s"], flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--timeout", type=int, default=210)
    ap.add_argument("--scenario", choices=["memory", "rust", "all"], default="all")
    args = ap.parse_args()
    LAB.mkdir(mode=0o700)
    report = {"model": "workbuddy/deepseek", "thinking": "high",
              "source_commit": engineering.SOURCE_SHA, "cases": []}
    save(report)
    print("EXTENDED_LAB", LAB, flush=True)
    if args.scenario in ("memory", "all"):
        for runner in ("pi", "kroma"):
            memory_test(runner, report, args.timeout)
    if args.scenario in ("rust", "all"):
        for runner in ("kroma", "pi"):
            rust_test(runner, report, args.timeout)
    print("EXTENDED_REPORT", LAB / "results.json", flush=True)


if __name__ == "__main__":
    main()
