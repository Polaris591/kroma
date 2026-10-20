#!/usr/bin/env python3
"""Run an external coding agent on the frozen suite and grade it with the same oracle.

M2-03 compares this product against an external harness, so the comparison has to be fair in the
two places where it usually is not: the tasks come from the frozen suite (not from a prompt someone
wrote for the occasion), and the pass/fail comes from the out-of-workspace verifier (not from the
agent's own claim that it finished).

The agent is whatever CLI you point it at; this script only knows how to set up the task, run the
command in the task workspace, and read back a token count when the CLI prints one.

Usage:
    python3 run-external-agent.py --suite suite-dev.json --out codex-dev.json \
        --agent-cmd "codex exec --skip-git-repo-check {prompt}" --env COBBLEMON_API_KEY=@path

Exit codes: 0 every task graded, 2 setup problem (suite/verifier missing).
"""
import argparse
import json
import os
import shlex
import subprocess
import sys
import time
from pathlib import Path

VERIFIER = Path("/opt/kroma-exp/verifier.py")


def load_suite(path):
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    return data["tasks"] if isinstance(data, dict) else data


def materialise(task, workspace):
    for relative, content in task.get("files", {}).items():
        target = workspace / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")


def grade(task, workspace):
    result = subprocess.run(
        [sys.executable, str(VERIFIER), "--suite", str(SUITE), "--task", task["id"],
         "--workspace", str(workspace), "--json"],
        capture_output=True, text=True,
    )
    return result.returncode == 0, result.returncode, (result.stdout or result.stderr)[-400:]


def tokens_from(text):
    """The CLIs print their own usage; take the last `tokens used` number when it is there."""
    marker = "tokens used"
    index = text.lower().rfind(marker)
    if index < 0:
        return None
    tail = text[index + len(marker):]
    # The CLIs print thousands separators ("1,590"); reading digits until a non-digit would report
    # the leading group as the whole number.
    digits = ""
    for char in tail:
        if char.isdigit():
            digits += char
        elif char == "," and digits:
            continue
        elif digits:
            break
        elif char in " \n\t:":
            continue
        else:
            break
    return int(digits) if digits else None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--suite", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--root", default="/tmp/kroma-external-agent")
    parser.add_argument("--agent-cmd", required=True,
                        help="command template; {prompt} is substituted")
    parser.add_argument("--env", action="append", default=[],
                        help="KEY=VALUE, or KEY=@path to read the value from a file")
    parser.add_argument("--tasks", default="", help="comma separated task ids (default: all)")
    parser.add_argument("--timeout", type=float, default=900.0)
    args = parser.parse_args()

    global SUITE
    SUITE = Path(args.suite)
    if not VERIFIER.is_file():
        print(f"verifier not found at {VERIFIER}", file=sys.stderr)
        return 2
    tasks = load_suite(args.suite)
    wanted = [t for t in args.tasks.split(",") if t]
    if wanted:
        tasks = [t for t in tasks if t["id"] in wanted]

    env = dict(os.environ)
    for assignment in args.env:
        key, _, value = assignment.partition("=")
        env[key] = Path(value[1:]).read_text().strip() if value.startswith("@") else value

    root = Path(args.root)
    root.mkdir(parents=True, exist_ok=True)
    records = []
    for task in tasks:
        workspace = root / task["id"]
        if workspace.exists():
            subprocess.run(["rm", "-rf", str(workspace)], check=True)
        workspace.mkdir(parents=True)
        materialise(task, workspace)
        # The prompt is passed as one argv element: substituting it into the template and then
        # splitting would hand the CLI only its first word.
        template = args.agent_cmd
        assert "{prompt}" in template, "the command template must contain {prompt}"
        prefix = template.replace("{prompt}", "").strip()
        argv = [*shlex.split(prefix), task["prompt"]]
        started = time.time()
        try:
            run = subprocess.run(argv, cwd=workspace, env=env,
                                 capture_output=True, text=True, timeout=args.timeout)
            output = (run.stdout or "") + (run.stderr or "")
            agent_exit = run.returncode
        except subprocess.TimeoutExpired as expired:
            output = ((expired.stdout or b"").decode(errors="replace")
                      + (expired.stderr or b"").decode(errors="replace"))
            agent_exit = -1
        wall_ms = int((time.time() - started) * 1000)
        passed, verifier_exit, verifier_tail = grade(task, workspace)
        record = {
            "task_id": task["id"],
            "passed": passed,
            "verifier_exit": verifier_exit,
            "verifier_tail": verifier_tail,
            "agent_exit": agent_exit,
            "tokens": tokens_from(output),
            "wall_ms": wall_ms,
            # Kept so a reader can check the token number against what the CLI actually printed.
            "agent_output_tail": output[-300:],
        }
        records.append(record)
        print(f"{task['id']:30s} pass={passed} tokens={record['tokens']} wall={wall_ms}ms",
              flush=True)

    report = {
        "format": "kroma.external-agent/1",
        "suite": str(SUITE),
        "agent_cmd": args.agent_cmd,
        "tasks": len(records),
        "passed": sum(1 for r in records if r["passed"]),
        "tokens": sum(r["tokens"] or 0 for r in records),
        "records": records,
    }
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n",
                              encoding="utf-8")
    print(f"\n{report['passed']}/{report['tasks']} passed, {report['tokens']} tokens")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
