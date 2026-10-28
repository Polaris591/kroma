#!/usr/bin/env python3
"""Real-model A/B/C tool-batching smoke benchmark in the existing sandbox."""
import csv
import json
import random
import re
from pathlib import Path

import fusion_ablation as bench

OUT = Path("/tmp/kroma-tool-batching")
bench.OUT = OUT
bench.RUNTIME = OUT / "runtime"
bench.WORK = bench.RUNTIME / "project"
bench.SESSION = bench.RUNTIME / "history.json"
# Let a model naturally batch independent inspection, with a real code-fix oracle.
bench.TASK = (
    "先检查 src/stats.py 和 src/percentiles.py 两个独立文件，并运行 "
    "python3 -m unittest discover -s tests -v。然后修复 median 偶数输入问题、"
    "percentile 的历史 nearest-rank 定义。只修改这两个源文件，不改测试、不安装依赖。"
    "重跑测试，报告历史 QA 码、算法标记和实际测试结果。"
)
original_sandbox = bench.sandbox_command
active_mode = "a"


def sandbox_with_mode(argv):
    command = list(argv)
    if "/kroma" in command:
        position = command.index("/kroma")
        command[position:position] = [
            "--setenv", "KROMA_TOOL_EXPERIMENT", active_mode,
        ]
    return original_sandbox(command)


bench.sandbox_command = sandbox_with_mode


def main():
    global active_mode
    OUT.mkdir(exist_ok=True)
    bench.validate_fixtures()
    connection = bench.credentials()
    rows = []
    order = list("abc")
    random.Random(20261026).shuffle(order)
    for mode in order:
        active_mode = mode
        row = bench.run_once(mode, 1, "batch", 120, connection)
        err_path = OUT / "logs" / f"batch-1-{mode}.err"
        stderr = err_path.read_text()
        parallel = re.findall(
            r"\[tool-ablation\] parallel_read_batch=(\d+) elapsed_ms=(\d+)",
            stderr
        )
        turns = [int(m) for m in re.findall(
            r"\[tool-ablation\] mode=[abc] same_turn_calls=(\d+)", stderr
        )]
        row.update({
            "same_turn_multi_groups": sum(n > 1 for n in turns),
            "max_calls_per_turn": max(turns, default=0),
            "parallel_groups": len(parallel),
            "parallel_tools": sum(int(n) for n, _ in parallel),
            "parallel_group_ms": sum(int(ms) for _, ms in parallel),
        })
        rows.append(row)
        with (OUT / "results.csv").open("w", newline="") as file:
            fields = list(rows[0])
            writer = csv.DictWriter(file, fieldnames=fields)
            writer.writeheader()
            writer.writerows(rows)
        print(json.dumps({
            key: row[key] for key in (
                "variant", "seconds", "algorithm_success", "foreground_requests",
                "foreground_input", "foreground_output", "apec_requests",
                "same_turn_multi_groups", "parallel_groups", "parallel_tools",
                "error_class",
            )
        }), flush=True)
    print("RESULTS", OUT / "results.csv")


if __name__ == "__main__":
    main()
