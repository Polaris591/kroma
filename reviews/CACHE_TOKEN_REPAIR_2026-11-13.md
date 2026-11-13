# Cache/token repair report (F1–F8)

> **User constraint from this point on: minimal testing.** Review, correction and
> final-review stages rely on the evidence already collected in this report plus
> static inspection of the diff. Do not re-run the whole library or the full
> integration suite; run only the smallest named test that covers a concrete new
> edit. Fewer tests is not permission to omit honest scope-of-validation notes.

Date: 2026-11-12/13. Baseline: HEAD `d6aa128` plus the pre-existing dirty working
tree captured at
`C:/Users/24524/AppData/Local/Temp/kroma-cache-fix-baseline-1794494813238`.
This report supersedes nothing: it is the repair record for
[`CACHE_TOKEN_DESIGN_REVIEW_2026-11-12.md`](CACHE_TOKEN_DESIGN_REVIEW_2026-11-12.md)
and does not overwrite it. The independent review of the first repair round is
reconciled in section 7.

No paid provider request, real session, user configuration or installed binary
was used or modified. Every number below comes from local `cargo test --offline`
runs. Nothing here is a cache-hit, cost or speed measurement.

## Design choices

1. **Stable admitted reference snapshots (F1).** The host admits one rendered
   reference block per user task, keyed by the raw transcript index, the ordinal
   of that message among real user turns, and its content digest, and replays it
   **after** the message it was admitted for. The ordinal matters: the same
   wording ("continue") legitimately repeats, and matching by text alone replayed
   a later task's block after an earlier identical message. Later tool rounds and
   later user turns only append, so each request is a prefix extension of the
   previous one. Snapshots freeze: a materialized block is never re-rendered or
   moved, because that is exactly what broke the prefix before. Only the current
   task's snapshot is re-rendered, and only when a tool reports that the source
   data changed (`invalidate_ephemeral_context`).
2. **Final-request identity instead of "disabled calibration" (F2).** A
   decorator publishes the exact provider-facing projection it sent
   (`ModelBackend::final_projection`, fully `project_for_budget`-applied), and a
   decorator that has already committed to host additions publishes the view it
   will send on the next request (`admitted_projection`) on the *same basis*.
   Admission calibration and post-response observation therefore hash the same
   bytes, and the host's own reference blocks are excluded from the calibrated
   transcript message count, so a measurement stays usable on later requests. The
   pre-existing safeguard is kept and made precise: a request-rewriting hook still
   reports `usage_matches_projection() == false`, and then the conservative
   estimator keeps control. The HTTP layer additionally publishes a content-free
   wire identity (payload fingerprint, byte count, tool count, native tool
   types) — computed only when `KROMA_CACHE_TRACE=1` — so provider-side tool
   transformations are visible without paying a serialization on every request.
3. **Cache writes as a subset, not an extra term (F3).** `Usage.cache_write` and
   `Usage.cache_write_1h` are optional; they are reported by Anthropic from
   `cache_creation_input_tokens` plus the TTL breakdown and by
   OpenAI-compatible endpoints from `input_tokens_details.cache_write_tokens`.
   They are never added to input again, unknown stays unknown, and old saved
   usage files remain readable because both `Usage` and `UsageMeter` are
   `#[serde(default)]`.
4. **Checkpoint payload parity with an explicit limitation (F4).** Checkpoints
   now make the same local-tool decision as the coding request (a duplicate local
   `web_search` stays suppressed) and keep the endpoint's native search
   definitions for Responses and Anthropic, with `tool_choice: none` (Responses)
   or `{"type":"none"}` (Anthropic) so a summary never runs another paid search.
   Neither field is added when native search is off. On the message side, a
   checkpoint source is the coding request's own projected prefix, so the
   decorator inserts the same admitted reference blocks at the same positions
   (`ModelBackend::checkpoint_projection_tokens` lets the planner count them). If
   an endpoint rejects `tool_choice`, the client falls back to the previous
   tool-free checkpoint and logs it: that pattern may have a cold tool prefix and
   is documented rather than claimed to be cache-identical.
5. **Narrow, validated, opt-in cache policy (F5).** `anthropic_cache_ttl`
   (`5m`/`1h`), `anthropic_stable_breakpoint` and `responses_prompt_cache_key`
   are profile fields rejected at load when invalid, applied identically to the
   coding request and its checkpoint, and never inferred from a model name, host
   or gateway vendor. Defaults send byte-identically what Kroma sent before.
6. **First-ingestion archive for large tool results (F6).** `src/tool_archive.rs`
   writes the authoritative bytes once beside the session archive and returns a
   bounded JSON preview that names every block (type and size), quotes diagnostic
   lines from the omitted middle, and states the archive path. A failed archive
   keeps the complete original result and says so; nothing can re-run a side
   effect. Archives are named `<session-id>.tool-output-…` so
   `kroma sessions prune` deletes them with their conversation.
7. **Conservative compaction stays the default (F7).** No threshold, retention
   or retry value changed. A test pins the 90% capacity trigger and the 32k
   retained window so an economic hypothesis cannot quietly replace them.

## Per-finding disposition

| Finding | Disposition | Evidence |
| --- | --- | --- |
| F1 sent-prefix instability | **Fixed.** Admitted snapshots at stable boundaries; replay, reopen, rewind, fork, invalidation and checkpoint-coverage lifecycles are explicit and tested. The replay table now refuses a new block instead of evicting an already-sent one, so the append-only guarantee cannot be silently broken by a capacity bound. | `plugins::model::tests` (21 tests), including `sent_reference_prefix_is_stable_across_tool_rounds`, `..._across_user_turns`, `identical_user_wording_still_anchors_each_block_to_its_own_turn`, `a_reopened_session_rebuilds_the_current_snapshot_at_the_same_position`, `a_rewind_drops_snapshots_the_transcript_no_longer_contains`, `a_checkpoint_covered_boundary_is_not_replayed_out_of_position`, `admission_withdraws_only_the_block_that_does_not_fit`. `cache_trace::tests::replayed_reference_reports_append_only_final_projection`. `tests/plugins_e2e.rs` (`memory_recall_survives_a_host_control_reference_in_the_projection`, `generic_context_and_task_finished_are_separate_from_model_completion`, `v2_request_context_and_task_summary_are_budgeted_and_fail_open`). |
| F1 remaining limitation | **Tested limitation.** Snapshots live in memory only. After a process restart the current task's snapshot is rebuilt at the same boundary position (byte-identical when the plugin returns the same packs), but earlier tasks' snapshots are not replayed, so the first post-reopen request can differ from the last pre-reopen one. Rationale: persisting rendered memory text would add a new derived-state format that could replay stale, now-forgotten content. Documented in `docs/project-memory.md`. |
| F2 projection/trace mismatch | **Fixed.** Final-projection calibration plus wire identity; a rewriting hook still disables calibration. The published projection and the planning projection now share one basis, and host reference blocks are excluded from the calibrated transcript count. | `plugins::model::tests::a_decorated_request_still_calibrates_the_next_one`, `cache_trace::tests` (5), `provider::tests::explicit_cache_policy_reaches_the_wire_and_its_checkpoint`, `provider::tests::default_cache_policy_sends_no_new_cache_fields`. Trace records now carry `projection`, `append_only`, `hooks_rewritten`, `reference_bytes`, `calibrated`, `protocol`, `model`, `endpoint_fingerprint`, `native_tools`, `payload_fingerprint`, `payload_bytes`, `tool_definitions`, `provider_request_id`, `elapsed_ms`, `first_token_ms`, `cache_write_tokens`. |
| F2 limitation | **Tested limitation / honest boundary.** A provider request id is only recorded when the endpoint sends one of `x-request-id`, `request-id`, `openai-request-id`; externally hosted model plugins keep their protocol and ids inside the executable, so the trace reports `projection: "provider_budget"`-equivalent fields only through the decorator's final projection and no request id. Streaming failures report `provider_request_id: null`. |
| F3 no cache-write accounting | **Fixed.** Optional cache-write and 1h-TTL fields flow through streams, non-streaming summaries, failures, aggregates, events, TUI/desktop plumbing and `/usage`. | `provider::anthropic::tests::stream_usage_includes_cache_reads_and_writes_without_double_counting_deltas`, `..._cache_creation_ttl_breakdown_stays_a_subset_of_input`, `usage::tests` (7), `cache_trace::tests::failed_request_preserves_reported_usage_without_logging_contents`. |
| F4 native-search checkpoint parity | **Fixed where the protocol allows it.** Responses and Anthropic payloads keep native definitions plus a forbid-invocation choice; local same-name filtering is shared with the coding request. | `provider::tests::checkpoint_keeps_the_native_search_tool_prefix_and_forbids_invoking_it`, `a_checkpoint_without_native_search_never_invents_a_tool_choice`, existing `checkpoint_payload_preserves_the_cached_prefix_and_appends_one_instruction`. |
| F4 limitation | **Tested limitation.** An endpoint that rejects `tool_choice` cannot both keep the native prefix and forbid invocation. Kroma then stops repeating the native tool in checkpoints (atomic fallback flag) and logs it; that arm is knowingly not cache-identical. |
| F5 no TTL/breakpoint/key policy | **Implemented as opt-in controls, defaults unchanged.** | `provider::tests::explicit_cache_policy_reaches_the_wire_and_its_checkpoint`, `default_cache_policy_sends_no_new_cache_fields`; validation in `ProviderProfile::cache_policy`. |
| F5 not changed | **Intentionally not changed.** No periodic prewarm, no inferred capability detection, no forced `prompt_cache_key`, no automatic TTL upgrade. Rationale: none of these is provable from a gateway name or model ID, and each costs money when wrong. |
| F6 large MCP/plugin text | **Fixed for first ingestion.** Archive once, structure-preserving preview, retrievable bytes, no re-run on archive failure. | `tool_archive::tests` (7), including `a_failed_archive_keeps_the_original_and_says_so`, `repeated_rounds_archive_separately_and_never_overwrite`, `large_structured_content_is_named_and_sized_not_dropped`, `middle_evidence_and_block_names_survive_the_preview`. |
| F6 not changed | **Intentionally not changed.** Historical tool results are not rewritten per round; only first ingestion is projected. Rationale: retroactive rewriting would invalidate the cached prefix and can hide unique evidence. |
| F7 90% compaction as cost policy | **Not a defect; no change.** Default remains capacity-conservative, now pinned by a test. | `context::tests::lazy_compaction_defaults_stay_capacity_conservative_not_cost_optimal`. |
| F8 evidence limits | **Fixed as tooling/documentation.** Offline regression lane plus estimator boundary tests plus a documented live protocol that is not executed. | `benchmarks/cache_token_regression.cjs`, `token_estimate::tests::multilingual_and_dense_payload_boundaries_stay_conservative`, `docs/performance.md` sections added in this change. |

## Changed files relative to the pre-existing baseline

Core (new behaviour):

- `src/model.rs` — `Usage.cache_write`/`cache_write_1h`, `Completion.provider_request_id`, `FinalProjection`, `WireObservation`, new `ModelBackend` hooks (`final_projection`, `admitted_projection`, `wire_observation`, `checkpoint_projection_tokens`).
- `src/plugins/model.rs` — admitted reference snapshots (`AdmittedReference`, `ReferenceTable`, `with_admitted_references`, `admit_reference`), transcript-ordinal alignment (`user_ordinal`, `dropped_user_turns`, `reference_for`), final-projection publication on the projected basis, admitted-projection planning view, checkpoint reference insertion, budget-refusal admission policy, invalidation/rewind/fork lifecycle, rewritten and new tests.
- `src/agent.rs` — planning with the admitted projection (with a safe fallback), observing with the sent view, trace observation, event fields.
- `src/cache_trace.rs` — `TraceObservation`, extended content-free record, `fingerprint` crate-visible, new tests.
- `src/provider.rs` — cache-write parsing, `CachePolicy`, wire observation, shared `projected_tools`, checkpoint native-tool parity with `tool_choice`, checkpoint rejection fallback, `provider_request_id`, parity/policy tests.
- `src/provider/anthropic.rs` — TTL breakdown, `CacheSettings`, stable breakpoint, cache-write fields.
- `src/usage.rs` — cache-write totals/unknowns, display lines, merge, report.
- `src/events.rs`, `src/cli_events.rs`, `src/desktop_host.rs`, `src/tui/{runtime_events,event_queue,view}.rs` — optional cache-write fields.
- `src/context_state.rs` — unchanged.
- `src/context.rs` — `CompactionUsage.cache_write`, host-reference-free calibrated message count, checkpoint estimate including host blocks, F7 invariant test.
- `src/profiles.rs`, `src/config.rs`, `src/app.rs`, `src/setup.rs` — cache policy fields, validation, wiring.
- `src/tool_archive.rs` (new) — first-ingestion archive/preview.
- `src/mcp.rs`, `src/plugins.rs`, `src/shell.rs` — large-result projection wiring, shared diagnostic helper.
- `src/protocol.rs` — `is_host_reference`.
- `src/sessions_prune.rs` — tool-output archives are pruned with their session.
- `src/cache_trace.rs` also gains `CacheTrace::enabled` (wire identity is diagnostics-only).
- `src/lib.rs` — module registration.
- `src/token_estimate.rs` — multilingual/dense boundary regression test.

Tests and docs:

- `tests/{architecture_backends,architecture_repair_regressions,context_budget_regressions,context_compaction_e2e,instruction_steering,interrupt_contract,lifecycle_contract,plugins_e2e,project_memory_e2e,session_branches,model_catalog_e2e}.rs` — new required fields only.
- `benchmarks/cache_token_regression.cjs` (new).
- `docs/performance.md`, `docs/reference/configuration.md`, `docs/project-memory.md`.
- `reviews/CACHE_TOKEN_REPAIR_2026-11-13.md` (this file).

## Test commands and results

All runs are `cargo test --offline`, no network, no credentials.

| Command | Result |
| --- | --- |
| `cargo build --offline` | passed |
| `cargo test --offline --lib` | **809 passed, 0 failed, 11 ignored** (one earlier run had a single flaky timing failure in `agent::idle_watchdog_tests::{...}`; it passes in isolation and in the next full run) |
| `cargo test --offline --lib plugins::model::tests::` | 17 passed (after the ordinal fix) |
| `cargo test --offline --lib cache_trace::tests::` | 5 passed |
| `cargo test --offline --lib usage::tests::` | 7 passed |
| `cargo test --offline --lib provider::tests::` | 70 passed |
| `cargo test --offline --lib provider::anthropic::tests::` | 11 passed |
| `cargo test --offline --lib tool_archive::` | 7 passed |
| `cargo test --offline --lib token_estimate::tests::` | 9 passed |
| `cargo test --offline --lib context::tests::lazy_compaction_defaults_stay_capacity_conservative_not_cost_optimal` | 1 passed |
| `cargo test --offline --tests --no-run` | compiled all integration targets |
| `cargo test --offline --test plugins_e2e --test project_memory_e2e --test session_branches --test architecture_backends` | `plugins_e2e` 71 passed after the F1 ordinal fix and its three updated expectations; `project_memory_e2e` 8 passed; `session_branches` 6 passed; `architecture_backends` 7 passed with the one pre-existing baseline failure below |
| `cargo test --offline --test mcp_skills_e2e --test context_compaction_e2e --test context_budget_regressions --test lifecycle_contract --test model_protocol_mock_e2e --test runtime_customization` | all passed (8, 6, 7, 9, 5, 6) |
| `node benchmarks/cache_token_regression.cjs` | all 9 offline groups passed; no provider contacted |
| `cargo test --offline --lib plugins::model::tests::` (correction round) | 21 passed, including `a_decorated_request_still_calibrates_the_next_one`, `a_checkpoint_carries_the_coding_projection_prefix`, `a_long_session_keeps_every_admitted_block_without_rewriting_the_prefix`, `a_full_replay_budget_refuses_the_new_block_and_keeps_sent_ones`, `a_compacted_view_maps_ordinals_instead_of_guessing_by_wording` |
| `cargo test --offline --lib context::tests::` (correction round) | 47 passed, 2 ignored |
| `cargo test --offline --lib sessions_prune::` / `tool_archive::` (correction round) | 3 / 7 passed |
| `cargo test --offline --lib --no-run` (correction round) | all library test targets compiled |
| `cargo test --offline --test plugins_e2e` (correction round) | 71 passed |

**Known remaining failure, not caused by this change:**
`architecture_backends::custom_context_state_survives_wal_and_process_restart`
(`assertion failed: err.is_err()` at `tests/architecture_backends.rs:355`). It was
reproduced on an isolated copy of the pre-change baseline source with
`KROMA_BUILD_REVISION=baseline cargo test --offline --test architecture_backends
custom_context_state_survives`, so it is a pre-existing defect in the baseline
working tree and was deliberately left alone.

## Behaviour changes visible to existing tests

Three `tests/plugins_e2e.rs` expectations encoded the old tail-append behaviour
("the reference is the last message" and "the whole sent list equals raw history +
1"). They were updated to the new contract: the block sits directly after the
user turn that caused it, every raw message is still carried unchanged and in
order, control notes keep their place, and an admitted block is never dropped by
a later request. Their other assertions (intent scoping, no leakage of host
control text, budget being a real budget rather than `available_input/100`,
recall-cache reuse and invalidation) were kept.

## Known remaining limitations

1. Reference snapshots are per process; after a restart only the current task's
   snapshot is rebuilt (see F1). Documented in `docs/project-memory.md`.
2. Checkpoints on an endpoint that rejects `tool_choice` fall back to a tool-free
   payload, which may not share the native tool prefix (see F4).
3. Provider request ids and per-call timings exist only for the built-in HTTP
   layer; an external model plugin keeps that information inside its executable.
4. `cache_write_tokens` is unknown for endpoints that do not report it (for
   example some OpenAI-compatible gateways and all DeepSeek responses today).
5. No live provider measurement was performed, so this report makes **no**
   cache-hit, token or cost saving claim; the live protocol in
   `docs/performance.md` is documented but not executed.
6. Pre-existing baseline failure listed above remains unfixed by design.
7. A checkpoint can only place admitted blocks after a foreground projection has
   established the transcript-to-view turn offset in this process. A `/compact`
   issued before any foreground request in a session reuses no blocks (it sends
   the plain source) rather than guessing a position. `dropped_user_turns` also
   refuses alignment when a request hook changes the user-turn sequence; the
   reference is then omitted for that request and logged.
8. The published basis is the provider *budget* projection plus host blocks, which
   is the same basis the conservative planner uses. A provider adapter may still
   serialize a slightly different byte sequence (for example Chat metadata that
   `project_for_budget` does not model), so the trace describes the internal
   request identity, not a byte-for-byte wire dump. This matches the pre-existing
   calibration basis.
9. The 64 KiB / 2 000-line large-result threshold is a fixed conservative default,
   not a measured optimum; a result in the 64–200 KiB band now costs a retrieval
   round, and `after_tool_result` interceptors see the preview for such results.

## 7. Independent review reconciliation

The first repair round was reviewed
(`…/cache-token-fixes/review.md`, verdict CHANGES_REQUIRED). Every finding was
reproduced against the code before acting.

| Review finding | Verified? | Action |
| --- | --- | --- |
| P1 calibrated `message_count` inflated by injected references, so calibration silently fell back | Yes: `message_count` counted every projected message while `transcript_matches` bounds it by the durable transcript | Fixed. `is_host_reference` blocks are excluded from the count, and the durable measurement no longer stores a value the transcript can never match. |
| P1 published projection and calibrated projection had different bases (`project_for_budget` applied to one) | Yes: the published view was the pre-projection view, the planning view was the projected one, so every tool round mismatched | Fixed. The published projection is projected exactly once, and both `admitted_projection` and `checkpoint_projection_tokens` use that same basis. Regression test `a_decorated_request_still_calibrates_the_next_one`. |
| P1 checkpoints lost the cached prefix whenever a reference existed | Yes: the source omitted host blocks that the coding request now sends early in the conversation | Fixed. Checkpoints receive the admitted blocks at the same positions (offset-validated), and `checkpoint_projection_tokens` makes the planner count them. Test `a_checkpoint_carries_the_coding_projection_prefix`. |
| P1 the 32-entry bound silently dropped admitted blocks | Yes: `admit` evicted the oldest entry, which rewrote the prefix from that turn | Fixed *without* accepting a byte cap that still evicts: admission is refused when the replay budget is full, so already-sent blocks keep their position, with a one-time log. Tests `a_long_session_keeps_every_admitted_block_without_rewriting_the_prefix`, `a_full_replay_budget_refuses_the_new_block_and_keeps_sent_ones`. |
| P2 ordinals not revalidated after a shift | Yes, as a narrow collision when wording repeats | Fixed by construction: ordinals are re-derived from the transcript in `retain_valid`, and alignment requires the projection's user turns to be a *suffix* of the transcript's by digest (`dropped_user_turns`); otherwise no block is inserted. Test `a_compacted_view_maps_ordinals_instead_of_guessing_by_wording`. |
| P2 tool-output archives never pruned, contradicting the module claim | Yes: `sessions_prune::session_id` matched only four suffixes | Fixed. Archives are named `<session-id>.tool-output-…` and the pruner groups them with their session; `docs/design-notes.md` now states retention and the interceptor consequence. |
| P2 "no serialization by default" comment was wrong | Yes: the wire identity serialized every payload | Fixed. `remember_wire` runs only when `KROMA_CACHE_TRACE=1`, and the comment states that. |
| P2 `calibrated` derived from a flag instead of actual use | Yes | Fixed. One `calibration_eligible` value now drives both the trace field and the observation, and its doc says it records eligibility, not later reuse. |
| P2 `Completion.provider_request_id` is a breaking struct field | Yes, for out-of-tree backends | Documented here rather than reshaped: the trait is internal to this binary crate, and every in-tree implementation is updated. |
| P2 F6 threshold changes everyday results, unmeasured; hooks see the preview | Yes | Documented in `docs/design-notes.md` and `docs/performance.md` (threshold, retrieval cost, interceptor visibility). A configuration knob was **not** added: no evidence exists for a better default, and an unvalidated knob invites tuning without measurement. |

Rejected recommendation: replacing the count bound with a byte bound that still
evicts the oldest block. The operator's priority is that a previously sent
request stays a prefix of the next one; any eviction of an admitted block violates
that, so the full-budget path now refuses the *new* block instead. The cost of that
policy is a request that cannot fit its admitted blocks at all; section 9 records
how it now degrades (send the validated base view with no blocks, log the cold
prefix, keep the table) instead of failing the task.

## 8. Recommended next step

Independent review of this correction round (static diff plus the named test
evidence above), then a single targeted re-run of `plugins::model::tests` and
`context::tests` only if the review changes the reference, calibration or
checkpoint code.

## 9. Final review (CHANGES_REQUIRED) — disposition

The final acceptance review
(`…/cache-token-fixes/final-review.md`) confirmed all previous P1s by reading the
current source, and raised one new P1 plus several P2s. Every item was checked
against the code before acting. **The final review's verdict was not ACCEPT and
this report does not claim acceptance**; it records what was done and what remains.

| Final-review finding | Verified? | Action |
| --- | --- | --- |
| **P1** a decorated request can exceed the hard limit and fail the task: after withdrawing the current task's block, the code still propagates the budget error when historical blocks do not fit, while the Agent above already degrades | Yes: `validate_view(&candidate, ...)?` bailed with the hard-limit error, and the baseline always degraded | **Fixed.** After withdrawing the current task's block, if the surviving historical blocks still do not fit, the request is sent with the already-validated base view, `reference_bytes` is published as `0`, the replay table is left untouched (no capacity eviction), and one transition log says the reference prefix is degraded and the cached prefix may be cold. The base view is still validated on its own and an over-budget base view still fails. Test `a_request_degrades_instead_of_failing_when_admitted_blocks_stop_fitting` (tiny input cap sized from the crate's own estimator; asserts the request is sent, no block carried, `reference_bytes == 0`, `usage_matches_projection()` true, the historical block keeps its bytes, and an over-budget base view still errors). |
| P2 blocks for *surviving* turns are shed at every checkpoint (boundaries are indices into the prepared view, which re-indexes) | Yes: `retain_valid` receives the prepared projection, so a summary prefix shifts indices and older entries drop | **Documented, not restructured.** Quality-only (the prefix is already rewritten at that moment and the baseline carried only the current task's packs). `docs/project-memory.md` now states it. Fixing it would change the entry identity model, which is beyond the authorized scope. |
| P2 checkpoint insertion is never exercised with a truncated source | Yes, no such test exists | **Not added** (minimal-testing constraint, one capacity regression allowed). Offset math is unchanged and reasoned in section 7; placed on the not-re-verified list below. |
| P2 `final_projection` publish clones/projects on every request; the "borrowed, no extra copy" comment is false on the zero-injection path | Yes | **Comment corrected** to state that publishing clones and projects once per request, and why. The publish path itself was not restructured (the Agent's trace/calibration relies on it; the cost is milliseconds and has no correctness impact). |
| P2 the F7 test asserts on ambient `KROMA_COMPACTION_TARGET_TOKENS` | Yes, a brittle environment dependency | **Removed** that assertion; the test keeps the behavioural invariants (90 % trigger, 32 k retention, trigger < hard) and notes that such settings belong to configuration tests. |
| P2 continuation round drops all blocks | **Checked; not reproducible in the current code.** The continuation user message is appended to the same `view` handed to the decorator (`src/agent.rs` before `complete_with_activity`), so the user-turn sequence still matches and ordinals are unchanged; blocks are replayed. The continuation round does break the prefix by itself (pre-existing behaviour, independent of blocks). | Recorded as documentation of the pre-existing continuation rewrite in `docs/project-memory.md`; no code change. |
| P2 reports/docs do not state the P1 consequence | Yes | **Fixed** in `docs/project-memory.md` (degradation path, cold prefix, table preserved, hard limit not relaxed) and in this section. |

Not re-run in this round (minimal testing): the full `--lib` suite,
`plugins_e2e`, the offline benchmark lane, and every integration binary. The
previous rounds' results are reused and labelled as such. This round ran exactly
two named groups (`plugins::model::tests` 22 passed, `context::tests` 47 passed /
2 ignored) plus the compile they trigger. Nothing here observes a provider cache
hit, TTL, `tool_choice` acceptance, token or cost effect, so no cache or cost
acceptance is claimed; the live protocol in `docs/performance.md` remains
documented and unexecuted.

Remaining accepted limitations after this round: per-process reference snapshots
(reopen rebuilds only the current task's block); blocks for earlier tasks are shed
at the first checkpoint after they were admitted (safe, cache-only); a request may
be sent with its reference prefix degraded when even historical blocks no longer
fit (logged, table preserved, prefix may be cold); checkpoints on endpoints that
reject `tool_choice: none` are tool-free; provider request ids/timings and
`cache_write_tokens` stay unknown where unreported; the large-result threshold is
an unmeasured conservative default; `Completion.provider_request_id` is a breaking
field for out-of-tree backends; the pre-existing
`architecture_backends::custom_context_state_survives_wal_and_process_restart`
failure remains unfixed by design.
