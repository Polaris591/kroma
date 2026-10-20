#!/usr/bin/env python3
"""Out-of-workspace oracle for Kroma experiment suites.

Why this exists: `kroma bench` runs the suite's own `verify_command` inside the
workspace the agent just edited. An agent that edits `tests/` can therefore pass
without solving the task. This script re-establishes the oracle from the frozen
suite before running the check, and lives outside the graded workspace so the
agent cannot rewrite it.

Contract (deliberately narrow, one task at a time):

    verifier.py --suite <suite.json> --task <task-id> --workspace <dir> [--json]

Exit codes: 0 = pass, 1 = fail, 2 = usage/setup error (never silently a pass).

Convention: files whose suite path starts with `tests/` are oracle files and are
restored from the suite; every other file is left exactly as the agent left it.
That convention is checked, not assumed: a task with no `tests/` file is rejected
with exit 2 rather than graded without an oracle.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

ORACLE_PREFIX = "tests/"


def load_suite(path: Path) -> list[dict]:
    doc = json.loads(path.read_text(encoding="utf-8"))
    if isinstance(doc, dict):
        tasks = doc.get("tasks")
        if not isinstance(tasks, list):
            raise SystemExit(f"{path}: suite has no `tasks` list")
        return tasks
    if not isinstance(doc, list):
        raise SystemExit(f"{path}: suite must be a list or {{tasks: [...]}}")
    return doc


def restore_oracle(task: dict, workspace: Path) -> int:
    restored = 0
    for rel, content in (task.get("files") or {}).items():
        if rel.startswith(ORACLE_PREFIX):
            dest = workspace / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(content, encoding="utf-8")
            restored += 1
    return restored


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--suite", required=True)
    ap.add_argument("--task", required=True)
    ap.add_argument("--workspace", required=True)
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args(argv)

    suite_path = Path(args.suite)
    workspace = Path(args.workspace)
    if not suite_path.is_file():
        print(f"verifier: suite not found: {suite_path}", file=sys.stderr)
        return 2
    if not workspace.is_dir():
        print(f"verifier: workspace not found: {workspace}", file=sys.stderr)
        return 2

    tasks = load_suite(suite_path)
    task = next((t for t in tasks if t.get("id") == args.task), None)
    if task is None:
        ids = ", ".join(str(t.get("id")) for t in tasks)
        print(f"verifier: task {args.task!r} not in suite ({ids})", file=sys.stderr)
        return 2

    cmd = task.get("verify_command")
    if not isinstance(cmd, list) or not cmd:
        print(f"verifier: task {args.task!r} has no verify_command", file=sys.stderr)
        return 2

    restored = restore_oracle(task, workspace)
    if restored == 0:
        print(
            f"verifier: task {args.task!r} ships no {ORACLE_PREFIX}* oracle file; "
            "refusing to grade without one",
            file=sys.stderr,
        )
        return 2

    proc = subprocess.run(cmd, cwd=workspace, capture_output=True, text=True)
    verdict = {
        "task": args.task,
        "suite": str(suite_path.resolve()),
        "oracle_files_restored": restored,
        "command": cmd,
        "exit_code": proc.returncode,
        "passed": proc.returncode == 0,
        "stdout_tail": proc.stdout[-4000:],
        "stderr_tail": proc.stderr[-4000:],
    }
    if args.json:
        print(json.dumps(verdict, ensure_ascii=False))
    else:
        print(f"{args.task}: {'PASS' if verdict['passed'] else 'FAIL'} (exit {proc.returncode})")
        if not verdict["passed"]:
            print(proc.stdout[-2000:])
            print(proc.stderr[-2000:], file=sys.stderr)
    return 0 if verdict["passed"] else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
