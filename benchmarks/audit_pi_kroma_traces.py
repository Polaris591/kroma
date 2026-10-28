#!/usr/bin/env python3
"""Offline, read-only forensic analysis of the Pi/Kroma paired Rust logs.

Uses archived process logs only: no model/API calls, no source changes,
no secrets or verbatim tool outputs are printed.
"""
import argparse
import json
from pathlib import Path
import re


def safe_json(line):
    try:
        return json.loads(line)
    except (ValueError, TypeError):
        return {}


def tool_text(result):
    return "".join(
        x.get("text", "") for x in (result or {}).get("content", [])
        if isinstance(x, dict)
    )


def parse_pi(directory):
    calls, ids, request_input, count_multi = [], {}, [], 0
    for line in (directory / "stdout.log").open(errors="replace"):
        event = safe_json(line)
        kind = event.get("type")
        if kind == "tool_execution_start":
            row = {
                "name": event.get("toolName"),
                "command": (event.get("args") or {}).get("command", ""),
                "path": (event.get("args") or {}).get("path", ""),
                "duration_ms": None, "result_chars": 0, "output": "",
            }
            ids[event.get("toolCallId")] = row
            calls.append(row)
        elif kind == "tool_execution_end":
            row = ids.get(event.get("toolCallId"))
            if row is not None:
                row["duration_ms"] = event.get("durationMs")
                row["output"] = tool_text(event.get("result"))
                row["result_chars"] = len(row["output"])
        elif kind == "message_end" and event.get("message", {}).get("role") == "assistant":
            msg = event["message"]
            content = msg.get("content") or []
            count_multi += sum(x.get("type") == "toolCall" for x in content) > 1
            usage = msg.get("usage") or {}
            request_input.append(sum(usage.get(k, 0) or 0 for k in ("input", "cacheRead", "cacheWrite")))
    return {"calls": calls, "request_input": request_input, "multi_turns": count_multi}


def parse_kroma(directory):
    calls, request_input, count_multi = [], [], 0
    for line in (directory / "session.json.wal").open(errors="replace"):
        message = safe_json(line.split("\t", 1)[0]).get("message") or {}
        if message.get("role") == "assistant":
            tool_calls = message.get("tool_calls") or []
            count_multi += len(tool_calls) > 1
            for call in tool_calls:
                fn = call.get("function") or {}
                args = safe_json(fn.get("arguments", "{}"))
                calls.append({
                    "name": fn.get("name"),
                    "command": args.get("command", ""),
                    "path": args.get("path", ""),
                    "duration_ms": None, "result_chars": 0, "output": "",
                })
        elif message.get("role") == "tool":
            for row in calls:
                if row["output"] == "" and row["duration_ms"] is None:
                    row["output"] = message.get("content") or ""
                    row["result_chars"] = len(row["output"])
                    # Use a sentinel while waiting for the profile durations.
                    row["duration_ms"] = -1
                    break
    log = (directory / "stderr.log").read_text(errors="replace")
    times = [float(x) for x in re.findall(r"^\[profile\] tool_done[^\n]*elapsed_ms=([0-9.]+)", log, re.M)]
    assert len(times) == len(calls), (str(directory), len(times), len(calls))
    for row, elapsed in zip(calls, times):
        row["duration_ms"] = elapsed
    for line in log.splitlines():
        if line.startswith("[usage] request:"):
            match = re.search(r"\binput:\s*(\d+)", line)
            if match:
                request_input.append(int(match.group(1)))
    return {"calls": calls, "request_input": request_input, "multi_turns": count_multi}


def classify(row):
    cmd = row["command"].lower()
    if row["name"] in ("edit", "write"):
        return "edit"
    if "python3 - <<" in cmd and ("write_text" in cmd or "p.write_text" in cmd or ".replace(" in cmd):
        return "edit"
    if "cargo test" in cmd and "wal_append_controls_recent_session_order" in cmd:
        return "target_test"
    if "cargo test" in cmd:
        return "full_test"
    if "cargo check" in cmd:
        return "cargo_check"
    if "cargo fmt" in cmd:
        return "format"
    if any(x in cmd for x in ("df -h", "du -sh", "env | grep", "tmpdir=")):
        return "environment"
    return "inspect"


def summarize(directory, runner):
    data = parse_pi(directory) if runner == "pi" else parse_kroma(directory)
    calls, inputs = data["calls"], data["request_input"]
    classified = [classify(call) for call in calls]
    first_edit = next((i + 1 for i, category in enumerate(classified) if category == "edit"), None)
    milliseconds = sum(row["duration_ms"] or 0 for row in calls)
    disk_issue = any("No space left on device" in row["output"] for row in calls)
    filtered_target = sum(
        1 for row, category in zip(calls, classified)
        if category == "target_test" and "tail " in row["command"]
        and "test result: ok" in row["output"]
        and "test sessions::engineering_regression::wal_append_controls_recent_session_order ... ok" not in row["output"]
    )
    return {
        "runner": runner,
        "requests": len(inputs),
        "input_tokens": sum(inputs),
        "input_last_five": sum(inputs[-5:]),
        "tools": len(calls),
        "tool_elapsed_s": round(milliseconds / 1000, 3),
        "tool_result_chars": sum(x["result_chars"] for x in calls),
        "multi_tool_responses": data["multi_turns"],
        "first_edit_tool_index": first_edit,
        "inspect_calls": classified.count("inspect"),
        "target_test_commands": sum("cargo test" in c["command"] and "wal_append_controls_recent_session_order" in c["command"] for c in calls),
        "full_suite_commands": sum("cargo test" in c["command"] and "wal_append_controls_recent_session_order" not in c["command"] for c in calls),
        "disk_full_error": disk_issue,
        "target_test_output_hidden_by_tail": filtered_target,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path("/tmp/kroma-pi-crossover-96767baa"))
    args = parser.parse_args()
    report = safe_json((args.root / "results.json").read_text())
    for repeat in (1, 2, 3):
        for runner in ("pi", "kroma"):
            directory = args.root / "rust_regression" / str(repeat) / runner
            summary = summarize(directory, runner)
            matching = next((x for x in report["results"] if x["task"] == "rust_regression" and x["repeat"] == repeat and x["runner"] == runner), {})
            summary["wall_s"] = matching.get("elapsed_s")
            summary["passed"] = matching.get("passed")
            print(json.dumps({"pair": repeat, **summary}, ensure_ascii=False))


if __name__ == "__main__":
    main()
