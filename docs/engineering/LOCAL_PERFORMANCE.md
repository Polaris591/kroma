# Local performance baseline (C00-01)

Status: **measurement only**. No SLO threshold, no gate, no claim of "leading" or
"excellent". Unmeasured workloads are listed as gaps, not as passes.

Blueprint reference: `design/Kroma_Blueprint_v4.0/KROMA_MASTER_BLUEPRINT_v4.0_20261018.md`
§6.2 (three cost views), §6.3 (workload catalogue P0..P7), §6.4 (record toolchain,
hardware, profile, input scale, warm/cold state, repeat order and noise before any
threshold). Contracts: INV-04 (complete resource accounting, parents and children),
INV-14 (evidence levels not mixed; skipped is not passed).

Script: `scripts/measure-local-baseline.py` (stdlib only, no model, no network).

## Measured tree and binary identity

| Field | Value |
|---|---|
| Repo | `/opt/kroma-v4/source`, branch `integration/v4` |
| Commit | `93133a40918711607f2615aefdb5a322b9ff5ebc` |
| Tree | `b0a3b38efef6e1186610dc18c619f26de2e384af` |
| Untracked at build time | `scripts/measure-local-baseline.py` (this package only) |
| Binary | `/opt/kroma-p1/target/release/kroma` |
| Binary sha256 | `344efc95bb6ba69b16e44a9e0a6e08f2fbe73f8b77a9df2aaca89c20a43cf1dc` |
| Binary size / mtime | 16 358 920 bytes, built from this tree |
| Build | `CARGO_TARGET_DIR=/opt/kroma-p1/target CARGO_INCREMENTAL=0 cargo build -p kroma-cli --release --offline --locked`, exit 0, 1m06s |
| Build log | `/opt/kroma-v4/evidence/local-performance/build.log` |

The build log shows `Compiling kroma-core v1.5.0 (/opt/kroma-v4/source/crates/kroma-core)`
and `Compiling kroma-cli v1.5.0 (/opt/kroma-v4/source/crates/kroma-cli)`, so the
measured binary is linked from this tree, not from a stale cache. No prebuilt binary
was trusted: `/usr/local/bin/kroma` (sha256 `c03b6790…`, mtime 2026-10-14) predates this
tree and was only used for a feasibility probe, never as a measurement subject.

Side effect, recorded for honesty: `/opt/kroma-p1/target` is shared with another
checkout (`/opt/kroma-p1` at `ce50fb57…`). Before this build that path held
`kroma` sha256 `239773c564c8691225e71caf9fc84d18925c269f6c643a20813b28aee69ebbff`
(built from the other tree, 2026-10-18T16:56 local). This build replaced that artifact.
No `cargo clean` and no file deletion was run; disk headroom was checked first
(4.86 GB free, above the 2 GB floor).

## Environment

| Field | Value |
|---|---|
| CPU | AMD EPYC 7K62, 4 vCPU visible (`nproc` = 4), KVM guest |
| RAM | 3 818 804 kB total |
| Kernel | 6.12.95+deb13-amd64, x86_64 |
| Toolchain | rustc 1.85.1 (4eb161250 2025-03-15), cargo 1.85.1 (d73d2caf9 2024-12-31), pinned by `rust-toolchain.toml` |
| Build profile | `release` (opt-level default for release, no LTO/panic tweaks in `Cargo.toml`) |
| Python (harness) | 3.13.5 |
| `/usr/bin/time` | **absent** on this host; peak values come from /proc sampling plus `wait4` rusage for the main PID |

## Workloads

| ID | Command | Why | Status |
|---|---|---|---|
| P0 | `kroma --version` | process load cost only; explicitly **not** an interactive startup | measured |
| P1 | cold start → can accept input | needs an explicit model-free readiness signal | **UNMEASURED** |
| P2 | `kroma acp` + JSON-RPC `workspace.graph` (`{"query":"process supervisor","max_nodes":20}`) | real static symbol-graph navigation over this repo's sources; model-free | measured |
| P3 | `kroma acp --workspace <generated tree>` + `workspace.graph` | first index of a generated 2000-module tree; model-free | measured, **at the index file cap** |
| P4 | 4 concurrent `kroma acp` sessions, each one `workspace.graph` | concurrency cost against the same repository | measured |

P1 gap: the ACP `initialize` round trip is model-free and was used as the readiness
signal in an earlier attempt, but that attempt hung (see "Harness defects found and
fixed") and this package did not re-establish a reliable readiness signal inside its
budget. `--help` and `--version` are **not** substituted for interactive readiness, and
no TUI/REPL startup is reported. P1 remains a gap.

P2 input scale (identical in all repeats): `total_files = 189`, `total_symbols = 4634`,
`ranked = 20` nodes returned for query terms `["process","supervisor"]`.

P3 input scale: 2000 generated modules written under the evidence directory (never inside
this repo). The graph reports `total_files = 1200`, i.e. **the file cap stopped the index
before the corpus was exhausted**; the figure describes indexing *at the cap*, not
"2000 files indexed". Because the generated modules are tiny and the query is a single
exact symbol, P3's 34.5 ms is **not** comparable to P2's 291 ms as "more files, less time":
different query selectivity and file sizes. The honest reading is "a 1200-file index of
small modules costs ~35 ms", not "indexing scales flat".

P4: 4 sessions, all `graph_ok`. Per-session `graph_ms` 296–484 ms against 291 ms solo, and
the summed process-tree PSS 174–177 MB against 48 MB solo (≈3.7×, not 4×, because shared
pages are counted once per session).

## Method

- Repeats: 3 warm repeats per workload, fixed order `p0,p2` per repeat, recorded as
  `order` in every raw record.
- Warm only: the page cache is **not** dropped (`drop_caches` is not used), so cold-start
  is unmeasured. A cold run would need host-level control this package does not have.
- Per-run deadline: 30 s (`--timeout 30`), enforced as one monotonic deadline for all
  waits; on expiry the process group receives SIGTERM/SIGKILL and reaping is bounded.
- Resource accounting is the **process tree** discovered under `/proc`
  (parent → children → grandchildren), summed per sample:
  - `Rss`: resident pages per process. Shared pages (libc, mapped files) are counted
    again in every process that maps them, so a tree sum double counts shared memory.
  - `Pss`: `/proc/<pid>/smaps_rollup` proportional set size; shared pages are divided by
    the number of sharers, so a tree sum approximates the tree's real footprint.
  - `main_process_maxrss_kb`: `wait4` rusage, main PID only — reported for comparison,
    never used as the tree figure.
  - Known blind spots: children that fork and exit between two samples are invisible
    (P0 samples every 2 ms, P2 every 10 ms); RSS/PSS do not include page cache the
    process faults in on behalf of the filesystem.
- Noise control: this package ran nothing else concurrently, but the host is shared — a
  foreign test binary from another checkout (`/opt/kroma-ds-build/debug/deps/bench_contract-…`)
  was running during these runs. It is not part of this package and its effect on the
  numbers is not quantified, so the figures above are single-host, contended-machine
  samples. With 3 points no tail statistics (p95/p99) are claimed.

A pre-fix run directory `run-20261018T130415Z` is retained as defect evidence (P2 timed
out three times because the initialize request was never written). It is **not** a
measurement and must not be quoted as one.

## Results (run `run-20261018T130744Z`)

Raw data: `/opt/kroma-v4/evidence/local-performance/run-20261018T130744Z/report.json`
plus `raw-p0-r{1,2,3}.log`, `raw-p2-r{1,2,3}.log` (full stdout/stderr per repeat).

P0 `kroma --version` (exit 0 in all 3, stdout `kroma 1.5.0`):

| repeat | wall ms | tree peak RSS kB | tree peak PSS kB | main maxrss kB | tree samples |
|---|---|---|---|---|---|
| 1 | 3.782 | 5 320 | 3 094 | 18 688 | 1 |
| 2 | 3.470 | 2 988 | 1 652 | 18 948 | 1 |
| 3 | 4.355 | 5 692 | 3 199 | 18 948 | 1 |

Median wall 3.78 ms (min 3.47, max 4.36). Tree sampling at 2 ms only catches 0–1 samples
for a 4 ms process, so the RSS/PSS columns for P0 are weak evidence; `main maxrss`
(~18.7 MB) is the more stable number and covers the main PID only.

P2 `kroma acp` + `workspace.graph` (exit 0 in all 3):

| repeat | initialize ms | graph ms | tree peak RSS kB | tree peak PSS kB | tree samples |
|---|---|---|---|---|---|
| 1 | 7.788 | 288.52 | 50 352 | 47 628 | 24 |
| 2 | 5.980 | 296.18 | 49 776 | 47 052 | 25 |
| 3 | 6.860 | 309.44 | 50 308 | 47 584 | 25 |

Median graph query 296.2 ms (min 288.5, max 309.4), spread ≈ 7 % of the median over 3
warm repeats — too few points to separate noise from variance. Peak tree footprint
≈ 50.3 MB RSS / 47.6 MB PSS for a 189-file, 4 634-symbol workspace. The RSS/PSS gap is
the shared-page double counting described above.

No comparison, ratio or "faster than X" statement is made: there is no second binary,
profile or baseline in this package to compare against.

## Harness defects found and fixed

1. The first script version waited for the ACP `initialize` response without ever
   writing the request to stdin, so it blocked until the harness deadline and then
   blocked again in an unbounded `wait4`. That run was killed by the controller; nothing
   was measured from it.
2. Fixed by (a) sending the `initialize` request before waiting, and (b) replacing all
   unbounded waits with one per-run monotonic deadline, bounded `WNOHANG` reaping, and
   SIGTERM→SIGKILL process-group cleanup in `finally`.
3. Failure probe (`/opt/kroma-v4/evidence/local-performance/fault-probe/`): a stub
   "binary" that ignores stdin and sleeps 300 s, measured with `--timeout 5`. The script
   returned in 5 s with `timed_out=true`, `exit_code=-9`, `init_ok=false`, no leftover
   child process. This is the evidence that a hung workload can no longer hang the
   measurement.

## Gaps (do not read these as passes)

- P1 startup-to-accepting-input: unmeasured (no reliable model-free readiness signal).
- Cold-cache start: unmeasured (no `drop_caches`).
- P2 single-file edit: needs an agent turn (model).
- P5 long output, P6 repeated compaction, P7 fixed-model end-to-end cost: need a model
  conversation or a real provider run; the model-free surfaces cannot inject one. P7 is the
  M1-06 pilot's workload, not this package's.
- P3 stops at the index file cap (1200 of 2000 generated modules), so the cost *past* the
  cap is unmeasured.
- Peak-memory figures for P0 are undersampled; no p95/p99 for anything.
- Windows and macOS: not measured here; the script is Linux-only (`/proc`).

## Reproduce

```sh
# once, if the release binary does not exist yet (disk headroom required first)
CARGO_TARGET_DIR=/opt/kroma-p1/target CARGO_INCREMENTAL=0 \
  cargo build -p kroma-cli --release --offline --locked

python3 scripts/measure-local-baseline.py \
  --kroma /opt/kroma-p1/target/release/kroma \
  --out /opt/kroma-v4/evidence/local-performance/run-$(date -u +%Y%m%dT%H%M%SZ) \
  --repo /opt/kroma-v4/source --repeats 3 --timeout 30
```

The script prints the report path and one summary line per workload; the report records
binary digest, commit/tree, toolchain, hardware, method, every raw record and the
`unmeasured` block. Exit code is 0 even when a workload times out — a timeout is data
(`timed_out: true`, `exit_code` reflects the signal), not a pass.
