#!/usr/bin/env python3
"""P7: full A/B/F ablation on continuous Rust + independent Python coding.

Only a test-copy's outgoing system prompt is changed. The actual Kroma
project is read-only to the experiment; all code edits happen in private
isolated fixtures. Each trial is checked by external acceptance tests.

Commands: setup, calibrate, run, run-python, run-rust, report, trace.
No agent task-level timeout, turn budget or token quota.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
LAB = Path("/root/kroma-p7-abf")
SEED = LAB / "seed"
RUNTIME = LAB / "runtime"
TARGET = LAB / "runtime-target"
BINARY = TARGET / "release" / "kroma"
RESULTS = LAB / "results.json"
ARMS = ("A", "B", "F")
RUST_REPEATS = 5
RUST_STAGES = (0, 1, 2, 3)  # remove / rename / JSON / integrated review
PY_REPEATS = 2
PY_TASKS = ("bug", "feature")


def import_script(path, name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "benchmarks" / path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


p3 = import_script("p3_real_dev.py", "p7_p3")
p1 = import_script("p0p1_fair.py", "p7_p1")
p6 = import_script("p6_rust_prompt.py", "p7_p6")

FIRST = "For coding tasks, focus inspections on relevant files."
B_CLAUSE = ("For coding tasks, use focused searches of relevant code; once "
            "the needed evidence is found, edit rather than re-inspecting unchanged files.")
F_CLAUSE = ("For coding tasks, focus inspections on relevant files; once "
            "the needed evidence is found, edit rather than re-inspecting unchanged files.")
UNSAFE_NAMES = ("KROMA_MODEL", "KROMA_BASE_URL", "KROMA_API_KEY",
                "KROMA_CONFIG_FILE", "KROMA_PROVIDER_PROFILE", "KROMA_DEEPSEEK_THINKING")


def save(path, data):
    tmp = path.with_suffix(".new")
    tmp.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
    tmp.replace(path)


def checksum(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def relative_source_files():
    return [p.relative_to(ROOT) for p in p3.source_files()]


def seed_digest():
    # Hash input order must match the source snapshots below. Sorting only
    # one side silently invalidates identical byte-for-byte project copies.
    paths = [SEED / p for p in relative_source_files()]
    return p3.snapshot_digest(paths, SEED)


def setup():
    if LAB.exists():
        raise RuntimeError(f"Refusing to overwrite existing P7 lab: {LAB}")
    LAB.mkdir(mode=0o700)
    SEED.mkdir(mode=0o700)
    RUNTIME.mkdir(mode=0o700)

    files = relative_source_files()
    for p in files:
        dest = SEED / p
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / p, dest)
    for item in ("Cargo.toml", "Cargo.lock", "README.md", ".env.example", "src", ".cargo"):
        path = SEED / item
        dest = RUNTIME / item
        if path.is_dir():
            shutil.copytree(path, dest)
        elif path.is_file():
            shutil.copy2(path, dest)
    agent = RUNTIME / "src" / "agent.rs"
    raw = agent.read_text()
    needle = "system.push_str(EXECUTION_GUIDANCE);"
    if raw.count(needle) != 1 or raw.count(FIRST) != 1:
        raise RuntimeError("Prompt insertion anchor changed: refuse invalid experiment")
    inject = '''let arm = std::env::var("KROMA_P7_ARM").unwrap_or_default();
        let variant = match arm.as_str() {
            "B" => Some("For coding tasks, use focused searches of relevant code; once the needed evidence is found, edit rather than re-inspecting unchanged files."),
            "F" => Some("For coding tasks, focus inspections on relevant files; once the needed evidence is found, edit rather than re-inspecting unchanged files."),
            _ => None,
        };
        if let Some(clause) = variant {
            system.push_str(&EXECUTION_GUIDANCE.replace(
                "For coding tasks, focus inspections on relevant files.", clause
            ));
        } else {
            system.push_str(EXECUTION_GUIDANCE);
        }'''
    agent.write_text(raw.replace(needle, inject, 1))

    metadata = {
        "project_git_head": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "frozen_source_sha256": seed_digest(),
        "frozen_agent_sha256": checksum(SEED / "src/agent.rs"),
        "frozen_tools_sha256": checksum(SEED / "src/tools.rs"),
        "runtime_agent_sha256": checksum(agent),
        "model": "configured WorkBuddy DeepSeek High; provider credentials NOT included",
        "design": "three-arm randomized rotation, single frozen baseline and runtime",
        "arms": {"A": FIRST, "B": B_CLAUSE, "F": F_CLAUSE},
        "rust_repeats": RUST_REPEATS, "rust_stages": list(RUST_STAGES),
        "python_repeats": PY_REPEATS, "python_tasks": list(PY_TASKS),
        "agent_timeout": None, "agent_token_limit": None,
        "acceptance": "independent P3 Rust CLI verification with dummy keys; P1 hidden Python tests",
        "cost_note": "report uncached/cached/output independently; provider billing price not assumed",
    }
    save(RESULTS, {"metadata": metadata, "trials": []})
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(TARGET)
    print("P7_BUILD_START", flush=True)
    start = time.monotonic()
    subprocess.run(["cargo", "build", "--release", "--locked", "--offline", "--quiet"],
                   cwd=RUNTIME, env=env, check=True)
    print("P7_BUILD_OK", round(time.monotonic() - start, 2), "binary", BINARY, flush=True)


def base_env(work, session, arm):
    env = os.environ.copy()
    for name in UNSAFE_NAMES:
        env.pop(name, None)
    env.update(KROMA_P7_ARM=arm, KROMA_WORKDIR=str(work),
               KROMA_SESSION_FILE=str(session), KROMA_PROFILE="1",
               CARGO_TARGET_DIR=str(ROOT / "target"))
    return env


def agent_run(prompt, work, logs, env):
    logs.mkdir(parents=True, exist_ok=False)
    stdout, stderr = logs / "stdout.log", logs / "stderr.log"
    start = time.monotonic()
    with stdout.open("wb") as out, stderr.open("wb") as err:
        os.chmod(stdout, 0o600)
        os.chmod(stderr, 0o600)
        process = subprocess.Popen([str(BINARY), prompt], cwd=work, env=env,
                                   stdin=subprocess.DEVNULL, stdout=out, stderr=err)
        exit_code = process.wait()  # No task-level deadline or token budget
    return round(time.monotonic() - start, 3), exit_code


def metrics(log):
    # Keep unavailable usage distinguishable from genuine zero token usage.
    result = p6.parse_usage(log)
    profile = re.search(r"^\[profile\] wall_ms=([^\n]+)", log, re.M)
    result["profile"] = {}
    if profile:
        for name, raw in re.findall(r"([a-z_]+)=([0-9.]+)", profile.group(1)):
            if name.endswith("_ms") or name.endswith("_bytes"):
                result["profile"][name] = float(raw)
    commands = re.findall(r"^\[shell\] (.+)$", log, re.M)
    classes = [p6.classify_command(x) for x in commands]
    first_edit = next((i for i, val in enumerate(classes) if val == "possible_edit"), len(classes))
    result["trace"] = {
        "shell_command_lines": len(commands),
        "exact_duplicate_command_lines": len(commands) - len(set(commands)),
        "inspection_class_commands": classes.count("inspection"),
        "verification_class_commands": classes.count("verification"),
        "possible_edit_class_commands": classes.count("possible_edit"),
        "inspections_before_possible_edit": classes[:first_edit].count("inspection"),
        "method": "conservative regex over first line of each [shell] record; heuristic, not true action count",
    }
    return result


def init_rust(repeat):
    experiment = LAB / f"rust-{repeat:02d}"
    experiment.mkdir(mode=0o700, exist_ok=True)
    expected_digest = seed_digest()
    for arm in ARMS:
        source = experiment / arm / "source"
        if source.exists():
            paths = [source / p for p in relative_source_files()]
            if not all(p.is_file() for p in paths):
                raise RuntimeError(f"Partial initial source copy: {source}")
            # A stage changes its own source checkout. Only compare initial
            # bytes if that arm has not begun any agent development yet.
            if not (experiment / arm / "logs").exists():
                if p3.snapshot_digest(paths, source) != expected_digest:
                    raise RuntimeError(f"Initial project mismatch: {source}")
            continue
        for rel in relative_source_files():
            to = source / rel
            to.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(SEED / rel, to)
        paths = [source / p for p in relative_source_files()]
        if p3.snapshot_digest(paths, source) != expected_digest:
            raise RuntimeError(f"Initial project mismatch: {source}")
    return experiment


def rust_stage(repeat, arm, stage_index):
    experiment = init_rust(repeat)
    work = experiment / arm / "source"
    name, instruction = p3.STAGES[stage_index]
    logs = experiment / arm / "logs" / f"{stage_index+1:02d}-{name}"
    if logs.exists():
        raise RuntimeError(f"Prior unrecorded trial requires review: {logs}")
    env = base_env(work, experiment / arm / "session.json", arm)
    print("P7_START rust", repeat, arm, name, flush=True)
    elapsed, exit_code = agent_run(instruction + "\n\n" + p3.GUARD, work, logs, env)
    parsed = metrics((logs / "stderr.log").read_text(errors="replace"))
    p3.P3 = experiment
    p3.CARGO_TARGET = ROOT / "target"
    try:
        checks = p3.acceptance(arm, stage_index)
    except Exception as exc:
        checks = {"acceptance_harness_error": False}
        print("P7_ACCEPTANCE_ERROR", type(exc).__name__, str(exc), flush=True)
    session = experiment / arm / "session.json"
    session_ok = session.is_file() and Path(str(session) + ".wal").is_file()
    passed = exit_code == 0 and session_ok and all(value is True for value in checks.values())
    return {
        "suite": "rust", "repeat": repeat, "arm": arm,
        "stage": name, "stage_index": stage_index,
        "seconds": elapsed, "exit": exit_code, "session_ok": session_ok,
        "checks": checks, "passed": passed, "usage": parsed,
        "logs": str(logs), "source": str(work),
    }


def python_trial(repeat, arm, task):
    folder = LAB / f"python-{repeat:02d}" / task / arm
    if folder.exists():
        raise RuntimeError(f"Prior unrecorded Python run requires review: {folder}")
    folder.mkdir(parents=True, mode=0o700)
    src = p1.prepare(task, f"p7-{task}-{repeat}-{arm}")
    work = folder / "source"
    shutil.move(str(src), work)
    if p1.verify(task, work):
        raise RuntimeError(f"Python baseline already passes acceptance: {task}")
    env = base_env(work, folder / "session.json", arm)
    instruction = p1.TASKS[task] + (
        "\nWork only inside this isolated project checkout. Do not read "
        "real API credentials, modify other projects, use network "
        "operations, or make Git commits. Complete required verification."
    )
    logs = folder / "logs"
    print("P7_START python", repeat, arm, task, flush=True)
    elapsed, exit_code = agent_run(instruction, work, logs, env)
    parsed = metrics((logs / "stderr.log").read_text(errors="replace"))
    acceptance_name = "test_p1_regression.py" if task == "bug" else "test_p1_feature.py"
    reference = p1.BUG_TEST if task == "bug" else p1.FEATURE_TEST
    preserved = (work / "tests" / acceptance_name).read_text() == reference
    try:
        accepted = p1.verify(task, work)
        upstream = subprocess.run(
            ["python3", "-m", "pytest", "-q",
             "tests/test_urlutils.py" if task == "bug" else "tests/test_strutils.py"],
            cwd=work, capture_output=True, text=True).returncode == 0
    except Exception as exc:
        accepted, upstream = False, False
        print("P7_PYTHON_ACCEPTANCE_ERROR", type(exc).__name__, str(exc), flush=True)
    checks = {"hidden_acceptance": accepted, "unchanged_hidden_test": preserved,
              "upstream_tests": upstream}
    passed = exit_code == 0 and all(checks.values())
    return {
        "suite": "python", "repeat": repeat, "arm": arm,
        "stage": task, "stage_index": 0,
        "seconds": elapsed, "exit": exit_code, "session_ok": True,
        "checks": checks, "passed": passed, "usage": parsed,
        "logs": str(logs), "source": str(work),
    }


def calibrate():
    if not BINARY.is_file():
        raise RuntimeError("Run setup before calibration")
    results = []
    for arm in ARMS:
        folder = LAB / "calibration" / arm
        folder.mkdir(parents=True, mode=0o700, exist_ok=False)
        work = folder / "source"
        work.mkdir()
        (work / "marker.txt").write_text("P7_MARKER_OK\n")
        elapsed, code = agent_run(
            "Read marker.txt through the shell tool. Reply with only the marker value.",
            work, folder / "logs", base_env(work, folder / "session.json", arm))
        log = (folder / "logs" / "stdout.log").read_text(errors="replace")
        parsed = metrics((folder / "logs" / "stderr.log").read_text(errors="replace"))
        passed = code == 0 and "P7_MARKER_OK" in log and parsed["tools"] >= 1
        results.append({"arm": arm, "pass": passed, "seconds": elapsed,
                        "requests": parsed["requests"], "tools": parsed["tools"]})
        print("P7_CALIBRATION", arm, passed, "seconds", elapsed, flush=True)
        if not passed:
            save(LAB / "calibration.json", results)
            raise RuntimeError(f"Tool calibration failed for {arm}")
    save(LAB / "calibration.json", results)


def schedule(suite):
    if suite in ("python", "all"):
        for repeat in range(1, PY_REPEATS + 1):
            for j, task in enumerate(PY_TASKS):
                offset = (repeat + j - 1) % 3
                order = ARMS[offset:] + ARMS[:offset]
                for arm in order:
                    yield "python", repeat, arm, task, 0
    if suite in ("rust", "all"):
        for repeat in range(1, RUST_REPEATS + 1):
            init_rust(repeat)
            for stage_index in RUST_STAGES:
                offset = (repeat + stage_index - 1) % 3
                order = ARMS[offset:] + ARMS[:offset]
                for arm in order:
                    yield "rust", repeat, arm, p3.STAGES[stage_index][0], stage_index


def run(suite):
    if not (LAB / "calibration.json").is_file():
        raise RuntimeError("Calibrate all arms before launching real coding")
    if not all(x["pass"] for x in json.loads((LAB / "calibration.json").read_text())):
        raise RuntimeError("Calibration is not fully passing")
    data = json.loads(RESULTS.read_text())
    for kind, repeat, arm, stage, stage_index in schedule(suite):
        if any(x["suite"] == kind and x["repeat"] == repeat
               and x["stage"] == stage and x["arm"] == arm for x in data["trials"]):
            continue
        row = (rust_stage(repeat, arm, stage_index) if kind == "rust"
               else python_trial(repeat, arm, stage))
        data["trials"].append(row)
        save(RESULTS, data)
        print("P7_END", kind, repeat, arm, stage,
              "pass", row["passed"], "seconds", row["seconds"],
              "requests", row["usage"]["requests"],
              "tools", row["usage"]["tools"], flush=True)
    report()


def report():
    data = json.loads(RESULTS.read_text())
    print("P7_TOTAL", len(data["trials"]), "of",
          len(ARMS) * (RUST_REPEATS * len(RUST_STAGES) + PY_REPEATS * len(PY_TASKS)),
          "stages", flush=True)
    for suite in ("python", "rust", "both"):
        group = [x for x in data["trials"] if x["suite"] == suite or suite == "both"]
        for arm in ARMS:
            rows = [x for x in group if x["arm"] == arm]
            if not rows:
                continue
            u = [x["usage"] for x in rows]
            print("P7_SUMMARY", suite, arm, "n", len(rows),
                  "pass", sum(x["passed"] for x in rows),
                  "mean_s", round(statistics.mean(x["seconds"] for x in rows), 2),
                  "total_s", round(sum(x["seconds"] for x in rows), 2),
                  "requests", sum(m["requests"] for m in u),
                  "tools", sum(m["tools"] for m in u),
                  "uncached", sum(m["uncached"] for m in u),
                  "cached", sum(m["cached"] for m in u),
                  "output", sum(m["output"] for m in u),
                  "timeout_retries", sum(m["model_timeouts"] for m in u),
                  "apec_errors", sum(m["apec_errors"] for m in u),
                  flush=True)


def trace():
    data = json.loads(RESULTS.read_text())
    for suite in ("python", "rust", "both"):
        for arm in ARMS:
            rows = [x["usage"]["trace"] for x in data["trials"]
                    if x["arm"] == arm and (x["suite"] == suite or suite == "both")]
            if not rows:
                continue
            print("P7_TRACE", suite, arm, "n", len(rows),
                  "duplicate", sum(x["exact_duplicate_command_lines"] for x in rows),
                  "inspections", sum(x["inspection_class_commands"] for x in rows),
                  "before_edit", sum(x["inspections_before_possible_edit"] for x in rows),
                  "verification", sum(x["verification_class_commands"] for x in rows),
                  flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("setup", "calibrate", "run", "run-python",
                                          "run-rust", "report", "trace"))
    action = parser.parse_args().action
    {"setup": setup, "calibrate": calibrate, "run": lambda: run("all"),
     "run-python": lambda: run("python"), "run-rust": lambda: run("rust"),
     "report": report, "trace": trace}[action]()
