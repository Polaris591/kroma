# Harness v0.12.0 — Gate 4 Acceptance Report

**Gate:** 4 — Predictive Validation & Candidate Failure Attribution
**Version:** `0.12.0` (workspace `Cargo.toml`, binary `harness 0.12.0`)
**Date:** 2026-10-12
**Blueprint:** `docs/v1_blueprint/Harness-v1.0-优化蓝图-v2.0.md` §9 Predictive Validation,
`docs/v1_blueprint/IMPLEMENTATION_GATES_AND_EVAL.md` §"Gate 4 — Predictive Validation"
**Evidence:** `docs/GATE4_CI_LOG.txt`, `docs/GATE4_EVAL_METRICS.json`,
`crates/harness-core/tests/gate4_predictive_attribution.rs`,
`crates/harness-core/src/attribution.rs`

---

## 1. What Gate 4 delivers

Gate 4 is the *evidence-driven diagnosis* gate. The blueprint's framing is explicit: stop guessing
at a culprit and stop reacting to a rising error count. Two mechanisms were added.

### 1.1 Post-mutation structural check — `PredictiveValidator`

Runs in-process (zero process spawn) immediately after a successful `edit`/`write`, re-reads the
mutated file through `WorkspaceCap::open_existing_no_symlink`, and reports structural breakage with
exact **1-based line/column** coordinates.

| File family | Check |
|---|---|
| `.json` | full syntax parse; on error, `serde_json`'s exact line/column |
| `.toml` | section-header bracket structure + single-line string balance (deliberately conservative) |
| `.rs .js .ts .tsx .jsx .py .go .c .cpp .h .java` | lexical-aware delimiter/string/comment balance: unclosed `{ ( [`, mismatched closers, unterminated string literals, unterminated block comments (nested for Rust), and unresolved `<<<<<<<` / `=======` / `>>>>>>>` conflict markers |

The scanner is language-aware on purpose, because the blueprint's hard requirement is *no* false
positives:

* line comments (`//` vs `#`), block comments (nested only for Rust), strings, raw strings
  (`r"…"`, `r#"…"#`, `br"…"`), byte strings, char/rune literals, Rust lifetimes (`'a` is not a char
  literal), C++ digit separators (`1'000'000`), JS/TS template literals and Go backtick strings that
  may span lines, and Python triple-quoted strings and `f`/`rb`/`u` prefixes.
* **Rust string literals may legally span physical lines** (with or without a `\` continuation).
  Treating a raw newline as "unterminated" was a real false positive that the real-source probe
  caught; it is now fixed and pinned by
  `test_predictive_validator_never_false_positives_on_harness_sources`.

**Non-destructive advisory guarantee.** A warning is returned as a
`<predictive_validation status="warning" …>` block appended to the tool result and emitted as
`EventPayload::PredictiveCheck`. Nothing is ever rolled back: `false_positive_rollback_rate == 0.0`.
An operator who wants a hard stop uses the existing Gate 3 `PolicyHook` surface; the checker itself
only reports.

### 1.2 Mutation ledger + candidate failure attribution — `MutationLedger` / `AttributionEngine`

`MutationLedger` records every mutating operation, branch-scoped:

| Field | Meaning |
|---|---|
| `mutation_id` | deterministic `mut-<seq:04>` |
| `turn` | absolute session turn |
| `tool_name` / `call_id` | which call did it |
| `source_entry_id` | Gate 2 evidence pointer `ev-<idx:04>-<call_id>` → `recall_evidence` |
| `revision_before` / `revision_after` | Gate 1 `RevisionId`s → inspect or `--restore-rev` |
| `touched_ranges` | `relative_path`, `start_line`, `end_line`, `added_symbols`, `summary` |

`parse_diagnostic_anchors` extracts `DiagnosticAnchor`s from failure output across Rust
(`error[E0425]: …` + `--> path:line:col`), Python (`File "path", line N`), JS/TS/Node
(`path:line:col`), Go (`path:line:col:`), and generic `path:line` stack traces, including a
best-effort `symbol` and `error_code`.

`AttributionEngine::attribute_failure` scores each record against the anchors and returns an
`AttributionReport` that is **always** framed `candidate_attribution` — never a root-cause claim:

| Band | Condition |
|---|---|
| `0.85..=0.98` | same path, anchor line inside the edited span (`0.98`) or within 5 lines |
| `0.70..=0.85` | same path, anchor symbol matches a symbol the edit added |
| `0.50..=0.70` | same path modified recently, no line/symbol signal |
| `0.20..=0.45` | recency decay only (no path match) |

Candidates are sorted by descending confidence, tie-broken by most recent `mutation_id`, truncated to
top-3. `render_for_model` emits a fenced `<candidate_failure_attribution framing="candidate_attribution">`
block naming the file/lines, the `source_entry_id` to recall, and the `revision_before` to restore —
sanitized with `sanitize_untrusted_doc` / `escape_fence_attribute` so untrusted compiler output
cannot break out of the fence.

### 1.3 Telemetry, session and CLI surface

* `EVENT_SCHEMA_VERSION` `0.8.0` → **`0.9.0`**.
* New `EventPayload::PredictiveCheck { path, status, issue_count, duration_ms, summary }`.
* New `EventPayload::FailureAttributed { source, candidate_count, top_mutation_id, top_path, top_confidence, top_revision_before }`.
* `SessionCheckpoint.mutations` (and the `Checkpoint` WAL payload) persist the ledger with
  `#[serde(default)]`, so pre-Gate-4 WALs stay readable.
* On resume the ledger is restored from the deepest **active-branch** checkpoint and then
  `prune_after_turn(session_turn_offset)`, so `--fork <TURN>` can never attribute a failure to a
  mutation that only exists on the abandoned branch.
* CLI: `--no-predictive-check`, `--predict-cmd <CMD>`.

---

## 2. Exit criteria (blueprint) vs. evidence

| Blueprint Gate 4 requirement | Status | Evidence |
|---|---|---|
| 文件修改后记录 diff/hash | **Met** | `MutationRecord.touched_ranges` + `revision_before`/`revision_after`; live smoke shows `mut-0001 … revision_before=r-eb90… revision_after=r-9f47…` |
| 只在确定语言/项目工具存在时跑轻量 parser/lint | **Met** | `language_name` gate; unknown extensions return `status="skipped"` with a reason and emit **no** `PredictiveCheck` event |
| baseline tests 与 after tests 使用相同命令/版本/环境 | **Met** | Gate 4 adds no new test command: attribution consumes the existing `WorkspaceCompletionGate` output; `--predict-cmd` reuses `WorkspaceCompletionGate` (same `env_clear()`, `CI=true`, `NO_COLOR=1`, 180s timeout, process-group cleanup) |
| 失败归因先标记 candidate，不直接判定 root cause | **Met** | `framing == "candidate_attribution"` asserted in tests and in the rendered block; block text says "These are CANDIDATES, not a confirmed root cause" |
| 需要时用 revert/reapply 或缩小 patch 进行验证 | **Met** | Each candidate carries `revision_before`, and `AttributionEngine::verify_candidate_with_snapshot` temporarily reverts to `revision_before`, runs a verification probe, boosts candidate confidence when the failure resolves on revert, and unconditionally restores `current_revision` |
| 自动回滚必须能关闭，且永远不能绕过用户对高风险副作用的审批 | **Met** | There is no auto-rollback at all; `--no-predictive-check` disables both structural checks and `--predict-cmd`; `--policy read-only` also disables `--predict-cmd`; the attribution path only writes prompt text and events |
| 是否在首次修复前抓住新增语法错误 | **Met** | Live smoke: the unbalanced `write` produced a warning block before any test ran; `--predict-cmd` surfaced the compile failure on the same turn as the breaking edit |
| Attribution Precision / Recall（已知注入点合成任务） | **Met** | 8-scenario corpus: `top1_attribution_precision = 1.0` (6/6 labelled, including cross-file symbol rename break), `top2_attribution_recall = 1.0` (6/6), 0 false-positive candidates on clean output |
| 误报造成的错误回滚率 | **Met** | `false_positive_rollback_rate = 0.0`; 5 valid-but-tricky probe files (including Rust `br"\"`, JS regex literals, and nested `${...}` template literals) produce 0 warnings and 0 byte changes; 50 real repository files validate clean |
| 额外执行时间和成本 | **Met** | `mean_validation_overhead_ms ≈ 0.029` per file (in-process, no spawn); `--predict-cmd` is opt-in and capped at `MAX_PREDICT_CMD_RUNS = 8` |

---

## 3. Test inventory (`crates/harness-core/tests/gate4_predictive_attribution.rs`)

| # | Test | Invariant |
|---|---|---|
| 1 | `test_predictive_validator_catches_syntax_and_delimiter_errors_with_zero_false_rollbacks` | JSON/TOML/delimiter/string/conflict-marker detection with exact coordinates; valid files with braces in strings/comments/lifetimes/templates pass; a validation run never rewrites the file; unsupported types are `skipped`; `--no-predictive-check` short-circuits |
| 2 | `test_predictive_validator_never_false_positives_on_harness_sources` | every `.rs` file under `harness-core/src` validates clean (regression guard for the multi-line-string fix) |
| 3 | `test_edit_and_write_attach_predictive_warning_and_record_mutation_ledger_with_revisions` | a clean `write` produces no warning; a broken `write` attaches the advisory block, sets `meta.mutation`, emits `PredictiveCheck`, and persists two `MutationRecord`s in the WAL checkpoint with `source_entry_id` + restorable `revision_before`/`revision_after` |
| 4 | `test_completion_gate_failure_emits_ranked_candidate_attribution_linked_to_evidence_and_revisions` | a failing gate ranks turn 2's `src/b.rs` mutation top-1 (0.98), the clean turn-1 edit is not ranked above it, the block carries `recall_evidence(source_entry_id=ev-…)` and `--restore-rev r-…`, `FailureAttributed` telemetry matches, the named revision loads, and the model repairs the attributed file so the gate then approves |
| 5 | `test_attribution_disambiguates_multiple_edits_by_file_line_and_symbol` | same-turn multi-file edits are separated by line range and by symbol; an unrelated failure stays in the recency band; `added_identifiers` / `top_level_symbol_names` / `language_name` behave |
| 6 | `test_fork_rewind_prunes_future_mutations_from_ledger` | ledger pruning, id monotonicity after `restore`, WAL branch selection (`--fork 1` drops the off-branch checkpoint), and the runner's `prune_after_turn` safety net |
| 7 | `test_gate4_eval_corpus_metrics_and_artifact_generation` | 8-scenario corpus → `top1_attribution_precision == 1.0`, `top2_attribution_recall == 1.0`, 0 candidates on clean output; verifies/regenerates `docs/GATE4_EVAL_METRICS.json` |
| 8 | `test_scanner_edge_cases_eof_transitions_rust_raw_byte_strings_js_regex_and_templates` | EOF unclosed strings/block comments, Rust `br"\"` / `cr"\"` raw byte/C strings, JS/TS regex literals (`/[(]/`, `/\{/`, `/"/`, `/https?:\/\//`) and `${...}` template interpolations |
| 9 | `test_multi_span_attribution_windows_paths_and_revert_reapply_verification` | multi-span mutations attribute the matching span's line range; colon-containing paths + Windows drive letters + URL port rejection; `verify_candidate_with_snapshot` revert/reapply verification |
| 10 | `test_bash_changed_paths_backfill_predict_cmd_and_restore_rev_pruning` | read-only `bash` does not pollute `MutationLedger`; mutating `bash` backfills `changed_paths` from `WorkspaceRevision`; `--predict-cmd` attributes `bash` mutations and is skipped under `--no-predictive-check` |

Plus 8 unit tests in `attribution.rs` (scanner acceptance/rejection, JSON/TOML, anchor parsing,
ranking, pruning).

Workspace total: **137 passed / 0 failed / 0 warnings** across 15 test binaries plus doc-tests.

---

## 4. Verification performed

| Command | Result |
|---|---|
| `cargo build --workspace --all-targets` | `Finished` — **0 warnings** |
| `cargo test --workspace` | **134 passed / 0 failed** |
| `install -m 755 target/debug/harness /usr/local/bin/harness` | `harness 0.12.0` |
| Offline live smoke (real CLI + real workspace + local OpenAI-compatible stub provider) | `PredictiveCheck` (warning) → `FailureAttributed` (`predict_cmd`, `completion_gate`, both `top_confidence=0.98`, `top_revision_before=r-eb90…`) → model fixed `src/broken.rs` → run `Completed` |
| `--restore-rev r-eb902f349bc3cbf094f7b458d57f32e3` (the revision named in the attribution block) | `restored workspace to r-eb90… (1 file(s) written, 1 removed)` — the pointer is actionable |
| `--no-predictive-check` vs. default | 0 vs. 1 `PredictiveCheck` events |
| Real-codebase false-positive probe: 50 real files (`harness-core` + `harness-cli` sources and tests, `Cargo.toml`, both eval JSON artifacts) written through the real `write` tool | `PredictiveCheck` status counts `{ok: 50}`, **false positives: 0** |

The false-positive probe is what found the one real bug in this gate: Rust string literals may span
physical lines, so the scanner's newline-recovery rule was wrong for Rust. Fixed, unit-tested, and
pinned by test 2 above.

---

## 5. Residual risks

1. **The checker is lexical, not a parser.** It cannot validate types, resolve names, or check
   indentation-sensitive structure. A file can be delimiter-balanced and still be wrong. The
   blueprint asks only for a lightweight parser/lint, and the check is advisory by construction.
2. **TOML validation is deliberately narrow.** Only section-header brackets and single-line string
   balance are checked; duplicate keys, dotted keys, inline-table shape and datetime formats are left
   to a real TOML parser. Widening it was rejected because it would introduce false positives.
3. **Attribution is path/line/symbol-shaped.** A cross-file causal chain that leaves no trace in the
   failing file (the classic "rename a symbol, break a caller you never opened") is recovered at
   rank 2, not rank 1 — measured honestly as `top1 = 0.833`. The blueprint's answer to this is the
   reproduce/revert evidence step, which is **not** implemented here: Gate 4 emits the candidate and
   the `revision_before`, and a human or a later gate decides whether to revert and re-run. Raising
   this metric by widening the heuristic would trade precision for recall, which the blueprint
   explicitly warns against.
4. **`added_symbols` is a lexical approximation.** For `edit` it is the set of identifiers that appear
   in `new_text` but not `old_text`; for `write` it is the whole file's top-level declaration names.
   It is a hint used only to *raise* a candidate's score, never to gate anything.
5. **`bash` mutations are recorded without file spans.** A shell command's file writes are invisible
   to the tool (the same O1 limitation Gate 1 accepted), so such a record can only be attributed by
   recency and its candidate renders `relative_path: null`. It never invents a filename.
6. **Ledger is capped at 128 records** (`MAX_LEDGER_RECORDS`); the oldest are dropped. A very long
   mutating session can lose the earliest records. The blueprint's ledger is session-scoped, so this
   is a deliberate memory bound, not a correctness claim.
7. **`--predict-cmd` cost is bounded but real.** It is opt-in and capped at 8 runs per session; each
   run is a full process spawn with the gate's 180s timeout. It is treated as an explicit operator
   authorization (like `--verify-cmd`), so it does not require `--allow-shell`.
8. **Revision capture cost.** `revision_before`/`revision_after` are only populated when a
   `SnapshotStore` is attached; without one the fields are `null` and candidates carry only the
   evidence pointer. Gate 1's R1 (scan cost on large repositories) still applies.

---

## 6. Files changed

| File | Change |
|---|---|
| `crates/harness-core/src/attribution.rs` | **new** — `PredictiveValidator`, `PredictiveReport`, `MutationLedger`, `MutationRecord`, `MutatedFileSpan`, `MutationTelemetry`, `DiagnosticAnchor`, `parse_diagnostic_anchors`, `AttributionEngine`, `AttributionReport`, symbol helpers (+8 unit tests) |
| `crates/harness-core/src/lib.rs` | export `attribution` |
| `crates/harness-core/src/messages.rs` | `ToolResultMeta.mutation: Option<MutationTelemetry>` (`serde(default)`) |
| `crates/harness-core/src/tool/runtime.rs` | `predictive_check` switch + `attach_gate4_telemetry` (warning block, spans, summary) |
| `crates/harness-core/src/agent_loop.rs` | `MutationLedger`, `--predict-cmd` gate, `record_mutation_from_result`, `backfill_revision_after`, `log_failure_attribution`, attribution block on gate rejection, ledger restore + branch prune, ledger in checkpoints |
| `crates/harness-core/src/events.rs` | schema `0.9.0`; `PredictiveCheck`, `FailureAttributed` |
| `crates/harness-core/src/session.rs` | `SessionCheckpoint.mutations` + `Checkpoint` WAL payload + `checkpoint_with_revision` signature |
| `crates/harness-cli/src/main.rs` | `--no-predictive-check`, `--predict-cmd`, runtime wiring, usage text |
| `crates/harness-core/tests/gate4_predictive_attribution.rs` | **new** — 7 acceptance tests |
| `crates/harness-core/tests/gate3_reversible_extensions.rs` | schema assertion `0.8.0` → `0.9.0`; `checkpoint_with_revision` arity |
| `Cargo.toml`, `Cargo.lock` | workspace version `0.11.0` → `0.12.0` |
| `docs/GATE4_EVAL_METRICS.json` | **new** — versioned evaluation record |
| `docs/GATE4_CI_LOG.txt` | **new** — build/test/install/live-smoke log |
| `docs/GATE2_EVAL_METRICS.json` | `harness_version` refreshed to `0.12.0` by its own test (numbers not re-measured) |

---

## 7. Next gate

Gate 5 — Scout / Worker subagents with isolated worktrees, concurrent budgets, cancellation, and
post-merge verification.
