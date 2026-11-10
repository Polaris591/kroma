#!/usr/bin/env python3
"""Round 9: original vs complete-function code inspection guidance.

This is a bounded live benchmark, not a claim of product superiority.
It runs the same built binary and DeepSeek High endpoint in isolated task
fixtures. Only KROMA_EXPERIMENT_COMPLETE_FUNCTION_READ is varied.
"""
from __future__ import annotations
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import time

import round4_windows as base
import agent_task_suite as suite
from round6_rust_trace_diagnosis import summarize_kroma

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path("D:/Kroma/target-kroma-check/release/kroma.exe")
LAB = Path("D:/Kroma/experiments/kroma-round9-complete-function-read")
ARMS = ("baseline", "complete")
SEED = 20261110
TARGET_TEST = "trash_saved_session_is_recoverable_and_refuses_live_locks"
RESTORE_TEST = "restore_recovers_snapshot_moved_before_its_wal_on_crash"
TASKS = ("rust_restore_metadata", "rust_multifile")
RESTORE_PROMPT = (
    "This is a real multi-module Rust codebase. Undoing a trashed saved session "
    "recovers the transcript and title, but loses its selected model and usage "
    "counters. Find the regression and fix restoration without damaging "
    "existing lock, metadata or journal behavior. Do not modify or remove "
    "existing tests. Run the relevant targeted cargo test --locked --offline "
    "--lib and report exactly which tests passed. Work only inside the "
    "isolated checkout, do not commit or access credentials."
)
MISSING = (
    "                if let Some(model) = metadata.model {\n"
    "                    self.set_model_choice(id, model)?;\n"
    "                }\n"
    "                if let Some(usage) = metadata.usage {\n"
    "                    self.set_usage(id, usage)?;\n"
    "                }\n"
)


def environment(project: Path, trial: Path, arm: str) -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if not k.startswith("KROMA_")}
    env.update({
        "KROMA_PROVIDER_PROFILE": "deepseek",
        "KROMA_DEEPSEEK_THINKING": "off",
        "KROMA_REASONING_EFFORT": "high",
        "KROMA_PROFILE": "1",
        "KROMA_PLUGIN_DIR": str(trial / "disabled-global-plugins" / "plugins"),
        "KROMA_WORKDIR": str(project),
        "KROMA_SESSION_FILE": str(trial / "session.json"),
        "CARGO_TARGET_DIR": "D:/Kroma/target-kroma-check",
        "CARGO_INCREMENTAL": "0",
    })
    env["KROMA_EXPERIMENT_PROMPT_ARM"] = "complete" if arm == "complete" else "baseline"
    return env


def prepare_restore(project: Path) -> str:
    archive = subprocess.check_output(["git", "archive", "HEAD"], cwd=ROOT)
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as jar:
        jar.extractall(project, filter="data")
    path = project / "src" / "sessions.rs"
    source = path.read_text(encoding="utf-8")
    if source.count(MISSING) != 1:
        raise RuntimeError("fixture drift: expected model-and-usage restoration once")
    marker = "#[cfg(test)]\nmod tests"
    if source.count(marker) != 1:
        raise RuntimeError("fixture drift: tests module not found")
    frozen_tests = source.split(marker, 1)[1]
    path.write_text(source.replace(MISSING, "", 1), encoding="utf-8")
    return hashlib.sha256(frozen_tests.encode()).hexdigest()


def tests_untouched(project: Path, expected_hash: str) -> bool:
    text = (project / "src" / "sessions.rs").read_text(encoding="utf-8")
    marker = "#[cfg(test)]\nmod tests"
    if text.count(marker) != 1:
        return False
    original = text.split(marker, 1)[1]
    return hashlib.sha256(original.encode()).hexdigest() == expected_hash


def cargo_test(project: Path, env: dict, pattern: str, limit: int = 70) -> tuple[bool, str]:
    result = subprocess.run(
        ["cargo", "test", "--locked", "--offline", "--lib", pattern],
        cwd=project, env=env, text=True, capture_output=True, timeout=limit,
    )
    return result.returncode == 0, (
        result.stderr[-350:] + "\n" + result.stdout[-350:]
    ).strip()


def prepare_fixture(task: str, project: Path) -> str:
    if task == "rust_restore_metadata":
        return prepare_restore(project)
    if task == "rust_multifile":
        suite.create_fixture(project, base.TASKS[task])
        return hashlib.sha256(
            (project / "tests" / "basic.rs").read_bytes()
        ).hexdigest()
    raise ValueError("unknown task")


def verify(task: str, project: Path, env: dict, frozen: str) -> tuple[bool, str]:
    if task == "rust_restore_metadata":
        if not tests_untouched(project, frozen):
            return False, "existing Rust tests were modified"
        a, msg_a = cargo_test(project, env, TARGET_TEST)
        b, msg_b = cargo_test(project, env, RESTORE_TEST)
        return a and b, ("restore and WAL regression pass" if a and b
                         else ("trash: " + msg_a + " recovery: " + msg_b)[-900:])
    hidden = project / "tests" / "hidden_round9.rs"
    if hashlib.sha256((project / "tests" / "basic.rs").read_bytes()).hexdigest() != frozen:
        return False, "existing small Rust test was modified"
    hidden.write_text(base.HIDDEN_RUST_TEST, encoding="utf-8")
    result = subprocess.run(
        ["cargo", "test", "--locked", "--offline", "--quiet"],
        cwd=project, env=env, text=True, capture_output=True, timeout=65,
    )
    return result.returncode == 0, (
        "small Rust tests pass" if result.returncode == 0 else
        (result.stderr[-550:] + result.stdout[-150:])[-700:]
    )


def preflight() -> None:
    project = LAB / "fixture-preflight" / "project"
    if project.exists():
        raise RuntimeError("preflight fixture already exists; use a fresh lab")
    project.mkdir(parents=True)
    frozen = prepare_restore(project)
    env = environment(project, project.parent, "baseline")
    pass_before, detail = cargo_test(project, env, TARGET_TEST, 100)
    if pass_before:
        raise RuntimeError("negative control unexpectedly passes: defective fixture")
    if not tests_untouched(project, frozen):
        raise RuntimeError("negative control unexpectedly altered original tests")
    print("NEGATIVE_CONTROL_FAILS_AS_EXPECTED", detail[-320:], flush=True)


def run_one(task: str, repeat: int, arm: str, timeout: int) -> dict:
    trial = LAB / task / str(repeat) / arm
    project = trial / "project"
    project.mkdir(parents=True, exist_ok=False)
    frozen = prepare_fixture(task, project)
    env = environment(project, trial, arm)
    prompt = RESTORE_PROMPT if task == "rust_restore_metadata" else base.TASKS[task]["prompt"]
    start = time.monotonic()
    process = subprocess.Popen(
        [str(BINARY), "--print", "--no-mcp", prompt],
        cwd=project, env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if sys.platform == "win32" else 0,
    )
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        if sys.platform == "win32":
            subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                           capture_output=True, timeout=20)
        else:
            process.kill()
        stdout, stderr = process.communicate(timeout=15)
    elapsed = round(time.monotonic() - start, 3)
    (trial / "stdout.log").write_bytes(stdout)
    (trial / "stderr.log").write_bytes(stderr)
    metrics, _, tools, profile = base.parse_kroma(
        stdout.decode("utf-8", "replace"),
        stderr.decode("utf-8", "replace"),
        trial / "session.json",
    )
    trace = summarize_kroma(trial / "stderr.log")
    valid, detail = verify(task, project, env, frozen)
    agent_ran_test = bool(trace and trace.get("tests", 0) > 0)
    finished = bool(process.returncode == 0 and not timed_out)
    return {
        "task": task, "repeat": repeat, "arm": arm,
        "wall_s": elapsed, "exit": process.returncode, "timeout": timed_out,
        "verified": valid, "verification_detail": detail,
        "agent_ran_test": agent_ran_test, "finished": finished,
        "passed": bool(valid and finished),
        "strict_pass": bool(valid and finished and agent_ran_test),
        "metrics": metrics, "profile": profile,
        "tools": len(tools),
        "trace": {
            k: trace.get(k) for k in (
                "model_replies", "tool_calls", "model_ms", "reported_tool_ms",
                "inspections", "tests", "edits", "first_edit_number",
                "inspections_before_first_edit", "tests_before_first_edit",
                "duplicates_exact_inspections",
            )
        } if trace else None,
        "logs": str(trial),
    }


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--timeout", type=int, default=165)
    p.add_argument("--tasks", default="rust_restore_metadata,rust_multifile")
    p.add_argument("--repeat-start", type=int, default=1)
    p.add_argument("--repeats", type=int, default=1)
    p.add_argument("--max-runs", type=int, default=4)
    p.add_argument("--arm", choices=ARMS, default=None)
    p.add_argument("--self-test", action="store_true")
    p.add_argument("--preflight", action="store_true")
    args = p.parse_args()
    if args.self_test:
        assert all(name in TASKS for name in args.tasks.split(","))
        assert "without damaging" in RESTORE_PROMPT
        assert "self.set_usage" in MISSING and "self.set_model_choice" in MISSING
        print("round9 driver self-test OK", flush=True)
        return
    if not (45 <= args.timeout <= 240 and 1 <= args.repeats <= 3 and
            1 <= args.max_runs <= 12):
        p.error("invalid limits")
    if not BINARY.is_file():
        p.error("missing candidate binary")
    LAB.mkdir(parents=True, exist_ok=True)
    if args.preflight:
        preflight()
        return
    names = [s.strip() for s in args.tasks.split(",") if s.strip()]
    if not names or any(s not in TASKS for s in names):
        p.error("unknown task")
    report_file = LAB / "results.json"
    meta = {
        "schema": 1, "seed": SEED, "arms": list(ARMS),
        "model": "deepseek-v4.1-flash", "thinking": "high",
        "binary_sha256": hashlib.sha256(BINARY.read_bytes()).hexdigest(),
        "source_head": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
        "fixture_fingerprint": hashlib.sha256((MISSING + RESTORE_PROMPT).encode()).hexdigest(),
        "small_task_fingerprint": hashlib.sha256(
            base.TASKS["rust_multifile"]["prompt"].encode()
        ).hexdigest(),
    }
    report = json.loads(report_file.read_text(encoding="utf-8")) if report_file.exists() else {
        **meta, "results": []
    }
    for key, value in meta.items():
        if report.get(key) != value:
            raise RuntimeError("lab metadata mismatch: " + key)
    done = {(r["task"], r["repeat"], r["arm"]) for r in report["results"]}
    num = 0
    for rep in range(args.repeat_start, args.repeat_start + args.repeats):
        for task in names:
            order = list(ARMS)
            # Explicitly counterbalance the recovery runs: the first two
            # historical repeats used complete first, so repeat 3 reverses.
            if task == "rust_restore_metadata":
                if rep == 3:
                    order.reverse()
            elif rep % 2 == 0:
                order.reverse()
            for arm in order:
                if args.arm is not None and args.arm != arm:
                    continue
                if (task, rep, arm) in done:
                    continue
                if num >= args.max_runs:
                    break
                print("START", task, rep, arm, flush=True)
                try:
                    row = run_one(task, rep, arm, args.timeout)
                except Exception as exc:
                    row = {"task": task, "repeat": rep, "arm": arm,
                           "passed": False, "strict_pass": False,
                           "runner_error": type(exc).__name__ + ": " + str(exc)[:200]}
                report["results"].append(row)
                report_file.write_text(
                    json.dumps(report, ensure_ascii=False, indent=2) + "\n",
                    encoding="utf-8",
                )
                num += 1
                stats = row.get("metrics") or {}
                trace = row.get("trace") or {}
                print("END", task, rep, arm, "passed", row.get("passed"),
                      "strict", row.get("strict_pass"), "verified", row.get("verified"),
                      "wall", row.get("wall_s"), "requests", stats.get("requests"),
                      "input", stats.get("input"), "tools", row.get("tools"),
                      "pre_edit", trace.get("inspections_before_first_edit"),
                      "issue", row.get("runner_error"), flush=True)
            if num >= args.max_runs:
                break
        if num >= args.max_runs:
            break
    print("REPORT", report_file, "rows", len(report["results"]), flush=True)


if __name__ == "__main__":
    main()
