#!/usr/bin/env python3
"""Measure a local, model-free Kroma baseline on one already-built binary.

Two workloads only (blueprint v4 §6.3), no thresholds, no model, no network:

  P0  `kroma --version`                    process load cost, *not* an
                                           interactive startup
  P2  `kroma acp` + `workspace.graph`      static symbol-graph navigation over
                                           the measured workspace

P1 (cold start -> can accept input) is intentionally not measured here; see the
`unmeasured` block this script writes into its report.

Resource accounting is per process *tree* found under /proc, never the main PID
alone. Two kernel counters are reported separately:

  Rss  resident pages of each process; shared pages are counted again in every
       process that maps them, so a tree sum double counts shared memory.
  Pss  /proc/<pid>/smaps_rollup proportional set size: shared pages are divided
       by the number of sharers, so a tree sum approximates real footprint.

Sampling cannot observe a child that forks and exits between two samples, and it
does not control the page cache (no drop_caches), so every repeat is warm-cache.

Every run is bounded by one monotonic deadline: waits stop at the deadline, the
process group gets SIGTERM then SIGKILL, and reaping is bounded too. A hung
binary therefore produces a `timed_out` record instead of blocking the script.

Usage:
  scripts/measure-local-baseline.py --kroma /path/to/kroma \\
      --out /opt/kroma-v4/evidence/local-performance/<run> \\
      [--repo DIR] [--repeats 3] [--timeout 60]
"""

import argparse
import hashlib
import json
import os
import platform
import select
import shutil
import signal
import subprocess
import sys
import threading
import time

CASES = ("p0", "p2", "p3", "p4")

# Fixed P2 input so repeats stay comparable: a real symbol search, not a toy string.
GRAPH_PARAMS = {"query": "process supervisor", "max_nodes": 20}

# P3 indexes a generated tree, so its query names a symbol that tree actually defines.
CASE_PARAMS = {"p3": {"query": "generated_entry_00042", "max_nodes": 20}}

# P4 runs this many `kroma acp` sessions at once against the same repository.
P4_SESSIONS = 4

UNMEASURED = [
    {
        "id": "P1-startup-ready",
        "reason": "no reliable model-free readiness signal was demonstrated inside the bounded "
        "deadline, so startup-to-accepting-input is unmeasured; `--help`/`--version` are not "
        "substituted for interactive readiness",
    },
    {
        "id": "P2-single-file-edit",
        "reason": "the blueprint's P2 edit workload needs an agent turn (model); not measured",
    },
    {
        "id": "cold-cache-start",
        "reason": "cold page cache cannot be produced without drop_caches; repeats are warm",
    },
    {
        "id": "P5-long-output",
        "reason": "a huge single tool output needs an agent turn to produce it; the model-free "
        "surfaces expose no way to inject one, so P5 is not measured here",
    },
    {
        "id": "P6-compaction",
        "reason": "repeated compaction needs a long model conversation; measuring the projector "
        "alone would not be the P6 workload the blueprint describes",
    },
    {
        "id": "P7-end-to-end",
        "reason": "fixed-model full success cost and human interventions need a real provider run; "
        "that is the M1-06 pilot, not this package",
    },
]


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def run_capture(argv, timeout=20):
    try:
        out = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, check=False)
    except (OSError, subprocess.SubprocessError) as exc:
        return {"error": str(exc)}
    return {"exit": out.returncode, "stdout": out.stdout.strip().splitlines()[:4]}


def git_identity(repo):
    return {
        "repo": repo,
        "commit": run_capture(["git", "-C", repo, "rev-parse", "HEAD"]),
        "tree": run_capture(["git", "-C", repo, "rev-parse", "HEAD^{tree}"]),
        "branch": run_capture(["git", "-C", repo, "rev-parse", "--abbrev-ref", "HEAD"]),
        "dirty_paths": run_capture(["git", "-C", repo, "status", "--porcelain"]),
    }


def hardware_identity():
    cpu_model = None
    mem_total_kb = None
    try:
        with open("/proc/cpuinfo", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                if line.startswith("model name"):
                    cpu_model = line.split(":", 1)[1].strip()
                    break
    except OSError:
        pass
    try:
        with open("/proc/meminfo", encoding="utf-8") as fh:
            for line in fh:
                if line.startswith("MemTotal:"):
                    mem_total_kb = int(line.split()[1])
                    break
    except (OSError, ValueError):
        pass
    return {
        "kernel": platform.release(),
        "machine": platform.machine(),
        "cpu_model": cpu_model,
        "cpu_count": os.cpu_count(),
        "mem_total_kb": mem_total_kb,
        "python": platform.python_version(),
    }


def proc_ppid_map():
    ppid = {}
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        try:
            with open("/proc/%s/stat" % name, "rb") as fh:
                data = fh.read()
        except OSError:
            continue
        tail = data.rsplit(b")", 1)[-1].split()  # comm may contain spaces/parens
        if len(tail) < 2:
            continue
        try:
            ppid[int(name)] = int(tail[1])
        except ValueError:
            continue
    return ppid


def process_tree(root):
    children = {}
    for pid, parent in proc_ppid_map().items():
        children.setdefault(parent, []).append(pid)
    tree, queue = [], [root]
    while queue:
        pid = queue.pop()
        if pid in tree:
            continue
        tree.append(pid)
        queue.extend(children.get(pid, ()))
    return tree


def pid_memory(pid):
    """(rss_kb, pss_kb); pss is None when smaps_rollup is unreadable."""
    rss = pss = None
    try:
        with open("/proc/%d/smaps_rollup" % pid, encoding="utf-8") as fh:
            for line in fh:
                if line.startswith("Rss:"):
                    rss = int(line.split()[1])
                elif line.startswith("Pss:"):
                    pss = int(line.split()[1])
                if rss is not None and pss is not None:
                    break
    except (OSError, ValueError):
        pass
    if rss is None:
        try:
            with open("/proc/%d/status" % pid, encoding="utf-8") as fh:
                for line in fh:
                    if line.startswith("VmRSS:"):
                        rss = int(line.split()[1])
                        break
        except (OSError, ValueError):
            pass
    return rss, pss


class TreeSampler(threading.Thread):
    def __init__(self, root_pid, interval_s):
        super().__init__(daemon=True)
        self.root_pid = root_pid
        self.interval = interval_s
        self.stop_event = threading.Event()
        self.samples = 0
        self.peak_rss_kb = 0
        self.peak_pss_kb = 0
        self.peak_procs = 0
        self.pss_missing = False

    def run(self):
        while not self.stop_event.is_set():
            rss_total = pss_total = live = 0
            for pid in process_tree(self.root_pid):
                rss, pss = pid_memory(pid)
                if rss is None:
                    continue
                live += 1
                rss_total += rss
                if pss is None:
                    self.pss_missing = True
                else:
                    pss_total += pss
            if live:
                self.samples += 1
                self.peak_rss_kb = max(self.peak_rss_kb, rss_total)
                self.peak_pss_kb = max(self.peak_pss_kb, pss_total)
                self.peak_procs = max(self.peak_procs, live)
            self.stop_event.wait(self.interval)

    def stop(self):
        self.stop_event.set()
        self.join(timeout=5)


class Child:
    """posix_spawn child in its own session; stdin writable, stdout/stderr read by line."""

    def __init__(self, binary, argv, env):
        stdin_r, self.stdin_w = os.pipe()
        self.out_r, out_w = os.pipe()
        self.err_r, err_w = os.pipe()
        actions = [
            (os.POSIX_SPAWN_DUP2, stdin_r, 0),
            (os.POSIX_SPAWN_DUP2, out_w, 1),
            (os.POSIX_SPAWN_DUP2, err_w, 2),
        ]
        self.started = time.perf_counter()
        self.pid = os.posix_spawn(binary, [binary, *argv], env, file_actions=actions, setsid=True)
        os.close(stdin_r)
        os.close(out_w)
        os.close(err_w)
        self.out_buf = b""
        self.err_buf = b""
        self.out_eof = False
        self.err_eof = False
        self.reaped = None

    def send(self, line):
        try:
            os.write(self.stdin_w, (line + "\n").encode())
        except OSError:
            pass

    def close_stdin(self):
        if self.stdin_w is not None:
            try:
                os.close(self.stdin_w)
            except OSError:
                pass
            self.stdin_w = None

    def _pump(self, timeout):
        fds = [fd for fd, eof in ((self.out_r, self.out_eof), (self.err_r, self.err_eof)) if not eof]
        if not fds:
            return False
        ready, _, _ = select.select(fds, [], [], timeout)
        for fd in ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                chunk = b""
            if not chunk:
                if fd == self.out_r:
                    self.out_eof = True
                else:
                    self.err_eof = True
                continue
            if fd == self.out_r:
                self.out_buf += chunk
            else:
                self.err_buf += chunk
        return True

    def wait_for_line(self, predicate, deadline):
        """Returns (line, seconds_since_start) or (None, None) once the deadline passes."""
        while True:
            while b"\n" in self.out_buf:
                line, self.out_buf = self.out_buf.split(b"\n", 1)
                if predicate(line):
                    return line, time.perf_counter() - self.started
            remaining = deadline - time.perf_counter()
            if remaining <= 0:
                return None, None
            if not self._pump(min(remaining, 0.05)) and self.out_eof:
                return None, None

    def drain(self, deadline):
        while time.perf_counter() < deadline:
            if not self._pump(0.02) and self.out_eof and self.err_eof:
                return True
        return False

    def signal_tree(self, sig):
        try:
            os.killpg(self.pid, sig)
        except OSError:
            pass

    def reap(self, deadline):
        """Bounded reap: WNOHANG polling, SIGKILL at the deadline, never blocks forever."""
        if self.reaped is not None:
            return self.reaped
        while True:
            wpid, status, usage = os.wait4(self.pid, os.WNOHANG)
            if wpid == self.pid:
                code = -os.WTERMSIG(status) if os.WIFSIGNALED(status) else os.WEXITSTATUS(status)
                self.reaped = (code, int(usage.ru_maxrss) if usage is not None else None)
                return self.reaped
            if time.perf_counter() >= deadline:
                self.signal_tree(signal.SIGKILL)
                end = time.perf_counter() + 5.0
                while time.perf_counter() < end:
                    wpid, status, usage = os.wait4(self.pid, os.WNOHANG)
                    if wpid == self.pid:
                        code = (
                            -os.WTERMSIG(status) if os.WIFSIGNALED(status) else os.WEXITSTATUS(status)
                        )
                        self.reaped = (
                            code,
                            int(usage.ru_maxrss) if usage is not None else None,
                        )
                        return self.reaped
                    time.sleep(0.02)
                self.reaped = (None, None)
                return self.reaped
            time.sleep(0.01)

    def close_fds(self):
        for fd in (self.stdin_w, self.out_r, self.err_r):
            if fd is not None:
                try:
                    os.close(fd)
                except OSError:
                    pass
        self.stdin_w = None


def build_p3_workspace(out_dir, files):
    """A synthetic large tree: `files` modules, each importing three neighbours.

    Deterministic content, so repeats are comparable and the P3 query has a real answer.
    The tree is generated under the evidence directory, never inside the measured repo.
    """
    root = os.path.join(out_dir, "_p3_workspace")
    if os.path.isdir(root):
        shutil.rmtree(root)
    src = os.path.join(root, "src")
    os.makedirs(src)
    for i in range(files):
        body = "".join(
            "use crate::generated_entry_%d;\n" % ((i + k) % files) for k in (1, 2, 3)
        )
        body += "pub fn generated_entry_%d(x: usize) -> usize { x + %d }\n" % (i, i)
        with open(os.path.join(src, "generated_%05d.rs" % i), "w", encoding="utf-8") as fh:
            fh.write(body)
    return root


def run_concurrent(case, binary, env, workspace, timeout, interval, logdir, repeat, order, sessions):
    """Run `sessions` identical model-free sessions at once and report the aggregate.

    Each session is one `kroma acp` process doing initialize + workspace.graph, so the
    aggregate is the cost of N sessions rather than N times the cost of one.
    """
    from concurrent.futures import ThreadPoolExecutor

    with ThreadPoolExecutor(max_workers=sessions) as pool:
        futures = [
            pool.submit(
                run_case, case, binary, env, workspace, timeout, interval, logdir, repeat, order + i
            )
            for i in range(sessions)
        ]
        rows = [future.result() for future in futures]
    walls = [row["wall_ms"] for row in rows if row.get("wall_ms") is not None]
    pss = [row["tree_peak_pss_kb"] for row in rows if row.get("tree_peak_pss_kb") is not None]
    return {
        "case": "p4",
        "repeat": repeat,
        "order": order,
        "argv": ["acp"],
        "sessions": sessions,
        "workspace": workspace,
        "timeout_s": timeout,
        "sample_interval_ms": round(interval * 1000, 2),
        "timed_out": any(row["timed_out"] for row in rows),
        "exit_code": 0 if all(row["exit_code"] == 0 for row in rows) else 1,
        "wall_ms": max(walls) if walls else None,
        "tree_peak_pss_kb": sum(pss) if pss else None,
        "all_graph_ok": all(row.get("graph_ok") for row in rows),
        "per_session": rows,
    }


def run_case(case, binary, env, workspace, timeout, interval, logdir, repeat, order):
    init_request = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}},
        }
    )
    graph_request = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "workspace.graph",
            "params": CASE_PARAMS.get(case, GRAPH_PARAMS),
        }
    )
    if case == "p0":
        argv = ["--version"]
    elif case == "p3":
        # P3 must index the generated tree, not whatever directory this script happens to run in.
        argv = ["acp", "--workspace", workspace]
    else:
        argv = ["acp"]
    record = {
        "case": case,
        "repeat": repeat,
        "order": order,
        "argv": argv,
        "workspace": workspace if case in ("p2", "p3") else None,
        "timeout_s": timeout,
        "sample_interval_ms": round(interval * 1000, 2),
        "timed_out": False,
    }

    child = Child(binary, argv, env)
    sampler = TreeSampler(child.pid, interval)
    sampler.start()
    # One monotonic deadline for the whole run; every wait below is bounded by it.
    deadline = child.started + timeout
    try:
        if case == "p0":
            child.close_stdin()
            child.drain(deadline)
            record["wall_ms"] = round((time.perf_counter() - child.started) * 1000, 3)
            first = child.out_buf.decode("utf-8", "replace").splitlines()[:1]
            record["version_line"] = first[0] if first else None
        else:
            child.send(init_request)
            line, ready_s = child.wait_for_line(
                lambda raw: b'"id":1' in raw.replace(b" ", b""), deadline
            )
            record["init_ok"] = line is not None
            record["init_ms"] = None if ready_s is None else round(ready_s * 1000, 3)
            if line is None:
                record["timed_out"] = True
            else:
                sent = time.perf_counter()
                child.send(graph_request)
                graph_line, _ = child.wait_for_line(
                    lambda raw: b'"id":2' in raw.replace(b" ", b""), deadline
                )
                record["graph_ok"] = graph_line is not None
                record["graph_ms"] = (
                    None if graph_line is None else round((time.perf_counter() - sent) * 1000, 3)
                )
                if graph_line is None:
                    record["timed_out"] = True
                else:
                    payload = json.loads(graph_line.decode("utf-8", "replace"))
                    result = payload.get("result") or {}
                    record["graph_result"] = {
                        "total_files": result.get("total_files"),
                        "total_symbols": result.get("total_symbols"),
                        "ranked": len(result.get("ranked") or []),
                        "query_terms": result.get("query_terms"),
                    }
                child.close_stdin()
                child.drain(deadline)
    finally:
        exit_code, maxrss = child.reap(deadline)
        if exit_code is None:
            record["reap_failed"] = True
        sampler.stop()
        child.close_fds()

    record["exit_code"] = exit_code
    record["main_process_maxrss_kb"] = maxrss
    record["tree_peak_rss_kb"] = sampler.peak_rss_kb or None
    record["tree_peak_pss_kb"] = sampler.peak_pss_kb or None
    record["tree_peak_processes"] = sampler.peak_procs or None
    record["tree_samples"] = sampler.samples
    record["tree_pss_unavailable"] = sampler.pss_missing
    record["stdout_bytes"] = len(child.out_buf)
    record["stdout_sha256"] = hashlib.sha256(child.out_buf).hexdigest()
    record["stderr_bytes"] = len(child.err_buf)

    log_name = "raw-%s-r%d.log" % (case, repeat)
    with open(os.path.join(logdir, log_name), "w", encoding="utf-8") as fh:
        fh.write("# argv=%s exit=%s timed_out=%s\n" % (argv, exit_code, record["timed_out"]))
        fh.write("## stdout\n" + child.out_buf.decode("utf-8", "replace"))
        fh.write("\n## stderr\n" + child.err_buf.decode("utf-8", "replace"))
    record["raw_log"] = log_name
    return record


def _median(values):
    ordered = sorted(values)
    mid = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[mid]
    return round((ordered[mid - 1] + ordered[mid]) / 2, 3)


def summarize(records):
    summary = {}
    for case in CASES:
        rows = [r for r in records if r["case"] == case]
        if not rows:
            continue
        entry = {"repeats": len(rows), "all_exit_zero": all(r["exit_code"] == 0 for r in rows)}
        entry["timed_out"] = any(r["timed_out"] for r in rows)
        for field in ("wall_ms", "init_ms", "graph_ms"):
            values = [r[field] for r in rows if r.get(field) is not None]
            if values:
                entry[field] = {
                    "values": values,
                    "median": _median(values),
                    "min": min(values),
                    "max": max(values),
                }
        for field in ("tree_peak_rss_kb", "tree_peak_pss_kb", "main_process_maxrss_kb"):
            values = [r[field] for r in rows if r.get(field) is not None]
            if values:
                entry[field] = {"values": values, "max": max(values)}
        summary[case] = entry
    return summary


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--kroma", required=True, help="already-built kroma binary to measure")
    parser.add_argument("--out", required=True, help="raw JSON/log directory (keep it outside the repo)")
    parser.add_argument("--repo", default=".", help="workspace used by P2 and identified in the report")
    parser.add_argument("--repeats", type=int, default=3, help="warm repeats per workload (default 3)")
    parser.add_argument("--timeout", type=float, default=60.0, help="per-run deadline in seconds")
    parser.add_argument("--only", default="p0,p2,p3,p4", help="comma separated subset of p0,p2,p3,p4")
    parser.add_argument(
        "--p3-files", type=int, default=2000, help="modules generated for the P3 workspace"
    )
    args = parser.parse_args()

    cases = [c.strip() for c in args.only.split(",") if c.strip()]
    unknown = [c for c in cases if c not in CASES]
    if unknown:
        parser.error("unknown case(s): %s (known: %s)" % (", ".join(unknown), ",".join(CASES)))
    if args.repeats < 1:
        parser.error("--repeats must be >= 1")
    if args.timeout <= 0:
        parser.error("--timeout must be > 0")

    binary = os.path.abspath(args.kroma)
    if not os.access(binary, os.X_OK):
        parser.error("not executable: %s" % binary)
    repo = os.path.abspath(args.repo)
    os.makedirs(args.out, exist_ok=True)

    stat = os.stat(binary)
    env = {
        "PATH": "/usr/local/bin:/usr/bin:/bin",
        "HOME": os.environ.get("HOME", "/root"),
        "LANG": "C.UTF-8",
        "TZ": "UTC",
    }
    intervals = {"p0": 0.002, "p2": 0.010, "p3": 0.010, "p4": 0.010}
    p3_workspace = build_p3_workspace(args.out, args.p3_files) if "p3" in cases else None

    started_at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    records = []
    order = 0
    for repeat in range(1, args.repeats + 1):
        for case in cases:
            order += 1
            workspace = p3_workspace if case == "p3" else repo
            if case == "p4":
                records.append(
                    run_concurrent(
                        "p2",
                        binary,
                        env,
                        repo,
                        args.timeout,
                        intervals[case],
                        args.out,
                        repeat,
                        order,
                        P4_SESSIONS,
                    )
                )
            else:
                records.append(
                    run_case(
                        case,
                        binary,
                        env,
                        workspace,
                        args.timeout,
                        intervals[case],
                        args.out,
                        repeat,
                        order,
                    )
                )

    report = {
        "schema": "kroma.local-performance.v1",
        "started_at": started_at,
        "finished_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "binary": {
            "path": binary,
            "sha256": sha256_file(binary),
            "size_bytes": stat.st_size,
            "mtime_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(stat.st_mtime)),
        },
        "source": git_identity(repo),
        "toolchain": {
            "rustc": run_capture(["rustc", "--version"]),
            "cargo": run_capture(["cargo", "--version"]),
        },
        "hardware": hardware_identity(),
        "method": {
            "repeats": args.repeats,
            "order": "fixed per repeat: %s" % ",".join(cases),
            "per_run_deadline_s": args.timeout,
            "cache_state": "warm (page cache not dropped; cold-cache start unmeasured)",
            "resource_accounting": "sum over the /proc process tree; Rss double counts shared "
            "pages, Pss apportions them; children shorter than one sample are missed; "
            "main_process_maxrss_kb covers the main PID only",
            "thresholds": "none: numbers only, no SLO is set here",
        },
        "records": records,
        "summary": summarize(records),
        "unmeasured": UNMEASURED,
    }

    report_path = os.path.join(args.out, "report.json")
    with open(report_path, "w", encoding="utf-8") as fh:
        json.dump(report, fh, indent=2, sort_keys=True)
        fh.write("\n")
    print(report_path)
    for case, entry in report["summary"].items():
        print(
            "%s repeats=%d exit_zero=%s timed_out=%s %s"
            % (
                case,
                entry["repeats"],
                entry["all_exit_zero"],
                entry["timed_out"],
                json.dumps(
                    {
                        k: v
                        for k, v in entry.items()
                        if k in ("wall_ms", "init_ms", "graph_ms", "tree_peak_rss_kb", "tree_peak_pss_kb")
                    },
                    sort_keys=True,
                ),
            )
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
