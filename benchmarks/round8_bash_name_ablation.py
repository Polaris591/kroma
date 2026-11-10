#!/usr/bin/env python3
"""Single-factor test: model-facing built-in Bash tool name shell vs bash.

Imports the same frozen Rust fixture and independent verifier as round7.
Model, endpoint, High, prompt, executor and output format are identical.
Only KROMA_EXPERIMENT_SHELL_TOOL_NAME changes. Results are exploratory.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import round7_readable_shell_holdout as fixture

ROOT = Path(__file__).resolve().parents[1]
LAB = Path("D:/Kroma/experiments/kroma-round8-bash-wire-name")
ARMS = ["shell", "bash"]

def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--lab", type=Path, default=LAB)
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--repeat-start", type=int, default=1)
    parser.add_argument("--repeats", type=int, default=2)
    parser.add_argument("--max-runs", type=int, default=4)
    parser.add_argument("--arm", choices=ARMS)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        assert set(ARMS) == {"shell", "bash"}
        assert "KROMA_EXPERIMENT_SHELL_TOOL_NAME" in Path(fixture.__file__).read_text(encoding="utf-8")
        print("round8 ablation self-test OK", flush=True)
        return
    if not (40 <= args.timeout <= 300 and 1 <= args.repeats <= 4 and 1 <= args.max_runs <= 8):
        parser.error("invalid benchmark limits")
    lab = args.lab.resolve()
    lab.mkdir(parents=True, exist_ok=True)
    file = lab / "results.json"
    meta = {
        "schema": 1,
        "task": fixture.TASK,
        "variable": "shell_tool_wire_name_only",
        "arms": ARMS,
        "model": "deepseek-v4.1-flash",
        "thinking": "high",
        "binary_sha256": hashlib.sha256(fixture.BINARY.read_bytes()).hexdigest(),
        "prompt_sha256": hashlib.sha256(fixture.PROMPT.encode()).hexdigest(),
        "source_head": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
    }
    report = json.loads(file.read_text(encoding="utf-8")) if file.exists() else {
        **meta, "results": []
    }
    for key, value in meta.items():
        if report.get(key) != value:
            raise RuntimeError("lab metadata mismatch: " + key)
    done = {(item.get("repeat"), item.get("arm")) for item in report["results"]}
    launched = 0
    for repeat in range(args.repeat_start, args.repeat_start + args.repeats):
        order = ARMS if repeat % 2 else list(reversed(ARMS))
        for arm in order:
            if args.arm and arm != args.arm:
                continue
            if (repeat, arm) in done:
                continue
            if launched >= args.max_runs:
                break
            print("START", repeat, arm, flush=True)
            try:
                result = fixture.run_one(arm, repeat, args.timeout, lab)
            except Exception as exc:
                result = {
                    "task": fixture.TASK, "repeat": repeat, "arm": arm,
                    "passed": False, "strict_pass": False,
                    "runner_error": type(exc).__name__ + ": " + str(exc)[:200],
                }
            report["results"].append(result)
            file.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
            launched += 1
            metrics = result.get("metrics") or {}
            print("END", repeat, arm, "wall", result.get("wall_s"),
                  "pass", result.get("passed"), "strict", result.get("strict_pass"),
                  "requests", metrics.get("requests"), "input", metrics.get("input"),
                  "tools", result.get("tools"), "issue", result.get("runner_error"),
                  flush=True)
        if launched >= args.max_runs:
            break
    print("REPORT", file, "rows", len(report["results"]), flush=True)

if __name__ == "__main__":
    main()
