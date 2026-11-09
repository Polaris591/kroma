"use strict";
const {WebError, httpUrl, download} = require("./http");
const {extractPage, plainText} = require("./page");
const {createStore} = require("./cache");
// Mirrors the [settings] block in plugin.toml for the two behavioural defaults.
const DEFAULTS = Object.freeze({provider: "auto", max_results: 6});
const TIMEOUT_MS = 15000;
const DEFAULT_MAX_CHARS = 12000;
const SOURCE_WARNING = "Source exceeded the 1.5 MB download limit; the document tail is unavailable. Use a more specific/raw URL.";
const KEY_ENV = Object.freeze({exa: "EXA_API_KEY", brave: "BRAVE_SEARCH_API_KEY",
  tavily: "TAVILY_API_KEY", parallel: "PARALLEL_API_KEY", you: "YDC_API_KEY",
  perplexity: "PERPLEXITY_API_KEY", serper: "SERPER_API_KEY", kagi: "KAGI_API_KEY", bocha: "BOCHA_API_KEY",
  firecrawl: "FIRECRAWL_API_KEY", searchapi: "SEARCHAPI_API_KEY",
  serpapi: "SERPAPI_API_KEY", linkup: "LINKUP_API_KEY",
  search1api: "SEARCH1API_API_KEY", valyu: "VALYU_API_KEY"});
const FIRECRAWL_BASE = "https://api.firecrawl.dev";
const FREE_WARNING = "No search credential is configured, so this used a keyless backend (Firecrawl low volume, SearXNG or Mwmbl): smaller coverage, weaker Chinese results. Set a provider key for dependable results.";

function apiKey(provider, settings) {
  const key = settings[provider + "_api_key"] || process.env[KEY_ENV[provider]];
  if (typeof key !== "string" || !key.trim()) {
    throw new WebError("CONFIG_ERROR", "Configure " + KEY_ENV[provider] + " in the environment or run: kroma plugins set web " + provider + "_api_key <key>");
  }
  return key.trim();
}
function hasKey(provider, settings) {
  return !!(settings[provider + "_api_key"] || process.env[KEY_ENV[provider]]);
}
function passages(value) {
  return Array.isArray(value) ? value.filter(x => typeof x === "string").join("\n") : "";
}
function bounded(value, fallback, lo, hi) {
  const n = Number(value);
  return value !== null && value !== "" && Number.isFinite(n)
    ? Math.max(lo, Math.min(hi, Math.floor(n))) : fallback;
}

// Successful searches are reused briefly: repeating the same question should
// not pay a second provider call. Failures are never cached.
const SEARCH_CACHE = new Map();
const SEARCH_CACHE_TTL = 300000;
const SEARCH_CACHE_LIMIT = 50;

// Providers that accept a freshness filter, and how each names it.
const TIME_FILTERS = new Set(["exa", "tavily", "brave", "perplexity", "serper", "serpapi", "bocha", "searxng", "search1api"]);
const TIME_VALUES = ["day", "week", "year"];

function isoSince(range) {
  const days = {day: 1, week: 7, year: 365}[range] ?? 1;
  return new Date(Date.now() - days * 86400000).toISOString().slice(0, 10);
}
function googleTbs(range) {
  return "qdr:" + ({day: "d", week: "w", year: "y"}[range] ?? "d");
}
function searchCacheKey(query, count, provider, chain, range) {
  return JSON.stringify([query, count, provider, chain ?? null, range ?? ""]);
}

// Providers name the publication date differently; keep whichever one is there.
const DATE_KEYS = ["publishedDate", "published_date", "publication_date", "datePublished", "date", "published", "publish_date", "age", "page_age"];
function dateOf(item) {
  const key = DATE_KEYS.find(name => typeof item?.[name] === "string" && item[name].trim());
  const value = key ? item[key].trim() : "";
  const blank = new Set(["n/a", "na", "unknown", "-", "null"]);
  return value && !blank.has(value.toLowerCase()) ? value : undefined;
}

function outline(text) {
  const items = [];
  let truncated = false;
  const heading = new RegExp(String.raw`^(#{1,6})\s+(.+?)\s*$`, "gm");
  for (const match of text.matchAll(heading)) {
    if (items.length >= 40) { truncated = true; break; }
    items.push({level: match[1].length, text: match[2].replace(new RegExp(String.raw`\[([^\]]*)\]\([^)]*\)`, "g"), "$1").slice(0, 120), start_char: match.index});
  }
  return {items, truncated};
}

function searchSites(query) {
  return [...query.matchAll(/(?:^|\s)(-?)site:([^\s]+)/gi)].map(match => {
    try {
      return {exclude: match[1] === "-", host: httpUrl(/^https?:\/\//i.test(match[2]) ? match[2] : `https://${match[2]}`).hostname};
    } catch { throw new WebError("INVALID_QUERY", "site: must specify a valid domain"); }
  });
}

function normalizeResults(results, sites, count) {
  if (!Array.isArray(results)) throw new WebError("INVALID_RESPONSE", "Search provider returned an invalid results array");
  const includes = sites.filter(site => !site.exclude), excludes = sites.filter(site => site.exclude);
  const seen = new Set(), clean = [];
  for (const item of results) {
    if (!item || typeof item.url !== "string") continue;
    let url;
    try { url = httpUrl(item.url); } catch { continue; }
    const matches = site => url.hostname === site.host || url.hostname.endsWith(`.${site.host}`);
    if ((includes.length && !includes.some(matches)) || excludes.some(matches)) continue;
    url.hash = "";
    for (const key of [...url.searchParams.keys()]) {
      if (/^(utm_.+|fbclid|gclid|msclkid)$/i.test(key)) url.searchParams.delete(key);
    }
    if (seen.has(url.href)) continue;
    seen.add(url.href);
    const snippet = plainText(item.snippet);
    clean.push({title: plainText(item.title).slice(0, 180), url: url.href,
      ...(item.published ? {published: plainText(item.published).slice(0, 40)} : {}),
      snippet: snippet.length > 480 ? snippet.slice(0, 480) + "…" : snippet,
      ...(snippet.length > 480 ? {snippet_truncated: true} : {})});
    if (clean.length >= count) break;
  }
  return clean;
}

async function searchProvider(provider, query, settings, timeout, count, sites, range) {
  let url, headers, types, parse, body, param, term;
  const includes = sites.filter(site => !site.exclude).map(site => site.host);
  const excludes = sites.filter(site => site.exclude).map(site => site.host);
  const keywords = query.replace(/(?:^|\s)-?site:[^\s]+/gi, " ").trim() || query;
  if (provider === "exa") {
    url = new URL("https://api.exa.ai/search");
    headers = {"x-api-key": apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    // Exa has native domain filters; keep the local checks as well.
    body = JSON.stringify({query: keywords, type: "auto", numResults: count,
      contents: {highlights: true}, ...(range ? {startPublishedDate: isoSince(range)} : {}),
      ...(includes.length ? {includeDomains: includes} : {}),
      ...(excludes.length ? {excludeDomains: excludes} : {})});
    parse = text => JSON.parse(text).results?.map(x => x && ({title: x.title, url: x.url, published: dateOf(x), snippet: passages(x.highlights)}));
  } else if (provider === "tavily") {
    url = new URL("https://api.tavily.com/search");
    headers = {Authorization: "Bearer " + apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({query: keywords, search_depth: "basic", max_results: count,
      include_answer: false, include_raw_content: false, ...(range ? {time_range: range} : {}),
      ...(includes.length ? {include_domains: includes} : {}),
      ...(excludes.length ? {exclude_domains: excludes} : {})});
    parse = text => JSON.parse(text).results?.map(x => x && ({title: x.title, url: x.url, published: dateOf(x), snippet: x.content}));
  } else if (provider === "you") {
    url = new URL("https://ydc-index.io/v1/search");
    headers = {"X-API-Key": apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({query, count});
    parse = text => JSON.parse(text).results?.web?.map(x => x && ({title: x.title, url: x.url, published: dateOf(x),
      snippet: passages(x.snippets) || x.description}));
  } else if (provider === "parallel") {
    url = new URL("https://api.parallel.ai/v1/search");
    headers = {"x-api-key": apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({objective: query, search_queries: [keywords], mode: "fast",
      max_chars_total: count * 1800,
      advanced_settings: {max_results: count, ...(sites.length ? {source_policy: {
        ...(includes.length ? {include_domains: includes} : {}),
        ...(excludes.length ? {exclude_domains: excludes} : {})
      }} : {})}});
    parse = text => JSON.parse(text).results?.map(x => x && ({title: x.title, url: x.url, published: dateOf(x), snippet: passages(x.excerpts)}));
  } else if (provider === "brave") {
    url = new URL("https://api.search.brave.com/res/v1/web/search");
    url.searchParams.set("count", String(count));
    if (range) url.searchParams.set("freshness", {day: "pd", week: "pw", year: "py"}[range]);
    headers = {"X-Subscription-Token": apiKey(provider, settings)};
    types = ["application/json"];
    parse = text => JSON.parse(text).web?.results?.map(x => x && ({...x, snippet: x.description, published: dateOf(x)}));
  } else if (provider === "perplexity") {
    url = new URL("https://api.perplexity.ai/search");
    headers = {Authorization: "Bearer " + apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({query, ...(range ? {search_recency_filter: range} : {})});
    parse = text => JSON.parse(text).results?.map(x => x && ({title: x.title, url: x.url, published: dateOf(x), snippet: x.snippet}));
  } else if (provider === "serper") {
    // Google results through Serper; the API itself has no domain parameters.
    url = new URL("https://google.serper.dev/search");
    headers = {"X-API-KEY": apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({q: query, num: count, ...(range ? {tbs: googleTbs(range)} : {})});
    parse = text => JSON.parse(text).organic?.map(x => x && ({title: x.title, url: x.link, published: dateOf(x), snippet: x.snippet}));
  } else if (provider === "kagi") {
    url = new URL("https://kagi.com/api/v1/search");
    headers = {Authorization: "Bearer " + apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({query});
    parse = text => JSON.parse(text).data?.search?.map(x => x && ({title: x.title, url: x.url, published: dateOf(x), snippet: x.snippet}));
  } else if (provider === "bocha") {
    // Chinese web index built for AI use; Bing-style response envelope.
    url = new URL("https://api.bochaai.com/v1/web-search");
    headers = {Authorization: "Bearer " + apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({query, count, summary: false,
      ...(range ? {freshness: {day: "oneDay", week: "oneWeek", year: "oneYear"}[range]} : {})});
    parse = text => JSON.parse(text).data?.webPages?.value?.map(x => x && ({title: x.name, url: x.url, published: dateOf(x),
      snippet: x.snippet || x.summary}));
  } else if (provider === "firecrawl") {
    // Works with or without a key: the vendor allows low-volume keyless use.
    url = new URL(FIRECRAWL_BASE + "/v2/search");
    headers = {"Content-Type": "application/json",
      ...(firecrawlKey(settings) ? {Authorization: "Bearer " + firecrawlKey(settings)} : {})};
    types = ["application/json"];
    body = JSON.stringify({query, limit: count});
    parse = text => {
      const data = JSON.parse(text);
      if (data.success === false) throw new WebError("INVALID_RESPONSE", "Firecrawl search reported failure");
      return data.data?.web?.map(x => x && ({title: x.title, url: x.url, published: dateOf(x), snippet: x.description}));
    };
  } else if (provider === "searchapi") {
    url = new URL("https://www.searchapi.io/api/v1/search");
    url.searchParams.set("engine", "google");
    url.searchParams.set("q", query);
    url.searchParams.set("api_key", apiKey(provider, settings));
    types = ["application/json"];
    parse = text => JSON.parse(text).organic_results?.map(x => x && ({title: x.title, url: x.link, published: dateOf(x), snippet: x.snippet}));
  } else if (provider === "serpapi") {
    url = new URL("https://serpapi.com/search");
    url.searchParams.set("engine", "google");
    url.searchParams.set("q", query);
    url.searchParams.set("api_key", apiKey(provider, settings));
    if (range) url.searchParams.set("tbs", googleTbs(range));
    types = ["application/json"];
    parse = text => JSON.parse(text).organic_results?.map(x => x && ({title: x.title, url: x.link, published: dateOf(x), snippet: x.snippet}));
  } else if (provider === "linkup") {
    url = new URL("https://api.linkup.so/v1/search");
    url.searchParams.set("q", query);
    url.searchParams.set("depth", "standard");
    url.searchParams.set("outputType", "searchResults");
    headers = {Authorization: "Bearer " + apiKey(provider, settings)};
    types = ["application/json"];
    parse = text => JSON.parse(text).results?.map(x => x && ({title: x.name, url: x.url, published: dateOf(x),
      snippet: x.content || x.snippet}));
  } else if (provider === "search1api") {
    // Aggregator with per-engine routing; google is the default engine.
    url = new URL("https://api.search1api.com/search");
    headers = {Authorization: "Bearer " + apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({query, max_results: count, ...(range && range !== "week" ? {time_range: range} : {}),
      ...(includes.length ? {include_sites: includes} : {}),
      ...(excludes.length ? {exclude_sites: excludes} : {})});
    parse = text => JSON.parse(text).results?.map(x => x && ({title: x.title, url: x.link, published: dateOf(x), snippet: x.snippet || x.content}));
  } else if (provider === "valyu") {
    // Research-oriented index with citations; web scope keeps it a search tool.
    url = new URL("https://api.valyu.ai/v1/search");
    headers = {"x-api-key": apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({query, search_type: "web", max_num_results: count, response_length: "short"});
    parse = text => JSON.parse(text).results?.map(x => x && ({title: x.title, url: x.url, published: dateOf(x), snippet: x.content}));
  } else if (provider === "exa_public") {
    // Keyless Exa: same index, no credential, rate limited.
    return exaSearchPublic(query, count, timeout);
  } else if (provider === "searxng") {
    if (!settings.searxng_url) throw new WebError("CONFIG_ERROR", "Configure searxng_url with JSON search enabled");
    url = httpUrl(settings.searxng_url);
    if (!url.pathname.endsWith("/search")) url.pathname = url.pathname.replace(/\/$/, "") + "/search";
    url.searchParams.set("format", "json");
    if (range) url.searchParams.set("time_range", range);
    types = ["application/json"];
    parse = text => JSON.parse(text).results?.map(x => x && ({...x, snippet: x.content, published: dateOf(x)}));
  } else if (provider === "mwmbl") {
    // Free, keyless community index with a documented JSON API; no site: syntax.
    url = new URL("https://api.mwmbl.org/api/v1/search/");
    types = ["application/json"];
    param = "s";
    term = keywords;
    parse = text => {
      const data = JSON.parse(text);
      if (!Array.isArray(data)) throw new WebError("INVALID_RESPONSE", "Mwmbl returned an unexpected response");
      return data.map(item => item && {url: item.url,
        title: (Array.isArray(item.title) ? item.title : []).map(part => part?.value ?? "").join(""),
        snippet: (Array.isArray(item.extract) ? item.extract : []).map(part => part?.value ?? "").join("")});
    };
  } else {
    throw new WebError("CONFIG_ERROR", `Unknown provider: ${provider}; choose one with: kroma plugins set web provider exa (or auto, free, brave, you, tavily, parallel, perplexity, serper, kagi, bocha, exa_public, firecrawl, searchapi, serpapi, linkup, search1api, valyu, searxng, mwmbl)`);
  }
  if (!body) url.searchParams.set(param ?? "q", term ?? query);
  const response = await download(url.href, {timeout, headers, types, bytes: 900000,
    ...(body ? {method: "POST", body} : {})});
  if (response.truncated) throw new WebError("INVALID_RESPONSE", "Search response exceeded the 900 KB limit");
  try { return parse(response.text); }
  catch (error) {
    if (error instanceof WebError) throw error;
    throw new WebError("INVALID_RESPONSE", `${provider} returned invalid search data; check the endpoint or use another provider`);
  }
}

// Default order is quality-first: semantic and agent-tuned indexes first, then
// broad web indexes, then the self-hosted and free fallbacks. A chain is worth
// its cost only because it runs when something actually failed.
const PROVIDER_ORDER = Object.freeze(["exa_public", "exa", "tavily", "brave", "kagi", "you", "serper",
  "perplexity", "parallel", "serpapi", "searchapi", "linkup", "bocha", "search1api", "valyu", "firecrawl", "searxng", "mwmbl"]);
const KEYLESS = new Set(["exa_public", "firecrawl", "searxng", "mwmbl"]);

function parseChain(value) {
  if (typeof value !== "string" || !value.trim()) return null;
  const names = value.split(",").map(item => item.trim().toLowerCase()).filter(Boolean);
  const known = new Set([...Object.keys(KEY_ENV), ...KEYLESS]);
  const unknown = names.filter(name => !known.has(name));
  if (!names.length || unknown.length) {
    throw new WebError("CONFIG_ERROR", `search_chain must list provider names (${[...known].join(", ")}); unknown: ${unknown.join(", ") || "empty"}`);
  }
  return names;
}

function available(name, settings) {
  if (name === "searxng") return !!settings.searxng_url;
  if (KEYLESS.has(name)) return true;
  return hasKey(name, settings);
}

// auto = the owner's chain, or the default quality order, filtered to what can
// actually answer. A Chinese query starts at the Chinese index when one is
// configured; explicit providers never chain.
function candidatesFor(provider, settings, query, range) {
  if (provider === "free") {
    const free = ["exa_public", "firecrawl", "searxng", "mwmbl"].filter(name => available(name, settings));
    // The keyless Exa endpoint has no date filter; a free search that asks for
    // one starts at the free backend that can honor it.
    return range && free.includes("searxng") ? ["searxng", ...free.filter(name => name !== "searxng")] : free;
  }
  if (provider !== "auto") return [provider];
  const order = parseChain(settings.search_chain) ?? PROVIDER_ORDER;
  let chain = order.filter(name => available(name, settings));
  if (chain.includes("bocha") && /[\u3400-\u9fff]/.test(query)) {
    chain = ["bocha", ...chain.filter(name => name !== "bocha")];
  }
  return chain;
}

async function search(query, config = {}, wanted, timeRange = "") {
  if (typeof query !== "string" || !query.trim()) throw new WebError("INVALID_QUERY", "query must be a nonempty string");
  if (query.length > 600) throw new WebError("INVALID_QUERY", "query exceeds 600 characters");
  query = query.trim();
  const settings = {...DEFAULTS, ...config};
  const count = bounded(wanted ?? settings.max_results, 6, 1, 10);
  const provider = String(settings.provider ?? "auto").toLowerCase();
  const range = String(timeRange ?? "").toLowerCase();
  if (range && !TIME_VALUES.includes(range)) {
    throw new WebError("INVALID_ARGUMENT", "time_range must be day, week or year");
  }
  const sites = searchSites(query);
  const candidates = candidatesFor(provider, settings, query, range);
  const cacheKey = searchCacheKey(query, count, provider, candidates.join(","), range);
  const cached = SEARCH_CACHE.get(cacheKey);
  if (cached && Date.now() - cached.time <= SEARCH_CACHE_TTL) {
    return JSON.stringify({...cached.payload, cached: true}, null, 2);
  }
  const free = candidates.every(name => KEYLESS.has(name));
  const deadline = Date.now() + TIMEOUT_MS;
  const attempts = [];
  let lastError;
  for (const selected of candidates) {
    const remaining = deadline - Date.now();
    if (remaining <= 0) {
      const error = new WebError("TIMEOUT", "Search exceeded its 15 second budget; no further provider was called");
      error.attempts = attempts;
      throw error;
    }
    try {
      const results = normalizeResults(await searchProvider(selected, query, settings, remaining, count, sites, range), sites, count);
      if (!results.length) throw new WebError("NO_RESULTS", "No usable results matching the query/domain restrictions; refine the query or check the provider.");
      const filterApplied = !!range && TIME_FILTERS.has(selected) && !(selected === "search1api" && range === "week");
      const payload = {query, provider: selected, results,
        ...(filterApplied ? {time_range: range} : {}),
        ...(range && !filterApplied ? {time_range_ignored: true} : {}),
        ...(attempts.length ? {attempts} : {}),
        ...(free ? {mode: "free", warning: FREE_WARNING} : {})};
      SEARCH_CACHE.set(cacheKey, {payload, time: Date.now()});
      while (SEARCH_CACHE.size > SEARCH_CACHE_LIMIT) SEARCH_CACHE.delete(SEARCH_CACHE.keys().next().value);
      return JSON.stringify(payload, null, 2);
    } catch (error) {
      lastError = error;
      // A misconfiguration or a bad query cannot be fixed by the next backend.
      if (["CONFIG_ERROR", "INVALID_QUERY", "INVALID_ARGUMENT", "INVALID_URL"].includes(error.code)) break;
      attempts.push({provider: selected, code: error.code, message: error.message,
        ...(error.status !== undefined ? {status: error.status} : {})});
    }
  }
  const error = lastError ?? new WebError("NO_RESULTS", "No provider answered; check the plugin settings.");
  error.attempts = attempts;
  throw error;
}

// Exa publishes a keyless MCP endpoint. It searches the same index as the paid
// API and can read pages that need JavaScript, so it backs the free path before
// any paid rendering. Sessions are per process; an expired one is retried once.
const EXA_MCP = "https://mcp.exa.ai/mcp";
let exaSession = null;

function sseMessages(text) {
  const messages = [];
  for (const block of String(text).split(/\n\n+/)) {
    const line = block.split("\n").find(item => item.startsWith("data:"));
    if (!line) continue;
    try { messages.push(JSON.parse(line.slice(5).trim())); } catch { /* keep-alive or partial frame */ }
  }
  return messages;
}

async function exaRpc(payload, timeout) {
  const response = await download(EXA_MCP, {
    timeout, method: "POST", bytes: 4000000,
    types: ["application/json", "text/event-stream", "text/plain"],
    headers: {"content-type": "application/json", accept: "application/json, text/event-stream",
      ...(exaSession ? {"mcp-session-id": exaSession} : {})},
    body: JSON.stringify(payload)
  });
  if (!exaSession) exaSession = response.headers.get("mcp-session-id") ?? "";
  const answer = sseMessages(response.text).find(item => item.id === payload.id);
  if (!answer) throw new WebError("INVALID_RESPONSE", "Exa public endpoint returned no answer");
  if (answer.error) throw new WebError("INVALID_RESPONSE", "Exa public endpoint: " + (answer.error.message ?? "error"));
  return answer.result;
}

async function exaInit(timeout) {
  if (exaSession) return;
  await exaRpc({jsonrpc: "2.0", id: 1, method: "initialize",
    params: {protocolVersion: "2024-11-05", capabilities: {}, clientInfo: {name: "kroma-web", version: "4.1"}}}, timeout);
}

async function exaTool(name, args, timeout) {
  await exaInit(timeout);
  const id = Date.now();
  const call = callId => exaRpc({jsonrpc: "2.0", id: callId, method: "tools/call", params: {name, arguments: args}}, timeout);
  try {
    return await call(id);
  } catch (error) {
    // Only an expired session is worth one retry; anything else is the answer.
    exaSession = null;
    await exaInit(timeout);
    return call(id + 1);
  }
}

function exaBlocks(text) {
  return String(text).split(/\n(?=Title: )/).map(block => ({
    title: block.match(/^Title: (.*)$/m)?.[1]?.trim() ?? "",
    url: block.match(/^URL: (\S+)/m)?.[1] ?? "",
    published: dateOf({published: block.split("Published: ")[1]?.split(String.fromCharCode(10))[0]}),
    snippet: (block.split(/^Highlights:\s*$/m)[1] ?? "").trim()
  })).filter(item => item.url);
}

async function exaSearchPublic(query, count, timeout) {
  const result = await exaTool("web_search_exa", {query, numResults: Math.max(count, 4)}, timeout);
  return exaBlocks((result?.content ?? []).map(part => part?.text ?? "").join("\n"));
}

async function exaFetchPublic(url, timeout) {
  const result = await exaTool("web_fetch_exa", {urls: [url.href]}, timeout);
  const text = (result?.content ?? []).map(part => part?.text ?? "").join("\n").trim();
  if (!text) throw new WebError("UNREADABLE_PAGE", "Exa public fetch returned no text");
  const title = text.match(/^#\s+(.+)$/m)?.[1]?.trim() ?? url.href;
  return {title: title.slice(0, 1000), text, url: url.href, source_truncated: false, provider: "exa_public"};
}

const pages = createStore({limit: 24});

function linkChars(text) {
  let total = 0;
  for (const match of text.matchAll(/\[[^\]]*\]\([^)]*\)/g)) total += match[0].length;
  return total;
}

// A page that only yields navigation is a JavaScript application shell, not
// content: say so instead of handing the model 300 characters of menu links.
function readable(page) {
  const text = page.text;
  if (!text.trim() || text.length < 40) {
    throw new WebError("UNREADABLE_PAGE", "The page returned almost no readable text. It may require JavaScript or a login; use a static/raw URL or rely on search snippets.");
  }
  if (text.length < 400 && linkChars(text) > text.length / 2) {
    throw new WebError("UNREADABLE_PAGE", "The page returned navigation links only. It likely needs JavaScript; use a static/raw URL or rely on search snippets.");
  }
  return page;
}

function findPassages(page, needle, maxChars) {
  if (typeof needle !== "string" || !needle.trim() || needle.length > 200 || /[\x00-\x1f\x7f]/.test(needle)) {
    throw new WebError("INVALID_ARGUMENT", "find must be a nonempty, printable phrase of at most 200 characters");
  }
  const query = needle.trim().toLocaleLowerCase();
  const lower = page.text.toLocaleLowerCase();
  const offsets = [];
  let cursor = 0;
  while (offsets.length < 50) {
    const index = lower.indexOf(query, cursor);
    if (index < 0) break;
    offsets.push(index);
    cursor = index + Math.max(1, query.length);
  }
  const chunks = [];
  let used = 0;
  for (const offset of offsets) {
    if (chunks.length >= 5) break;
    const before = Math.max(0, page.text.lastIndexOf("\n", Math.max(0, offset - 500)) + 1);
    const start = Math.max(before, offset - 240);
    const end = Math.min(page.text.length, offset + query.length + 340);
    const snippet = page.text.slice(start, end).trim();
    if (used + snippet.length > maxChars && chunks.length) break;
    chunks.push({start_char: start, text: snippet.slice(0, Math.max(0, maxChars - used))});
    used += snippet.length;
  }
  return {find: needle.trim(), match_count: offsets.length, matches_capped: offsets.length === 50,
    matches: chunks};
}

function firecrawlKey(settings) {
  const key = settings.firecrawl_api_key || process.env.FIRECRAWL_API_KEY;
  return typeof key === "string" && key.trim() ? key.trim() : "";
}

// Optional remote rendering. Reached only for pages the static reader cannot
// use (JavaScript shell, 403/429), and only when the owner stored a key: one
// key, no switches, and the result says rendering was used so cost is visible.
async function fetchFirecrawl(url, settings) {
  const signal = AbortSignal.timeout(TIMEOUT_MS);
  const response = await download(FIRECRAWL_BASE + "/v2/scrape", {timeout: TIMEOUT_MS, bytes: 1500000,
    types: ["application/json"], method: "POST",
    headers: {Authorization: "Bearer " + firecrawlKey(settings), "Content-Type": "application/json"},
    body: JSON.stringify({url: url.href, formats: ["markdown"], onlyMainContent: true})});
  if (response.truncated) throw new WebError("INVALID_RESPONSE", "Firecrawl response exceeded the 1.5 MB limit");
  let envelope;
  try { envelope = JSON.parse(response.text); }
  catch { throw new WebError("INVALID_RESPONSE", "Firecrawl returned invalid data"); }
  if (!envelope || envelope.success === false) {
    throw new WebError("INVALID_RESPONSE", "Firecrawl reported failure or an invalid response");
  }
  const data = envelope.data;
  const metadata = data?.metadata;
  if (metadata?.error || (metadata?.statusCode !== undefined &&
    (!Number.isInteger(metadata.statusCode) || metadata.statusCode < 200 || metadata.statusCode >= 300))) {
    throw new WebError("HTTP_ERROR", "Firecrawl could not retrieve the source page");
  }
  if (typeof data?.markdown !== "string" || !data.markdown.trim()) {
    throw new WebError("UNREADABLE_PAGE", "Firecrawl returned no readable Markdown");
  }
  return {title: typeof metadata?.title === "string" ? metadata.title.slice(0, 1000) : url.href,
    text: data.markdown, url: url.href, source_truncated: false, provider: "firecrawl"};
}

async function readDirect(url) {
  const response = await download(url.href, {timeout: TIMEOUT_MS, bytes: 1500000,
    types: ["text/html", "text/plain", "text/markdown", "application/json", "application/xhtml+xml", "application/xml", "text/xml", "application/pdf"]});
  const isHTML = response.contentType.includes("html")
    || !response.contentType && /^\s*(?:<!doctype html|<html\b|<head\b|<title\b|<main\b|<article\b)/i.test(response.text);
  if (response.contentType.includes("pdf") || /^%PDF-/.test(response.text)) {
    // Rendering services convert PDFs to Markdown; without a key say so plainly.
    throw new WebError("PDF_DETECTED", "This is a PDF. Configure firecrawl_api_key to convert PDFs to text.");
  }
  if (!response.contentType && /^(PK\x03\x04|GIF8|\x89PNG)/.test(response.text)) {
    throw new WebError("UNSUPPORTED_CONTENT", "Binary documents are not supported; use an HTML, text or PDF URL");
  }
  const extracted = isHTML ? extractPage(response.text, response.url) : {title: response.url, text: response.text};
  const page = {title: extracted.title.slice(0, 1000), text: extracted.text, url: response.url,
    source_truncated: response.truncated, provider: "http"};
  if (isHTML) readable(page);
  return page;
}

async function fetchPage(address, config = {}, requestedChars, startChar = 0, find = null, section = null) {
  const url = httpUrl(address);
  const settings = {...DEFAULTS, ...config};
  const maxChars = bounded(requestedChars ?? DEFAULT_MAX_CHARS, DEFAULT_MAX_CHARS, 1000, 30000);
  if (!Number.isSafeInteger(startChar) || startChar < 0) {
    throw new WebError("INVALID_OFFSET", "start_char must be a nonnegative integer");
  }
  if (find !== null && section !== null) {
    throw new WebError("INVALID_ARGUMENT", "find and section are separate modes; find searches the whole page, section reads one chapter");
  }
  if (find !== null && startChar !== 0) {
    throw new WebError("INVALID_ARGUMENT", "find and start_char are separate modes; set start_char = 0 when using find");
  }
  const cached = pages.get(url.href);
  if (startChar > 0 && !cached) {
    throw new WebError("CACHE_MISS", "Continuation snapshot expired or is unavailable for " + url.href +
      "; re-read it from start_char = 0 with this URL or the requested_url of the previous result, then use its new next_start.");
  }
  let page = cached;
  if (!page) {
    try {
      page = youtubeId(url.href) ? await readYoutube(url) : await readDirect(url);
    } catch (error) {
      // Firecrawl covers what the static reader cannot reach at all: a page
      // that yields no text, a blocked 403/429, or a host this network cannot
      // resolve or connect to. A wrong URL costs one attempt, never a loop.
      const eligible = ["UNREADABLE_PAGE", "NETWORK_ERROR", "TIMEOUT", "PDF_DETECTED"].includes(error.code)
        || (error.code === "HTTP_ERROR" && [403, 429].includes(error.status));
      if (!eligible) throw error;
      try {
        // Exa's public endpoint is free, so it answers before any paid render.
        page = error.code === "PDF_DETECTED" && firecrawlKey(settings)
          ? await fetchFirecrawl(url, settings)
          : await exaFetchPublic(url, TIMEOUT_MS);
      } catch (fallback) {
        if (!firecrawlKey(settings)) throw error;
        page = await fetchFirecrawl(url, settings);
      }
    }
    pages.set([url.href, page.url], page);
  }
  if (startChar > page.text.length) {
    throw new WebError("INVALID_OFFSET", `start_char exceeds document length (${page.text.length}); read from 0`);
  }
  if (find !== null) {
    const found = findPassages(page, find, maxChars);
    // Passages are a selection, not the whole document, so only the source cap is
    // reported instead of claiming the page was read completely.
    return JSON.stringify({provider: page.provider, url: page.url, requested_url: url.href, title: page.title,
      ...found, total_chars: page.text.length, next_start: null, truncated: page.source_truncated === true,
      ...(page.source_truncated ? {warning: SOURCE_WARNING} : {})}, null, 2);
  }
  const {items: headings, truncated: headingsTruncated} = outline(page.text);
  let from = startChar, to = page.text.length, chosen = null;
  if (section !== null) {
    const wanted = String(section).trim().toLowerCase();
    if (!wanted) throw new WebError("INVALID_ARGUMENT", "section must be a nonempty heading name");
    const hit = headings.find(item => item.text.trim().toLowerCase() === wanted)
      ?? headings.find(item => item.text.toLowerCase().includes(wanted));
    if (!hit) {
      const names = headings.slice(0, 8).map(item => "\"" + item.text + "\"").join(", ");
      throw new WebError("SECTION_NOT_FOUND", names
        ? "No heading matches " + section + "; the page headings start with: " + names
        : "This page has no headings; read it from start_char = 0 instead");
    }
    chosen = hit.text;
    from = hit.start_char;
    const next = headings.find(item => item.start_char > hit.start_char && item.level <= hit.level);
    to = next ? next.start_char : page.text.length;
  }
  let end = Math.min(to, from + maxChars);
  // Avoid breaking a UTF-16 surrogate pair at the continuation boundary.
  if (end < to && /[\uD800-\uDBFF]/.test(page.text[end - 1])) end--;
  const text = page.text.slice(from, end);
  return JSON.stringify({provider: page.provider, url: page.url, requested_url: url.href, title: page.title,
    ...(headings.length ? {headings} : {}),
    ...(headingsTruncated ? {headings_truncated: true} : {}),
    ...(chosen ? {section: chosen} : {}),
    text, chars: text.length, start_char: from, total_chars: page.text.length,
    truncated: end < to || page.source_truncated,
    next_start: end < to ? end : null,
    ...(page.source_truncated ? {warning: SOURCE_WARNING} : {})}, null, 2);
}

// Several pages in one call: comparing sources is a normal research step and
// one round trip beats three. Each address fails on its own.
async function fetchMany(addresses, config = {}, requestedChars, find = null, section = null) {
  if (!Array.isArray(addresses) || !addresses.length) {
    throw new WebError("INVALID_ARGUMENT", "urls must be a nonempty array of URLs");
  }
  if (addresses.length > 10) {
    throw new WebError("INVALID_ARGUMENT", `urls accepts at most 10 addresses per call (got ${addresses.length})`);
  }
  const results = new Array(addresses.length);
  let next = 0;
  // Four at a time: the pages are independent, and answer order is preserved.
  const worker = async () => {
    while (next < addresses.length) {
      const index = next++;
      try {
        results[index] = JSON.parse(await fetchPage(addresses[index], config, requestedChars, 0, find, section));
      } catch (error) {
        results[index] = {url: String(addresses[index] ?? ""), error: error.message || String(error), code: error.code ?? "ERROR"};
      }
    }
  };
  await Promise.all(Array.from({length: Math.min(4, addresses.length)}, worker));
  return JSON.stringify({count: results.length, results}, null, 2);
}

function youtubeId(value) {
  const url = httpUrl(value);
  const host = url.hostname.replace(/^www\./, "");
  if (host === "youtu.be") return url.pathname.slice(1).split("/")[0] || null;
  if (host === "youtube.com" || host === "m.youtube.com") {
    if (url.pathname === "/watch") return url.searchParams.get("v");
    const match = url.pathname.match(/^\/(?:shorts|embed|live)\/([\w-]{6,})/);
    return match ? match[1] : null;
  }
  return null;
}

// YouTube ships caption tracks with the watch page, so a transcript needs no
// API key, no ffmpeg and no model. Frames and local video files stay out of
// scope and say so instead of pretending.
async function readYoutube(url) {
  const id = youtubeId(url.href);
  const watch = await download("https://www.youtube.com/watch?v=" + id, {timeout: TIMEOUT_MS, bytes: 4000000, types: ["text/html"]});
  const title = watch.text.match(/<title>([^<]*)<\/title>/)?.[1]?.replace(/&amp;/g, "&").replace(/ - YouTube$/, "") ?? ("YouTube " + id);
  const tracks = watch.text.match(/"captionTracks":(\[.*?\])/s)?.[1];
  if (!tracks) throw new WebError("NO_CAPTIONS", "This video has no caption track; ask for a video with captions or a text source.");
  let list;
  try { list = JSON.parse(tracks.replace(/\u0026/g, "&")); } catch { list = null; }
  const track = list?.find(t => /^zh/i.test(t.languageCode ?? ""))
    ?? list?.find(t => /^en/i.test(t.languageCode ?? "")) ?? list?.[0];
  if (!track?.baseUrl) throw new WebError("NO_CAPTIONS", "Caption track metadata is missing; the page layout may have changed.");
  const NO_TRACK = "YouTube withholds the caption file from plain HTTP clients; use yt-dlp locally for a transcript, or read the video page for its description.";
  let captions;
  try {
    captions = await download(track.baseUrl + "&fmt=json3", {timeout: TIMEOUT_MS, bytes: 4000000, types: ["application/json", "text/plain"]});
  } catch {
    throw new WebError("NO_CAPTIONS", NO_TRACK);
  }
  let events = [];
  try { events = JSON.parse(captions.text).events ?? []; } catch { events = []; }
  const lines = [];
  let bucket = -1;
  for (const event of events) {
    if (!event.segs) continue;
    const text = event.segs.map(seed => seed.utf8 ?? "").join("").replace(/\s+/g, " ").trim();
    if (!text) continue;
    const seconds = Math.floor((event.tStartMs ?? 0) / 1000);
    if (seconds - bucket >= 15 || !lines.length) {
      bucket = seconds;
      lines.push(`[${String(Math.floor(seconds / 60)).padStart(2, "0")}:${String(seconds % 60).padStart(2, "0")}] ${text}`);
    } else {
      lines[lines.length - 1] += " " + text;
    }
  }
  if (!lines.length) throw new WebError("NO_CAPTIONS", NO_TRACK);
  return {title, text: lines.join("\n"), url: "https://www.youtube.com/watch?v=" + id, source_truncated: false, provider: "youtube"};
}

async function dispatch(req) {
  if (!req || req.type !== "tool") throw new WebError("INVALID_REQUEST", "Only tool requests are supported");
  // Which provider is billed belongs exclusively to user configuration, not
  // the model/tool arguments. Reject old overrides rather than silently
  // ignoring an attempt to switch vendor.
  if (req.arguments && Object.hasOwn(req.arguments, "provider")) {
    throw new WebError("INVALID_ARGUMENT", "Select a web provider in the plugin settings, not in tool arguments");
  }
  if (req.name === "web_search") return search(req.arguments?.query, req.config, req.arguments?.count, req.arguments?.time_range ?? "");
  if (req.name === "web_fetch") {
    const args = req.arguments ?? {};
    if (args.urls !== undefined) {
      if (args.start_char !== undefined) throw new WebError("INVALID_ARGUMENT", "start_char only applies to a single url");
      return fetchMany(args.urls, req.config, args.max_chars, args.find ?? null, args.section ?? null);
    }
    return fetchPage(args.url, req.config, args.max_chars, args.start_char, args.find ?? null, args.section ?? null);
  }
  throw new WebError("UNKNOWN_TOOL", "Unknown tool " + req.name);
}

module.exports = {dispatch, search, fetchPage, fetchMany, plainText, httpUrl, candidatesFor, readable};
