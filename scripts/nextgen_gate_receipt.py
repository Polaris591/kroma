#!/usr/bin/env python3
"""KR-A04: one machine-readable gate receipt for the whole quality gate.

CI and a local run must invoke the *same* real command set, so this script runs the commands
itself rather than trusting a log someone pasted. The expensive workspace test is not re-run here
(it is already recorded per batch); instead its evidence file is hashed and bound into the receipt,
so a receipt can never claim a test result whose log has since changed.

Usage: nextgen_gate_receipt.py <source-root> [--evidence docs/nextgen/evidence/<file>]...
Exit:  0 every gate passed, 1 at least one gate failed (the receipt is still written).
"""
import hashlib
import json
import os
import subprocess
import sys
from datetime import datetime, timezone

GATES = [
    ("fmt", ["cargo", "fmt", "--all", "--", "--check"]),
    ("check", ["cargo", "check", "--workspace", "--locked"]),
    ("clippy", ["cargo", "clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"]),
]


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def tree_sha256(root):
    h = hashlib.sha256()
    entries = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames
                             if d not in {".git", "target", "__pycache__", "node_modules"})
        for name in sorted(filenames):
            if name.endswith(".pyc"):
                continue
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, root).replace(os.sep, "/")
            if rel.startswith("docs/nextgen/"):
                continue
            entries.append((rel, full))
    entries.sort()
    for rel, full in entries:
        mode = "100755" if os.access(full, os.X_OK) else "100644"
        h.update(f"{mode} {sha256_file(full)} {rel}\n".encode())
    return h.hexdigest()


def main(argv):
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 1
    root = os.path.abspath(argv[1])
    evidence = []
    i = 2
    while i < len(argv):
        if argv[i] == "--evidence" and i + 1 < len(argv):
            evidence.append(argv[i + 1])
            i += 2
        else:
            i += 1

    results = []
    all_ok = True
    for name, cmd in GATES:
        started = datetime.now(timezone.utc)
        proc = subprocess.run(cmd, cwd=root, capture_output=True, text=True)
        entry = {
            "gate": name,
            "command": " ".join(cmd),
            "exit_code": proc.returncode,
            "duration_s": round((datetime.now(timezone.utc) - started).total_seconds(), 2),
            "stdout_tail": proc.stdout.strip()[-800:],
            "stderr_tail": proc.stderr.strip()[-800:],
        }
        results.append(entry)
        if proc.returncode != 0:
            all_ok = False

    bound = []
    for path in evidence:
        full = path if os.path.isabs(path) else os.path.join(root, path)
        if os.path.isfile(full):
            bound.append({"path": os.path.relpath(full, root), "sha256": sha256_file(full)})
        else:
            bound.append({"path": path, "sha256": None, "missing": True})
            all_ok = False

    doc = {
        "task": "KR-A04",
        "generated_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "source_tree_sha256": tree_sha256(root),
        "gates": results,
        "evidence": bound,
        "all_passed": all_ok,
        "note": (
            "The three fast gates are executed by this script so CI and a local run cannot drift. "
            "The workspace test suite is expensive and is bound by evidence hash instead: a receipt "
            "whose log has changed no longer matches."
        ),
    }
    out_dir = os.path.join(root, "docs", "nextgen")
    os.makedirs(out_dir, exist_ok=True)
    out = os.path.join(out_dir, "gate_receipt.json")
    with open(out, "w") as fh:
        json.dump(doc, fh, indent=2)
        fh.write("\n")
    print(json.dumps({
        "written": os.path.relpath(out, root),
        "all_passed": all_ok,
        "gates": {g["gate"]: g["exit_code"] for g in results},
        "evidence_bound": len(bound),
    }, indent=2))
    return 0 if all_ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
