# Kroma v0.4 — 2026-10-29 review fixes

Baseline: `342ae51` (Windows `main`). This change set does not rewrite Agent Loop, WAL, TRC/APEC policy, shell processes, or user sessions.

## Changes

1. **Provider-aware context estimates** — `ModelBackend::project_for_budget` removes known wire-omitted Chat reasoning, incompatible Responses/Anthropic opaque blocks, and assistant/tool metadata only from the estimated view. `Compression::prepare` now uses the estimated view for soft/hard thresholds, normal TRC choices, and emergency preview decisions. The emergency preview code also updates projected JSON size incrementally rather than re-serializing every iteration. The raw transcript, original APEC snapshot, and actual model request are preserved.
2. **Consecutive image follow-up** — a log/file or a second image attached on the immediately following user turn no longer automatically disables the previous image. Current attachments have first claim on the 16 MiB active-image budget. Text-only model capability disables active image reuse. Earlier images remain references after that follow-up.
3. **Atomic plugin model/budget replacement** — `RuntimeComponents::replace_model(backend, ContextConfig)` is the sole supported model mutation path; `model()` exposes a read-only handle. A new backend can no longer accidentally retain the default model's context budget through direct public-field replacement.
4. **Opaque reasoning provenance** — successful Responses and Anthropic model outputs get a stable non-secret marker for model/protocol/endpoint, preserved with the durable session. Stateless request projections replay encrypted Responses reasoning and signed Anthropic thinking only to the matching model, or from the immediately active turn for untagged legacy history. Ordinary text/tool results survive cross-model switches. The WAL remains unchanged.
5. **Parallel MCP startup** — independent MCP servers initialize/list tools concurrently, each with its existing 15 s timeout. Tool registration remains in sorted configuration order for stable tool schemas and prompt caching.
6. **Prompt contribution activation** — ordered contributions are appended as an exact suffix instead of skipping a contribution whose text appears as a quoted substring inside system instructions. Repeated projection remains idempotent.

## Windows checks

- `cargo test --locked --offline -q`: 199 passed, 8 ignored, no failures (post-change).
- `cargo clippy --locked --offline --all-targets -- -D warnings`: passed.
- `cargo build --release --locked --offline -q`: passed, `kroma 0.4.0`.
- `cargo fmt --all -- --check`: passed after targeted formatting edits.
- Regression tests include previous image + new log, previous image + new large image, false APEC from unsent reasoning, model/budget atomic replacement, same/cross-model opaque reasoning, quoted Prompt text, and actually overlapping MCP HTTP server handshakes while preserving registration order.

## Design boundaries and future live acceptance

- Byte-based Token estimates remain approximations; they are not a replacement for the provider's tokenizer, image billing, server caching rules, or returned input token count. The aim is eliminating known double counting and avoidable compaction, not claiming exact token usage.
- Opaque reasoning is conservatively constrained to the exact configured model identity, even where some OpenAI model-family members may support reuse. This avoids assuming compatibility that the adapter cannot verify.
- The mock tests establish request shape and state preservation but do **not** substitute for real upstream Responses/Anthropic tool-turn tests, manual TUI keyboard/clipboard checks, or comparative measured cache/cost trials.
- Replacing a backend outside `RuntimeComponents` must still pair it with the matching context budget (as `Agent::change_model` already does).
- The existing Git Bash/PowerShell platform roadmap and the intentional absence of approval gates are unchanged.

References: https://developers.openai.com/api/docs/guides/reasoning and https://platform.claude.com/docs/en/about-claude/models/extended-thinking-models
