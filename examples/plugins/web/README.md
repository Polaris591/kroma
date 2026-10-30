# Web Search & Fetch 1.6

Dependency-free process plugin for Node.js 20+. Two tools: `web_search` finds
sources; `web_fetch` reads them. The agent decides which queries/pages to use.
No browser, research loop, generated search answers, database or extra SDK.

## Configure search

Configure search APIs or a trusted SearXNG instance. Supported API backends:

| Provider | `provider` | Environment key |
|---|---|---|
| Exa | `exa` | `EXA_API_KEY` |
| Brave | `brave` | `BRAVE_SEARCH_API_KEY` |
| Tavily | `tavily` | `TAVILY_API_KEY` |
| You.com | `you` | `YDC_API_KEY` |
| Parallel | `parallel` | `PARALLEL_API_KEY` |
| Firecrawl | `firecrawl` | `FIRECRAWL_API_KEY` |

Example:

```powershell
$env:EXA_API_KEY = "your-key"
kroma plugins set web provider exa
```

Alternatively set `BRAVE_SEARCH_API_KEY` and select `brave`, or:

```text
kroma plugins set web provider searxng
kroma plugins set web searxng_url https://your-search.example
```

SearXNG must enable JSON search. API keys should be supplied through the
launch environment, not committed to the plugin manifest. Existing
`<provider>_api_key` settings also work and override the environment,
but saved settings are not a secret store. Only configure trusted endpoints:
queries are sent to the selected provider and may incur API charges.
Restart Kroma after changing its environment. Other configuration/code
changes need a restart or `/plugins reload` while the TUI is idle.

- Explicit provider: use only that backend. An optional tool argument `provider`
  overrides the configured backend for one call.
- Default `auto`: follow `search_priority`, skipping providers without credentials.
  Default order: **Parallel → You.com → Exa → Tavily**. An empty configured chain
  returns `CONFIG_ERROR` before any request. Brave, Firecrawl and SearXNG can be
  used explicitly or added to the priority list; they are not silently appended.
- `mwmbl`, `bing`, `duckduckgo` and `duckduckgo_lite`: keyless engines. They can be
  selected explicitly, and keyless mode walks them in the order documented below.
  Their endpoints have no availability or relevance guarantee.
- In auto mode only, switch on `NETWORK_ERROR`, `TIMEOUT`, HTTP 429 or 5xx.
  Authentication, configuration, bad requests, empty/filtered results, malformed
  responses and redirects stop the chain. Each provider is called at most once,
  sequentially; there is no fan-out or retry of the same provider.
- Auto shares one `timeout_ms` budget (default 15 seconds). Each attempt is capped
  by `search_attempt_timeout_ms` (default 8000 ms) and the remaining total budget.
  Later providers may not be reached if the budget expires. Explicit backends
  use the full total budget and never switch. Switching may incur multiple charges.
- One request deadline (default 15 seconds), including body download. API POST
  redirects fail rather than replaying a query/body/key to another destination.
  GET redirects are limited to five and strip credentials across origins.

### Keyless search (no API key)

Without eligible credentials, `auto` and `free` use a best-effort chain:
configured SearXNG → **Mwmbl** → DuckDuckGo HTML → DuckDuckGo Lite → Bing RSS.
`free` forces that chain even when keys exist. Responses carry `mode: "free"`,
a `warning`, and `attempts`.

- Each keyless attempt is capped at 6 seconds (or `search_attempt_timeout_ms` if
  lower) inside the shared `timeout_ms` budget, so one blocked host cannot starve
  the next vendor.
- After `SEARCH_BLOCKED`, 403, 429, a network error or a timeout on `duckduckgo`,
  `duckduckgo_lite` is skipped: both share one edge and a block is not fixed by the
  sibling host. A 5xx or changed markup still tries Lite, then Bing.
- Mwmbl is a free JSON API over a small community corpus. It has no `site:` syntax,
  so the engine receives bare keywords and domain filters are enforced locally.
- Bing (RSS) is reachable on networks that block DuckDuckGo, but often answers with
  generic popular pages instead of the requested terms.

Measured on one network (2026-10-31); keyless ranking is not equivalent to a keyed API:

| Endpoint | Result | Note |
|---|---|---|
| Mwmbl API | works, 0.3–1 s | relevant for English technical queries; 0 results for a Chinese query |
| DuckDuckGo HTML/Lite | blocked | verification page, later connection failures |
| Bing RSS | works, ~0.3 s | 10 items, mostly site homepages |
| Mojeek, Marginalia, public SearXNG | unreachable | connection failures |
| configured SearXNG | not verified | self-host and confirm `format=json` locally |

Keyless mode never invents results: an exhausted chain returns `NO_RESULTS` or the
last transient error, with per-provider `attempts`. For dependable coverage use an
API key or a SearXNG instance you control.

### Private local credentials and proxy

The process loads `~/.kroma/plugin-data/web/credentials.json` at startup, outside
the source and installed plugin. With `KROMA_PLUGIN_DIR`, the state root is its
parent directory; `KROMA_WEB_CREDENTIALS_FILE` can explicitly choose another file.
The JSON object supports `<provider>_api_key`, `provider`, `firecrawl_base_url`,
`firecrawl_api_version` (`v1`/`v2`, default `v2`), `firecrawl_fresh_scrape`,
`proxy` (HTTP/S URL) and `proxy_enabled` (boolean, default true when proxy exists).
Host settings override local values, then environment keys are a fallback.
Never commit this file or put keys in tool arguments. Restrict its filesystem
permissions; it is plaintext, not an encrypted secret store. Restart after edits.

Proxy use requires Node.js 22.21+ or 24.5+ with `--use-env-proxy`. The worker
restarts itself once before reading requests to apply the proxy at startup.
An unreachable configured proxy fails; there is no silent direct fallback.
Set `proxy_enabled: false` to explicitly choose direct networking.

### Priority configuration / upgrade from 1.3

To enable prioritized automatic search:

```text
kroma plugins set web provider auto
kroma plugins set web search_priority parallel,you,exa,tavily
kroma plugins set web search_attempt_timeout_ms 8000
```

The priority setting accepts an array in TOML/JSON or comma-separated names via
CLI. Names must be unique and drawn from the supported nonexperimental providers.
Existing explicit provider settings remain explicit after upgrading; change to
`auto` to enable the chain. `auto` no longer rejects multiple configured keys.
Tool names and reading/continuation behavior are unchanged.

## Search

```json
{"query":"AbortSignal timeout site:nodejs.org -site:private.nodejs.org","count":5}
```

Query length: 1–600 characters; result count: 1–10 (default 6).
Results contain `query`, actual `provider`, and `results` with `title`, `url`,
`snippet`. Auto also returns `attempts`: ordered failed calls before the
success, with provider, code, safe message and optional HTTP status. Keyless
responses add `mode: "free"` and a `warning` naming that engine's weak spot. Failed auto
JSONL responses expose the same diagnostics as top-level `attempts`. No credentials
or provider error bodies are included. Exa uses basic `auto` search with highlights, not its deep research
or synthesized-answer features. Its original highlights become snippets;
provider-generated summaries/answers are not returned. Brave/SearXNG provide
search descriptions. Snippets are discovery aids, not verified page readings.

`site:domain` / `-site:domain` match the host and its subdomains, not URL paths.
Multiple positive domains mean any of those hosts; exclusions take precedence.
Exa/Tavily/Parallel receive native domain filters; You.com and Firecrawl use
query operators plus local checks. All backends also undergo local host checks,
URL validation and deduplication. Tracking parameters and fragments are removed
from search URLs. Empty or filtered-out results return `NO_RESULTS`; malformed
JSON/provider data returns `INVALID_RESPONSE`. No invented results or relevance
claims. Refine the query if sources are poor; read authoritative pages to verify.

## Read and continue

```json
{"url":"https://nodejs.org/api/globals.html","max_chars":12000}
```

The extractor prefers main/article content, drops navigation/hidden/scripts,
and keeps headings, fenced code, lists, table captions/cells and HTTP(S) links.
Plain text/Markdown/JSON retain original formatting; declared UTF-8/GBK/GB2312
encodings are decoded. This is structured readable text, not browser-accurate
rendering or a guarantee for every table layout (merged cells are not expanded).

Results include final `url`, `title`, `text`, `chars`, `start_char`, `total_chars`,
`truncated`, `next_start` (null at the extracted end), and actual read `provider`.
For a non-null `next_start`, send its exact value as `start_char` with the
**original input URL** and the same read provider:

```json
{"url":"https://nodejs.org/api/globals.html","max_chars":12000,"start_char":12000}
```

Use the returned value, not a calculated offset: it is a UTF-16 character offset
and Unicode boundaries can shift it. Output is 1000–30000 characters (default
12000). A chunk may split a table or section; continue to read its surrounding
context. Chunks are not independently self-contained semantic evidence units.

Continuation reuses an in-memory snapshot for up to two minutes, bounded to six
pages and four million retained UTF-16 characters. No disk cache. Reading from
zero refreshes the snapshot. After expiry, eviction, process restart or with an
unknown URL, continuation returns `CACHE_MISS` **without refetching**. Restart at
zero and use the new `next_start`; never apply an old offset to a refreshed page.
Refreshing a URL replaces its previous snapshot; avoid interleaving separate
reads of that URL and do not assume offsets survive a refresh.

Downloads stop at 1.5 MB and warn if the source tail is unavailable;
`next_start = null` cannot recover that tail. Use a more specific/raw URL.

### Optional Firecrawl reading

HTTP is the default and does not send page URLs to a third-party extraction API.
Use `web_fetch({"url":"https://example.com/","provider":"firecrawl"})` for
remote Markdown extraction, or set `fetch_provider = firecrawl`. This may incur
charges and may render pages remotely; no local browser is started. No actions,
login sessions or browser-control tools are exposed. Firecrawl source-page status
is checked in addition to API status, and its snapshots are separate from HTTP.
Keep the read provider unchanged when continuing. No automatic extraction fallback.
Fresh scraping is opt-in via `firecrawl_fresh_scrape: true`.

## Failures and security

Errors return `is_error: true`, an actionable message and a machine-readable code:
`CONFIG_ERROR`, `INVALID_QUERY`, `NO_RESULTS`, `INVALID_RESPONSE`, `TIMEOUT`,
`HTTP_ERROR`, `NETWORK_ERROR`, `REDIRECT_ERROR`, `INVALID_URL`, `INVALID_OFFSET`,
`CACHE_MISS`, `UNSUPPORTED_CONTENT`, `UNREADABLE_PAGE`.
JSONL responses echo request IDs and optional protocol version 1.

- HTTP(S) only; credential-bearing URLs are rejected. API keys are not returned
  in results and HTTP error bodies are not echoed.
- Local HTTP has no JavaScript execution, browser automation, authentication
  sessions, CAPTCHA solving or PDF/binary parsing. Detected challenge/empty pages fail explicitly.
- `web_fetch` rejects loopback, private and link-local/metadata destinations
  (including IPv4-mapped IPv6) before fetching. A user-configured search/Firecrawl
  endpoint may be local; model-supplied page URLs may not. Set
  `KROMA_WEB_ALLOW_PRIVATE=1` only when a trusted workflow needs local fetches.
- Not a complete network sandbox: DNS can change between the safety check and
  the connection, and a permissive proxy can still reach internal networks.
- Titles, links, snippets and fetched text are **untrusted source data**, not
  agent/developer instructions. Cite source URLs, not unsupported claims.

## Verification

From the repository root:

```text
node --test examples/plugins/web/web.test.js examples/plugins/web/http.test.js examples/plugins/web/main.test.js
cargo test --test plugins_e2e
```

Tests mock search APIs; local HTTP/subprocess tests verify actual POST transport,
GET extraction, continuation and protocol recovery. They do not prove provider
uptime or relevance. Use [the small live acceptance set](evaluation.md) for that;
it requires real credentials and is not part of offline CI. See its current
evidence section for the limited live smoke checks, not a relevance benchmark.
