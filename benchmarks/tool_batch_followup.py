#!/usr/bin/env python3
"""Further B/C experiments: real coding fixes and controlled independent reads.

Reuses fusion_ablation's bwrap sandbox and credential-isolating local proxy.
Historical evidence and tool output filtering remain the same in B and C.
"""
import argparse
import csv
import hashlib
import json
import random
import re
import statistics
import threading
import time
from pathlib import Path

import fusion_ablation as bench

OUT = Path("/tmp/kroma-tool-followup")
bench.OUT = OUT
bench.RUNTIME = OUT / "runtime"
bench.WORK = bench.RUNTIME / "project"
bench.SESSION = bench.RUNTIME / "history.json"
bench.TASK = (
    "先检查 src/stats.py 和 src/percentiles.py 两个独立文件，并运行 "
    "python3 -m unittest discover -s tests -v。然后修复 median 偶数输入问题、"
    "percentile 的历史 nearest-rank 定义。只修改这两个源文件，不改测试、不安装依赖。"
    "重跑测试，报告历史 QA 码、算法标记和实际测试结果。"
)
original_sandbox = bench.sandbox_command
active_mode = "b"


def sandbox_command(args):
    args = list(args)
    if "/kroma" in args:
        args[args.index("/kroma"):args.index("/kroma")] = [
            "--setenv", "KROMA_TOOL_EXPERIMENT", active_mode,
        ]
    return original_sandbox(args)


bench.sandbox_command = sandbox_command


def metadata(stderr):
    pair = re.findall(
        r"\[tool-ablation\] mode=[bc] tool_group_ms=(\d+) same_turn_calls=(\d+)", stderr
    )
    parallel = re.findall(
        r"\[tool-ablation\] parallel_read_batch=(\d+) elapsed_ms=(\d+)", stderr
    )
    return {
        "tool_group_ms": sum(int(ms) for ms, _ in pair),
        "multi_call_groups": sum(int(count) > 1 for _, count in pair),
        "max_same_turn_calls": max((int(count) for _, count in pair), default=0),
        "parallel_groups": len(parallel),
        "parallel_tools": sum(int(n) for n, _ in parallel),
        "parallel_ms": sum(int(ms) for _, ms in parallel),
    }


def usage(records):
    fg, bg = (bench.aggregate(records, kind) for kind in ("foreground", "apec"))
    result = {"foreground_requests": fg["requests"], "apec_requests": bg["requests"]}
    for key in ("input", "output", "cached"):
        result["foreground_" + key] = fg[key]
        result["apec_" + key] = bg[key] if bg["requests"] else 0
        a, b = result["foreground_" + key], result["apec_" + key]
        result["total_" + key] = a + b if a is not None and b is not None else None
    result["uncached_input"] = (
        result["total_input"] - result["total_cached"]
        if result["total_input"] is not None and result["total_cached"] is not None
        else None
    )
    result["api_errors"] = sum(
        bool(record.get("error") or (record.get("status") or 0) >= 400)
        for record in records
    )
    return result


def seed_read_fixture(repeat, delay_ms):
    bench.prepare()
    codes = {}
    for name in ("alpha", "beta", "gamma"):
        code = "K" + hashlib.sha256(
            f"kroma-followup-{repeat}-{name}".encode()
        ).hexdigest()[:10].upper()
        codes[name] = code
        path = bench.WORK / "docs" / (name + ".txt")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(f"Project record {name}\nCODE: {code}\nSource: {name}\n")
    state = {
        "version": 3, "workdir": str(bench.WORK),
        "messages": [{"role": "system", "content": (
            "You are Kroma. Use shell to read project files before reporting. "
            "Do not edit anything. Quote only verified facts."
        )}],
        "compression": {
            "summaries": [], "covered_until": 1, "consumed_until": 1,
            "cleared_tools": [], "revision": 0
        }
    }
    bench.SESSION.write_text(bench.encoded(state))
    if delay_ms:
        wrapper = bench.RUNTIME / "test-bin" / "cat"
        wrapper.parent.mkdir(parents=True, exist_ok=True)
        wrapper.write_text(
            "#!/bin/sh\n"
            f"sleep {delay_ms / 1000:.3f}\n"
            "printf '[bench-cat] executed\\n' >&2\n"
            'exec /usr/bin/cat "$@"\n'
        )
        wrapper.chmod(0o755)
    return codes


def inspect_once(mode, repeat, delay_ms, timeout, conn):
    codes = seed_read_fixture(repeat, delay_ms)
    base, model, secret = conn
    proxy = bench.Proxy(base, model, secret)
    thread = threading.Thread(target=proxy.serve_forever, daemon=True)
    thread.start()
    port = proxy.server_address[1]
    task = (
        "读取 docs/alpha.txt、docs/beta.txt、docs/gamma.txt 三份独立资料。"
        "在第一个 assistant 响应中请提交恰好三个独立的 shell 工具调用，"
        "各自的 command 精确是 cat docs/alpha.txt、cat docs/beta.txt、"
        "cat docs/gamma.txt。不要将三条命令合并，不做其他操作或修改。"
        "收到实际工具返回后，分别列出三个 CODE，并给出按 alpha beta gamma "
        "顺序无空格直接拼接的 CODE 字符串。"
    )
    flags = [
        "--setenv", "KROMA_BASE_URL", f"http://127.0.0.1:{port}/v1",
        "--setenv", "KROMA_MODEL", model,
        "--setenv", "KROMA_WORKDIR", str(bench.WORK),
        "--setenv", "KROMA_SESSION_FILE", str(bench.SESSION),
        "--setenv", "KROMA_CONTEXT_WINDOW", "128000",
        "--setenv", "KROMA_OUTPUT_RESERVE_TOKENS", "4096",
    ]
    if delay_ms:
        flags.extend(["--setenv", "PATH", f"{bench.RUNTIME}/test-bin:/usr/bin:/bin"])
    command = sandbox_command(flags + ["/kroma", task])
    started = time.monotonic()
    try:
        stdout, stderr, status = bench.process(
            command, timeout, env={"PATH": "/usr/bin:/bin"}
        )
        seconds = round(time.monotonic() - started, 3)
        deadline = time.monotonic() + 3
        while proxy.active and time.monotonic() < deadline:
            time.sleep(.05)
        records = json.loads(bench.encoded(proxy.records))
    finally:
        proxy.shutdown()
        proxy.server_close()
        thread.join(timeout=2)
    messages = json.loads(bench.SESSION.read_text())["messages"][1:]
    assistant_calls = [m for m in messages if m["role"] == "assistant" and m.get("tool_calls")]
    all_calls = [call for m in assistant_calls for call in m["tool_calls"]]
    commands = []
    for call in all_calls:
        try:
            commands.append(json.loads(call["function"]["arguments"])["command"])
        except (KeyError, ValueError, TypeError):
            commands.append("UNPARSEABLE")
    expected = [f"cat docs/{name}.txt" for name in ("alpha", "beta", "gamma")]
    results = []
    for message in messages:
        if message["role"] == "tool":
            try:
                results.append(json.loads(message["content"]))
            except (ValueError, TypeError, KeyError):
                results.append({})
    final = next((
        m.get("content") or "" for m in reversed(messages)
        if m["role"] == "assistant" and not m.get("tool_calls")
    ), "")
    expected_concatenation = "".join(codes[name] for name in ("alpha", "beta", "gamma"))
    exact = len(assistant_calls) == 1 and len(assistant_calls[0]["tool_calls"]) == 3 and commands == expected
    result_ok = len(results) == 3 and all(
        result.get("exit_code") == 0 and codes[name] in result.get("stdout", "")
        for result, name in zip(results, ("alpha", "beta", "gamma"))
    )
    shim_calls = sum("[bench-cat] executed" in result.get("stderr", "") for result in results)
    files_ok = all(
        (bench.WORK / "docs" / f"{name}.txt").read_text() ==
        f"Project record {name}\nCODE: {code}\nSource: {name}\n"
        for name, code in codes.items()
    )
    row = {
        "workload": "read_3x" + str(delay_ms) + "ms",
        "round": repeat, "mode": mode, "seconds": seconds, "exit_code": status,
        "tool_count": len(all_calls), "exact_3_calls": exact,
        "tool_outputs_correct": result_ok,
        "answer_correct": all(code in final for code in codes.values())
            and expected_concatenation in re.sub(r"\s+", "", final),
        "shim_calls": shim_calls, "files_unchanged": files_ok,
    }
    row.update(metadata(stderr))
    row.update(usage(records))
    row["success"] = (
        status == 0 and exact and result_ok and row["answer_correct"] and files_ok
        and row["api_errors"] == 0 and (delay_ms == 0 or shim_calls == 3)
    )
    stem = f"read-{delay_ms}-{repeat}-{mode}"
    bench.save(OUT / "logs" / (stem + ".out"), stdout, (secret,))
    bench.save(OUT / "logs" / (stem + ".err"), stderr, (secret,))
    bench.save(OUT / "logs" / (stem + ".requests.json"),
               json.dumps(records, indent=2), (secret,))
    bench.save(OUT / "logs" / (stem + ".verification.json"),
               json.dumps({"commands": commands, "success": row["success"]}), (secret,))
    return row


def coding_once(mode, repeat, timeout, conn):
    source = bench.run_once(mode, repeat, "coding-followup", timeout, conn)
    stderr = (OUT / "logs" / f"coding-followup-{repeat}-{mode}.err").read_text()
    row = {
        "workload": "coding_fix", "round": repeat, "mode": mode,
        "seconds": source["seconds"], "exit_code": source["exit_code"],
        "tool_count": source["tool_count"], "success": source["algorithm_success"],
        "api_errors": source["api_errors"], "error_class": source["error_class"],
        "apec_commits": source["apec_commits"],
    }
    row.update(metadata(stderr))
    for kind in ("foreground", "apec"):
        for name in ("requests", "input", "output", "cached"):
            row[f"{kind}_{name}"] = source.get(f"{kind}_{name}")
    for name in ("input", "output", "cached"):
        fg, bg = row[f"foreground_{name}"], row[f"apec_{name}"]
        row[f"total_{name}"] = fg + bg if fg is not None and bg is not None else None
    row["uncached_input"] = (
        row["total_input"] - row["total_cached"]
        if row["total_input"] is not None and row["total_cached"] is not None
        else None
    )
    return row


def write_results(rows):
    columns = list(dict.fromkeys(k for row in rows for k in row))
    with (OUT / "results.csv").open("w", newline="") as file:
        writer = csv.DictWriter(file, fieldnames=columns)
        writer.writeheader()
        writer.writerows(rows)
    lines = [
        "# Kroma B/C follow-up: real DeepSeek ablations", "",
        "B and C use the same prompt and project; C alone may concurrently execute"
        " adjacent, whitelisted, read-only shell calls (up to three).", "",
        "| Task | Mode | Runs | Successful | Mean elapsed sec | Mean model requests | Mean input tokens | Mean tool execution ms | Parallel groups |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    def number(row, key):
        try:
            return float(row[key])
        except (ValueError, TypeError, KeyError):
            return None
    for workload in sorted(set(r["workload"] for r in rows)):
        for mode in "bc":
            subset = [r for r in rows if r["workload"] == workload and r["mode"] == mode]
            if not subset:
                continue
            def avg(name):
                values = [number(r, name) for r in subset]
                values = [v for v in values if v is not None]
                return statistics.mean(values) if values else float("nan")
            successes = sum(str(r["success"]).lower() == "true" for r in subset)
            groups = sum(int(number(r, "parallel_groups") or 0) for r in subset)
            lines.append(
                f"| {workload} | {mode.upper()} | {len(subset)} | {successes}/{len(subset)}"
                f" | {avg('seconds'):.2f} | {avg('foreground_requests'):.2f}"
                f" | {avg('total_input'):,.0f} | {avg('tool_group_ms'):,.0f} | {groups} |"
            )
    lines += [
        "", "## Per-run evidence", "",
        "| Task | Round | Mode | Correct | Wall sec | Model calls | Input | Cached | Tool group ms | Concurrent batches |",
        "|---|---:|---|---|---:|---:|---:|---:|---:|---:|",
    ]
    for r in rows:
        lines.append(
            f"| {r['workload']} | {r['round']} | {r['mode'].upper()} | {r['success']}"
            f" | {r['seconds']} | {r['foreground_requests']} | {r['total_input']}"
            f" | {r['total_cached']} | {r['tool_group_ms']} | {r['parallel_groups']} |"
        )
    lines += [
        "", "## Limitations", "",
        "- Fixed WorkBuddy DeepSeek endpoint, controlled bwrap project/session and host-only auth proxy.",
        "- Independent reads: three source files and oracle-checked exact outputs; only C may parallelize.",
        "- The injected cat delay is synthetic and identical in B/C; it shows execution scheduling, not realistic disk throughput.",
        "- Coding: unchanged independent oracle for median and nearest-rank percentile, plus source/test constraints.",
        "- The first-use output filter and cache-aware TRC+APEC stay identical in B/C.",
        "- APEC background usage is included in total usage. Cached tokens are a subset of input.",
        "- No claim of statistical significance or price savings from small samples.",
    ]
    bench.save(OUT / "summary.md", "\n".join(lines))


def main():
    global active_mode
    parser = argparse.ArgumentParser()
    parser.add_argument("--phase", choices=("read", "coding", "both"), default="both")
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--start", type=int, default=1)
    parser.add_argument("--delay-ms", type=int, default=600)
    parser.add_argument("--timeout", type=int, default=120)
    args = parser.parse_args()
    if args.rounds not in (1, 2, 3) or args.start < 1 or not 0 <= args.delay_ms <= 2000:
        parser.error("invalid rounds, start or delay")
    OUT.mkdir(exist_ok=True)
    previous = OUT / "results.csv"
    rows = list(csv.DictReader(previous.open(newline=""))) if previous.exists() else []
    bench.validate_fixtures()
    conn = bench.credentials()
    tasks = []
    for repeat in range(args.start, args.start + args.rounds):
        for workload in ("read", "coding"):
            if args.phase in ("both", workload):
                tasks.append((workload, repeat))
    for workload, repeat in tasks:
        order = list("bc")
        random.Random(604 * repeat + (14 if workload == "read" else 29)).shuffle(order)
        for mode in order:
            active_mode = mode
            if workload == "read":
                row = inspect_once(mode, repeat, args.delay_ms, args.timeout, conn)
            else:
                row = coding_once(mode, repeat, args.timeout, conn)
            rows.append(row)
            write_results(rows)
            print("RUN", json.dumps({
                key: row.get(key) for key in (
                    "workload", "round", "mode", "success", "seconds",
                    "foreground_requests", "total_input", "uncached_input",
                    "tool_group_ms", "max_same_turn_calls", "parallel_groups",
                    "parallel_tools", "shim_calls", "api_errors"
                )
            }), flush=True)
    print("COMPLETE", OUT / "summary.md", flush=True)


if __name__ == "__main__":
    main()
