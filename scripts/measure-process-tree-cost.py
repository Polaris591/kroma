#!/usr/bin/env python3
"""C00-05: what a capability that is present but unused costs the process tree.

Three questions, measured on `kroma serve`, which starts the whole daemon and then sits idle - so
the numbers are the product's own process cost and no model call is involved:

* **lazy work** - a session that never touches the repo graph spawns no worker and writes no index;
* **shared dependency** - releasing one daemon does not disturb another that shares the binary,
  the host caches and the parent directory;
* **plateau** - repeated start/stop cycles settle at a bounded peak instead of climbing.

Usage: python3 measure-process-tree-cost.py --kroma ./target/debug/kroma --cycles 6 --out FILE
"""
import argparse
import json
import os
import signal
import socket
import statistics
import subprocess
import time
from pathlib import Path

ENVELOPE = "100000"


def tree_pids(root):
    pids, frontier = {root}, [root]
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


def dir_bytes(root):
    total = 0
    for path in Path(root).rglob("*"):
        try:
            if path.is_file():
                total += path.stat().st_size
        except OSError:
            pass
    return total


def port_open(port, timeout=1.0):
    with socket.socket() as sock:
        sock.settimeout(timeout)
        return sock.connect_ex(("127.0.0.1", port)) == 0


def start_daemon(kroma, workspace, port):
    return subprocess.Popen(
        [kroma, "serve", "--workspace", str(workspace), "--listen", f"127.0.0.1:{port}",
         "--max-tokens", ENVELOPE],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )


def wait_listening(proc, port, timeout=20.0):
    started = time.time()
    while time.time() - started < timeout:
        if proc.poll() is not None:
            return False
        if port_open(port):
            return True
        time.sleep(0.1)
    return False


def stop(proc):
    if proc.poll() is None:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--kroma", required=True)
    parser.add_argument("--cycles", type=int, default=6)
    parser.add_argument("--base-port", type=int, default=19100)
    parser.add_argument("--workspace", default="/tmp/kroma-c0005/workspace")
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()

    kroma = str(Path(args.kroma).resolve())
    workspace = Path(args.workspace)
    workspace.mkdir(parents=True, exist_ok=True)
    (workspace / "README.md").write_text("# scratch\n", encoding="utf-8")

    cycles = []
    for index in range(args.cycles):
        port = args.base_port + index
        before = dir_bytes(workspace)
        proc = start_daemon(kroma, workspace, port)
        try:
            if not wait_listening(proc, port):
                cycles.append({"cycle": index, "failed_to_start": True})
                continue
            time.sleep(0.5)
            pids = tree_pids(proc.pid)
            cycles.append({
                "cycle": index,
                "peak_pss_kb": tree_pss_kb(pids),
                "children": len(pids) - 1,
                "workspace_bytes_written": dir_bytes(workspace) - before,
            })
            print(f"cycle {index}: pss={cycles[-1]['peak_pss_kb']}KB "
                  f"children={cycles[-1]['children']} "
                  f"workspace_bytes={cycles[-1]['workspace_bytes_written']}", flush=True)
        finally:
            stop(proc)

    # Shared dependency: two daemons, one released, the other must keep listening.
    port_a, port_b = args.base_port + 50, args.base_port + 51
    other = workspace.parent / "workspace-b"
    other.mkdir(parents=True, exist_ok=True)
    (other / "README.md").write_text("# scratch b\n", encoding="utf-8")
    first = start_daemon(kroma, workspace, port_a)
    second = start_daemon(kroma, other, port_b)
    shared = {"first_listening": wait_listening(first, port_a),
              "second_listening": wait_listening(second, port_b)}
    try:
        shared["second_alive_before"] = second.poll() is None
        shared["second_pss_before_kb"] = tree_pss_kb(tree_pids(second.pid))
        stop(first)
        time.sleep(1.0)
        shared["second_alive_after"] = second.poll() is None
        shared["second_answers_after"] = port_open(port_b)
        shared["second_pss_after_kb"] = tree_pss_kb(tree_pids(second.pid))
    finally:
        stop(second)
    print("shared release:", json.dumps(shared), flush=True)

    healthy = [c for c in cycles if not c.get("failed_to_start")]
    half = max(1, len(healthy) // 2)
    first_half = [c["peak_pss_kb"] for c in healthy[:half]]
    second_half = [c["peak_pss_kb"] for c in healthy[half:]]
    summary = {
        "cycles": len(cycles),
        "failed_to_start": sum(1 for c in cycles if c.get("failed_to_start")),
        "children_max": max((c["children"] for c in healthy), default=None),
        "workspace_bytes_max": max((c["workspace_bytes_written"] for c in healthy), default=None),
        "peak_pss_kb_first_half_median": statistics.median(first_half) if first_half else None,
        "peak_pss_kb_second_half_median": statistics.median(second_half) if second_half else None,
        "plateau_growth_basis_points": (
            round((statistics.median(second_half) - statistics.median(first_half)) * 10_000
                  / statistics.median(first_half))
            if first_half and second_half and statistics.median(first_half) else None
        ),
        "shared_release": shared,
    }
    report = {
        "format": "kroma.process_tree_cost/1",
        "kroma": kroma,
        "method": {
            "lazy": "daemon started and left idle: child processes spawned and workspace bytes written",
            "plateau": f"{args.cycles} start/stop cycles, peak process-tree PSS per cycle",
            "shared": "two daemons sharing binary, host caches and parent directory; one released, "
                      "the other probed on its port",
        },
        "summary": summary,
        "cycles": cycles,
    }
    text = json.dumps(report, ensure_ascii=False, indent=2)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + "\n", encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
