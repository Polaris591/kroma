"use strict";
const {WebError, httpUrl, download} = require("./http");
const {extractPage, plainText, entities} = require("./page");
const DEFAULTS = Object.freeze({provider: "auto", max_results: 6, max_chars: 12000, timeout_ms: 15000,
  search_priority: Object.freeze(["parallel", "you", "exa", "tavily"]), search_attempt_timeout_ms: 8000});

// Keyless engines cannot honour every ranking signal, so say what each one is
// bad at instead of presenting their output as equivalent to a keyed API.
const FREE_NOTES = Object.freeze({
  mwmbl: " Mwmbl indexes a small community corpus; Chinese and niche topics are often missing.",
  bing: " Bing RSS often ignores specific terms and returns generic popular pages."
});

const KEY_ENV = Object.freeze({exa: "EXA_API_KEY", brave: "BRAVE_SEARCH_API_KEY",
  tavily: "TAVILY_API_KEY", parallel: "PARALLEL_API_KEY", you: "YDC_API_KEY", firecrawl: "FIRECRAWL_API_KEY"});
function apiKey(provider, settings) {
  const key = settings[provider + "_api_key"] || process.env[KEY_ENV[provider]];
  if (typeof key !== "string" || !key.trim()) throw new WebError("CONFIG_ERROR", "Configure " + KEY_ENV[provider]);
  return key.trim();
}
function passages(value) {
  return Array.isArray(value) ? value.filter(x => typeof x === "string").join("\n") : "";
}
function firecrawlUrl(settings, endpoint) {
  const url = httpUrl(settings.firecrawl_base_url || process.env.FIRECRAWL_BASE_URL || "https://api.firecrawl.dev");
  if (url.protocol !== "https:" && !["localhost", "127.0.0.1", "[::1]"].includes(url.hostname)) {
    throw new WebError("CONFIG_ERROR", "Firecrawl API requires HTTPS (HTTP is allowed only on loopback)");
  }
  const version = settings.firecrawl_api_version || "v2";
  if (!["v1", "v2"].includes(version)) throw new WebError("CONFIG_ERROR", "firecrawl_api_version must be v1 or v2");
  url.pathname = url.pathname.replace(/\/$/, "").replace(/\/v[12]$/, "") + "/" + version + "/" + endpoint;
  url.search = "";
  url.hash = "";
  return url;
}
function firecrawlData(text) {
  const envelope = JSON.parse(text);
  if (envelope?.success !== true) throw new WebError("INVALID_RESPONSE", "Firecrawl reported failure or an invalid response");
  return envelope.data;
}

function bounded(value, fallback, lo, hi) {
  const n = Number(value);
  return value !== null && value !== "" && Number.isFinite(n)
    ? Math.max(lo, Math.min(hi, Math.floor(n))) : fallback;
}

function searchLink(href) {
  if (!href) return "";
  try {
    const url = new URL(entities(href), "https://html.duckduckgo.com");
    if ((url.hostname === "duckduckgo.com" || url.hostname.endsWith(".duckduckgo.com")) && url.searchParams.has("uddg")) {
      return httpUrl(url.searchParams.get("uddg")).href;
    }
    return httpUrl(url.href).href;
  } catch { return ""; }
}

function parseBingRSS(xml, count) {
  const results = [], seen = new Set();
  for (const match of String(xml).matchAll(/<item\b[^>]*>([\s\S]*?)<\/item>/gi)) {
    const field = tag => match[1].match(new RegExp(String.raw`<${tag}\b[^>]*>([\s\S]*?)<\/${tag}>`, "i"))?.[1] ?? "";
    const url = entities(field("link").replace(/<!\[CDATA\[([\s\S]*?)\]\]>/g, "$1")).trim();
    if (seen.has(url)) continue;
    try { httpUrl(url); } catch { continue; }
    seen.add(url);
    results.push({title: plainText(field("title")), url, snippet: plainText(field("description"))});
    if (results.length >= count) break;
  }
  return results;
}

function parseDDG(html, count, lite = false) {
  const results = [], seen = new Set();
  const linkClass = lite ? "result-link" : "result__a";
  const pattern = new RegExp(String.raw`<a\b(?=[^>]*\bclass=["'][^"']*\b${linkClass}\b[^"']*["'])([^>]*)>([\s\S]*?)<\/a>`, "gi");
  const hits = [...html.matchAll(pattern)];
  for (let i = 0; i < hits.length; i++) {
    const hit = hits[i];
    const url = searchLink(hit[1].match(/\bhref=["']([^"']+)["']/i)?.[1]);
    if (!url || seen.has(url)) continue;
    seen.add(url);
    const after = html.slice(hit.index + hit[0].length, hits[i + 1]?.index ?? hit.index + hit[0].length + 3200);
    const snippet = after.match(/<(?:a|div|span|td)\b[^>]*class=["'][^"']*\b(?:result__snippet|result-snippet)\b[^"']*["'][^>]*>([\s\S]*?)<\/(?:a|div|span|td)>/i)?.[1] ?? "";
    results.push({title: plainText(hit[2]), url, snippet: plainText(snippet)});
    if (results.length >= count) break;
  }
  return results;
}

// Public HTML is not an API: detect challenges/markup changes instead of
// presenting their navigation or verification links as search results.
function parsePublicSearch(text, provider) {
  if (/anomaly\.js|id\s*=\s*["'](?:challenge-form|anomaly-form)["']|<title[^>]*>\s*(?:Just a moment|Access denied|Verify you are human)/i.test(text)) {
    throw new WebError("SEARCH_BLOCKED", "Public search returned a verification page; no CAPTCHA solving or browser automation is attempted");
  }
  if (provider === "bing") {
    if (!/<rss\b/i.test(text)) throw new WebError("INVALID_RESPONSE", "Bing did not return an RSS search response");
    return parseBingRSS(text, 50);
  }
  const results = parseDDG(text, 50, provider === "duckduckgo_lite");
  if (!results.length && !/no-results|result--no-result|no results found|no more results/i.test(text)) {
    throw new WebError("INVALID_RESPONSE", "DuckDuckGo returned unrecognized search markup");
  }
  return results;
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
    clean.push({title: plainText(item.title).slice(0, 500), url: url.href,
      snippet: plainText(item.snippet).slice(0, 1800)});
    if (clean.length >= count) break;
  }
  return clean;
}

async function searchProvider(provider, query, settings, timeout, count, sites) {
  let url, headers, types, parse, body, param, term;
  const includes = sites.filter(site => !site.exclude).map(site => site.host);
  const excludes = sites.filter(site => site.exclude).map(site => site.host);
  const keywords = query.replace(/(?:^|\s)-?site:[^\s]+/gi, " ").trim() || query;
  if (provider === "exa") {
    const key = apiKey(provider, settings);
    url = new URL("https://api.exa.ai/search");
    headers = {"x-api-key": key, "Content-Type": "application/json"};
    types = ["application/json"];
    // Exa has native domain filters; keep the local checks as well.
    body = JSON.stringify({
      query: keywords,
      type: "auto", numResults: count, contents: {highlights: true},
      ...(includes.length ? {includeDomains: includes} : {}),
      ...(excludes.length ? {excludeDomains: excludes} : {})
    });
    parse = text => JSON.parse(text).results?.map(x => x && ({
      title: x.title, url: x.url,
      snippet: passages(x.highlights)
    }));
  } else if (provider === "tavily") {
    url = new URL("https://api.tavily.com/search");
    headers = {Authorization: "Bearer " + apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({query: keywords, search_depth: "basic", max_results: count,
      include_answer: false, include_raw_content: false,
      ...(includes.length ? {include_domains: includes} : {}),
      ...(excludes.length ? {exclude_domains: excludes} : {})});
    parse = text => JSON.parse(text).results?.map(x => x && ({title: x.title, url: x.url, snippet: x.content}));
  } else if (provider === "you") {
    url = new URL("https://ydc-index.io/v1/search");
    headers = {"X-API-Key": apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({query, count});
    parse = text => JSON.parse(text).results?.web?.map(x => x && ({
      title: x.title, url: x.url, snippet: passages(x.snippets) || x.description
    }));
  } else if (provider === "parallel") {
    url = new URL("https://api.parallel.ai/v1/search");
    headers = {"x-api-key": apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({objective: query, search_queries: [keywords], mode: "basic",
      max_chars_total: count * 1800,
      advanced_settings: {max_results: count, ...(sites.length ? {source_policy: {
        ...(includes.length ? {include_domains: includes} : {}),
        ...(excludes.length ? {exclude_domains: excludes} : {})
      }} : {})}});
    parse = text => JSON.parse(text).results?.map(x => x && ({title: x.title, url: x.url, snippet: passages(x.excerpts)}));
  } else if (provider === "firecrawl") {
    url = firecrawlUrl(settings, "search");
    headers = {Authorization: "Bearer " + apiKey(provider, settings), "Content-Type": "application/json"};
    types = ["application/json"];
    body = JSON.stringify({query, limit: count});
    parse = text => {
      const data = firecrawlData(text);
      return (Array.isArray(data) ? data : data?.web)?.map(x => x && ({title: x.title, url: x.url, snippet: x.description}));
    };
  } else if (provider === "brave") {
    const key = apiKey(provider, settings);
    url = new URL("https://api.search.brave.com/res/v1/web/search");
    url.searchParams.set("count", String(count));
    headers = {"X-Subscription-Token": key};
    types = ["application/json"];
    parse = text => JSON.parse(text).web?.results?.map(x => x && ({...x, snippet: x.description}));
  } else if (provider === "searxng") {
    if (!settings.searxng_url) throw new WebError("CONFIG_ERROR", "Configure searxng_url with JSON search enabled");
    url = httpUrl(settings.searxng_url);
    if (!url.pathname.endsWith("/search")) url.pathname = url.pathname.replace(/\/$/, "") + "/search";
    url.searchParams.set("format", "json");
    types = ["application/json"];
    parse = text => JSON.parse(text).results?.map(x => x && ({...x, snippet: x.content}));
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
  } else if (provider === "bing") {
    url = new URL("https://www.bing.com/search");
    url.searchParams.set("format", "rss");
    types = ["xml", "rss", "text/html"];
    parse = text => parsePublicSearch(text, provider);
  } else if (["duckduckgo", "duckduckgo_lite"].includes(provider)) {
    url = new URL(provider === "duckduckgo" ? "https://html.duckduckgo.com/html/" : "https://lite.duckduckgo.com/lite/");
    types = ["text/html"];
    parse = text => parsePublicSearch(text, provider);
  } else throw new WebError("CONFIG_ERROR", `Unknown provider: ${provider}`);
  if (!body) url.searchParams.set(param ?? "q", term ?? query);
  const response = await download(url.href, {timeout, headers, types, bytes: 900000, allowPrivateNetwork: true,
    ...(body ? {method: "POST", body} : {})});
  if (response.truncated) throw new WebError("INVALID_RESPONSE", "Search response exceeded the 900 KB limit");
  try { return parse(response.text); }
  catch (error) {
    if (error instanceof WebError) throw error;
    throw new WebError("INVALID_RESPONSE", `${provider} returned invalid search data; check the endpoint or use another provider`);
  }
}

async function search(query, config = {}, wanted) {
  if (typeof query !== "string" || !query.trim()) throw new WebError("INVALID_QUERY", "query must be a nonempty string");
  if (query.length > 600) throw new WebError("INVALID_QUERY", "query exceeds 600 characters");
  query = query.trim();
  const settings = {...DEFAULTS, ...config};
  const count = bounded(wanted ?? settings.max_results, 6, 1, 10);
  const timeout = bounded(settings.timeout_ms, 15000, 2000, 60000);
  const provider = String(settings.provider).toLowerCase();
  const sites = searchSites(query);
  let providers = [provider];
  let free = provider === "free";
  if (provider === "auto") {
    const priority = Array.isArray(settings.search_priority) ? settings.search_priority
      : typeof settings.search_priority === "string" ? settings.search_priority.split(",").map(x => x.trim()) : null;
    // Validate the entire list before using any credentials or making requests.
    if (!priority?.length || priority.length > 7 || new Set(priority).size !== priority.length
      || priority.some(name => !Object.hasOwn(KEY_ENV, name) && name !== "searxng")) {
      throw new WebError("CONFIG_ERROR", "search_priority must be a nonempty, unique list of exa, brave, tavily, you, parallel, firecrawl or searxng (array or comma-separated names)");
    }
    providers = priority.filter(name => name === "searxng" ? !!settings.searxng_url
      : !!(settings[name + "_api_key"] || process.env[KEY_ENV[name]]));
    free = !providers.length;
  }
  // Keyless order: a configured SearXNG, then a real free API, then public HTML
  // endpoints, then Bing's RSS. Ranking quality differs sharply between them.
  if (free) providers = [...(settings.searxng_url ? ["searxng"] : []), "mwmbl", "duckduckgo", "duckduckgo_lite", "bing"];
  const chained = provider === "auto" || free;
  let skipDuckDuckGoLite = false;
  const deadline = Date.now() + timeout;
  const attemptTimeout = bounded(settings.search_attempt_timeout_ms, 8000, 500, 60000);
  // Keyless endpoints are tried sequentially, so cap each one to leave room for
  // a different vendor instead of letting one slow host consume the budget.
  const perAttempt = free ? Math.min(attemptTimeout, 6000) : attemptTimeout;
  const attempts = [];
  for (const selected of providers) {
    // HTML and Lite share an edge. A CAPTCHA, rate limit, or unreachable edge
    // will not be fixed by the sibling host, so keep the budget for another vendor.
    if (selected === "duckduckgo_lite" && skipDuckDuckGoLite) continue;
    const remaining = deadline - Date.now();
    if (remaining <= 0) {
      const error = new WebError("TIMEOUT", "Search exhausted its total timeout budget; no further provider was called");
      error.attempts = attempts;
      throw error;
    }
    try {
      const results = normalizeResults(await searchProvider(selected, query, settings,
        chained ? Math.min(remaining, perAttempt) : remaining, count, sites), sites, count);
      if (!results.length) throw new WebError("NO_RESULTS", "No usable results matching the query/domain restrictions; refine the query or check the provider.");
      return JSON.stringify({query, provider: selected, results, ...(chained ? {attempts} : {}),
        ...(free ? {mode: "free", warning: "Keyless public search is best-effort; results may be incomplete or irrelevant. Verify sources with web_fetch." + (FREE_NOTES[selected] ?? "")} : {})}, null, 2);
    } catch (error) {
      if (!chained) throw error;
      if (selected === "duckduckgo" && (["SEARCH_BLOCKED", "NETWORK_ERROR", "TIMEOUT"].includes(error.code)
        || [403, 429].includes(error.status))) skipDuckDuckGoLite = true;
      attempts.push({provider: selected, code: error.code, message: error.message,
        ...(error.status !== undefined ? {status: error.status} : {})});
      const transient = ["TIMEOUT", "NETWORK_ERROR"].includes(error.code)
        || error.code === "HTTP_ERROR" && (error.status === 429 || error.status >= 500 && error.status <= 599);
      const freeRecoverable = free && (["SEARCH_BLOCKED", "NO_RESULTS", "INVALID_RESPONSE", "UNSUPPORTED_CONTENT"].includes(error.code)
        || error.code === "HTTP_ERROR" && [403, 408].includes(error.status));
      if ((!transient && !freeRecoverable) || selected === providers[providers.length - 1]) {
        error.attempts = attempts;
        throw error;
      }
    }
  }
}

// In-memory only: continuation reads the same extracted document, without
// putting browsing history on disk. Bound both entry count and retained text.
const pages = new Map();
const CACHE_TTL = 120000;
function cachePage(key, page) {
  for (const [url, entry] of pages) {
    if (Date.now() - entry.time > CACHE_TTL) pages.delete(url);
  }
  pages.delete(key);
  pages.set(key, {page, time: Date.now()});
  let chars = [...pages.values()].reduce((sum, entry) => sum + entry.page.text.length, 0);
  while (pages.size > 6 || chars > 4000000) {
    const oldest = pages.keys().next().value;
    chars -= pages.get(oldest).page.text.length;
    pages.delete(oldest);
  }
}

async function fetchPage(address, config = {}, requestedChars, startChar = 0) {
  const url = httpUrl(address);
  const settings = {...DEFAULTS, ...config};
  const maxChars = bounded(requestedChars ?? settings.max_chars, 12000, 1000, 30000);
  const timeout = bounded(settings.timeout_ms, 15000, 2000, 60000);
  if (!Number.isSafeInteger(startChar) || startChar < 0) {
    throw new WebError("INVALID_OFFSET", "start_char must be a nonnegative integer");
  }
  const provider = String(settings.fetch_provider || "http").toLowerCase();
  if (!["http", "firecrawl"].includes(provider)) throw new WebError("CONFIG_ERROR", "fetch_provider must be http or firecrawl");
  const key = provider + ":" + url.href;
  const cached = pages.get(key);
  let page = startChar > 0 && cached && Date.now() - cached.time < CACHE_TTL ? cached.page : null;
  if (startChar > 0 && !page) {
    pages.delete(key);
    throw new WebError("CACHE_MISS", "Continuation snapshot expired or is unavailable; read the same URL from start_char = 0 and use its new next_start.");
  }
  if (!page && provider === "firecrawl") {
    page = await fetchFirecrawl(url, settings, timeout);
    cachePage(key, page);
  }
  if (!page) {
    const response = await download(url.href, {timeout, bytes: 1500000,
      types: ["text/html", "text/plain", "text/markdown", "application/json", "application/xhtml+xml", "application/xml", "text/xml"]});
    const isHTML = response.contentType.includes("html") || !response.contentType && /^\s*(?:<!doctype html|<html\b|<head\b|<title\b|<main\b|<article\b)/i.test(response.text);
    const extracted = isHTML ? extractPage(response.text, response.url) : {title: response.url, text: response.text};
    if (!response.contentType && /^(%PDF-|PK\x03\x04|\x89PNG)/.test(response.text)) {
      throw new WebError("UNSUPPORTED_CONTENT", "PDF and binary documents are not supported; use an HTML or text URL");
    }
    if (!extracted.text.trim() || (isHTML && (/^(just a moment|attention required|access denied|verify you are human)/i.test(extracted.title)
      || (extracted.text.length < 800 && /enable javascript|checking your browser|verify (?:that )?you are (?:a )?human/i.test(extracted.text))))) {
      throw new WebError("UNREADABLE_PAGE", "No readable page content; a JavaScript app, login or bot challenge may require a browser or a direct documentation/raw URL");
    }
    page = {...extracted, title: extracted.title.slice(0, 1000), url: response.url, source_truncated: response.truncated};
    cachePage(key, page);
  }
  if (startChar > page.text.length) {
    throw new WebError("INVALID_OFFSET", `start_char exceeds document length (${page.text.length}); read from 0`);
  }
  let end = Math.min(page.text.length, startChar + maxChars);
  // Avoid breaking a UTF-16 surrogate pair at the continuation boundary.
  if (end < page.text.length && /[\uD800-\uDBFF]/.test(page.text[end - 1])) end--;
  const text = page.text.slice(startChar, end);
  return JSON.stringify({provider, url: page.url, title: page.title, text, chars: text.length,
    start_char: startChar, total_chars: page.text.length,
    truncated: end < page.text.length || page.source_truncated,
    next_start: end < page.text.length ? end : null,
    ...(page.source_truncated ? {warning: "Source exceeded the 1.5 MB download limit; the document tail is unavailable. Use a more specific/raw URL."} : {})}, null, 2);
}

async function fetchFirecrawl(url, settings, timeout) {
  const response = await download(firecrawlUrl(settings, "scrape").href, {
    timeout, bytes: 1500000, types: ["application/json"], method: "POST", allowPrivateNetwork: true,
    headers: {Authorization: "Bearer " + apiKey("firecrawl", settings), "Content-Type": "application/json"},
    body: JSON.stringify({url: url.href, formats: ["markdown"], onlyMainContent: true,
      ...(settings.firecrawl_fresh_scrape === true || settings.firecrawl_fresh_scrape === "true" ? {maxAge: 0} : {})})
  });
  if (response.truncated) throw new WebError("INVALID_RESPONSE", "Firecrawl response exceeded the 1.5 MB limit");
  let data;
  try { data = firecrawlData(response.text); }
  catch { throw new WebError("INVALID_RESPONSE", "Firecrawl returned invalid scrape data"); }
  const metadata = data?.metadata;
  if (metadata?.error || (metadata?.statusCode !== undefined &&
    (!Number.isInteger(metadata.statusCode) || metadata.statusCode < 200 || metadata.statusCode >= 300))) {
    throw new WebError("HTTP_ERROR", "Firecrawl could not retrieve the source page successfully");
  }
  if (typeof data?.markdown !== "string" || !data.markdown.trim()) {
    throw new WebError("UNREADABLE_PAGE", "Firecrawl returned no readable Markdown");
  }
  return {url: url.href, title: typeof metadata?.title === "string" ? metadata.title.slice(0, 1000) : url.href,
    text: data.markdown, source_truncated: false};
}

async function dispatch(req) {
  if (!req || req.type !== "tool") throw new WebError("INVALID_REQUEST", "Only tool requests are supported");
  if (req.name === "web_search") return search(req.arguments?.query, {...req.config, ...(req.arguments?.provider ? {provider: req.arguments.provider} : {})}, req.arguments?.count);
  if (req.name === "web_fetch") return fetchPage(req.arguments?.url, {...req.config, ...(req.arguments?.provider ? {fetch_provider: req.arguments.provider} : {})}, req.arguments?.max_chars, req.arguments?.start_char);
  throw new WebError("UNKNOWN_TOOL", "Unknown tool " + req.name);
}

module.exports = {dispatch, search, fetchPage, parseBingRSS, parseDDG, plainText, entities, httpUrl};
