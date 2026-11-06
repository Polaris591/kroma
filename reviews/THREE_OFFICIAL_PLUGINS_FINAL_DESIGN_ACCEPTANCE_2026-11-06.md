# Kroma: three official plugins against the final design (2026-11-06)

This report distinguishes **implemented and exercised** from **not independently
verified**. The final contracts were recovered from the prior design discussion:
Web 2.0 static-first search/read, project memory zero-maintenance local recall,
and subagent Session-as-Job. Older repository 1.6/0.2/1.0 implementation notes
are not the acceptance standard when the final discussion supersedes them.

## Web 2.0 — source discovery, focused reading and bounded rendering

| Requirement | State | Evidence |
| --- | --- | --- |
| Exactly `web_search` + `web_fetch` public tools | Implemented | `examples/plugins/web/plugin.toml` |
| Default Parallel Fast → Brave → Exa, owner-only paid fallback | Implemented | `web.js`, web tests |
| Keyless search and transparent provider failures | Retained | web tests |
| Static HTTP/text first; preserve original read/continue semantics | Retained | `web.js`, web/http tests |
| `web_fetch(find=...)`: targeted bounded passages | Implemented | web `find` regression |
| Dynamic render only on unreadable static page; owner must opt in | Implemented using optional Firecrawl | web static/render regression |
| No browser/approval UI, no extra orchestration agent | Implemented | plugin shape |
| Live third-party relevance, rate limits, Cloudflare/JS rendering quality | **Not independently verified** | requires actual endpoints/credentials |

## Project memory — zero-maintenance local reference data

| Requirement | State | Evidence |
| --- | --- | --- |
| Git common-dir project ID, local SQLite, worktree sharing | Retained | `scope.rs`, project memory E2E |
| Task-boundary automatic low-trust recall, no match => no pack | Retained/repaired | `store_read.rs`, host model hooks |
| Verify exact latest user quote and accept immediately, max one per turn | Implemented with generic trusted-host tool context | protocol tests + real-host E2E |
| Forget/correct: atomic tombstone, immediate cache invalidation | Implemented | `store.rs`, host model-hooks test |
| Same-topic disputes excluded; one relevant notice; one correction resolves | Implemented when a stable topic is provided | protocol conflict and correction tests |
| Review path content fingerprint, missing/changed file => stale | Implemented for new path-bound review entries | `memory_files`, stale test |
| Automatic 30-day review/7-day pending and last-activity expiration | Implemented | store and protocol tests |
| Active limit 200, automatic archival | Implemented | 202-record regression |
| Default few commands; advanced migration commands not primary UI | Simplified | `/memory` help/status |
| No toolchain required by npm users; bundled disabled official plugin | Implemented in Linux package; CI for others | npm staging/smoke |
| Semantic similarity merging, file-content checks at huge scale, conflict clustering without model-supplied topic | **Not complete** | no independent accepted implementation/test |
| Actual one-week recall quality, error-rate, cache and token metrics | **Not measured** | needs real task trace/experiment |

## Subagents — Session-as-Job, no second Agent Loop

| Requirement | State | Evidence |
| --- | --- | --- |
| `run/status/wait/continue/cancel` and legacy `tasks` | Implemented | Node action lifecycle test |
| Background only while the tool worker stays alive; restart => interrupted/resume | Implemented | job Map + persisted evidence |
| Global three-child concurrency, cancellation of queued jobs | Implemented | global cap/queued cancellation tests |
| Native Kroma child process, saved WAL, resumed session | Retained | Node resume test |
| Five builtin profiles, project/user overrides and model tier routing | Retained | profile tests, child policy |
| Generic host ChildPolicy for tool filters, shell policy, token/turn admission | Implemented without naming plugin IDs in Core | `src/child_policy.rs` |
| Budget exhaustion returns paused/resumable partial result | Implemented | budget+resume test |
| Inherit active parent model endpoint/protocol/profile but not API key | Retained | host metadata + Node test |
| Child reports lower trust; suspicious instructions flagged | Retained | Node test |
| Completed run pruning, bounded evidence, ordered results | Retained | Node suite |
| Unknown tool-name warnings, OS-level read-only isolation | **Not complete / not promised as a sandbox** | profile restrictions are tool-level |
| Cross-platform process cancellation/performance with real models | **Not independently verified** | macOS/Windows matrix required |

## Distribution and final acceptance

The normal native npm package bundles disabled `web`, `subagents`, and
`project-memory` plugin manifests/scripts and the platform's `kroma-memory`
worker; npm launcher exposes the bundle through a **generic** discovery path,
while installed user plugins take priority. No runtime downloads or Cargo
install scripts. The Linux npm smoke exercised `npm pack`, offline-registry
install, plugin listing, enable, update, uninstall and worker ping. The npm CI
workflow was extended to build the memory worker per supported OS and package
all three plugins, but **remote Windows/macOS runners have not been observed in
this environment**.

**Observed local checks:** Rust core library 646 passed / 9 ignored; project
memory Rust tests 50 passed; Node web/subagent/memory test suites 160+ passed;
real-host project memory E2E passed; native npm tooling 25 passed; Linux npm
package install/update/uninstall smoke passed; core all-targets strict Clippy
passed with an isolated cached Cargo home. The final check logs and diffs
must be re-run after any subsequent code change.

The remaining unverified or incomplete requirements above should not be
silently marked completed. They are specific acceptance gates for a production
release, not authorization to skip tests, add approval workflows or redesign
Kroma's simple core.
