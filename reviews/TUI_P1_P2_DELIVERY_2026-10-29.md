# Kroma TUI P1/P2 delivery and verification — 2026-10-29

Source tree: `D:\Kroma\src_kroma\kroma-new-0a5500b`
Baseline: `4b2e623` (the Calm Transcript P0 implementation)
Scope: targeted TUI work only. No Agent Loop, provider payload, tool protocol, context policy or WAL format changes.

## Delivered

### P1 — input, navigation, and responsiveness

1. **Grapheme-aware editor and conversation wrapping.** Added the already-transitively-installed `unicode-segmentation 1.13` as a direct dependency. Left/Right, Backspace, Delete, visual row navigation, editor viewport, transcript wrap and mouse selection respect Unicode grapheme boundaries, including decomposed accents and joined emoji. Attachment markers remain atomic with undo/redo and history recall.
2. **Streaming scroll anchor.** When the reader scrolls upward, the first visible row is pinned as new model deltas arrive. Existing lazy historic paging adjusts for prepended rows. `Ctrl+End` returns to auto-following the latest output. A dedicated test applies 100 streaming updates while paused in history.
3. **Shared searchable picker.** One small Ratatui popup supports commands, saved model profiles, named sessions and `@file` completion. `Ctrl+P` opens commands; Tab cycles categories; text searches, arrows navigate, Enter selects, Esc preserves a draft. `/model` and `/sessions` open their relevant category; ambiguous `@path` Tab completion opens files, with directory traversal. Selecting a model or session submits the existing internal command path, not a second switching implementation. Paths are displayed with stable slash separators on Windows.
4. **Clipboard transport.** Right-click copy prefers the local native desktop clipboard, with OSC 52 fallback for remote SSH or missing clipboard access. The OSC 52 status no longer claims confirmed copy, because terminal acceptance cannot be observed.
5. **Measured draw optimization.** Unchanged usage-only events no longer invalidate the transcript cache; the final paragraph copies/renders only visible terminal rows, fixing an implicit `u16` scroll offset ceiling. ASCII history lines retain a fast wrapping path; non-ASCII lines use grapheme-safe wrapping. No new renderer or speculative partial-entry cache was introduced.

### P2 — readability and themes

1. Fenced Markdown code carries a language label and a consistent gutter.
2. `diff` fenced lines visually distinguish additions, deletions and hunk positions; scrollable `/diff` command output has corresponding semantic line treatments.
3. The monochrome theme uses the terminal's native foreground/background rather than hardcoded white-on-light.
4. The picker remains on top of history but does not cover the editor; labels and queries truncate on terminal-cell/grapheme boundaries. Size tests include 40x16, 80x24 and 160x40.

## Regression and performance evidence

- Full `cargo test --locked --offline -q`, `cargo fmt --all -- --check`, `cargo clippy --locked --offline --all-targets -- -D warnings` and Windows `cargo build --release --locked --offline -q` should all be run after the final file changes.
- New focused tests cover decomposed accents and joined-emoji deletion/undo, visual cell-column navigation, ZWJ grapheme wrapping/copy, pinned scroll under 100 updates, picker filtering/session selection/file completion/draft preservation, popup small-screen layout, diff semantic colors, and monochrome terminal colors.
- Local Ratatui TestBackend 80x24, Windows **Debug** streaming microbenchmark (60 frames each, 300 and 3000 historical assistant messages):
  - Before viewport optimization: 300 messages p95 **18.309 ms**; 3000 messages p95 **18.381 ms**.
  - After viewport-only optimization: 300 messages p95 **8.097–8.260 ms**; 3000 messages p95 **8.319–8.425 ms** (short-run variation).
  - Grapheme wrapping without the ASCII fast path regressed to roughly **18.5 ms p95**; after retaining a fast ASCII path while still segmenting Unicode graphemes, final p95 was **9.325 ms** (300) and **9.271 ms** (3000).
  - These measurements include simulated event delivery + drawing and are specific to this synthetic workload. They do not measure user-perceived model response speed, and a Release/real terminal comparison can differ.

## Deliberately unclaimed / human acceptance needed

- No true Windows Terminal or VS Code integrated-terminal session was available to this connector, so IME composition, mouse capture/right-click interception, and real clipboard round trips were **not manually validated**.
- The visual buffers were tested with Ratatui's TestBackend, not approved from real terminal screenshots or user blind testing.
- Code highlighting is deliberately lightweight: language labels, comments, diff statuses and readable wrapping; no heavyweight syntax-highlighting dependency, separate animation engine or new user-facing settings were added.
- Native clipboard success is detectable; OSC 52 delivery cannot verify whether a terminal accepted the sequence.
- This work does not change prompts or create additional LLM calls.
