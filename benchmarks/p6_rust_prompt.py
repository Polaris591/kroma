#!/usr/bin/env python3
"""P6: A/B cross-stage Rust development and system-clause ablation.

Four isolated outgoing prompt variants (A/B/E/F); all runtime/tool behavior
is otherwise byte-identical. Production code is never altered by benchmark.

Usage: python3 benchmarks/p6_rust_prompt.py setup|calibrate|ab|ablate|report|trace
"""
from __future__ import annotations

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
LAB = Path("/tmp/kroma-p6-rust-prompt")
RUNTIME = LAB / "runtime"
TARGET = LAB / "runtime-target"
TASK_TARGET = ROOT / "target"  # Disk-backed; /tmp is a small tmpfs on this server.
BINARY = TARGET / "release" / "kroma"
RESULTS = LAB / "results.json"

spec = importlib.util.spec_from_file_location("p3", ROOT / "benchmarks/p3_real_dev.py")
p3 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p3)

GUIDANCE_FIRST = "For coding tasks, focus inspections on relevant files."
B_CLAUSE = (
    "For coding tasks, use focused searches of relevant code; once the needed "
    "evidence is found, edit rather than re-inspecting unchanged files."
)
E_CLAUSE = "For coding tasks, use focused searches of relevant code."
F_CLAUSE = (
    "For coding tasks, focus inspections on relevant files; once the needed "
    "evidence is found, edit rather than re-inspecting unchanged files."
)


def write_json(path, value):
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")
    tmp.replace(path)


def replace_once(path, old, new):
    source = path.read_text()
    if source.count(old) != 1:
        raise RuntimeError(f"Expected one match in {path} for {old!r}, found {source.count(old)}")
    path.write_text(source.replace(old, new, 1))


def setup():
    if LAB.exists():
        raise RuntimeError(f"Refuse to overwrite existing P6 experiment at {LAB}")
    LAB.mkdir(mode=0o700)
    RUNTIME.mkdir(mode=0o700)
    for item in ("Cargo.toml", "Cargo.lock", "README.md", ".env.example", "src", ".cargo"):
        src = ROOT / item
        if src.is_dir():
            shutil.copytree(src, RUNTIME / item)
        elif src.exists():
            shutil.copy2(src, RUNTIME / item)
    agent = RUNTIME / "src/agent.rs"
    replace_once(agent, "system.push_str(EXECUTION_GUIDANCE);", """let arm = std::env::var("KROMA_P6_ARM").unwrap_or_default();
        let clause = match arm.as_str() {
            "B" => Some("For coding tasks, use focused searches of relevant code; once the needed evidence is found, edit rather than re-inspecting unchanged files."),
            "E" => Some("For coding tasks, use focused searches of relevant code."),
            "F" => Some("For coding tasks, focus inspections on relevant files; once the needed evidence is found, edit rather than re-inspecting unchanged files."),
            _ => None,
        };
        if let Some(clause) = clause {
            system.push_str(&EXECUTION_GUIDANCE.replace(
                "For coding tasks, focus inspections on relevant files.",
                clause
            ));
        } else {
            system.push_str(EXECUTION_GUIDANCE);
        }""")
    # A is exactly the original system prompt. B reproduces P5's winning text.
    meta = {
        "date": time.strftime("%Y-%m-%d"),
        "git_head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "baseline_agent_sha": hashlib.sha256((ROOT / "src/agent.rs").read_bytes()).hexdigest(),
        "baseline_tool_sha": hashlib.sha256((ROOT / "src/tools.rs").read_bytes()).hexdigest(),
        "baseline_tree_digest": p3.snapshot_digest(p3.source_files()),
        "same_model": "configured WorkBuddy DeepSeek High; no account credentials copied",
        "arms": {"A": GUIDANCE_FIRST, "B": B_CLAUSE, "E": E_CLAUSE, "F": F_CLAUSE},
        "ab_repeats": 3, "ab_stages": ["remove", "rename", "json"],
        "ablation_repeats": 2, "ablation_stages": ["remove"],
        "agent_timeout": None, "token_quota": None,
    }
    write_json(RESULTS, {"metadata": meta, "runs": []})
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(TARGET)
    print("P6_BUILD_START", flush=True)
    start = time.monotonic()
    subprocess.run(["cargo", "build", "--release", "--locked", "--offline", "--quiet"],
                   cwd=RUNTIME, env=env, check=True)
    print("P6_BUILD_OK", round(time.monotonic()-start, 2), "seconds", flush=True)


def get_experiment(kind, repeat):
    return LAB / f"{kind}-{repeat:02d}"


def init_trial_sources(kind, repeat, arms):
    experiment = get_experiment(kind, repeat)
    expected_files = p3.source_files()
    digest = p3.snapshot_digest(expected_files)
    for arm in arms:
        source = experiment / arm / "source"
        if source.exists():
            actual = [source / x.relative_to(ROOT) for x in expected_files]
            if not all(x.exists() for x in actual):
                raise RuntimeError(f"Missing files in {source}")
            # Do not check digest after stage 0, because the agents edit code.
            continue
        source.mkdir(parents=True, mode=0o700)
        for original in expected_files:
            destination = source / original.relative_to(ROOT)
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(original, destination)
        actual = [source / x.relative_to(ROOT) for x in expected_files]
        if p3.snapshot_digest(actual, source) != digest:
            raise RuntimeError(f"Unequal initial source bytes for {source}")
    return experiment


def calibrate():
    for arm in ("A", "B", "E", "F"):
        trial = LAB / "calibration" / arm
        trial.mkdir(parents=True, mode=0o700, exist_ok=False)
        work = trial / "source"
        work.mkdir()
        (work / "marker.txt").write_text("P6_MARKER_OK\n")
        env = runner_env(work, trial, arm)
        elapsed, exit_code = run_agent(
            "Use the shell tool to read marker.txt and reply with its marker exactly.",
            work, trial, env)
        text = (trial / "stdout.log").read_text(errors="replace")
        if exit_code != 0 or "P6_MARKER_OK" not in text:
            raise RuntimeError(f"Calibration failed for {arm}")
        print("P6_CALIBRATE", arm, "seconds", elapsed, "pass", True, flush=True)


def runner_env(work, trial, arm):
    env = os.environ.copy()
    for name in ("KROMA_MODEL", "KROMA_API_KEY", "KROMA_BASE_URL", "KROMA_CONFIG_FILE",
                 "KROMA_PROVIDER_PROFILE", "KROMA_DEEPSEEK_THINKING"):
        env.pop(name, None)
    session_root = trial.parent.parent if trial.parent.name == "logs" else trial
    env.update(KROMA_P6_ARM=arm, KROMA_WORKDIR=str(work),
               KROMA_SESSION_FILE=str(session_root / "session.json"),
               KROMA_PROFILE="1", CARGO_TARGET_DIR=str(TASK_TARGET))
    return env


def run_agent(prompt, work, logs, env):
    logs.mkdir(parents=True, exist_ok=True)
    stdout, stderr = logs / "stdout.log", logs / "stderr.log"
    start = time.monotonic()
    with stdout.open("wb") as out, stderr.open("wb") as err:
        os.chmod(stdout, 0o600)
        os.chmod(stderr, 0o600)
        process = subprocess.Popen([str(BINARY), prompt], cwd=work, env=env,
                                   stdin=subprocess.DEVNULL, stdout=out, stderr=err)
        code = process.wait()  # No task-level deadline/token quota.
    return round(time.monotonic() - start, 3), code


def parse_usage(log):
    records = []
    for line in log.splitlines():
        if not line.startswith("[usage] request:"):
            continue
        values = {}
        for key in ("input", "cached", "output"):
            match = re.search(r"\b" + key + r": (\d+|n/a)", line)
            values[key] = int(match.group(1)) if match and match.group(1) != "n/a" else None
        records.append(values)
    metrics = {
        "requests": len(records),
        "model_starts": len(re.findall(r"^\[profile\] model_start", log, re.M)),
        "model_finishes": len(re.findall(r"^\[profile\] model_done", log, re.M)),
        "tools": len(re.findall(r"^\[profile\] tool_done", log, re.M)),
        "model_timeouts": log.count("[model] request timed out"),
        "apec_errors": log.count("[apec] summary failed"),
        "cached": sum(x["cached"] or 0 for x in records),
        "uncached": sum(x["input"] - x["cached"] for x in records
                        if x["input"] is not None and x["cached"] is not None),
        "output": sum(x["output"] or 0 for x in records),
        "usage_unknown": any(None in x.values() for x in records),
    }
    apec = re.findall(r"^\[usage\] APEC: (\d+) request", log, re.M)
    metrics["apec_requests"] = sum(int(x) for x in apec)
    return metrics


def stage(kind, repeat, arm, index):
    experiment = get_experiment(kind, repeat)
    p3.P3 = experiment
    p3.CARGO_TARGET = TASK_TARGET
    work = experiment / arm / "source"
    stage_name, instruction = p3.STAGES[index]
    logs = experiment / arm / "logs" / f"{index+1:02d}-{stage_name}"
    if logs.exists():
        raise RuntimeError(f"Already ran {logs}")
    env = runner_env(work, logs, arm)
    prompt = instruction + "\n\n" + p3.GUARD
    print("P6_START", kind, repeat, arm, stage_name, flush=True)
    elapsed, exit_code = run_agent(prompt, work, logs, env)
    log = (logs / "stderr.log").read_text(errors="replace")
    metrics = parse_usage(log)
    # Acceptance uses private dummy keys, compiled from each agent's copy.
    try:
        checks = p3.acceptance(arm, index)
    except Exception as error:
        checks = {"acceptance_error": str(error), "acceptance_pass": False}
    session_path = experiment / arm / "session.json"
    session_evidence = {
        "checkpoint": session_path.is_file(),
        "wal": Path(str(session_path) + ".wal").is_file(),
    }
    # WAL must survive restart; the checkpoint can lag journal writes.
    success = exit_code == 0 and all(v is True for v in checks.values()) and all(session_evidence.values())
    row = {
        "kind": kind, "repeat": repeat, "arm": arm,
        "stage": stage_name, "stage_index": index,
        "elapsed_s": elapsed, "exit": exit_code, "pass": success,
        "acceptance": checks, "session": session_evidence,
        "metrics": metrics, "logs": str(logs),
    }
    print("P6_END", kind, repeat, arm, stage_name, "pass", success,
          "seconds", elapsed, "requests", metrics["requests"], "tools", metrics["tools"],
          "apec_errors", metrics["apec_errors"], flush=True)
    return row


def run(kind):
    plan = (("A", "B"), 3, 3) if kind == "ab" else (("A", "B", "E", "F"), 2, 1)
    arms, repeats, num_stages = plan
    data = json.loads(RESULTS.read_text())
    for repeat in range(1, repeats + 1):
        init_trial_sources(kind, repeat, arms)
        for index in range(num_stages):
            rotation = (repeat + index - 1) % len(arms)
            order = arms[rotation:] + arms[:rotation]
            for arm in order:
                if any(x["kind"] == kind and x["repeat"] == repeat and x["stage_index"] == index
                       and x["arm"] == arm for x in data["runs"]):
                    continue
                row = stage(kind, repeat, arm, index)
                data["runs"].append(row)
                write_json(RESULTS, data)
    report()


def report():
    data = json.loads(RESULTS.read_text())
    for kind in ("ab", "ablate"):
        for arm in (("A", "B") if kind == "ab" else ("A", "B", "E", "F")):
            rows = [x for x in data["runs"] if x["kind"] == kind and x["arm"] == arm]
            if not rows:
                continue
            print("P6_SUMMARY", kind, arm,
                  "n", len(rows),
                  "passed", sum(x["pass"] for x in rows),
                  "avg_wall", round(statistics.mean(x["elapsed_s"] for x in rows), 2),
                  "requests", sum(x["metrics"]["requests"] for x in rows),
                  "tools", sum(x["metrics"]["tools"] for x in rows),
                  "uncached", sum(x["metrics"]["uncached"] for x in rows),
                  "cached", sum(x["metrics"]["cached"] for x in rows),
                  "apec_errors", sum(x["metrics"]["apec_errors"] for x in rows),
                  flush=True)


def classify_command(command):
    """Conservative heuristic, not ground truth; traces remain auditable."""
    if re.search(r"\b(?:cargo\s+(?:test|fmt|clippy|check)|pytest|rustc\s+--version)\b", command):
        return "verification"
    if re.search(r"\b(?:python3?\s+-c|python3?\s+-\s*<<|perl\s+-[pi]|sed\s+-i|apply_patch|tee\s+|cat\s*>|printf\s+.*>\s*\S)", command):
        return "possible_edit"
    if re.search(r"\b(?:rg|grep|sed\s+-n|head|tail|cat|ls|find|wc|git\s+(?:status|diff))\b", command):
        return "inspection"
    return "other"


def trace():
    data = json.loads(RESULTS.read_text())
    rows = []
    for entry in data["runs"]:
        log = (Path(entry["logs"]) / "stderr.log").read_text(errors="replace")
        commands = re.findall(r"^\[shell\] (.+)$", log, re.M)
        kinds = [classify_command(cmd) for cmd in commands]
        duplicates = len(commands) - len(set(commands))
        first_edit = next((i for i, value in enumerate(kinds) if value == "possible_edit"), None)
        before_edit = kinds[:first_edit] if first_edit is not None else kinds
        rows.append({
            "kind": entry["kind"], "repeat": entry["repeat"], "arm": entry["arm"],
            "stage": entry["stage"], "commands": len(commands),
            "duplicate_exact_commands": duplicates,
            "inspections": kinds.count("inspection"),
            "verifications": kinds.count("verification"),
            "possible_edits": kinds.count("possible_edit"),
            "inspections_before_first_possible_edit": before_edit.count("inspection"),
            "classification_note": "Shell command regex heuristic; audit raw trace for actual code edits.",
        })
    write_json(LAB / "trace-analysis.json", rows)
    for kind in ("ab", "ablate"):
        for arm in (("A", "B") if kind == "ab" else ("A", "B", "E", "F")):
            selected = [x for x in rows if x["kind"] == kind and x["arm"] == arm]
            if not selected:
                continue
            print("P6_TRACE", kind, arm, "n", len(selected),
                  "inspect", sum(x["inspections"] for x in selected),
                  "before_edit", sum(x["inspections_before_first_possible_edit"] for x in selected),
                  "duplicate_commands", sum(x["duplicate_exact_commands"] for x in selected),
                  "verify", sum(x["verifications"] for x in selected), flush=True)


if __name__ == "__main__":
    parser = __import__("argparse").ArgumentParser()
    parser.add_argument("action", choices=("setup", "calibrate", "ab", "ablate", "report", "trace"))
    action = parser.parse_args().action
    if action == "setup":
        setup()
    elif action == "calibrate":
        calibrate()
    elif action in ("ab", "ablate"):
        run(action)
    elif action == "report":
        report()
    else:
        trace()
