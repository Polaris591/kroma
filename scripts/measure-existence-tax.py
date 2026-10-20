#!/usr/bin/env python3
"""E01, the two views the exposure benchmark cannot see: idle wakeups and the post-cancel plateau.

* idle wakeups: CPU ticks a session burns while doing nothing. A capability that is "present but
  dormant" should not cost scheduler time; if it does, that is the existence tax in its purest form.
* post-cancel plateau: how long the process tree keeps holding memory after the run is cancelled.
  A cancel that leaves the tree resident is not a cancel.

Both are read from /proc, so the numbers are the kernel's, not the tool's.

Usage: python3 measure-existence-tax.py --kroma ./target/debug/kroma --repeats 3 --out FILE
"""
import argparse
import json
import os
import signal
import statistics
import subprocess
import time
from pathlib import Path

EXPOSURES = {
    "full": [],
    "minimal": ["--tool-filter", "read,edit,write,bash"],
}


def tree_pids(root_pid):
    pids, frontier = {root_pid}, [root_pid]
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
    return pids


def tree_ticks(pids):
    total = 0
    for pid in pids:
        try:
            fields = open(f"/proc/{pid}/stat").read().rsplit(") ", 1)[1].split()
            total += int(fields[11]) + int(fields[12])
        except (OSError, IndexError):
            pass
    return total


def tree_pss_kb(pids):
    total = 0
    for pid in pids:
        try:
            for line in open(f"/proc/{pid}/smaps_rollup"):
                if line.startswith("Pss:"):
                    total += int(line.split()[1])
        except OSError:
            pass
    return total


def idle_wakeups(kroma, args, seconds):
    proc = subprocess.Popen([kroma, "acp", *args], stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    try:
        proc.stdin.write('{"jsonrpc":"2.0","id":1,"method":"initialize","params":'
                         '{"protocolVersion":1,"clientCapabilities":{}}}\n')
        proc.stdin.flush()
        proc.stdout.readline()
        pids = tree_pids(proc.pid)
        before = tree_ticks(pids)
        time.sleep(seconds)
        after = tree_ticks(tree_pids(proc.pid) | pids)
        return {"idle_seconds": seconds, "cpu_ticks": after - before,
                "ticks_per_second": (after - before) / seconds}
    finally:
        proc.kill()
        proc.wait()


def post_cancel_plateau(kroma, args, cancel_after, watch_for, workspace):
    proc = subprocess.Popen([kroma, "--profile", "gemini", "--workspace", workspace,
                             "--task", "Read README.md and summarise it in one line.",
                             *args],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(cancel_after)
    pids = tree_pids(proc.pid)
    samples = []
    proc.send_signal(signal.SIGTERM)
    started = time.time()
    while time.time() - started < watch_for:
        live = tree_pids(proc.pid) if proc.poll() is None else set()
        samples.append({"t": round(time.time() - started, 2),
                        "pss_kb": tree_pss_kb(live), "alive": bool(live)})
        time.sleep(0.25)
    if proc.poll() is None:
        proc.kill()
    proc.wait()
    resident = [s["pss_kb"] for s in samples if s["pss_kb"] > 0]
    return {
        "cancel_after_s": cancel_after,
        "watch_s": watch_for,
        "pids_at_cancel": len(pids),
        "released_after_s": next((s["t"] for s in samples if s["pss_kb"] == 0), None),
        "peak_after_cancel_kb": max(resident) if resident else 0,
        "samples": samples[::4],
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--kroma", required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--idle-seconds", type=float, default=10.0)
    parser.add_argument("--watch-seconds", type=float, default=10.0)
    parser.add_argument("--workspace", default="/tmp/kroma-existence-tax")
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()

    kroma = str(Path(args.kroma).resolve())
    workspace = Path(args.workspace)
    workspace.mkdir(parents=True, exist_ok=True)
    (workspace / "README.md").write_text("# scratch\n\nA tiny workspace.\n", encoding="utf-8")

    records = {"idle": [], "cancel": []}
    for repeat in range(1, args.repeats + 1):
        order = list(EXPOSURES) if repeat % 2 else list(reversed(list(EXPOSURES)))
        for name in order:
            idle = idle_wakeups(kroma, EXPOSURES[name], args.idle_seconds)
            idle.update({"exposure": name, "repeat": repeat})
            records["idle"].append(idle)
            cancel = post_cancel_plateau(kroma, EXPOSURES[name], 6.0, args.watch_seconds,
                                         str(workspace))
            cancel.update({"exposure": name, "repeat": repeat})
            records["cancel"].append(cancel)
            print(f"{name:8s} idle_ticks={idle['cpu_ticks']:4d} "
                  f"released_after={cancel['released_after_s']} "
                  f"peak_after_cancel_kb={cancel['peak_after_cancel_kb']}", flush=True)

    summary = {}
    for name in EXPOSURES:
        idle = [r["cpu_ticks"] for r in records["idle"] if r["exposure"] == name]
        released = [r["released_after_s"] for r in records["cancel"] if r["exposure"] == name]
        summary[name] = {
            "idle_cpu_ticks_median": statistics.median(idle) if idle else None,
            "idle_cpu_ticks_max": max(idle) if idle else None,
            "released_after_s_median": (statistics.median([r for r in released if r is not None])
                                        if any(r is not None for r in released) else None),
            "cancel_samples": len(released),
        }

    report = {"format": "kroma.existence_tax/1", "kroma": kroma,
              "method": {"idle": f"cpu ticks over {args.idle_seconds}s idle after initialize",
                         "cancel": "SIGTERM after 6s of a model run, PSS sampled for "
                                   f"{args.watch_seconds}s"},
              "summary": summary, "records": records}
    text = json.dumps(report, ensure_ascii=False, indent=2)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + "\n", encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
