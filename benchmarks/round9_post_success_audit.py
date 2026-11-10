#!/usr/bin/env python3
"""Read-only analysis of post-success work in completed Kroma task traces.

The proxy for a sufficient check is an actual tool result: a zero shell
exit code and a Cargo test summary with one or more passing tests, after
the first edit. This is an observational proxy, NOT a task-completion oracle.
"""
from __future__ import annotations

import argparse
from collections import Counter
import json
from pathlib import Path
import re

LAB = Path("D:/Kroma/experiments/kroma-round9-complete-function-read")
PASS = re.compile(r"test result: ok\.\s*([1-9][0-9]*) passed;")


def load_trace(path: Path) -> dict:
    events = []
    for line in path.read_text(encoding="utf-8").splitlines():
        obj = json.loads(line.split("\t", 1)[0])
        events.append(obj["message"])
    calls = []
    id_to_call = {}
    assistants = []
    for msg in events:
        if msg.get("role") != "assistant":
            continue
        assistants.append(msg)
        for call in msg.get("tool_calls") or []:
            name = call["function"]["name"]
            args = json.loads(call["function"]["arguments"])
            command = args.get("command", "")
            item = {
                "number": len(calls) + 1,
                "name": name,
                "command": command,
                "file": args.get("path"),
                "exit": None,
                "result": None,
            }
            calls.append(item)
            id_to_call[call["id"]] = item
    for msg in events:
        if msg.get("role") != "tool":
            continue
        call = id_to_call.get(msg.get("tool_call_id"))
        if call is None:
            continue
        try:
            result = json.loads(msg.get("content") or "{}")
        except ValueError:
            result = {}
        call["exit"] = result.get("exit_code")
        call["result"] = result
    return {"calls": calls, "assistants": assistants}


def analyze(trace: dict) -> dict:
    calls = trace["calls"]
    edits = [c["number"] for c in calls if c["name"] == "edit_file"]
    first_edit = min(edits, default=None)
    successful = [
        c for c in calls
        if c["name"] == "shell" and "cargo test" in c["command"]
        and c["exit"] == 0
        and PASS.search(str((c["result"] or {}).get("stdout", "")))
        and first_edit is not None and c["number"] > first_edit
    ]
    first_pass = successful[0]["number"] if successful else None
    after = [c for c in calls if first_pass is not None and c["number"] > first_pass]
    after_edit = [c["number"] for c in after if c["name"] == "edit_file"]
    after_test = [
        c["number"] for c in after
        if c["name"] == "shell" and "cargo test" in c["command"]
    ]
    return {
        "calls_total": len(calls),
        "first_edit": first_edit,
        "first_postedit_pass": first_pass,
        "calls_after_pass": len(after),
        "edits_after_pass": after_edit,
        "tests_after_pass": after_test,
        "model_responses": len(trace["assistants"]),
        "final_response_without_tools": bool(
            trace["assistants"]
            and not trace["assistants"][-1].get("tool_calls")
            and (trace["assistants"][-1].get("content") or "").strip()
        ),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=LAB)
    parser.add_argument("--task", default="rust_restore_metadata")
    parser.add_argument("--repeats", type=int, default=4)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        assert PASS.search("test result: ok. 10 passed; 0 failed; 0 ignored")
        assert not PASS.search("test result: ok. 0 passed; 0 failed")
        assert not PASS.search("test result: FAILED. 1 passed; 1 failed")
        print("post-success analyzer self-test OK")
        return
    for repeat in range(1, args.repeats + 1):
        for arm in ("baseline", "complete"):
            path = args.root / args.task / str(repeat) / arm / "session.json.wal"
            if not path.exists():
                continue
            row = analyze(load_trace(path))
            print(json.dumps({"repeat": repeat, "arm": arm, **row}, ensure_ascii=False))


if __name__ == "__main__":
    main()
