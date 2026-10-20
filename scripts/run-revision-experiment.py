#!/usr/bin/env python3
"""E03: does the run notice that a file changed underneath it?

`kroma bench` grades a task after the agent stops, which cannot express "the file changed while you
were working". This harness runs the agent directly, rewrites a file at fixed offsets during the
run, and then grades with the out-of-workspace verifier - so the only difference between arms is
what the product does with a fact that has gone stale.

Reported per run: whether it passed, the token usage the provider reported, how many times the
mutated file was read *after* the first revision, and whether the final source pins a stale value.

Usage: python3 run-revision-experiment.py --kroma ./target/debug/kroma --arm legacy --reps 1
"""
import argparse
import json
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

VERIFIER = "/opt/kroma-exp/verifier.py"
USAGE_PAIR = re.compile(r'"input_tokens"\s*:\s*(\d+)')
OUTPUT_PAIR = re.compile(r'"output_tokens"\s*:\s*(\d+)')


def lay_out(suite_task, root):
    if root.exists():
        shutil.rmtree(root)
    for relative, content in suite_task["files"].items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)


def apply_revision(suite_task, index, root):
    revision = suite_task["revisions"][index]
    (root / revision["path"]).write_text(revision["content"])


def grade(suite_path, task_id, root):
    completed = subprocess.run(
        [sys.executable, VERIFIER, "--suite", str(suite_path), "--task", task_id,
         "--workspace", str(root)],
        capture_output=True, text=True,
    )
    return completed.returncode == 0, completed.stdout.strip().splitlines()[-1:] or [""]


def token_totals(stdout):
    inputs = [int(value) for value in USAGE_PAIR.findall(stdout)]
    outputs = [int(value) for value in OUTPUT_PAIR.findall(stdout)]
    return {"input_tokens": sum(inputs), "output_tokens": sum(outputs),
            "usage_events": min(len(inputs), len(outputs))}


def reads_after_revision(stdout, path):
    """Count tool calls that touched the mutated file, and how many came after the first revision."""
    calls = []
    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        payload = event.get("payload") or {}
        if payload.get("tool_name") not in ("read", "grep", "codemode"):
            continue
        blob = json.dumps(payload)
        if path not in blob:
            continue
        calls.append({"tool": payload.get("tool_name"),
                      "at_ms": payload.get("started_at_ms") or payload.get("timestamp_ms")})
    return len(calls)


def run_one(kroma, suite_path, suite_task, arm, root, budget, timeout_s, profile):
    lay_out(suite_task, root)
    if arm == "taskstate":
        (root / ".kroma.toml").write_text('projection_profile = "ctx.taskstate.v1"\n')
    command = [kroma, "run", "--task", suite_task["prompt"], "--workspace", str(root),
               "--allow-write", "--allow-shell", "--max-tokens", str(budget),
               "--output-format", "jsonl"]
    if profile:
        command += ["--profile", profile]
    started = time.time()
    proc = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    pending = list(range(len(suite_task["revisions"])))
    revisions_applied = []
    stdout = ""
    while True:
        now = time.time() - started
        for index in list(pending):
            if now >= suite_task["revisions"][index]["after_s"]:
                apply_revision(suite_task, index, root)
                revisions_applied.append({"index": index, "at_s": round(now, 1),
                                          "path": suite_task["revisions"][index]["path"]})
                pending.remove(index)
        if proc.poll() is not None or now > timeout_s:
            break
        time.sleep(0.5)
    if proc.poll() is None:
        proc.kill()
    try:
        stdout, stderr = proc.communicate(timeout=30)
    except subprocess.TimeoutExpired:
        proc.kill()
        stdout, stderr = "", ""
    for index in pending:
        apply_revision(suite_task, index, root)
        revisions_applied.append({"index": index, "at_s": round(time.time() - started, 1),
                                  "path": suite_task["revisions"][index]["path"], "late": True})
    passed, _ = grade(suite_path, suite_task["id"], root)
    mutated = {revision["path"] for revision in suite_task["revisions"]}
    return {
        "task": suite_task["id"],
        "arm": arm,
        "passed": passed,
        "wall_s": round(time.time() - started, 1),
        "exit_code": proc.returncode,
        "tokens": token_totals(stdout),
        "revisions": revisions_applied,
        "tool_calls_on_mutated": sum(reads_after_revision(stdout, path) for path in mutated),
        "stderr_tail": stderr.strip().splitlines()[-3:],
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--kroma", required=True)
    parser.add_argument("--suite", default="/opt/kroma-exp/suite-revision.json")
    parser.add_argument("--arm", choices=["legacy", "taskstate"], required=True)
    parser.add_argument("--reps", type=int, default=1)
    parser.add_argument("--budget-tokens", type=int, default=400_000)
    parser.add_argument("--timeout-s", type=float, default=420.0)
    parser.add_argument("--profile", default="gemini")
    parser.add_argument("--workspace-root", default="/tmp/e03")
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()

    kroma = str(Path(args.kroma).resolve())
    suite_path = Path(args.suite)
    suite = json.loads(suite_path.read_text())
    runs = []
    for rep in range(args.reps):
        for task in suite:
            root = Path(args.workspace_root) / f"{args.arm}-{task['id']}-r{rep}"
            result = run_one(kroma, suite_path, task, args.arm, root, args.budget_tokens,
                             args.timeout_s, args.profile)
            result["rep"] = rep
            runs.append(result)
            print(f"{args.arm} {task['id']} r{rep}: pass={result['passed']} "
                  f"tok={result['tokens']['input_tokens'] + result['tokens']['output_tokens']} "
                  f"reads={result['tool_calls_on_mutated']} wall={result['wall_s']}s", flush=True)

    summary = {
        "arm": args.arm,
        "suite_sha256": __import__("hashlib").sha256(suite_path.read_bytes()).hexdigest(),
        "runs": len(runs),
        "passed": sum(1 for run in runs if run["passed"]),
        "tokens": sum(run["tokens"]["input_tokens"] + run["tokens"]["output_tokens"] for run in runs),
        "tool_calls_on_mutated": sum(run["tool_calls_on_mutated"] for run in runs),
    }
    report = {"format": "kroma.revision-experiment/1", "summary": summary, "runs": runs}
    text = json.dumps(report, ensure_ascii=False, indent=2)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + "\n", encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
