# Small live acceptance set

Manual evaluation, not a benchmark framework or offline unit test. Use the same
20 queries, `count: 5`, model (if any), timeout and network for each backend.
Record date and provider; run Exa and an available Brave/SearXNG configuration
separately, never `auto`. Each query incurs one search API call; reading the top
result adds an HTTP call. Do not run without the account owner's cost approval.

For each row record: success/error code, elapsed search milliseconds, whether
the top five include the expected authoritative source, and whether fetching
that result succeeds with useful evidence. Judge relevance manually: a matching
host alone is not a relevance success. Measure elapsed time around the tool call;
record API spend from the provider dashboard (this plugin does not estimate cost).
Summarize hits/20, readable relevant sources/20, median/p95 search latency and
actual billed spend. Do not count snippets as successfully read source pages.

## Historical keyless control run (2026-11-09, Mwmbl-only baseline)

Newer runs must use the current plugin (where keyless `auto` tries `exa_public`,
`firecrawl`, `searxng` when configured, and `mwmbl`). Historical `provider: "free"`
Mwmbl-only run measured on one network:

| Query | Provider | Result |
|---|---|---|
| `postgres index only scan` | mwmbl | 6 relevant, ~0.9 s |
| `rust tokio select cancellation` | mwmbl | 3 relevant, ~0.3 s |
| `欧盟 AI 法案 生效时间` | mwmbl | `NO_RESULTS`, ~1 s |

Conclusion: keyless search is a degraded fallback, not a substitute for a keyed
API, but it fails in about a second instead of spending the budget on blocked
scraped search pages. Repeat before claiming any keyless provider as a reliable
default elsewhere.

| # | Exact query | Expected evidence/source |
|---|---|---|
| 1 | `Node.js v22 AbortSignal.timeout site:nodejs.org` | Node 22 API, timeout behavior |
| 2 | `Node.js v22 fetch stability site:nodejs.org` | Node 22 global fetch reference |
| 3 | `Python 3.12 asyncio.TaskGroup site:docs.python.org` | Versioned TaskGroup docs |
| 4 | `Python 3.13 pathlib Path full_match site:docs.python.org` | New method and version |
| 5 | `Rust std sync OnceLock get_or_init site:doc.rust-lang.org` | Signature, initialization behavior |
| 6 | `tokio select cancellation safety site:docs.rs` | Cancellation safety list |
| 7 | `React 19 upgrade guide site:react.dev` | Official migration guide |
| 8 | `Svelte 5 migration guide site:svelte.dev` | Runes and migration examples |
| 9 | `TypeScript 5.6 release notes site:typescriptlang.org` | Official versioned changes |
| 10 | `PostgreSQL 16 INSERT ON CONFLICT site:postgresql.org` | Versioned SQL reference |
| 11 | `Node.js AbortController 超时 官方文档 site:nodejs.org` | Relevant official API despite Chinese query |
| 12 | `Python asyncio 任务取消 官方文档 site:docs.python.org` | Cancellation handling |
| 13 | `Rust 所有权 借用 官方教程 site:doc.rust-lang.org` | Relevant book chapter |
| 14 | `React useEffect 清理函数 site:react.dev` | Cleanup behavior and examples |
| 15 | `Svelte 5 状态迁移 site:svelte.dev -site:github.com` | Official Svelte guide; no excluded hosts |
| 16 | `HTTP 429 Retry-After site:developer.mozilla.org` | Rate-limit header behavior |
| 17 | `CSS grid-template-columns site:developer.mozilla.org` | Syntax and code examples |
| 18 | `Node.js v22 process exit codes site:nodejs.org` | Long page, exit code list/table |
| 19 | `SQLite pragma journal_mode site:sqlite.org` | Modes and caveats |
| 20 | `WebSailor 2507.02592 site:arxiv.org` | Correct paper, not keyword look-alike |

## Reading and recovery checks

These are separate from search relevance scoring:

- On a long official document, fetch with `max_chars: 1000`, then continue
  using its exact `next_start` and original input URL. Check the join has no gaps.
- Read a table and code example with a sufficiently large output budget. Check
  section, table headers, units and code indentation; fetch surrounding chunks
  when necessary. Merged-cell semantic expansion is not supported.
- With no credential and no SearXNG, expect a Mwmbl response marked
  `mode: "free"` with a warning; with several keys, `auto` must use the first in
  the documented order and make exactly one request. An explicit provider
  without its key returns `CONFIG_ERROR` and makes no request.
- A missing/expired continuation must return `CACHE_MISS`; start from zero.
- A configured provider must fail without a second provider call. Only the free
  chain (SearXNG, then Mwmbl) advances. Check `attempts` on failure. Use local
  tests for deterministic failures rather than deliberately hitting provider
  limits.

## Current evidence

Live smoke checks with local credentials: Parallel, You.com, Exa and Tavily each
returned two results for `Node.js AbortSignal timeout site:nodejs.org`, all on
nodejs.org or its subdomains. This confirms connectivity/authentication and
request compatibility only, not comparative relevance or billed cost.

The free chain was measured on one network in 2026-11-09: Mwmbl answers English
technical queries in about a second and returns `NO_RESULTS` for a Chinese
query. Brave had no key in that environment, so its coverage remains mocked.
Full 20-query relevance/cost evaluation: **not run**.
