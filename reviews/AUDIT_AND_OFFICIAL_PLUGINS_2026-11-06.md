# Kroma core audit repairs and official-plugin implementation: 2026-11-06

## Baseline and scope

- Base: GitHub `main` `985ebb1`, which already includes PR #1's Anthropic stale-thinking/checkpoint and cache trace fixes.
- Work performed in an isolated Devspace Git worktree, not the concurrently active `/root/kroma` checkout.
- No paid model/search-provider acceptance test was made. Do not interpret unit mock success as a real cache-hit or gateway compatibility measurement.
- The built-in compactor remains **lazy single-pass**, not the earlier TRC+APEC combination. Older ablation findings cannot establish that the new compactor is superior.

## Audit corrections delivered

| Area | Changes | Verification |
| --- | --- | --- |
| Agent / Git | Delay automatic snapshot until just before the first tool; pure Q&A avoids Git work. Derive snapshot turn labels from committed history across restarts. Preserve cancellation and pre-tool ordering. | `cli_checkpoints` (3/3), core `checkpoints` unit suite |
| Provider / Responses | Preserve multiple response message items, terminal refusals and opaque reasoning; avoid duplicating text when both deltas and final output arrive. | Responses parser tests (8/8) |
| Provider / compaction usage | Record all attempted checkpoint requests, preserve known token lower bounds after failed/truncated attempts, and identify attempts with unavailable usage. Conservatively retry an output-truncated coding request only if it streamed **no** text or reasoning and no tools ran. | Provider/context unit and mock suites |
| MCP tools | Include bounded input schemas in lazy `mcp_search`; no hidden argument guessing or unbounded schema injection. | `mcp_skills_e2e` lazy search + call test |
| Model hook / memory | A plugin tool can declare `invalidates_context` in its manifest; on success the model hook refreshes its ephemeral context pack. This is generic rather than hardcoding a particular plugin name in the Agent Loop. | Project memory request-context regression |
| Host tool/model context | Supply non-secret **current parent session** model ID, URL, protocol, thinking and selected provider profile to plugin tools. Never pass an API key in tool JSON. | Host Rust + subagent environment tests |
| Tooling / lint | Resolve platform-specific strict-Clippy warnings without suppressing them, preserve normal TUI rendering. | `cargo clippy --all-targets -- -D warnings` |

## Plugin deliverables

### Web Search & Fetch 1.7

Existing 1.6 search/read engines, keyless fallback, bounded text extraction, deterministic pagination and safe error codes remain. New billing boundaries: the model cannot pick a provider by tool argument, and `auto` defaults to at most one configured paid backend. Multiple paid calls require the **user** to set `paid_fallback=true`. Reduce default snippets, and do not require a page fetch for every search result. No client-side browser/login automation is introduced.

### Project Memory

Preserves local project identity, SQLite WAL, low-trust request_context, task_summary, user-anchored pending rules, review notes, search/read/forget, and human controls. Fixes: never backfill irrelevant memories when auto-recall has no matches; validate pending quote against only the current/latest user instruction and promote no more than one rule per task; after a successful forget, discard cached packs so the model cannot see the forgotten memory on the next turn; child agents can read memories but not write or promote them.

### Subagents 1.0 (implementation increments)

Bundles `general`, `explore`, `planner`, `reviewer`, and `verifier` Markdown profiles with strict validation. Supports user and project overrides, labels, complexity tiers, per-profile tool allow/deny and shell policy, model-turn/token admission, ordered batch results, resume and evidence retention. Child processes receive the current host connection metadata and use local profiles for credentials; no hard-coded DeepSeek default. Nested subagents and memory writes are disabled in child tool lists. Completed run archives are pruned by age/count while active and resumed runs are preserved. This is a tool-filter contract, **not** an OS sandbox.
Each child result is explicitly marked `untrusted_child_report` and instruction-shaped outputs are flagged for inspection without hiding the original answer.

## Recorded validation

Environment: Devspace Linux, Rust 1.85.1, `NO_COLOR` removed and terminal `TERM=xterm-256color` for TUI golden tests. Dependencies use the reachable local rsproxy Cargo index, without changing the repository's lockfile.

- Core `cargo test --locked --lib`: **646 passed, 9 pre-existing ignored, 0 failed**.
- Project-memory `cargo test --locked --manifest-path examples/plugins/project-memory/Cargo.toml`: **44 passed**.
- Node official-plugin suites (`web`, `subagents`, `memory`): **155 passed** after adding the result-trust regression.
- CLI checkpoints: **3 passed**.
- Core `cargo clippy --locked --all-targets -- -D warnings`: **passed**.
- `mcp_skills_e2e`: **8 passed**; `plugins_e2e`: **52 passed**;
  `project_memory_e2e`: **1 passed**.
- `context_compaction_e2e`: **7 passed**; `model_protocol_mock_e2e`: **4 passed**.
- `cargo fmt --all -- --check`, memory Cargo fmt, and `git diff --check`: required before publishing.
- Platform-specific builds remain outside this Linux acceptance window.

## Remaining acceptance risks (do not mark fixed without evidence)

1. **Anthropic signed-thinking tool loops:** local revision projection tests pass, but no real provider/model/thinking-mode matrix has verified tool-cycle compatibility and consecutive checkpoints. A strict provider can reject a client-side-compacted prefix in ways a mock cannot demonstrate.
2. **Responses continuous zero cache hits:** the actual upstream request/wire cache key, routing and cache TTL were never captured. Nothing here proves the original zero-hit issue is resolved. Opt-in sanitized request-level tracing and a provider reproduction are still needed.
3. **Current compression strategy:** no same-endpoint/same-model randomized ablation against TRC+APEC or Pi. The old experiment cannot validate lazy single-pass behavior.
4. **Project-memory completeness:** no persisted per-file content fingerprint invalidation, topic-level atomic replace or active-200-entry eviction; runtime package auto-installer/precompiled binaries have not been verified across all target platforms.
5. **Subagent distribution and execution:** Node-based plugin still needs a Node runtime or independently packaged executable; no signed multi-platform bundles. Shell "read-only" restricts literal commands, not arbitrary code executed from allowed tests; there is no isolation guarantee for filesystem writes.
6. **Web external benchmarking:** free endpoints vary in stability, and real paid-provider comparison was not run. Firecrawl extraction remains optional and billed; there is no local headless-browser fallback.
7. **Platforms:** Linux tests here cannot substitute for Windows/macOS builds or a real release smoke test. Full integration suite may require more disk space than the current Devspace root filesystem provides.

These are remaining work/evidence requirements, not reasons to discard confirmed local correctness fixes. No production credentials, chat content or provider reasoning payloads have been logged in this report.
