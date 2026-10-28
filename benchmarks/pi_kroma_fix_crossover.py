#!/usr/bin/env python3
"""Isolated, randomized before/after/Pi DeepSeek comparison.

Two Kroma arms use frozen binaries with identical config, prompt, tools, task
fixture, and provider. Pi is a third *product* reference, not a causal arm.
This script only writes under /tmp and never modifies the live checkout.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import random
import re
import shutil
import signal
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_BEFORE = Path("/tmp/kroma_before_fix_20261028")
DEFAULT_AFTER = Path("/tmp/kroma_after_fix_20261028")
TASKS = ("cross_file", "records", "rust_regression")
ARMS = ("before", "after", "pi")
PI = ["pi", "--offline", "--no-extensions", "--no-skills", "--no-mcp",
      "--no-context-files", "--no-approve", "--provider", "workbuddy",
      "--model", "deepseek", "--thinking", "high", "--mode", "json",
      "--print", "--no-session"]


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, ROOT / "benchmarks" / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


engineering = load("engineering_fix", "engineering_compare.py")
suite = load("tasks_fix", "agent_task_suite.py")
metrics = load("metrics_fix", "tool_ablation_live.py")


def source(task, work):
    work.mkdir(parents=True, exist_ok=True, mode=0o700)
    if task == "rust_regression":
        engineering.make_copy(work, "A", 1)
        return engineering.A_PROMPT + (
            "\nOnly work in the isolated source tree. Never read API keys or "
            "modify the injected regression test. Do not commit."
        )
    item = next(x for x in suite.TASKS if x["id"] == {
        "records": "implement_records", "cross_file": "cross_file_fix"
    }[task])
    suite.create_fixture(work, item)
    return item["prompt"] + (
        "\nOnly work in the isolated source tree. Do not access credentials "
        "or modify the provided tests."
    )


def verify(task, work):
    try:
        if task == "rust_regression":
            evidence = (work / "src/sessions.rs").read_text()
            if evidence.count("fn wal_append_controls_recent_session_order()") != 1:
                return False
            return bool(engineering.verify(work, "A", 1))
        return bool(suite.verify(work, {
            "records": "implement_records", "cross_file": "cross_file_fix"
        }[task], ""))
    except Exception:
        return False


def isolated_pi_config(lab):
    """Prevent ~/.pi/agent/APPEND_SYSTEM.md from biasing the Pi reference."""
    source_dir = Path.home() / ".pi" / "agent"
    target = lab / "pi-agent-isolated"
    target.mkdir(mode=0o700)
    for name in ("models.json", "auth.json"):
        src = source_dir / name
        if not src.is_file():
            raise FileNotFoundError("Pi credentials or model configuration unavailable")
        dst = target / name
        shutil.copyfile(src, dst)
        dst.chmod(0o600)
    (target / "settings.json").write_text("{}\n")
    (target / "settings.json").chmod(0o600)
    return target


def pi_usage(raw):
    result, _ = metrics.parse_pi(raw)
    reasoning = 0
    missing = False
    for line in raw.splitlines():
        try:
            e = json.loads(line)
        except (ValueError, TypeError):
            continue
        if e.get("type") != "message_end":
            continue
        m = e.get("message") or {}
        if m.get("role") != "assistant":
            continue
        u = m.get("usage") or {}
        value = u.get("reasoning")
        if value is None:
            missing = True
        else:
            reasoning += value
    result["reasoning"] = None if missing else reasoning
    return result


def kroma_usage(raw):
    result = metrics.parse_kroma(raw)
    total = 0
    missing = False
    for line in raw.splitlines():
        if not line.startswith("[usage] request:"):
            continue
        m = re.search(r"\breasoning:\s*(\d+|n/a)", line)
        if not m or m.group(1) == "n/a":
            missing = True
        else:
            total += int(m.group(1))
    result["reasoning"] = None if missing else total
    return result


def run(lab, task, repetition, arm, pi_agent_dir, before, after, timeout, provider=None):
    dest = lab / task / str(repetition) / arm
    work = dest / "source"
    prompt = source(task, work)
    env = {k: v for k, v in os.environ.items() if not k.startswith("KROMA_")}
    env["CARGO_TARGET_DIR"] = str(ROOT / "target")
    env["CARGO_INCREMENTAL"] = "0"
    env["TMPDIR"] = "/tmp"
    if arm == "pi":
        env["PI_CODING_AGENT_DIR"] = str(pi_agent_dir)
        cmd = (PI if provider is None else [
            'pi', '--offline', '--no-extensions', '--no-skills', '--no-mcp',
            '--no-context-files', '--no-approve', '--provider', 'cobblemon',
            '--model', 'gpt-6.1-sol', '--thinking', 'medium', '--mode', 'json',
            '--print', '--no-session']) + [prompt]
    else:
        env.pop("PI_CODING_AGENT_DIR", None)
        env.update(
            KROMA_PROVIDER_PROFILE="workbuddy",
            KROMA_PROFILE="1",
            KROMA_WORKDIR=str(work),
            KROMA_SESSION_FILE=str(dest / "session.json"),
            KROMA_REASONING_EFFORT="high",
            KROMA_DEEPSEEK_THINKING="off",
        )
        if provider is not None:
            env.pop('KROMA_PROVIDER_PROFILE', None)
            env.update(KROMA_BASE_URL=provider['baseUrl'],
                KROMA_API_KEY=provider['apiKey'], KROMA_MODEL='gpt-6.1-sol',
                KROMA_CONTEXT_WINDOW='500000', KROMA_REASONING_EFFORT='medium')
        cmd = [str(before if arm == "before" else after), "aligned", prompt]

    start = time.monotonic()
    proc = subprocess.Popen(
        cmd, cwd=work, env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
    )
    expired = False
    try:
        stdout, stderr = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        expired = True
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        stdout, stderr = proc.communicate(timeout=12)
    elapsed = round(time.monotonic() - start, 3)
    for name, data in (("stdout.log", stdout), ("stderr.log", stderr)):
        path = dest / name
        path.write_bytes(data)
        path.chmod(0o600)
    values = (pi_usage(stdout.decode(errors="replace"))
              if arm == "pi" else kroma_usage(stderr.decode(errors="replace")))
    check_start = time.monotonic()
    passed_check = verify(task, work)
    verify_time = round(time.monotonic() - check_start, 3)
    disk_error = b"No space left on device" in (stdout + stderr)
    return {
        "task": task, "repeat": repetition, "arm": arm,
        "elapsed_s": elapsed, "verification_s": verify_time,
        "passed": bool(proc.returncode == 0 and passed_check and not expired),
        "verified": passed_check, "exit": proc.returncode,
        "timeout": expired, "disk_error": disk_error,
        "metrics": values, "logs": str(dest)
    }


def sha256(path):
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--before", type=Path, default=DEFAULT_BEFORE)
    p.add_argument("--after", type=Path, default=DEFAULT_AFTER)
    p.add_argument("--tasks", default="cross_file,rust_regression")
    p.add_argument("--repeats", type=int, default=2)
    p.add_argument("--timeout-python", type=int, default=110)
    p.add_argument("--timeout-rust", type=int, default=190)
    p.add_argument("--seed", type=int, default=20261028)
    p.add_argument("--sol-medium", action="store_true")
    a = p.parse_args()
    tasks = a.tasks.split(",")
    assert 1 <= a.repeats <= 5 and all(task in TASKS for task in tasks)
    assert a.before.exists() and a.after.exists()
    lab = Path("/tmp") / ("kroma-fix-crossover-" + uuid.uuid4().hex[:9])
    lab.mkdir(mode=0o700)
    pi_dir = isolated_pi_config(lab)
    provider = None
    if a.sol_medium:
        provider = json.loads((Path.home()/'.pi/agent/models.json').read_text())['providers']['cobblemon']
        assert any(m['id'] == 'gpt-6.1-sol' for m in provider['models'])
    rng = random.Random(a.seed)
    pairs = [(task, rep) for task in tasks for rep in range(1, a.repeats + 1)]
    rng.shuffle(pairs)
    plan = []
    for task, rep in pairs:
        arms = list(ARMS)
        rng.shuffle(arms)
        plan.append({"task": task, "repeat": rep, "order": arms})
    report = {
        "model": "cobblemon/gpt-6.1-sol" if a.sol_medium else "workbuddy/deepseek",
        "thinking": "medium" if a.sol_medium else "high",
        "config": "both Kroma arms aligned reasoning_effort=high without thinking.type",
        "notes": [
            "Pi uses isolated agent dir without APPEND_SYSTEM.md",
            "Same Python/Rust fixtures, system prompt and tools for before/after",
            "Pi has its own system prompt and native tools; reference, not causal control",
            "Elapsed time excludes external independent verification",
            "Shared Cargo cache and model serving load are not controlled",
        ],
        "before_sha256": sha256(a.before),
        "after_sha256": sha256(a.after),
        "seed": a.seed, "plan": plan, "results": [],
    }
    path = lab / "results.json"
    path.write_text(json.dumps(report, indent=2) + "\n")
    print("LAB", lab, "PLAN", plan, flush=True)
    for group in plan:
        for arm in group["order"]:
            task, rep = group["task"], group["repeat"]
            print("START", task, rep, arm, flush=True)
            try:
                row = run(
                    lab, task, rep, arm, pi_dir, a.before, a.after,
                    a.timeout_rust if task == "rust_regression" else a.timeout_python,
                    provider
                )
            except Exception as exc:
                row = {"task": task, "repeat": rep, "arm": arm,
                       "passed": False, "error": f"{type(exc).__name__}: {str(exc)[:120]}"}
            report["results"].append(row)
            tmp = path.with_suffix(".tmp")
            tmp.write_text(json.dumps(report, indent=2) + "\n")
            tmp.replace(path)
            m = row.get("metrics") or {}
            print("END", task, rep, arm, "pass", row["passed"],
                  "wall_s", row.get("elapsed_s"),
                  "requests", m.get("requests"), "input", m.get("input"),
                  "cached", m.get("cached"), "output", m.get("output"),
                  "reasoning", m.get("reasoning"), "tools", m.get("tools"),
                  "timeout", row.get("timeout"), "disk_error", row.get("disk_error"),
                  flush=True)
    print("REPORT", path, flush=True)


if __name__ == "__main__":
    main()
