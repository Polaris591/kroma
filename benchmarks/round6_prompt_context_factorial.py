#!/usr/bin/env python3
"""Real two-factor prompt x context ablation (DeepSeek V4.1 Flash High).

P0: current production EXECUTION_GUIDANCE.
P1: *replace*, never append, the default execution guidance with a short form.
C0: current provider Shell JSON content.
C1: reformat Shell tool messages only into readable text while retaining every
    stdout/stderr byte, exit code, archive path and error status.
No LLM summarization, pruning, plugins, extra tools or new persistent state.

Real calls use the identical installed endpoint/model/key. Task verification is
independent. Sequential order rotates deterministically, even across resumes.
"""
from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import random
import re
import subprocess
import sys
import time

import agent_task_suite as suite
import round4_windows as previous

BINARY = Path("D:/Kroma/target-kroma-check/release/examples/prompt_context_ablation.exe")
LAB = Path("D:/Kroma/experiments/kroma-round6-factorial")
ARMS = ("p0c0", "p1c0", "p0c1", "p1c1")
SEED = 20261110
HIDDEN_MARKER = "\n#[cfg(test)]\nmod engineering_regression"


def deterministic_order(task: str, rep: int) -> list[str]:
    # Latin-style fixed rotation, never restart the same arm first on resume.
    initial = list(ARMS)
    random.Random(f"{SEED}:{task}").shuffle(initial)
    start = (rep - 1) % len(initial)
    return initial[start:] + initial[:start]


def prepare(project: Path, task_id: str):
    if task_id == "rust_wal_regression":
        previous.prepare_rust_wal_fixture(project)
        source = (project / "src" / "sessions.rs").read_text(encoding="utf-8")
        if HIDDEN_MARKER not in source:
            raise RuntimeError("WAL fixture missing injected regression")
        return HIDDEN_MARKER + source.split(HIDDEN_MARKER, 1)[1]
    suite.create_fixture(project, previous.TASKS[task_id])
    return None


def oracle(project: Path, task_id: str, frozen: str | None,
           answer: str, env: dict, timeout: int):
    if task_id == "rust_wal_regression":
        source = (project / "src" / "sessions.rs").read_text(encoding="utf-8")
        if source.count(frozen or "INVALID") != 1 or \
           source.count("fn wal_append_controls_recent_session_order()") != 1:
            return False
        p = subprocess.run(
            ["cargo", "test", "--locked", "--offline", "--quiet",
             "wal_append_controls_recent_session_order"],
            cwd=project, env=env, capture_output=True, timeout=timeout)
        return p.returncode == 0
    if task_id == "rust_multifile":
        (project / "tests" / "hidden_round4.rs").write_text(
            previous.HIDDEN_RUST_TEST, encoding="utf-8")
        p = subprocess.run(["cargo", "test", "--locked", "--offline", "--quiet"],
                           cwd=project, env=env, capture_output=True, timeout=timeout)
        return p.returncode == 0
    return bool(suite.verify(project, task_id, answer))


def profile_rows(stderr: str) -> dict:
    totals = {"model_ms": 0, "tool_ms": 0, "model_done": 0,
              "tool_done": 0, "formatted_shell_messages": 0,
              "formatted_requests": 0, "checkpoint_events": 0}
    for line in stderr.splitlines():
        a = re.match(r"^\[profile\] model_done .*elapsed_ms=([\d.]+)", line)
        if a:
            totals["model_done"] += 1
            totals["model_ms"] += float(a.group(1))
        b = re.match(r"^\[profile\] tool_done .*elapsed_ms=([\d.]+)", line)
        if b:
            totals["tool_done"] += 1
            totals["tool_ms"] += float(b.group(1))
        c = re.match(r"^\[ablation\] readable_shell_tool_results=(\d+)", line)
        if c:
            totals["formatted_requests"] += 1
            totals["formatted_shell_messages"] += int(c.group(1))
        if "[compact] requesting one checkpoint" in line:
            totals["checkpoint_events"] += 1
    totals["model_ms"] = round(totals["model_ms"], 2)
    totals["tool_ms"] = round(totals["tool_ms"], 2)
    return totals


def run_one(lab: Path, task_id: str, rep: int, arm: str,
            timeout: int, prewarm: bool) -> dict:
    trial = lab / task_id / str(rep) / arm
    project = trial / "project"
    project.mkdir(parents=True, exist_ok=False)
    frozen = prepare(project, task_id)
    env = {key: value for key, value in os.environ.items() if not key.startswith("KROMA_")}
    env.update({
        "KROMA_PROVIDER_PROFILE": "deepseek",
        "KROMA_DEEPSEEK_THINKING": "off",
        "KROMA_REASONING_EFFORT": "high",
        "KROMA_CONTEXT_WINDOW": "131072",
        "KROMA_OUTPUT_RESERVE_TOKENS": "4096",
        "KROMA_MAX_OUTPUT_TOKENS": "8192",
        "KROMA_PROFILE": "1",
        "KROMA_PLUGIN_DIR": str(trial / "disabled-global-plugins" / "plugins"),
        "KROMA_WORKDIR": str(project),
        "KROMA_SESSION_FILE": str(trial / "session.json"),
    })
    if task_id in ("rust_multifile", "rust_wal_regression"):
        env["CARGO_TARGET_DIR"] = str(trial / "cargo-target")
        env["CARGO_INCREMENTAL"] = "0"
    prewarm_seconds = 0
    if prewarm and task_id in ("rust_multifile", "rust_wal_regression"):
        start = time.monotonic()
        proc = subprocess.run(["cargo", "test", "--locked", "--offline", "--no-run", "--quiet"],
                              cwd=project, env=env, capture_output=True,
                              timeout=160)
        prewarm_seconds = round(time.monotonic() - start, 3)
        if proc.returncode:
            raise RuntimeError("Cargo prewarm failed " + proc.stderr.decode("utf-8","replace")[-280:])

    command = [str(BINARY), arm, previous.TASKS[task_id]["prompt"]]
    started = time.monotonic()
    proc = subprocess.Popen(
        command, cwd=project, env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if sys.platform == "win32" else 0,
    )
    timed_out = False
    try:
        stdout, stderr = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        if sys.platform == "win32":
            subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                           capture_output=True, timeout=20)
        else:
            proc.kill()
        stdout, stderr = proc.communicate(timeout=20)
    elapsed = round(time.monotonic() - started, 3)
    (trial / "stdout.log").write_bytes(stdout)
    (trial / "stderr.log").write_bytes(stderr)
    out = stdout.decode("utf-8", "replace")
    err = stderr.decode("utf-8", "replace")
    usage, _, tool_calls, profile = previous.parse_kroma(out, err, trial / "session.json")
    data = profile_rows(err)
    if "requests" not in usage or not usage.get("requests"):
        usage["requests"] = data["model_done"]
    try:
        verified = oracle(project, task_id, frozen, out, env, min(100,timeout))
    except Exception as exc:
        verified = False
        (trial / "oracle_error.txt").write_text(type(exc).__name__ + ": " + str(exc)[:300],
                                                  encoding="utf-8")
    edit_indexes = [index for index,row in enumerate(tool_calls)
                    if row["tool"] == "edit_file"
                    or (row["tool"] == "shell" and row["operation"] == "edit")]
    first_edit = min(edit_indexes) if edit_indexes else None
    pre_edit_inspections = sum(row["operation"] == "inspect"
                               for row in tool_calls[:first_edit if first_edit is not None else len(tool_calls)])
    after_edit_tests = sum(row["operation"] == "test" for i,row in enumerate(tool_calls)
                           if first_edit is not None and i > first_edit)
    # Keep both code correctness and compliance separate, never over-credit a
    # task that passes only because the external oracle ran the required test.
    tests_required = task_id in ("rust_multifile","rust_wal_regression")
    compliant = not tests_required or after_edit_tests > 0
    status = {
        "task": task_id, "repeat": rep, "arm": arm, "wall_s": elapsed,
        "exit": proc.returncode, "timeout": timed_out,
        "verified_code": verified,
        "verified_process": compliant,
        "passed": bool(verified and proc.returncode == 0 and not timed_out and compliant),
        "metrics": usage, "profile": profile, "phase": data,
        "tools": len(tool_calls), "types": dict(Counter(x["tool"] for x in tool_calls)),
        "first_edit_tool_position": first_edit + 1 if first_edit is not None else None,
        "inspections_before_first_edit": pre_edit_inspections,
        "tests_after_first_edit": after_edit_tests,
        "prewarm_seconds_outside_timing": prewarm_seconds,
        "logs": str(trial),
    }
    return status


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lab", type=Path, default=LAB)
    parser.add_argument("--tasks", default="rust_multifile,diagnose_large_log,rust_wal_regression")
    parser.add_argument("--repeat-start", type=int, default=1)
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--max-runs", type=int, default=2)
    parser.add_argument("--only-arm", choices=ARMS)
    parser.add_argument("--prewarm", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        assert set(deterministic_order("rust_multifile", 1)) == set(ARMS)
        assert deterministic_order("rust_multifile",1) != deterministic_order("rust_multifile",2)
        assert set(ARMS) == {"p0c0","p1c0","p0c1","p1c1"}
        print("round6 factorial runner self-test OK")
        return
    task_ids = [x.strip() for x in args.tasks.split(",") if x.strip()]
    if not task_ids or any(x not in previous.TASKS for x in task_ids):
        parser.error("unknown fixture")
    if not (1<=args.repeats<=5 and 1<=args.max_runs<=40 and 30<=args.timeout<=300):
        parser.error("invalid repeats/max-runs/timeout")
    if not BINARY.is_file():
        raise RuntimeError("release example missing; build cargo --release --example prompt_context_ablation")
    lab = args.lab.resolve()
    lab.mkdir(parents=True, exist_ok=True)
    output = lab / "results.json"
    metadata = {
        "schema":1,
        "seed":SEED, "model":"deepseek-v4.1-flash",
        "thinking":"high", "endpoint":"https://cobblemon.asia/v1",
        "binary_sha256":hashlib.sha256(BINARY.read_bytes()).hexdigest(),
        "arms":list(ARMS), "prewarm":args.prewarm,
    }
    if output.is_file():
        report = json.loads(output.read_text(encoding="utf-8"))
        for key,value in metadata.items():
            if report.get(key) != value:
                raise RuntimeError(f"existing lab mismatch: {key}")
    else:
        report = {**metadata,"results":[]}
        output.write_text(json.dumps(report,indent=2)+"\n",encoding="utf-8")
    finished = {(row["task"],row["repeat"],row["arm"]) for row in report["results"]}
    count = 0
    for task in task_ids:
        for rep in range(args.repeat_start,args.repeat_start+args.repeats):
            for arm in deterministic_order(task,rep):
                if args.only_arm and args.only_arm!=arm:
                    continue
                if (task,rep,arm) in finished:
                    continue
                if count>=args.max_runs:
                    break
                print("START",task,rep,arm,flush=True)
                try:
                    row = run_one(lab,task,rep,arm,args.timeout,args.prewarm)
                except Exception as exc:
                    row = {"task":task,"repeat":rep,"arm":arm,"passed":False,
                           "error":type(exc).__name__ + ": " + str(exc)[:280]}
                report["results"].append(row)
                finished.add((task,rep,arm))
                output.write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
                count+=1
                m=row.get("metrics") or {}
                d=row.get("phase") or {}
                print("END",task,rep,arm,"pass",row.get("passed"),
                      "wall",row.get("wall_s"),"calls",m.get("requests"),
                      "input",m.get("input"),"cached",m.get("cached"),
                      "output",m.get("output"),"tools",row.get("tools"),
                      "preedit",row.get("inspections_before_first_edit"),
                      "formatted",d.get("formatted_shell_messages"),flush=True)
            if count>=args.max_runs:break
        if count>=args.max_runs:break
    print("REPORT",output,"rows",len(report["results"]),flush=True)

if __name__ == "__main__":
    main()
