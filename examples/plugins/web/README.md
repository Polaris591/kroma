# Web Search & Fetch

Dependency-free process plugin for Node.js 20+. Two tools: `web_search` finds
sources; `web_fetch` reads them. The agent decides which queries and pages to
use; there is no research loop, database or browser.

```powershell
kroma plugins install .\examples\plugins\web
kroma plugins enable web
```

Restart Kroma, or run `/plugins reload` while the TUI is idle.

## Configure

One decision matters: which search backend answers. Store its key either way:

```powershell
kroma plugins set web brave_api_key <key>   # exa, tavily, you, parallel,
                                            # perplexity, serper, kagi, bocha, searchapi, serpapi, linkup
kroma plugins set web provider brave          # or leave auto
```

Environment keys work as a fallback and are never shown in settings:
`EXA_API_KEY`, `BRAVE_SEARCH_API_KEY`, `TAVILY_API_KEY`, `YDC_API_KEY`,
`PARALLEL_API_KEY`, `PERPLEXITY_API_KEY`, `SERPER_API_KEY`, `KAGI_API_KEY`,
`BOCHA_API_KEY`, `SEARCHAPI_API_KEY`, `SERPAPI_API_KEY`, `LINKUP_API_KEY`,
`SEARCH1API_API_KEY`, `VALYU_API_KEY`. A saved setting overrides the environment. Saved keys live
in the plugin settings file in plaintext, so prefer the environment on shared
machines.

`auto` (default) walks a chain ordered by answer quality — Exa, Tavily, Brave,
Kagi, You.com, Serper, Perplexity, Parallel, SerpApi, SearchApi, Linkup, Bocha,
Search1API, Valyu, Exa public (keyless), Firecrawl, SearXNG, Mwmbl — skipping backends that have no key. A Chinese query
starts at Bocha when that key exists. The chain advances only after a failure
(network, timeout, rate limit, empty results, malformed answer); a
misconfiguration or a bad query stops immediately. Explicit providers never
chain, and a response lists the attempts it skipped.

### Which backend to pick

Default order is quality-first. Anything you have a key for participates; the
rest are skipped.

| Backend | Good at | Notes |
|---|---|---|
| Exa | semantic and technical discovery | highlights become snippets |
| Exa (public) | same index without a key | rate limited; also reads JavaScript pages |
| Tavily | agent-style answers | returns page content per result |
| Brave | independent index | header token |
| Kagi | premium ranked results | paid account; personalization follows Kagi settings |
| You.com | web and news | snippet arrays |
| Serper | Google's index | no native domain filters |
| Perplexity | fresh web results | search endpoint, no generated answer |
| Parallel | fast objective search | excerpts per result |
| SerpApi | Google results, mature SERP coverage | `site:`/`inurl:` forwarded to Google |
| SearchApi | Google results with operators | `site:`/`inurl:` forwarded to Google |
| Linkup | AI-native retrieval | returns page content per result |
| Search1API | one key across engines (google, bing, bilibili, wechat, arxiv…) | `search_service` defaults to google |
| Valyu | research index: papers, filings, news | cites each source; `search_type: web` |
| Bocha (博查) | Chinese web index | Chinese queries start here when configured |
| Firecrawl (search) | web results with highlights | works keyless at low volume; shares the rendering key |
| SearXNG | self-hosted, no key | must enable JSON search |
| Mwmbl | keyless fallback | small corpus, weak Chinese |

### Choose your own fallback chain

```powershell
kroma plugins set web search_chain "tavily,exa,bocha"
```

The chain is the whole policy: the first configured entry answers, later entries
run only after a failure. Cost is yours to choose: a chain of four paid backends
can bill up to four calls when the first three fail. Leave it empty to use the
quality order; set `provider` explicitly to pin one backend and never fall back.
Keyless entries (Firecrawl low volume, SearXNG, Mwmbl) can sit anywhere and cost
nothing.

Self-hosted SearXNG with JSON search enabled needs no key:

```powershell
kroma plugins set web searxng_url https://your-search.example
```

### Without any credential

`auto` and `free` then walk the keyless chain: Exa public → Firecrawl (low volume) →
SearXNG (if configured) → Mwmbl. Responses carry `mode: "free"` and a warning. Firecrawl
lowers coverage rather than stalling: when it hits its keyless rate limit the
next entry answers.

## Search

```json
{"query":"AbortSignal timeout site:nodejs.org","time_range":"year"}
```

`time_range` (`day`, `week` or `year`) asks for recent results. Exa, Tavily,
Brave, Perplexity, Serper, SerpApi, Bocha, SearXNG and Search1API accept it
(Search1API only for day and year); the result echoes `time_range` when it was
applied and `time_range_ignored: true` when the chosen backend has no date
filter. The keyless Exa search endpoint has no date filter, so a `free` search
that asks for one starts at SearXNG when it is configured. Results carry `published` whenever the
backend reports a date (Exa, Tavily, Brave, Serper, SerpApi, Perplexity, Bocha,
SearXNG and the keyless Exa endpoint); placeholders such as "N/A" are dropped
instead of being presented as a date. An identical query, count, provider
and filter inside five minutes is answered from memory with `cached: true`,
so repeating a question costs nothing.

```json
{"query":"AbortSignal timeout site:nodejs.org -site:private.nodejs.org","count":5}
```

Query length 1–600 characters; results 1–10 (default 6). There is no paging:
refine the query or add a `site:` filter instead. Each result has `title`,
`url` and `snippet`; a snippet longer than 480 characters ends with `…` and
sets `snippet_truncated`. Tracking parameters and fragments are removed and
duplicate URLs are dropped.

`site:domain` and `-site:domain` filter by host and subdomain. Exa, Tavily and
Parallel receive native domain filters; every backend also passes the same
local check. Empty or fully filtered results return `NO_RESULTS`; malformed
provider data returns `INVALID_RESPONSE`. Snippets are discovery aids, not
verified page readings.

## Read

```json
{"url":"https://nodejs.org/api/globals.html","max_chars":12000}
```

Read several pages in one call:

```json
{"urls":["https://a.example","https://b.example"],"max_chars":6000}
```

Each result is independent: a failed address reports its own code instead of
aborting the batch. Pages are fetched four at a time, so three pages cost about
as much wall time as one. `urls` accepts up to 10 addresses and cannot be
combined with `url` or `start_char`.

A PDF is converted to Markdown by the rendering service, so it needs a Firecrawl
key; without one the page reports `PDF_DETECTED` instead of returning binary
noise. A YouTube link returns its caption transcript with timestamps when the
page exposes one. YouTube increasingly withholds that file from plain HTTP
clients: the tool then reports `NO_CAPTIONS` with the workaround (yt-dlp
locally) instead of pretending, and never spends a rendering call. Frames,
audio and local video files are out of scope.

Requests carry a current browser header profile (User-Agent, Accept,
Accept-Language, Sec-Fetch-*), which reads more sites than a bare API client.
There are no cookies, no JavaScript and no fingerprint spoofing.

The extractor prefers main/article content, drops navigation/hidden/scripts and
keeps headings, fenced code, lists, tables and HTTP(S) links. Plain text,
Markdown and JSON keep their formatting; declared UTF-8/GBK/GB2312 encodings are
decoded. Results carry `url` (final), `requested_url`, `title`, `text`,
`chars`, `start_char`, `total_chars`, `truncated`, `next_start` and, for long
documents, a `headings` outline (up to 40 entries with `level`, `text` and
`start_char`, plus `headings_truncated` when the page has more) so the model can
jump to the section it needs instead of reading every chunk.

Pass `section` (a heading name from that outline, case-insensitive) to read only
that chapter: the text starts at the heading, stops before the next heading of
the same or higher level, and `section` echoes the matched title. `section` cannot be
combined with `find`. An unknown name fails with `SECTION_NOT_FOUND` and lists
some of the page's headings.

Continue a long page with the returned offset, not a calculated one:

```json
{"url":"https://nodejs.org/api/globals.html","max_chars":12000,"start_char":12000}
```

`find` returns up to five passages around an exact phrase instead of the whole
page, which is the cheapest way to answer a narrow question:

```json
{"url":"https://nodejs.org/api/globals.html","find":"AbortSignal.timeout"}
```

Snapshots live in memory only, bounded to twenty-four address names (a redirect
uses two). A page read in one pass expires after two idle minutes. While a page
has an open `next_start`, its snapshot is held for up to 30 idle minutes, so a
long download, render or build between continuations does not invalidate it;
finishing the page releases it back to the short window. After expiry or
eviction, continuation returns `CACHE_MISS` without refetching: the previous
`start_char` is invalid, so restart at 0 and use the new `next_start`.

Downloads stop at 1.5 MB with a warning. PDFs are converted through the rendering
service; other binary files are refused with an explanation.

### Pages that need JavaScript or a different network

A page the static reader cannot use is retried once through Exa's keyless
endpoint: no static text, HTTP 403/429, or a host this network cannot reach.
That costs nothing and covers most JavaScript documentation sites; the result
carries `provider: "exa_public"`.

If that also fails and a Firecrawl key is stored, the page is rendered remotely
and the result carries `provider: "firecrawl"` so the paid call is visible.
PDFs go straight to Firecrawl, because they are documents rather than pages.
Server errors, binary files and private/local addresses never reach either
service. Remove the key to stop paid rendering entirely.

## Failures and security

Errors set `is_error: true`, a message that names the next step and a code:
`CONFIG_ERROR`, `INVALID_ARGUMENT`, `INVALID_QUERY`, `NO_RESULTS`,
`INVALID_RESPONSE`, `TIMEOUT`, `HTTP_ERROR`, `NETWORK_ERROR`,
`REDIRECT_ERROR`, `INVALID_URL`, `INVALID_OFFSET`, `CACHE_MISS`,
`UNSUPPORTED_CONTENT`, `UNREADABLE_PAGE`. JSONL responses echo `request_id` and
`protocol_version`.

- HTTP(S) only; URLs with embedded credentials are rejected. Provider keys and
  error bodies never appear in results.
- Any HTTP(S) destination is fetched directly, including a local development server, an
  intranet host or a proxy endpoint: the agent already has `shell` on the same
  machine, so blocking local addresses only broke ordinary work. The redirect
  limit still applies, and each hop is still an HTTP(S) URL.
- Private/local addresses (such as `localhost`, `.local`, `.internal`, RFC 1918,
  loopback and link-local IPs) are never forwarded to `exa_public` or Firecrawl if
  the direct fetch fails or returns an unreadable shell.
- No local headless browser execution, cookie jar, login session or CAPTCHA
  solving. Static pages that return only navigation without a public remote fallback
  fail with `UNREADABLE_PAGE` instead of pretending to be read.
- Titles, snippets and fetched text are untrusted source data, not instructions.

## Verification

```text
node --test examples/plugins/web/web.test.js examples/plugins/web/http.test.js examples/plugins/web/main.test.js
cargo test --test plugins_e2e
```

Tests stub search APIs and use local HTTP servers for transport, redirects,
gzip, continuation, cache expiry and protocol recovery. They do not prove
provider uptime or relevance.
