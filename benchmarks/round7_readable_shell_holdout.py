#!/usr/bin/env python3
"""Holdout Rust Unicode history-search repair: raw JSON vs readable Shell results.

Exactly the same candidate Kroma binary, DeepSeek model, endpoint, task fixture,
prompt and test oracle. Only KROMA_EXPERIMENT_READABLE_SHELL_RESULTS changes.
Model requests are billable. All task writes remain inside disposable projects.
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

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path("D:/Kroma/target-kroma-check/release/kroma.exe")
LAB = Path("D:/Kroma/experiments/kroma-round7-readable-shell-holdout")
SEED = 20261110
ARMS = ["json", "readable"]
TASK = "rust_history_unicode"
PROMPT = (
    "This is a real multi-module Rust project. The history_search tool can panic "
    "or return the wrong excerpt when a long saved message contains non-ASCII "
    "characters before a matched word. Diagnose the cause and repair it without "
    "changing or removing any tests. Preserve normal case-insensitive lookup and "
    "UTF-8 boundaries. Run the targeted cargo test --locked --offline --lib excerpts_ "
    "after the fix and report what passed. Work only inside this isolated checkout; "
    "do not commit or access credentials."
)
BUGGY = """fn excerpt(text: &str, query: &str) -> Option<String> {
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return None;
    }
    // This version incorrectly uses offsets into the case-folded text to
    // slice the original string. Some Unicode characters expand on folding.
    let lower = text.to_lowercase();
    let position = lower.find(&needle)?;
    let mut start = position.saturating_sub(160);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = position.saturating_add(needle.len()).saturating_add(240).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    Some(text[start..end].replace('\\n', " "))
}
"""
HIDDEN = """
#[cfg(test)]
mod round7_history_regression {
    use super::*;

    #[test]
    fn long_case_fold_expansion_preserves_the_match() {
        let content = "İ".repeat(240) + "TARGET_HISTORY_MARKER" + &"尾".repeat(75);
        let fragment = excerpt(&content, "target_history_marker").expect("must find marker");
        assert!(fragment.contains("TARGET_HISTORY_MARKER"), "wrong excerpt: {fragment:?}");
    }

    #[test]
    fn ordinary_ascii_case_folding_still_works() {
        let content = "before XyZ_MARKER after";
        let fragment = excerpt(content, "xyz_marker").expect("must find ascii match");
        assert!(fragment.contains("XyZ_MARKER"));
    }
}
"""


def export_fixture(project: Path) -> str:
    archive = subprocess.check_output(["git", "archive", "HEAD"], cwd=ROOT)
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as jar:
        jar.extractall(project, filter="data")
    source = project / "src" / "history_tools.rs"
    content = source.read_text(encoding="utf-8")
    start = content.index("fn excerpt(text: &str, query: &str) -> Option<String> {")
    end = content.index("\nfn searchable_text(", start)
    if content.count("fn excerpt(text: &str, query: &str) -> Option<String> {") != 1:
        raise RuntimeError("fixture drift: excerpt function not unique")
    if "original_offsets" not in content[start:end]:
        raise RuntimeError("fixture drift: Unicode mapping implementation changed")
    source.write_text(content[:start] + BUGGY + content[end:] + HIDDEN, encoding="utf-8")
    return hashlib.sha256(HIDDEN.encode()).hexdigest()


def cargo_env(project: Path) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if not key.startswith("KROMA_")}
    env["CARGO_TARGET_DIR"] = "D:/Kroma/target-kroma-check"
    env["CARGO_INCREMENTAL"] = "0"
    return env


def verify(project: Path, frozen_hash: str) -> tuple[bool, str]:
    source = (project / "src" / "history_tools.rs").read_text(encoding="utf-8")
    if not source.endswith(HIDDEN) or hashlib.sha256(HIDDEN.encode()).hexdigest() != frozen_hash:
        return False, "regression tests changed"
    try:
        result = subprocess.run(
            ["cargo", "test", "--locked", "--offline", "--lib", "excerpts_"],
            cwd=project, env=cargo_env(project), capture_output=True, text=True,
            timeout=100,
        )
        holdout = subprocess.run(
            ["cargo", "test", "--locked", "--offline", "--lib", "round7_history_regression"],
            cwd=project, env=cargo_env(project), capture_output=True, text=True,
            timeout=100,
        )
        if result.returncode == 0 and holdout.returncode == 0:
            return True, "Unicode regression and existing excerpt tests passed"
        return False, ("original tests: " + result.stderr[-500:] +
                        " holdout: " + holdout.stderr[-500:])[:1100]
    except Exception as exc:
        return False, type(exc).__name__ + ": " + str(exc)[:160]


def run_one(arm: str, repeat: int, timeout: int, lab: Path) -> dict:
    trial = lab / TASK / str(repeat) / arm
    project = trial / "project"
    project.mkdir(parents=True, exist_ok=False)
    frozen_hash = export_fixture(project)
    env = cargo_env(project)
    env.update({
        "KROMA_PROVIDER_PROFILE": "deepseek",
        "KROMA_DEEPSEEK_THINKING": "off",
        "KROMA_REASONING_EFFORT": "high",
        "KROMA_PROFILE": "1",
        "KROMA_PLUGIN_DIR": str(trial / "disabled-global-plugins" / "plugins"),
        "KROMA_WORKDIR": str(project),
        "KROMA_SESSION_FILE": str(trial / "session.json"),
    })
    if arm == "readable":
        env["KROMA_EXPERIMENT_READABLE_SHELL_RESULTS"] = "1"
    elif arm == "bash":
        env["KROMA_EXPERIMENT_SHELL_TOOL_NAME"] = "bash"
    start = time.monotonic()
    process = subprocess.Popen(
        [str(BINARY), "--print", "--no-mcp", PROMPT],
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
    wall = round(time.monotonic() - start, 3)
    (trial / "stdout.log").write_bytes(stdout)
    (trial / "stderr.log").write_bytes(stderr)
    out = stdout.decode("utf-8", "replace")
    err = stderr.decode("utf-8", "replace")
    metrics, answer, calls, profile = base.parse_kroma(
        out, err, trial / "session.json"
    )
    verified, note = verify(project, frozen_hash)
    agent_ran_test = any(x["operation"] == "test" for x in calls)
    return {
        "task": TASK, "repeat": repeat, "arm": arm,
        "wall_s": wall, "timeout": timed_out, "exit": process.returncode,
        "verified": verified, "verification_note": note,
        "agent_ran_test": agent_ran_test,
        "passed": bool(verified and process.returncode == 0 and not timed_out),
        "strict_pass": bool(verified and process.returncode == 0 and
                            not timed_out and agent_ran_test),
        "metrics": metrics, "profile": profile,
        "tools": len(calls), "tool_operations": {
            op: sum(x["operation"] == op for x in calls)
            for op in ("inspect", "edit", "test", "other")
        },
        "logs": str(trial),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--lab", type=Path, default=LAB)
    parser.add_argument("--timeout", type=int, default=210)
    parser.add_argument("--repeat-start", type=int, default=1)
    parser.add_argument("--repeats", type=int, default=2)
    parser.add_argument("--arm", choices=ARMS, default=None,
                        help="Run only one arm in an isolated recovery lab")
    parser.add_argument("--max-runs", type=int, default=4)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        assert "İ" in HIDDEN and "case-folded" in BUGGY
        assert "cargo test" in PROMPT and "round7_history_regression" in HIDDEN
        print("round7 fixture self-test OK", flush=True)
        return
    if not BINARY.is_file():
        parser.error(f"candidate release binary not found: {BINARY}")
    if not (40 <= args.timeout <= 300 and 1 <= args.repeats <= 4 and
            1 <= args.max_runs <= 8):
        parser.error("invalid benchmark limits")
    lab = args.lab.resolve()
    lab.mkdir(parents=True, exist_ok=True)
    path = lab / "results.json"
    meta = {
        "schema": 1, "task": TASK, "model": "deepseek-v4.1-flash",
        "thinking": "high", "seed": SEED, "arms": ARMS,
        "binary_sha256": hashlib.sha256(BINARY.read_bytes()).hexdigest(),
        "prompt_sha256": hashlib.sha256(PROMPT.encode()).hexdigest(),
        "source_head": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
    }
    report = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {
        **meta, "results": []
    }
    for key, value in meta.items():
        if report.get(key) != value:
            raise RuntimeError(f"lab mismatch {key}")
    done = {(row["repeat"], row["arm"]) for row in report["results"]}
    count = 0
    for repeat in range(args.repeat_start, args.repeat_start + args.repeats):
        order = ARMS if repeat % 2 else ARMS[::-1]
        for arm in order:
            if args.arm and args.arm != arm:
                continue
            if (repeat, arm) in done:
                continue
            if count >= args.max_runs:
                break
            print("START", TASK, repeat, arm, flush=True)
            try:
                row = run_one(arm, repeat, args.timeout, lab)
            except Exception as exc:
                row = {
                    "task": TASK, "repeat": repeat, "arm": arm,
                    "passed": False, "strict_pass": False,
                    "error": type(exc).__name__ + ": " + str(exc)[:200]
                }
            report["results"].append(row)
            path.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n",
                            encoding="utf-8")
            count += 1
            m = row.get("metrics") or {}
            print("END", repeat, arm, "pass", row.get("passed"),
                  "strict", row.get("strict_pass"),
                  "wall_s", row.get("wall_s"), "requests", m.get("requests"),
                  "input", m.get("input"), "tools", row.get("tools"),
                  "verification", row.get("verification_note", row.get("error")),
                  flush=True)
        if count >= args.max_runs:
            break
    print("REPORT", path, "rows", len(report["results"]), flush=True)


if __name__ == "__main__":
    main()
