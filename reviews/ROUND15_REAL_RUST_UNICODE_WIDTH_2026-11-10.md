# Round 15 — Real Rust Historical PR / Unicode Width (2026-11-10)

## Decision

**Evidence-based finishing remains an experimental option, not a proven universal winner. Do not merge it into production defaults yet.** In the first authentic Rust task the candidate reached independent correctness once in two runs, baseline reached none, but **both arms failed to normally finish in both attempts at 185 seconds**. This fails the pre-specified strict success metric for both variants and reveals real Rust code exploration remains a major bottleneck.

## Historical provenance

- Upstream repository: https://github.com/unicode-rs/unicode-width
- Real merged PR: https://github.com/unicode-rs/unicode-width/pull/89
- Pinned broken upstream base commit: `d8654db078bf9f93b5bfe1e6294a477cc4addc28`.
- Defect: Halfwidth katakana voiced/semi-voiced symbols U+FF9E/U+FF9F are incorrectly treated as zero width, causing `ﾊﾟｸﾞ` to render width 2 instead of 4.
- Real original implementation includes both Rust generated width tables and a Python Unicode generator. Both were pulled from the specified historical commit, with original contents stored at `benchmarks/round15_unicode_sources/`.
- Task asked agent to correct public widths and keep generator source-of-truth consistent.

## Offline host adaptations and their limits

The Windows host could not resolve the historical manifest's two optional `rustc-std-workspace-{std,core}` packages offline. Experiment copy's Cargo.toml removed **only these optional non-default dependencies** (also cleared their unused `rustc-dep-of-std` feature mapping). Historical source code, original generated runtime tables and generator were preserved.

The upstream `src/gen/tables_test.rs` generated unit-test blob was 1.36 MB and could not be retrieved via the GitHub connector; to compile on this host, the experiment copy disabled inclusion of the historical internal `src/test.rs` module in `src/lib.rs`. This changes *test coverage* but not runtime functionality. Therefore this is a **reduced authentic-source replay**, not a fully intact upstream checkout or CI reproduction. Evaluation used independent public and held-out integration test files; the latter were injected only **after each agent process exited**, and the generator consistency was checked. Do not claim untouched full Rust test suite passed.

Control in clean offline target: public integration tests **passed** before repair; hidden behavioral tests **failed for intended behavior**:
- `U+FF9E`: observed Some(0), expected Some(1).
- `ﾊﾟｸﾞ`: observed width 2, expected width 4.

The experiment's several failed setup attempts were **not paid agent runs** and must not be included in model A/B scores. The valid run directory is `D:/Kroma/experiments/kroma-round15-rust-real-issues-v6`.

## Live A/B evidence

One same research Kroma release binary; same existing DeepSeek v4.1 Flash High endpoint, model, shell tool name, output format, context policy. Only selected execution-guidance prompt differs. Two repetitions reversed arm order. Each arm had a separate Cargo build target and public-test prewarming before model timing; wall cap 185 seconds. Independent end-of-run behavioral + generator checks.

| Repeat | Arm | Strict finish | Independent correctness | Agent test invoked | Tools | First edit tool # | Total wall |
|---|---|---|---|---|---:|---:|---|
| 1 | baseline | no, timeout | **fail** | no | 18 | none | ≥185.125s |
| 1 | evidence-finish | no, timeout | **pass** | yes | 36 | 18 | ≥185.093s |
| 2 | evidence-finish | no, timeout | **fail** | no | 27 | 16 | ≥185.078s |
| 2 | baseline | no, timeout | **fail** | yes | 34 | 33 | ≥185.109s |

Only 1/4 source snapshots met the independent functional *and generator* acceptance. **Neither arm demonstrated a full correct, tested, normally finished run within cap.** Cannot claim latency advantage or calculate full token comparisons for censored trajectories.

Important trajectory pattern: baseline in repeat 1 used 17 pre-edit inspection calls and never edited; in repeat 2 first edit came at tool call #33. Candidate in repeat 1 read source repeatedly (30 inspection calls total) but achieved correct final code, still continued until deadline. The dominant remaining challenge is deep Rust investigation + compiling/generated data workflow, not merely an insufficient green-test stop instruction.

## Context from prior rounds

- Round 13 synthetic/fresh four-task Node+Rust portfolio: candidate 16/16 strict, baseline 14/16; all 32 final snippets rechecked, 32/32 code accepted. The total observed/capped time was ~27% lower with candidate but contains 2 baseline timeouts.
- Round 14 real-source JavaScript historical PRs (p-map #88/#91 and p-limit #108/#109): candidate 8/8 strict, baseline 7/8; all 16 independent rechecks passed. Candidate time wins 5/8, losses 3/8. These are **reduced-source** authentic PRs, not fully cloned and tested upstream CI.
- This Round 15 adds a **true Rust bug** but displays significant failures and no strict completion. Pooling these unlike tasks into a universal significance claim would be misleading.
- Round 12 older Unicode session-preview A/B was invalid due missing system-message fixture. Subsequent repaired *negative control* showed intended Unicode fail, but old A/B was never re-run. Do not use it for candidate safety claims.

## Quality checks and next decision

The benchmark runner's preflight negative control succeeded; model run logs and WAL saved in dedicated lab per arm. The research worktree's Kroma Rust unit tests previously passed **783 / 0 failed / 11 ignored** with an isolated Cargo target; CLIPPY and formatter checked earlier. New experiment did not change production main or defaults.

**No prompt merge.** Best next investment is either (a) a new genuinely independent Rust PR with medium complexity and a trustworthy offline harness, or (b) investigating the first-edit and repeated source-inspection path in Round 15. Stop treating "finish sooner" prompting as a complete solution to deep Rust planning/search inefficiency. Keep experiments default off. Do not add auto-stop, planner/verifier, or new approval mechanisms without direct evidence.
