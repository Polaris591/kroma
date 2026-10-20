#!/usr/bin/env python3
"""Directed checks for scripts/goal-interface.py (M0-02 A1/A2/A3).

No test framework: these are the three acceptance specs, each asserted once.
Run: python3 scripts/test-goal-interface.py
"""

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "goal-interface.py"
failures = []


def check(name, condition, detail=""):
    if condition:
        print(f"ok   {name}")
    else:
        print(f"FAIL {name} {detail}")
        failures.append(name)


def call(*args):
    return subprocess.run(
        [sys.executable, str(SCRIPT), *args], capture_output=True, text=True, cwd=ROOT
    )


# A1: probe reports facts read from the binary, not guessed flags.
probe = call("probe")
check("A1 probe exits 0", probe.returncode == 0, probe.stderr)
facts = json.loads(probe.stdout)
check("A1 binary resolved", facts["kroma_binary"] is not None)
check(
    "A1 version read from binary",
    bool(re.fullmatch(r"kroma \d+\.\d+\.\d+", facts["kroma_version"] or "")),
    facts.get("kroma_version"),
)
manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
expected = re.search(r'^version = "([^"]+)"', manifest, re.M).group(1)
check(
    "A1 version matches workspace manifest",
    facts["kroma_version"].endswith(expected),
    f"{facts['kroma_version']} vs {expected}",
)
check("A1 help text was actually run", facts["kroma_help_first_line"] != "")
missing = [name for name, present in facts["kroma_surfaces"].items() if not present]
check("A1 required surfaces declared in help", not missing, f"missing: {missing}")

# A2: a legal operation without its interface is BLOCKED, with the reason and exit 2.
blocked = call("authorize", "ci_artifact_read")
check("A2 blocked exits 2", blocked.returncode == 2, blocked.stdout)
check("A2 says BLOCKED", blocked.stdout.startswith("BLOCKED ci_artifact_read:"))
check("A2 carries a reason", len(blocked.stdout.split(":", 1)[1].strip()) > 20)

# A3: unauthorized operations are REFUSED with exit 3, and unknown ones too.
for operation in ("production_key", "release", "push_protected"):
    refused = call("authorize", operation)
    check(f"A3 {operation} refused", refused.returncode == 3, refused.stdout)
    check(f"A3 {operation} says REFUSED", refused.stdout.startswith(f"REFUSED {operation}:"))
unknown = call("authorize", "launch_missiles")
check("A3 unknown operation refused", unknown.returncode == 3, unknown.stdout)

allowed = call("authorize", "local_work")
check("A3 in-envelope operation allowed", allowed.returncode == 0, allowed.stdout)

print(f"\n{len(failures)} failure(s)")
sys.exit(1 if failures else 0)
