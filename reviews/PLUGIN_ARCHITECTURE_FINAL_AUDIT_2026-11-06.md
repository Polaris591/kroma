# Kroma plugin architecture: final-design implementation and audit

Date: 2026-11-06. This change is stacked on the final Web / Project Memory /
Subagents PR, not a replacement for the existing Rust Agent Loop. Principles:
few generic extension boundaries, process-backed JSONL plugins, default-off
official bundles, no reapproval loops, minimal maintenance and zero work when
no plugin is installed.

## Verified source changes

| Audit item | Implementation | Acceptance |
| --- | --- | --- |
| `task_summary` ran after a model response, before task/WAL completion | Generic `task_finished` callback from Agent after the task boundary; nonblocking, bounded, best effort. Legacy names supported. Session ID/archive captured before async dispatch | Model-only completion no longer fires; generic plugin E2E and legacy V2 regression |
| Context hooks hardcoded for project memory | Generic `provide_context` plus source-qualified, low-trust `<plugin-context>` projection; bounded by host token/byte budget; old `request_context` supported | Host E2E and request-cache regression |
| Conflict and last-activity notices silently dropped without IDs | Content-based short ID only for ephemeral `conflict` and `last_activity` notices; normal memory data still needs IDs | Unit regression |
| Plugin-provided markup could forge host tags | HTML-like escaping of block text and paths | Unit + generic plugin E2E |
| Partial tool/prompt contribution retained after a registration failure | Stage changes in cloned ToolRegistry/PromptComposer; commit only on success | Atomic failure regression |
| Malformed plugin tool schemas reached model requests | Bounded structural schema validation at manifest load | Invalid `required`/`items` regression |
| Broken optional plugin blocked all other plugins | Skip malformed optional plugin and report warning; retain explicit fail-closed behavior for selected model/context provider | Reload + model-provider E2E |
| Code changes repeatedly required plugin reapproval | One explicit enablement persists; source changes still force idle worker replacement | Unit and E2E changed-code tests |
| Bundled plugin could be deleted with `uninstall` | Reject removal outside the user plugin root; allow user-installed override of a bundled ID | Bundle and override test |
| Bundled plugin directory vanished when catalog refreshed | Retain its path in PluginCatalog | Bundle reload test |
| Event subscriptions silently drop a full queue | Bounded per-subscriber counters with exponentially spaced diagnostics; no backpressure into Agent | Existing slow-subscriber E2E; logs are not a durable event journal |
| Unchanged event subscriber restarted on unrelated idle reload | Reuse unchanged event worker | Same worker-definition reuse logic |
| Project-memory mutation left a cached context for the current task | `memory_remember` and `memory_forget` declare `invalidates_context` | Existing same-turn invalidation test and manifest check |

## Boundaries deliberately preserved

- One Agent loop, one authoritative WAL, provider-specific model implementation.
- No approvals, policy engine, Cordis/DI container, plugin-internal event bus,
  second memory service, or scheduled background daemon.
- A plugin hook is not authorization, and injected text is not trusted prompt.
- Failed or slow advisory plugins cannot fail a completed task. Model/context
  backend replacement still requires a restart, not a half-hot-swap.
- V1 process protocol and legacy V2 `request_context` / `task_summary` remain
  accepted. Official plugins are bundled but disabled by default.

## Remaining acceptance, not claimed complete

- **Non-durable post-task notifications:** best-effort callbacks are not an
  exactly-once crash-proof task journal. A process killed immediately after
  WAL commit may skip the advisory event, intentionally preserving the simple
  Agent. Any future exactly-once requirement needs a separate measured design.
- **Remote CI:** no new Windows/macOS builds or installed release smoke were
  observed locally. Linux tests are not substitutes for platform-specific CI.
- **Provider integration:** live Parallel/Brave/Exa/Firecrawl, token/billing
  usage and real-model subagent latency require user-configured endpoints and
  repeatable measurements, not fake success claims.
- **Context/cost experiments:** no real-model before/after token savings,
  prompt-cache or recall quality numbers have been produced for this change.
- **Source scans:** source fingerprints support process reuse, not arbitrary
  dependency graph attestation or an OS sandbox; plugins run with user rights.

Refer to test logs in the Devspace working environment and the associated
draft PR for the actual run counts. Do not treat an unexecuted platform matrix
or a mocked provider as a production acceptance pass.
