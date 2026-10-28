#!/usr/bin/env python3
"""Second-round A/B/C/D experiment: late verbose tools + post-compaction recall.

Uses fusion_ablation's bounded sandbox and credential-isolating local proxy.
All outputs stay in /tmp/kroma-fusion-stress, separate from earlier results.
"""
import argparse
import csv
import json
import math
import random
import time
import threading
from pathlib import Path

import fusion_ablation as bench

OUT = Path("/tmp/kroma-fusion-stress")
bench.OUT = OUT
bench.RUNTIME = OUT / "runtime"
bench.WORK = bench.RUNTIME / "project"
bench.SESSION = bench.RUNTIME / "history.json"
bench.WINDOW = 40000
bench.RESERVE = 4000
MARKER = "NOVA-58-Q"
SEED = bench.seed_messages
bench.TASK = (
    "先使用 shell 运行 python3 -m unittest discover -s tests -v，"
    "再只修复 src/stats.py 的 median 和 src/percentiles.py 的 percentile，"
    "不得更改测试文件或安装依赖。再次运行测试，"
    "最终列出历史中的 QA 签核码以及 nearest-rank 算法审计标记。"
)


def stress_messages():
    messages = SEED()
    # The original source note is buried in raw tool evidence early enough for
    # APEC to cover it, but doesn't match evidence_projection's keyword list.
    for message in messages:
        if message["role"] == "tool" and message.get("tool_call_id") == "archived_0":
            data = json.loads(message["content"])
            lines = data["stdout"].splitlines()
            lines[17] = "build 0 step 17: launch gate " + MARKER + " in internal note."
            data["stdout"] = "\n".join(lines)
            message["content"] = bench.encoded(data)
            break
    else:
        raise AssertionError("missing archived tool fixture")

    tag = "big_tail_call"
    messages.extend([
        {"role": "user", "content": "Archived diagnostic: examine a verbose build log."},
        {"role": "assistant", "content": None, "tool_calls": [
            {"id": tag, "type": "function", "function": {
                "name": "shell", "arguments": bench.encoded({"command": "echo historical"})}}]},
        {"role": "tool", "tool_call_id": tag, "content": bench.encoded({
            "exit_code": 0,
            "stdout": "\n".join(
                f"build tail step {i:03d}: unimportant compiled artifact progress "
                "and verbose repeat information about module compilation."
                for i in range(240)
            ),
            "stderr": ""
        })},
        {"role": "assistant", "content": "Verbose build diagnostics were reviewed."},
    ])
    # Must lie outside the RECENT_MESSAGES guard for C, yet after the APEC
    # snapshot cutoff so APEC cannot preempt TRC's late-tool candidate.
    for i in range(9):
        messages.extend([
            {"role": "user", "content": f"Unrelated closed follow-up {i}, no new decisions."},
            {"role": "assistant", "content": f"Closed follow-up {i} acknowledged."}
        ])
    return messages


bench.seed_messages = stress_messages


def validate():
    original_messages = stress_messages()
    data = bench.encoded(original_messages).encode()
    rough = math.ceil(len(data) / 3) + 256
    available = bench.WINDOW - bench.RESERVE
    if not available * .5 < rough < available * .9:
        raise ValueError(f"stress fixture must be above soft/below hard: {rough}")
    location = next(i for i, m in enumerate(original_messages)
                    if m.get("tool_call_id") == "archived_0")
    late_tool = next(i for i, m in enumerate(original_messages)
                     if m.get("tool_call_id") == "big_tail_call")
    assert late_tool < len(original_messages) - 6
    assert MARKER in original_messages[location]["content"]
    assert MARKER not in bench.encoded(bench.initial_files())
    return {"estimated_input": rough, "soft": available // 2,
            "hard": available * 9 // 10, "marker_index": location,
            "late_tool_index": late_tool, "messages": len(original_messages)}


def run_followup(connection, variant, repeat, timeout, marker_index):
    state = json.loads(bench.SESSION.read_text())
    cutoff = state["compression"]["covered_until"]
    summaries = state["compression"]["summaries"]
    older_covered = cutoff > marker_index
    summary_contains_marker = any(MARKER in s["text"] for s in summaries)
    base, model, key = connection
    proxy = bench.Proxy(base, model, key)
    thread = threading.Thread(target=proxy.serve_forever, daemon=True)
    thread.start()
    port = proxy.server_address[1]
    task = (
        "这是一次后续会话中的历史记忆核对。不要运行 shell，也不要重新读取文件、日志或会话归档。"
        "请从你当前收到的压缩对话记忆中回答：最早的 archived build 0 输出中，"
        "launch gate 的精确标识符是什么？另给出 QA_SIGNOFF 和 nearest-rank 审计标记。"
        "如果某项不确定就写 UNKNOWN，禁止猜测。"
    )
    command = bench.sandbox_command([
        "--setenv", "KROMA_BASE_URL", f"http://127.0.0.1:{port}/v1",
        "--setenv", "KROMA_MODEL", model,
        "--setenv", "KROMA_WORKDIR", str(bench.WORK),
        "--setenv", "KROMA_SESSION_FILE", str(bench.SESSION),
        "--setenv", "KROMA_CONTEXT_WINDOW", str(bench.WINDOW),
        "--setenv", "KROMA_OUTPUT_RESERVE_TOKENS", str(bench.RESERVE),
        "--setenv", "KROMA_FUSION_EXPERIMENT", variant, "/kroma", task])
    start = time.monotonic()
    try:
        out, err, status = bench.process(command, timeout,
                                         env={"PATH": "/usr/bin:/bin"})
        seconds = round(time.monotonic() - start, 3)
        limit = time.monotonic() + 3
        while proxy.active and time.monotonic() < limit:
            time.sleep(.05)
        records = json.loads(bench.encoded(proxy.records))
    finally:
        proxy.shutdown()
        proxy.server_close()
        thread.join(timeout=2)
    saved = json.loads(bench.SESSION.read_text())
    # Tool invocations in the *follow-up* (not old history).
    old_len = len(state["messages"])
    added = saved["messages"][old_len:]
    extra_tools = sum(len(m.get("tool_calls") or [])
                      for m in added if m["role"] == "assistant")
    foreground = bench.aggregate(records, "foreground")
    apec = bench.aggregate(records, "apec")
    stem = f"followup-{repeat}-{variant}"
    bench.save(OUT / "logs" / (stem + ".out"), out, (key,))
    bench.save(OUT / "logs" / (stem + ".err"), err, (key,))
    bench.save(OUT / "logs" / (stem + ".requests.json"),
               json.dumps(records, indent=2), (key,))
    return {"followup_exit": status, "followup_seconds": seconds,
            "covered_old_marker": older_covered,
            "summary_contains_marker": summary_contains_marker,
            "followup_marker_recalled": MARKER in out,
            "followup_qa_recalled": "KROMA-QA-27" in out,
            "followup_rank_recalled": "RANK-CEIL-84" in out,
            "followup_tool_calls": extra_tools,
            "followup_foreground_input": foreground["input"],
            "followup_foreground_cached": foreground["cached"],
            "followup_apec_input": apec["input"] if apec["requests"] else 0,
            "followup_apec_requests": apec["requests"]}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--runs", type=int, default=1)
    parser.add_argument("--start", type=int, default=1)
    parser.add_argument("--variants", default="a,b,c,d")
    parser.add_argument("--timeout", type=int, default=100)
    args = parser.parse_args()
    variants = args.variants.split(",")
    if any(v not in {"a", "b", "c", "d"} for v in variants):
        parser.error("invalid variant")
    if not 60 <= args.timeout <= 150:
        parser.error("timeout must be 60..150 seconds")
    OUT.mkdir(parents=True, exist_ok=True)
    details = validate()
    bench.save(OUT / "fixture.json", json.dumps(details, indent=2))
    print("STRESS FIXTURE:", details, flush=True)
    connection = bench.credentials()
    rows = []
    result_path = OUT / "results.csv"
    if args.start > 1 and result_path.exists():
        with result_path.open(newline="") as handle:
            rows = list(csv.DictReader(handle))
    for repeat in range(args.start, args.start + args.runs):
        order = list(variants)
        random.Random(872 + repeat).shuffle(order)
        for variant in order:
            row = bench.run_once(variant, repeat, "stress", args.timeout,
                                 connection)
            try:
                row.update(run_followup(connection, variant, repeat, args.timeout,
                                        details["marker_index"]))
            except Exception as error:
                row["followup_error_class"] = type(error).__name__
            rows.append(row)
            with (OUT / "results.csv").open("w", newline="") as handle:
                writer = csv.DictWriter(handle,
                                        fieldnames=list(dict.fromkeys(k for r in rows for k in r)))
                writer.writeheader()
                writer.writerows(rows)
            print(json.dumps(row, ensure_ascii=False), flush=True)
    print("RESULTS:", OUT / "results.csv", flush=True)


if __name__ == "__main__":
    main()
