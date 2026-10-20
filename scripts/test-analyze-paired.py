#!/usr/bin/env python3
"""Checks for scripts/analyze-paired.py with synthetic reports (no model, no network)."""
import json
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "analyze-paired.py"
failures = []


def check(name, condition, detail=""):
    print(f"ok   {name}" if condition else f"FAIL {name} {detail}")
    if not condition:
        failures.append(name)


def run(rows, *extra):
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as handle:
        json.dump({"results": rows}, handle)
        path = handle.name
    return subprocess.run(
        [sys.executable, str(SCRIPT), "--bench", path, "--baseline", "a", "--arm", "b", *extra],
        capture_output=True,
        text=True,
    )


def row(task, strategy, resolved, tokens):
    return {"task_id": task, "strategy": strategy, "resolved": resolved, "total_tokens": tokens}


# A consistent win: arm resolves everything, baseline resolves nothing.
win = []
for i in range(6):
    win.append(row(f"t{i}", "a", False, 1000))
    win.append(row(f"t{i}", "b", True, 900))
out = run(win, "--json")
payload = json.loads(out.stdout)
check("consistent win is improved", payload["verdict"] == "improved", out.stdout)
check("cost claim allowed after quality holds", payload["cost_claim_allowed"] is True)

# A coin flip: no consistent difference, so the interval must include zero.
mixed = []
for i in range(6):
    mixed.append(row(f"t{i}", "a", i % 2 == 0, 1000))
    mixed.append(row(f"t{i}", "b", i % 2 == 1, 1000))
payload = json.loads(run(mixed, "--json").stdout)
check("mixed results are inconclusive", payload["verdict"] == "inconclusive", payload["verdict"])

# A zero baseline denominator is undefined, not a free win.
zero = [row("t0", "a", True, 0), row("t0", "b", True, 500)]
payload = json.loads(run(zero, "--json").stdout)
check("zero baseline tokens -> undefined ratio", payload["token_ratio"] is None, payload)
check("the undefined task is named", payload["token_ratio_undefined_for"] == ["t0"], payload)

# Disjoint arms are a usage error, not an empty result.
bad = run([row("t0", "a", True, 10), row("t0", "c", True, 10)])
check("disjoint arms exit 2", bad.returncode == 2, bad.stdout + bad.stderr)

print(f"\n{len(failures)} failure(s)")
sys.exit(1 if failures else 0)
