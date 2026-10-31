#!/usr/bin/env python3
"""Small, reproducible Kroma V7 versus Pi smoke competition on frozen tasks.

Uses the already qualified, offline Exercism fixtures and independent test
verifier from competition_v3. Both harnesses use the same audited model, Max
effort, upstream endpoint, task wording, and fresh per-trial workspace.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import uuid

SOURCE = Path("/var/lib/kroma-competition-20261031/repository/benchmarks/competition_v3")
STATE = Path("/var/lib/kroma-competition-20261031/v7-pi-smoke-20261031")
BUILD = Path("/root/kroma-github/target/release/kroma")
TASKS = (
    "rust/acronym",
    "go/pig-latin",
    "javascript/affine-cipher",
    "python/transpose",
)
PORT = 18773
MIN_DISK_FREE = 1200 * 1024 * 1024
sys.path.insert(0, str(SOURCE))

import controller as pilot  # noqa: E402
import polyglot120 as suite  # noqa: E402
import report as metrics  # noqa: E402
from proxy import AuditGateway, MODEL  # noqa: E402


def save(path, data):
    pilot.atomic(path, data)


def summarize(results):
    report = {}
    for name in ("kroma", "pi"):
        selected = [r for r in results if r["runner"] == name]
        known = [r["telemetry"] for r in selected if r["telemetry"].get("usage_available")]
        report[name] = {
            "runs": len(selected),
            "passed": sum(r["success"] for r in selected),
            "wall_s": round(sum(r["wall_s"] for r in selected), 2),
            "api_calls": sum(r.get("model_api_requests", 0) for r in selected),
            "tool_calls": sum(t["tool_calls"] for t in known),
            "usage_runs": len(known),
            "total_tokens": sum(t["total_tokens"] for t in known),
            "uncached_input": sum(t["uncached_input"] for t in known),
            "cached_input": sum(t["cached_input"] for t in known),
            "output": sum(t["output"] for t in known),
        }
    return report


def main():
    if os.geteuid() != 0:
        raise RuntimeError("Isolated benchmark supervisor must run as root")
    if not BUILD.is_file():
        raise RuntimeError("Build the updated Kroma release binary first")
    suite.audit_source()
    candidates = json.loads((suite.MAIN / "tasks.json").read_text())["tasks"]
    by_id = {r["id"]: r for r in candidates}
    if any(task not in by_id for task in TASKS):
        raise RuntimeError("Selected task not in the frozen qualified dataset")
    base = json.loads(pilot.PI_SOURCE.read_text())["providers"]["cobblemon"]
    if not any(m["id"] == MODEL for m in base["models"]):
        raise RuntimeError("Pi and Kroma model IDs cannot be matched")

    STATE.mkdir(parents=True, exist_ok=True)
    STATE.chmod(0o755)
    (STATE / "bin").mkdir(exist_ok=True)
    (STATE / "bin").chmod(0o755)
    binary = STATE / "bin" / "kroma-v7"
    binary_hash = hashlib.sha256(BUILD.read_bytes()).hexdigest()
    if binary.exists() and hashlib.sha256(binary.read_bytes()).hexdigest() != binary_hash:
        raise RuntimeError("Existing benchmark binary differs from the new build")
    if not binary.exists():
        shutil.copy2(BUILD, binary)
        binary.chmod(0o755)

    git_sha = subprocess.check_output(
        ["git", "-C", "/root/kroma-github", "rev-parse", "HEAD"], text=True
    ).strip()
    plan = []
    for i, task in enumerate(TASKS):
        order = ("kroma", "pi") if i % 2 == 0 else ("pi", "kroma")
        for runner in order:
            plan.append({"id": task.replace("/", "--") + "--" + runner,
                         "task": task, "runner": runner,
                         "language": by_id[task]["language"]})
    info = {
        "source_sha": git_sha,
        "binary_sha256": binary_hash,
        "model": MODEL,
        "thinking": "max",
        "tasks": list(TASKS),
        "plan": plan,
    }
    path = STATE / "results.json"
    if path.exists():
        state = json.loads(path.read_text())
        if any(state.get(k) != v for k, v in info.items()):
            raise RuntimeError("An existing benchmark has a different binary or plan")
    else:
        state = {**info, "results": [], "finished": False}
        save(path, state)

    # Retain the original isolation, unmodified tests and native telemetry
    # extraction. Change only the snapshot paths and the Kroma executable.
    pilot.STATE = STATE
    pilot.PORT = PORT
    suite.MAIN = STATE
    metrics.STATE = STATE
    original_command = pilot.command_for

    def command(name, task_prompt):
        if name == "kroma":
            return [str(binary), "--json", task_prompt]
        return original_command(name, task_prompt)

    pilot.command_for = command
    token = uuid.uuid4().hex + uuid.uuid4().hex
    factors = {
        "F": "Avoid nonessential narration between tool calls.",
        "C": "Prefer the simplest correct implementation consistent with the existing codebase.",
        "E": "Locate relevant symbols before reading large files.",
    }
    gateway = AuditGateway(base["baseUrl"], base["apiKey"], token, STATE,
                           port=PORT, prompt_factors=factors)
    gateway.start()
    finished = {r["id"] for r in state["results"]}
    try:
        for item in plan:
            if item["id"] in finished:
                continue
            if shutil.disk_usage(STATE).free < MIN_DISK_FREE:
                raise RuntimeError("disk guard: benchmark stopped without deleting old files")
            gateway.set_expected_factors("FCE" if item["runner"] == "kroma" else "")
            print("START", len(finished) + 1, "of", len(plan), item["id"], flush=True)
            row = suite.run_one(by_id[item["task"]], item, base, gateway, token)
            if row.get("infrastructure_paused"):
                raise RuntimeError(str(row["infrastructure_paused"]))
            state["results"].append(row)
            finished.add(item["id"])
            save(path, state)
            print("END", row["id"], "passed", row["success"],
                  "seconds", row["wall_s"], "tokens",
                  row["telemetry"].get("total_tokens"),
                  "requests", row["model_api_requests"], flush=True)
        state["finished"] = len(finished) == len(plan)
        state["summary"] = summarize(state["results"])
        save(path, state)
        print("SUMMARY", json.dumps(state["summary"], ensure_ascii=False), flush=True)
    finally:
        gateway.stop()


if __name__ == "__main__":
    main()
