# Kroma TUI full-width layout and Ctrl+Click links — 2026-10-29

Source: `D:\Kroma\src_kroma\kroma-new-0a5500b`
Previous baseline: `efd7378`
Scope: TUI and one Windows-only ShellExecute feature. No Agent Loop, model prompts, tooling, compaction or WAL changes.

## Changes

- Removed the 100-terminal-column cap. On a 210-column terminal the content area now uses over 200 columns, with small symmetric outer margins; the input composer and table/code/diff rows follow the same adaptive width.
- Recognizes explicit Markdown hyperlinks (including label-only links like `[审查报告 SR-001](./reviews/SR-001.md)`) with the actual destination preserved independently of the displayed label. Link regions are resolved from the same Unicode-aware, wrapped transcript layout as the visible cells.
- Recognizes standalone `http://`, `https://`, `www.` and `file://` URLs, Windows/Unix file paths, quoted paths containing spaces, and compiler locations such as `src/provider.rs:128`. Long literal URLs in narrow terminals recover the full original address when clicked.
- Link text is underlined, highlighted while hovered, and shows the target in the footer. `Ctrl+left-click` opens it; normal left drag continues to select text and right click continues to copy. Link hitboxes are recomputed after viewport scroll and terminal resizing.
- Windows uses ShellExecuteW with direct Unicode arguments for the OS default browser/app (no interpolated PowerShell/cmd invocation). Local relative files resolve against the active workdir and are checked for existence. Other platforms use the system opener.
- No new theme settings, animation system, OSC-8-in-buffer hacks, or additional LLM requests.

## Coverage

- Wide and narrow TestBackend layouts; Markdown table display with 210-column window.
- Unicode/CJK labels, links across wrapped rows, named Markdown links vs. actual destinations, bare HTTP links, Windows file paths, compiler line suffixes, quoted paths with spaces.
- Ctrl-click on a missing file returns explicit error feedback without selecting text.
- Scroll up and back to bottom updates hitboxes instead of using stale screen coordinates.
- Existing TUI, session, provider, tool, context tests and long-history rendering benchmark.

## Boundaries

- Real Windows Terminal/VS Code terminal mouse, hover and native ShellExecute browser opening are not verified by TestBackend. Human interactive acceptance is still required.
- Rich Markdown links in ordinary prose and literal addresses in visible table/code text are covered. Non-address custom Markdown labels inside table cells are currently not preserved as clickable targets.
- Some terminal emulators independently intercept Ctrl+click on detected URLs; no OSC-8 escape sequences are inserted by Kroma to avoid duplicate browser opening and layout inconsistencies.
- Mouse hover requires terminal mouse-motion reporting. Ctrl-click can still work without a hover event when the terminal forwards modifier mouse events.
- Neither a nonexistent local file nor a malformed URL is opened; the user sees a diagnostic in the footer.

References:
- https://github.com/MicrosoftDocs/terminal/blob/main/TerminalDocs/tips-and-tricks.md
- https://github.com/crossterm-rs/crossterm/blob/master/src/event.rs
- https://github.com/ratatui/ratatui/issues/2854
