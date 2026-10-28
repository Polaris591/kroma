# Kroma TUI / Calm Transcript — P0 implementation

Date: 2026-10-29
Base: `3d32350`
Platform: Windows / Rust 1.85+ / Ratatui 0.29

## Scope completed

- **Reasoning**: default collapsed. Each reasoning entry shows a short state marker rather than repeating Ctrl+R instructions. Ctrl+R still expands every available provider-exposed reasoning segment; incremental deltas and final reconciliation are unchanged.
- **Tools**: collapsed successful tools take one cell-bounded command line. Running tools retain the final line of live output. Collapsed failed tools show a second line containing the exit code and first useful diagnostic. Ctrl+O still reveals the full existing short tool-result detail. Failure diagnostics render consistently both live and after WAL reload.
- **Draft safety**: plain Esc no longer deletes drafts or attachments; Esc in file-path mode still cancels that mode. Ctrl+C retains its existing idle-clear/idle-quit/running-cancel semantics.
- **Status hierarchy**: header holds product identity, cached session display name when available, and activity. Footer holds model ID, Think level, context consumption and a single shortcut hint. Model is visible at 80 columns (unlike the prior >=85-column-only header rule); narrow terminals compact the metadata.
- **Notification life cycle**: temporary notices disappear when the user next interacts or starts a new task. Failed/cancelled task notifications are additionally preserved as scrollable local activity entries after transcript reconciliation; durable WAL is not rewritten.
- **Empty state**: one quiet ready line instead of multiple marketing/example lines.

## Scope deliberately unchanged

No new UI runtime, startup wizard, sidebars, terminal animation, plugin SDK, context handling, Agent Loop logic, tool result persistence format, or provider API requests.

## Verification

- Ratatui TestBackend at **40x16, 80x24, 110x30, 160x40**, including model / context / thinking visibility.
- Failed-tool summary fits within its reading column and is retained after restoring a durable tool message.
- Live thinking can be hidden without losing deltas, then shown with Ctrl+R; exact final reconciliation remains.
- Esc preserves drafts and cancels only the file-path prompt, while Ctrl+C clears when idle.
- Temporary footer notices are dismissible while important errors remain scrollable.
- Existing TUI tests and full Rust tests, `cargo fmt --all -- --check`, strict Clippy and Windows Release build all run on Windows.

## Follow-up (P1/P2)

Still pending by design: grapheme/IME editing, scroll anchor under rapid streamed updates, selection and clipboard acceptance in real Windows Terminal and VS Code terminal, a small shared fuzzy selector for command/model/session/file, long-session draw latency measurements, code/diff presentation and mono-theme contrast. These should be evaluated before any larger UI refactor.

## Limits

Automated TestBackend verifies buffer cells, not the appearance of a live terminal. No interactive Windows Terminal video or human visual acceptance took place in this run. The local task-error UI entry is not intended to change the underlying WAL or to survive a new terminal process.
