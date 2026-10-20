#!/usr/bin/env python3
"""E01/E04, CPU half: does pinning fewer tool schemas change startup and idle residency?

This measures only what needs no model: process start to an answered ACP `initialize`, and the
process-tree PSS while the session sits idle. The schema bytes themselves never leave the process
until a model request, so "schema size" and "wasted model actions" are **not** measured here - they
need a provider turn and are reported as gaps.

Arms: `full` (default), `on-demand` (`--on-demand-tools`), `minimal` (`--tool-filter read,edit,write,bash`).

Usage: python3 measure-exposure-cost.py --kroma ./target/debug/kroma --repeats 5 --out FILE
"""
import argparse
import json
import os
import statistics
import subprocess
import time
from pathlib import Path

ARMS = {
    "full": [],
    "on-demand": ["--on-demand-tools"],
    "minimal": ["--tool-filter", "read,edit,write,bash"],
}


def tree_pss_kb(root_pid):
    """Sum Pss over the process and its children, straight from /proc."""
    pids, total, frontier = {root_pid}, 0, [root_pid]
    while frontier:
        pid = frontier.pop()
        try:
            for task in os.listdir(f"/proc/{pid}/task"):
                for line in open(f"/proc/{pid}/task/{task}/children"):
                    for child in line.split():
                        child = int(child)
                        if child not in pids:
                            pids.add(child)
                            frontier.append(child)
        except OSError:
            pass
    for pid in pids:
        try:
            for line in open(f"/proc/{pid}/smaps_rollup"):
                if line.startswith("Pss:"):
                    total += int(line.split()[1])
        except OSError:
            pass
    return total


def run_once(kroma, args, idle_seconds, interval):
    started = time.perf_counter()
    proc = subprocess.Popen(
        [kroma, "acp", *args],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    request = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1, "clientCapabilities": {}}}
    )
    try:
        proc.stdin.write(request + "\n")
        proc.stdin.flush()
        line = proc.stdout.readline()
        init_ms = (time.perf_counter() - started) * 1000.0
        answered = bool(line.strip())
        samples = []
        deadline = time.perf_counter() + idle_seconds
        while time.perf_counter() < deadline:
            samples.append(tree_pss_kb(proc.pid))
            time.sleep(interval)
        return {
            "init_answered": answered,
            "init_ms": round(init_ms, 3),
            "idle_seconds": idle_seconds,
            "pss_samples_kb": samples,
            "pss_median_kb": statistics.median(samples) if samples else None,
            "pss_max_kb": max(samples) if samples else None,
        }
    finally:
        proc.kill()
        proc.wait()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--kroma", required=True)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--idle-seconds", type=float, default=3.0)
    parser.add_argument("--interval", type=float, default=0.1)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()

    kroma = str(Path(args.kroma).resolve())
    records = []
    # Alternating order per repeat so a warm page cache does not always favour the same arm.
    order = list(ARMS)
    for repeat in range(1, args.repeats + 1):
        for arm in (order if repeat % 2 else list(reversed(order))):
            record = run_once(kroma, ARMS[arm], args.idle_seconds, args.interval)
            record.update({"arm": arm, "repeat": repeat})
            records.append(record)

    summary = {}
    for arm in ARMS:
        rows = [row for row in records if row["arm"] == arm]
        inits = [row["init_ms"] for row in rows]
        pss = [row["pss_median_kb"] for row in rows]
        summary[arm] = {
            "repeats": len(rows),
            "init_ms_median": round(statistics.median(inits), 3),
            "init_ms_min": min(inits),
            "init_ms_max": max(inits),
            "pss_median_kb": statistics.median(pss),
            "pss_min_kb": min(pss),
            "pss_max_kb": max(pss),
        }

    report = {
        "format": "kroma.exposure_cost/1",
        "kroma": kroma,
        "arms": list(ARMS),
        "method": {
            "startup": "spawn -> initialize answered, measured once per run",
            "residency": f"process-tree PSS every {args.interval}s for {args.idle_seconds}s idle",
            "order": "alternating per repeat",
            "not_measured": [
                "tool schema bytes (they are only serialised into a model request)",
                "wasted model actions (needs a model turn)",
            ],
        },
        "summary": summary,
        "records": records,
    }
    text = json.dumps(report, ensure_ascii=False, indent=2)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + "\n", encoding="utf-8")
    for arm, row in summary.items():
        print(
            f"{arm:10s} init_ms {row['init_ms_median']:7.2f}  pss_kb {row['pss_median_kb']:8.0f}"
            f"  [{row['pss_min_kb']:.0f}-{row['pss_max_kb']:.0f}]"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
