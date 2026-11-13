# Desktop browser checks

These checks exercise the real Svelte application with a synthetic Desktop Host.
They do not connect a provider, read local conversations, start MCP processes or
change the production API. The fixture is never imported by `src/`.

1. Use the existing Vite server, or start one from the repository root:

   ```sh
   npm --prefix desktop run dev
   ```

2. With an existing Playwright `Browser`, run every suite in its own fresh,
   isolated context (never a page containing personal drafts):

   ```js
   import { verifyDesktopUi } from './desktop/tests/ui-smoke.mjs';
   import { verifyDesktopSettings } from './desktop/tests/settings-ui.mjs';
   import { verifyDesktopCapabilities } from './desktop/tests/capabilities-ui.mjs';
   import { verifyFileSuggestions } from './desktop/tests/file-suggest-ui.mjs';
   import { verifyAttachments } from './desktop/tests/attachment-ui.mjs';
   import { verifyWorkspaceReview } from './desktop/tests/workspace-review-ui.mjs';
   import { verifyTurnReview } from './desktop/tests/turn-review-ui.mjs';
   import { verifyCodeCopy } from './desktop/tests/code-copy-ui.mjs';
   import { verifyMessageLayout } from './desktop/tests/message-layout-ui.mjs';
   import { verifyDesktopFeatures } from './desktop/tests/desktop-features-ui.mjs';
   import { verifyDesktopBasics } from './desktop/tests/desktop-basics-ui.mjs';
   import { verifySessionRuntime } from './desktop/tests/session-runtime-ui.mjs';
   import { verifyWorkflowCompletion } from './desktop/tests/workflow-completion-ui.mjs';
   import { verifyDesignReview } from './desktop/tests/design-review-ui.mjs';
   import { verifyVisualPolish } from './desktop/tests/visual-polish-ui.mjs';

   const baseURL = 'http://127.0.0.1:1420/';
   for (const verify of [
     verifyDesktopUi, verifyDesktopSettings, verifyDesktopCapabilities,
     verifyFileSuggestions, verifyAttachments, verifyWorkspaceReview,
     verifyTurnReview, verifyCodeCopy, verifyMessageLayout,
     verifyDesktopFeatures, verifyDesktopBasics, verifySessionRuntime,
     verifyWorkflowCompletion, verifyDesignReview, verifyVisualPolish,
   ]) {
     const context = await browser.newContext();
     try { console.log(await verify(await context.newPage(), baseURL)); }
     finally { await context.close(); }
   }
   ```

   Each module installs its own synthetic host, so use a fresh context per
   module. This uses the browser automation environment's existing Playwright;
   it adds no production dependency and is not part of `npm test` or CI yet.

3. For the Playwright MCP runner, generate the equivalent single-function files:

   ```sh
   node desktop/tests/build-browser-check.mjs
   ```

   Open one isolated browser context per file, then call
   `browser_run_code_unsafe` with `filename` set to the absolute path of
   `.playwright-mcp/desktop-ui-check.js`, `.playwright-mcp/desktop-settings-check.js`
   and `.playwright-mcp/desktop-capabilities-check.js`.
   The generated files and any screenshots are local verification artifacts.
   Do not run against an unrelated browser context containing personal drafts.
   The `file-suggest-ui.mjs`, `attachment-ui.mjs`, `workspace-review-ui.mjs`,
   `turn-review-ui.mjs`, `code-copy-ui.mjs`, `message-layout-ui.mjs` and
   `desktop-features-ui.mjs` suites
   are run through the import form in step 2 only; the single-file generator
   still covers the three suites above.

The main assertions cover message layout and reading/input column alignment,
wide/narrow sidebar state, the combined model/effort panel and focus, draft
retention, variable-height input and latest-message positioning, Markdown
sanitization and long code, IME confirmation, queue/cancel/restart behavior,
plugin questions, theme/density/row overrides, all widget placements, the
narrow plugin drawer keeping frames mounted, and workspace escape.

The feature batch suite (`desktop-features-ui.mjs`) covers the shortcut
reference (sidebar entry and F1, focus return, Escape, not preempting an open
dialog, narrow window), fence language tags (declared, undeclared, oversized
and HTML-shaped, exact copy), and pinned conversations (ordering, fold, group
isolation, search, reload persistence, corrupt and unwritable local storage).

The `@file` suggestion suite (`file-suggest-ui.mjs`) covers the composer's
asynchronous suggestion lifecycle: a query repeated after another query
(`a -> b -> a`) keeping only the newest answer, a superseded failure not clearing
the current candidates, candidates of a replaced query staying hidden and
uninsertable while its successor is still debouncing, IME confirmation neither
inserting nor sending while a confirmed Enter still inserts, and switching
sessions or starting a task dropping the previous candidates.

The attachment suite (`attachment-ui.mjs`) covers the composer's asynchronous
attachment lifecycle: a pasted image being written once, previewed and sent with
the task, a running task accepting pasted and picked files into the next draft
without sending them into the active task, a full draft writing no ninth image and creating no preview URL,
overlapping pastes ending at eight attachments with previews only for the two it
kept, a late paste answer not returning after `A -> B -> A`, a paste still in
flight when a task starts neither attaching nor reporting, picked attachments
following their draft across a session switch, and a plugin text replacement
keeping both the attachments and the paste in flight. The fixture answers
`save_pasted_image` with a synthetic temp path and returns the paths assigned to
`__KROMA_UI_FIXTURE__.dialog` from the native open dialog; holds and failures
use the existing helpers. The suite counts `URL.createObjectURL` calls in the
page, so a rejected attachment is proven to create no preview.

The settings and dialog assertions cover profile drafts surviving profile and
page switches without autosave, save failures keeping input, omitted versus
explicitly cleared API keys, only-save not changing the connection or default,
default/connection failures staying failures, native modal Tab range, Escape
ownership and opener focus, nested overlays, busy dialogs staying visible,
retrying a saved-but-unconnected model without duplicates, rewind confirmation
during failure and retry, both diff views coexisting, partial cleanup failures
being reported, plugin pages and custom iframes, and the 780×560 layout.

The workspace review suite (`workspace-review-ui.mjs`) covers the refresh
lifecycle of the read-only workspace diff: a refresh re-reading the diff of a
path that survived it while the pre-refresh diff is no longer readable or
copyable and no file can be picked, a vanished selection falling back to the
first entry of the new report, empty and non-repository reports clearing the
selection, diff and copy state, a failed refresh clearing the old view and being
recovered with the same refresh button, a diff answer that outlived its request
never replacing the current one, and a clipboard receipt arriving after a
refresh or a file switch leaving the current copy state alone. It stubs
`navigator.clipboard.writeText` in the page — with a switch to keep the receipt
pending — to observe the copy state; the fixture exposes
`__KROMA_UI_FIXTURE__.workspace` (`is_repo`, `message`, `entries`, `diffs`) so a
check can model the repository changing on disk between refreshes.

The turn review suite (`turn-review-ui.mjs`) covers the read-only per-turn file
list across conversation boundaries: a normal conversation opening the files of
the chosen turn, an empty file list opening no panel and reporting nothing, two
quick picks keeping only the newest turn, a failing request for the visible
conversation reporting its own message, a file list held across `A -> B -> A`
neither opening a panel nor reporting on the success or the failure path (and a
later pick still working), and a checkpoint answer held while another
conversation is previewed never attaching its turns or change buttons to that
preview, while the previewed conversation keeps its own empty result. Closing an
open panel through a UI-driven navigation is not exercised, because the native
modal dialog makes the sidebar inert, and the unmount guard is not reached.

The code copy suite (`code-copy-ui.mjs`) covers the per-block copy control the
Markdown component adds after sanitizing: one native button for each fenced
block copying that block's own text (indentation, blank lines, HTML characters
and the newline before the closing fence included), no control for inline code,
a failure staying retryable without touching another block's feedback, keyboard
`Enter` on the focused control, an answer edit or a streaming re-render keeping
one control per block, a clipboard receipt that outlived its block changing no
current control, sanitizing and HTTP(S)-only link interception still holding (a
copy click is not a link click), and a 380-pixel window keeping the control
visible without widening the app. Answers are edited through
`__KROMA_UI_FIXTURE__.state.snapshot.messages` and re-read through the existing
model control, so no fixture branch exists for this feature.

The per-item capability suite (`capabilities-ui.mjs`) replaces the earlier
category-level suite. It covers the unified five-category settings entry and
absence of MCP approval UI, with eight coverage groups:

- Independent tool switches, disabled items remaining listed, and failed global
  saves preserving the displayed state until a successful retry.
- Optional **全开 / 全关** shortcuts as grouped saves, with search-filtered batch
  actions changing only matching items and leaving hidden items unchanged.
- Independent MCP services, Skills and plugins; a single plugin management section
  with one activation control, preserving plugin installation and configuration.
- **开启 / 关闭 / 跟随默认** independently for each session item, without changing
  sibling tools, MCP services or Skills.
- A session disabling one plugin while enabling another default-disabled plugin,
  without rewriting global defaults; plugin UI visibility, draft and focus retention.
- Per-item overrides surviving reload, failed session saves retaining the previous
  value, and resetting one override without rewriting another group.
- New/switched conversation isolation, live plugin UI restoration, narrow-screen
  list bounds, and bulk inheritance reset scoped to the selected list.
- Capability changes being unavailable during an active task.

Verification on 2026-11-12: all 12 main-UI, 10 settings/dialog and 8 per-item
capability groups passed against the real Svelte application with a synthetic
host; all three returned `runtimeErrors: []`. The per-item checks also reopen a
single tool after bulk-off without reopening its siblings. Frontend checks found
no errors or warnings, all 160 unit tests passed, and the production build passed.
The original preview port was unavailable, so this run used a separate local
test preview on port 1421 without restarting the user's desktop application.

File-suggestion verification on 2026-11-12: all six `file-suggest-ui.mjs`
groups passed against the same application and host, with `runtimeErrors: []`;
`ui-smoke.mjs` still passed all 12 groups after the fixture change. The suite
was also run one fix component at a time, and each group failed while its
component was missing (stale same-query answer, superseded failure, debounce
window, session switch, task start), so the checks measure the fix rather than
the fixture. This run used a separate dev server on port 1422 and an installed
Edge build through Playwright, because no Playwright browser download is present
in this environment.

Attachment verification on 2026-11-12: all eight `attachment-ui.mjs` groups
passed against the real Svelte application with a synthetic host
(`runtimeErrors: []`), and the 12 main-UI, 10 settings/dialog, 8 per-item
capability and 6 file-suggestion groups still passed in the same browser run.
`npm run check` reported no diagnostics, all 160 unit tests passed and the
production build succeeded. The only browser console message was the dev
server's missing `/favicon.ico`.

Workspace review verification on 2026-11-13: all seven `workspace-review-ui.mjs`
groups passed against the real Svelte application with a synthetic host
(`runtimeErrors: []`). The suite also proves that a held refresh disables the
file rows (a forced click starts no diff request) and the copy button, and that
a clipboard receipt kept pending across a refresh or a file switch never
revives or clears the current copy state. Only this suite was run in that pass;
the type check was not repeated for these guards, and the other suites and the
production build were not repeated either.

Turn review verification on 2026-11-13: all seven `turn-review-ui.mjs` groups
passed against the real Svelte application with a synthetic host
(`runtimeErrors: []`), on a separate Vite server on port 1426 with the installed
Edge build. Only this suite was run in that pass; `npm run check` reported no
diagnostics, and the earlier suites, `npm test` and the production build were
not repeated.

Code copy verification on 2026-11-13: all six `code-copy-ui.mjs` groups passed
against the real Svelte application with a synthetic host (`runtimeErrors: []`),
on a separate Vite server on port 1427 with the installed Edge build. The first
two runs failed on suite defects only — a streamed text event must be
`text_delta`, and only the long block scrolls at 380 pixels — and the third run
passed every group. `npm run check` reported no diagnostics; the other suites,
`npm test` and the production build were not run in this pass. The only browser
console message was the dev server's missing `/favicon.ico`.

## Current design-review verification

The full 15-suite browser run passed against the real Svelte application and a
synthetic host, each in its own fresh Edge context. Every suite reported no
uncaught page errors. This includes the attachment, concurrent-session and
workflow-completion suites, not only visual smoke checks.

The new `design-review-ui.mjs` suite contains 13 design regression groups:
conversation menus and rename focus; save validation and draft reset; locked
connection forms; password inputs and serialized plugin saves; failed switches
and uninstall recovery; honest context measurements and nested focus; empty
diffs and retry; actual progress rendering and key/value layout; iframe loading
retry; 380px navigation and popover bounds; reduced motion; and a 780×560 window
with a multiline draft, expanded queue and file suggestions. Input-method arrow
keys must not change the selected suggestion.

The new `visual-polish-ui.mjs` suite adds 7 regression groups for neutral primary
actions, safe welcome prompts, distinct model-selection/connection/draft states,
plugin auto-selection and ephemeral secret drafts, background-only light themes,
readable custom accents, narrow layouts, and system high-contrast mode. It
composites actual browser colours to check representative normal text, helper
text, placeholders and button labels against a 4.5:1 contrast threshold. This is
not a complete WCAG compliance audit.

Type checking reported 0 errors and 0 warnings, all **198 unit tests** across
31 files passed, and the production build passed. Screenshots of chat, welcome,
model settings, capability settings, plugin settings and floating surfaces were
inspected at 1280×850, 780×560 and 380×700, including a light theme. See
[the design review record](../../docs/desktop-design-review.md) for component
coverage and [the visual system](../../docs/desktop-visual-system.md) for the
current palette, interaction roles and verification boundaries. Older verification notes above are historical,
not the current test contract (notably, draft storage is now v2 and attachments
chosen during a task belong to the next draft).

Synthetic-host persistence checks do not substitute for real Rust runtime tests.
The core and selected integration checks passed separately; the Desktop native
library test program compiled but could not start on this Windows GNU environment
(`0xc0000139 / STATUS_ENTRYPOINT_NOT_FOUND`), so those native cases are not counted
as passing. See the implementation record for the full verification boundary.

Disabling does not uninstall items or delete configuration. MCP and Skills
definitions still come from existing files, not a new editor; there is no MCP
approval step and no actual capability replacement during an active task.

The synthetic host is installed only in the main frame, not inside sandboxed
plugin iframes. Presentation injection resolves the app's already-loaded Vite
module URL so hot-reload timestamps cannot create a second, disconnected store.

The fixture can hold a command open (`__KROMA_UI_FIXTURE__.hold('connect')`),
release it with an error (`release('connect', 'message')`), or fail the next
call (`failures.save_profile = 'message'`). A hold covers one call and can be
limited to one query (`hold('project_files', 'app')`, then
`release('project_files', 'message', 'app')`); `project_files` answers
`src/<query>-<call>.ts`, so a repeated query is distinguishable per request.
A hold can also name one turn (`hold('turn_files', 1)`, then
`release('turn_files', 'message', 1)`). The `turnReview` option answers
`checkpoint_turns` and `turn_files` per session and per turn, and its
`__KROMA_UI_FIXTURE__.turnReview` copy can be rewritten between calls, so a
late answer stays distinguishable from the one the visible conversation asked
for. Both answers are built from the session that made the call, not from the
session visible when a held call is released.

None of this exists in `src/`.

A mock host is not proof of actual provider calls, native file dialogs, Windows
window controls, macOS/Linux WebView behavior, memory use or scrolling FPS.
Use the native acceptance checklist in `desktop/README.md` for those checks.
