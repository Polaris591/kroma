#!/usr/bin/env python3
"""Goal interface probe and authorization gate (M0-02).

Two jobs, both deliberately small:

1. `probe` reads what the toolchain actually is — version, help text, git state,
   and (with `--ci`) whether the CI API answers. It never guesses a flag: the
   recorded facts come from running the binary.
2. `authorize <operation>` answers "may I do this here?" from `docs/goal/ENVELOPE.json`.
   The answer is an exit code, because a warning nobody reads is not a control.

Usage:
    python3 goal-interface.py probe [--ci] [--out FILE]
    python3 goal-interface.py authorize <operation>

Exit codes:
    0  allowed, or probe completed
    2  BLOCKED: legal operation, missing interface or credential (reason printed)
    3  REFUSED: operation is not authorized by the envelope (reason printed)
    4  the envelope is missing or malformed
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ENVELOPE = ROOT / "docs" / "goal" / "ENVELOPE.json"
KROMA = ROOT / "target" / "debug" / "kroma"
CI_REPO = "Polaris591/kroma"

# Surfaces this goal depends on. The probe reports whether the binary's own help
# mentions each one; it does not parse the help into a grammar it does not have.
REQUIRED_SURFACES = ("run", "repl", "acp", "serve", "bench")


def run(cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    except (FileNotFoundError, subprocess.TimeoutExpired, PermissionError) as exc:
        print(f"BLOCKED required tool {cmd[0]} unavailable: {exc}", file=sys.stderr)
        raise SystemExit(2) from exc


def probe_local():
    facts = {"format": "kroma.goal.probe/1", "repo_root": str(ROOT)}

    facts["kroma_binary"] = str(KROMA) if KROMA.exists() else None
    if KROMA.exists():
        version = run([str(KROMA), "--version"])
        facts["kroma_version"] = version.stdout.strip()
        facts["kroma_version_exit"] = version.returncode
        help_run = run([str(KROMA), "--help"])
        lines = help_run.stdout.splitlines()
        facts["kroma_help_first_line"] = lines[0] if lines else ""
        facts["kroma_help_sha256"] = hashlib.sha256(help_run.stdout.encode()).hexdigest()
        facts["kroma_surfaces"] = {
            name: bool(re.search(rf"(^|\s){name}(\s|$|\|)", help_run.stdout, re.M))
            for name in REQUIRED_SURFACES
        }

    git_head = run(["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"])
    facts["head"] = git_head.stdout.strip()
    branch = run(["git", "-C", str(ROOT), "rev-parse", "--abbrev-ref", "HEAD"])
    facts["branch"] = branch.stdout.strip()
    remotes = run(["git", "-C", str(ROOT), "remote", "-v"])
    facts["remotes"] = remotes.stdout.strip()

    facts["python"] = sys.version.split()[0]
    facts["rustc"] = run(["rustc", "--version"]).stdout.strip()
    return facts


def probe_ci(facts):
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    facts["ci"] = {"repo": CI_REPO, "token_present": bool(token)}

    base = f"https://api.github.com/repos/{CI_REPO}"
    for name, url in (
        ("repo", base),
        ("runs", f"{base}/actions/runs?per_page=1"),
    ):
        probe = run(["curl", "-sS", "-o", "/dev/null", "-w", "%{http_code}", "-m", "15", url])
        facts["ci"][f"{name}_http"] = probe.stdout.strip()

    runs = run(["curl", "-sS", "-m", "20", f"{base}/actions/runs?per_page=1"])
    try:
        payload = json.loads(runs.stdout)
    except ValueError:
        facts["ci"]["latest_run"] = "UNREADABLE"
        return facts
    latest = (payload.get("workflow_runs") or [{}])[0]
    facts["ci"]["latest_run"] = latest.get("id")
    facts["ci"]["latest_run_conclusion"] = latest.get("conclusion")

    artifacts = run(["curl", "-sS", "-m", "20", f"{base}/actions/runs/{latest.get('id')}/artifacts"])
    try:
        facts["ci"]["latest_artifact_count"] = json.loads(artifacts.stdout).get("total_count")
    except ValueError:
        facts["ci"]["latest_artifact_count"] = "UNREADABLE"
    facts["ci"]["artifact_download"] = "AVAILABLE" if token else "BLOCKED_NO_TOKEN"
    return facts


def load_envelope():
    try:
        return json.loads(ENVELOPE.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        print(f"ENVELOPE_UNREADABLE {exc}", file=sys.stderr)
        raise SystemExit(4)


def authorize(operation):
    operations = load_envelope().get("operations", {})
    entry = operations.get(operation)
    if entry is None:
        print(f"REFUSED unknown operation '{operation}'; known: {', '.join(sorted(operations))}")
        return 3
    decision = entry.get("decision")
    if decision == "allowed":
        print(f"ALLOWED {operation}")
        return 0
    if decision == "blocked":
        print(f"BLOCKED {operation}: {entry.get('blocked_reason')}")
        return 2
    print(f"REFUSED {operation}: {entry.get('reason')}")
    return 3


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    probe = sub.add_parser("probe")
    probe.add_argument("--ci", action="store_true", help="also probe the GitHub API")
    probe.add_argument("--out", type=Path)
    auth = sub.add_parser("authorize")
    auth.add_argument("operation")
    args = parser.parse_args()

    if args.command == "authorize":
        return authorize(args.operation)

    facts = probe_local()
    if args.ci:
        facts = probe_ci(facts)
    text = json.dumps(facts, ensure_ascii=False, indent=2)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
