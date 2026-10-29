# TUI usability and bilingual interface

## Scope and result

Implements the five agreed usability changes plus language selection and a
searchable, described slash-command catalog. No runtime model prompts, tool
execution semantics, persisted messages, dependencies or CI gates changed.

- Tool activity: short labels for common commands, one-line fallback, elapsed
  time when measured, failures visible while collapsed. Ctrl+O retains full
  commands and result details including output-file references. stderr with a
  successful exit is a warning, not an inferred failed process.
- Stable phase/completion/cancellation status above the composer. No invented
  progress percentages or assertions that successful task completion proves
  all checks passed.
- Historical viewport stays pinned on new output and completion. New updates
  are counted (not misrepresented as distinct messages); Ctrl+End clears them.
- Comfortable prose width defaults to 100 terminal cells. Code/table gutters
  keep engineering width; /layout wide restores full-width prose. Source-line
  identities remain stable for resize anchoring and clickable links.
- Footer favors model/reasoning/context percentage and last-request usage;
  /usage retains detailed measurements. Missing usage is never rendered as 0.
- /language zh|en (no argument toggles) localizes UI chrome, command descriptions,
  help and common notifications. Original model answers, Shell/MCP output and
  upstream diagnostics remain verbatim. Shared CLI command result prose may
  remain English; it is not machine-translated or used as a hidden status key.
- / and /commands open described command completion; arrows select, Tab or
  Enter inserts, and a subsequent Enter executes. Esc preserves the draft.
  Ctrl+P keeps its existing searchable category picker behavior.
- Preferences are validated and atomically replaced in ~/.kroma/ui.toml; failed
  writes preserve the current in-memory settings. Tests inject temporary paths
  instead of writing the developer's actual settings.

## Review

Reviewed correctness, readability, architecture, safety and rendering cost in
README.md, tui/{mod,picker,view,logo,command_catalog,preferences,tool_display}.rs,
and usage.rs, together with callers and existing CI. Reuses the renderer,
command handlers and atomic-file utility; no new plugin framework or unsafe.
New settings/catalog responsibilities are isolated rather than exposed to the
Agent. Existing tool transcript data is retained for expansion, so very large
MCP responses still carry their underlying memory cost; this change does not
claim to solve transport resource limits.

Findings fixed during implementation:

| Severity | Scenario | Disposition |
|---|---|---|
| medium | An open completion picker could consume Ctrl+C during an active task. | fixed: active cancellation takes priority, covered by regression test. |
| medium | Truncating formatted tool details could hide the archived output path. | fixed: preserve details and file references; truncate only collapsed display. |
| medium | Task completion reloading history could pull a reader back to latest. | fixed: capture and restore history position across final reconciliation. |
| low | A two-pass prose reflow could change source-line identities on resize. | fixed: apply width policy in the existing single wrapping pass. |

No unresolved blocking finding was identified in this scoped self-review.

## Verification

- cargo test --locked: 279 passed, 9 ignored (real-provider experiments and
  existing benchmarks); includes 89 passing TUI tests, one TUI benchmark ignored.
- cargo fmt --all -- --check: passed.
- cargo clippy --locked --workspace --all-targets --all-features -- -D warnings: passed.
- git diff --check: passed.
- New behavior tests cover command typing, Tab/Enter safety, argument input,
  Escape/cancellation, preference persistence and refusal, 30/40/80/160-column
  bilingual rendering, unknown usage, prose/code width, history reconciliation,
  full commands, output-file links and stderr warnings.

These are automated Ratatui TestBackend/event tests, not a claim of interactive
Windows Terminal/SSH/clipboard visual acceptance. Linux/macOS and live-provider
lanes were not run locally. Changes are left uncommitted for user review.

## Follow-up round: screenshot feedback

| Severity | Scenario | Disposition |
|---|---|---|
| medium | Every stderr byte was treated as a warning, so ordinary `cargo build` progress showed a warning marker on a successful exit. | fixed: only explicit diagnostic prefixes (`warning:`, `error[`, `fatal:`, Chinese equivalents) produce an advisory; exit status still governs failure. |
| medium | Compound commands (`cd X && cargo build`) kept the full raw command as the collapsed label and wrapped to several rows. | fixed: leading `cd … &&` segments are skipped for the label only; unknown scripts get a neutral one-line label. Raw command and output remain under Ctrl+O. |
| low | Prose was clamped to 100 cells even on very wide terminals, leaving large empty margins. | fixed: comfortable width is 75% of the viewport, bounded to 100–160 cells and never wider than the terminal; `/layout wide` still restores full width. |
| low | Generated notices (`Resumed session:`, `Clipboard image`, notification labels) stayed English in Chinese mode. | fixed: localized at render/notification time; user content, file names, tool output and upstream errors are untouched. |
| low | History navigation was announced both above the composer and in the footer. | fixed: the composer row owns it; the footer hint is suppressed while browsing. |

### Additional verification

- cargo test --locked: 282 passed, 9 ignored.
- cargo fmt --all -- --check, cargo clippy --locked --workspace --all-targets
  --all-features -- -D warnings, git diff --check: passed.
- New test `screenshot_regressions_compact_tools_localized_labels_and_single_history_hint`
  asserts one-row collapsed tools at 40/80/180/240 columns, no warning on
  compile progress, localized resume/attachment labels, and exactly one
  `Ctrl+End` hint while browsing.

## Third round: screenshot review

Three visual defects were visible in a 184x44 Chinese session screenshot; the
rest of the frame (role gutters, prose width, tool rows, composer) needed no
change. No prompt, provider, tool or persistence behaviour changed.

| Severity | Observation | Disposition |
|---|---|---|
| medium | The collapsed `◇ 推理摘要 · Ctrl+R` line had no row below it, so it read as a header for the `◆ Kroma` answer that followed. Every other block is separated by one blank row. | fixed: reasoning gets the same one-row separator. |
| medium | The footer's second row was one run-on of measurements, a command reference and key hints (`上次输入 … · 缓存命中 87% · /usage  ·  Enter 发送 · /help 帮助`). `/usage` sat between the numbers and the keys, and the hint group stopped mid-row. | fixed: measurements keep the left edge, `/usage` plus the key hints are right-aligned; measurements win and the hints are dropped when the terminal cannot hold both. |
| low | The cache figure lost its accent colour in Chinese because only the English `Cache `/`Hit ` prefixes were recognised. | fixed: the localized labels are recognised too. |
| low | The completion row said `已完成 · 请查看上方结果` — a row of pure acknowledgement with no information. | fixed: `✓ 已完成 · 8.3s`; failed and cancelled keep their actionable guidance and also report the turn duration. |

`usage_status` was split into `usage_variants`/`usage_status_fitting` so the
footer can ask whether a measurement fits *whole* before spending the row on it;
a truncated measurement would read as a different number.

### Verification

- `cargo test --locked`: 255 passed, 9 ignored (includes 49 TUI tests).
- `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`, `git diff --check`: passed.
- `cargo build --release --locked --offline`: passed.
- New tests: reasoning/answer separation; footer left/right split with the gap
  and right-edge assertions at 120 columns; outcome marker plus duration for
  completed, failed and cancelled turns.
- Still automated TestBackend buffers, not a real Windows Terminal session; the
  visual result was inspected as rendered text at 40/80/120/184 columns.

### Fix after the first deployment

The deployed build showed `✓ 已完成` without a duration. Cause: `execute_task`
calls `drain_final_usage`, which forwards only measurement events, so
`UiEvent::TaskFinished` could be dropped before `UiState` saw it and
`task_elapsed` stayed `None`. The unit test passed because it sent the event
directly.

- `UiState::finish_task()` now owns the transition (`running = false` plus the
  duration), the `TaskFinished` handler calls it, and `execute_task` calls it
  when the task future resolves, before `load_history`.
- The completion test no longer sends `TaskFinished`; it asserts the row keeps
  its duration when only `finish_task()` runs, which is the real path.
- `cargo test --locked` 255 passed / 9 ignored, `cargo fmt --check`,
  `cargo clippy -- -D warnings`, `git diff --check`, release build: passed.

### Readability round

Screenshot measurements at 179x55: text started 2.5 cells from the left edge and
stopped at column 133, leaving ~43 empty cells on the right; message bodies began
one cell left of their own role label; the `◆ Kroma` label was white like body
text, so turns had no colour anchor.

- The reading column is centred (symmetric margins, same line length) instead of
  hugging the left edge. `/layout wide` still fills the terminal.
- Message bodies hang under the role label (five cells) instead of starting left
  of it, so every block shares one text column.
- `◆ Kroma` uses the accent colour that the mark already had.
- The prose/engineering width split inside `wrap_document` is gone: the column
  owns the width policy, so a line is never measured against two limits. Code
  and tables now share the reading column instead of poking into the margin.
- Tests: `widescreen_centres_the_reading_column_instead_of_gluing_it_left`
  (symmetric margins, `/layout wide` fills, 40-column terminals unchanged),
  `wrapping_keeps_source_line_identity_within_the_reading_column`, footer
  right-edge assertion expressed against the centred column.
- `cargo test --locked` 255 passed / 9 ignored, fmt, clippy `-D warnings`,
  `git diff --check`, release build: passed.
