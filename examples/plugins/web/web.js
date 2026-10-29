"use strict";
// No dependencies; Node 20+ provides fetch and TextDecoder.
const DEFAULTS = Object.freeze({ provider: "bing", max_results: 6, max_chars: 12000, timeout_ms: 15000 });
const UA = "Mozilla/5.0 (compatible; Kroma-Web/1.0)";
function bounded(v, fallback, lo, hi) {
  const n = Number(v);
  return Number.isFinite(n) ? Math.max(lo, Math.min(hi, Math.floor(n))) : fallback;
}
function entities(text) {
  return String(text ?? "").replace(/&(#(?:x[0-9a-f]+|\d+)|[a-z]+);/gi, (raw, code) => {
    if (code.startsWith("#")) {
      const hex = code[1]?.toLowerCase() === "x";
      const point = Number.parseInt(code.slice(hex ? 2 : 1), hex ? 16 : 10);
      return point > 0 && point <= 0x10ffff && !(point >= 0xd800 && point <= 0xdfff)
        ? String.fromCodePoint(point) : raw;
    }
    return {amp:"&",lt:"<",gt:">",quot:'"',apos:"'",nbsp:" ",mdash:"—",ndash:"–",hellip:"…"}[code.toLowerCase()] ?? raw;
  });
}
function plainText(html) {
  return entities(String(html ?? "")
    .replace(/<!--[\s\S]*?-->/g, " ")
    .replace(/<(script|style|svg|nav|footer|header|noscript|form|iframe|aside)\b[^>]*>[\s\S]*?<\/\1\s*>/gi, " ")
    .replace(/<(br|\/p|\/div|\/h[1-6]|\/li|\/tr|\/article|\/section)\b[^>]*>/gi, "\n")
    .replace(/<[^>]*>/g, " "))
    .replace(/\r/g, "").replace(/[ \t]+/g, " ")
    .replace(/\n\s*\n\s*\n+/g, "\n\n").trim();
}
function httpUrl(value) {
  const url = new URL(String(value ?? ""));
  if (!["https:", "http:"].includes(url.protocol)) throw Error("Only HTTP(S) URLs are supported");
  return url;
}
function searchLink(href) {
  try {
    const url = new URL(entities(href), "https://html.duckduckgo.com");
    if (url.hostname.endsWith("duckduckgo.com") && url.searchParams.get("uddg")) {
      return httpUrl(url.searchParams.get("uddg")).href;
    }
    return httpUrl(url.href).href;
  } catch { return ""; }
}
function parseBingRSS(xml,count) {
  const results=[],seen=new Set();
  for(const match of String(xml).matchAll(/<item\b[^>]*>([\s\S]*?)<\/item>/gi)){
    const field=tag=>match[1].match(new RegExp("<"+tag+"\\b[^>]*>([\\s\\S]*?)<\\/"+tag+">","i"))?.[1] ?? "";
    const url=entities(field("link")).trim();
    if(seen.has(url))continue;
    try{httpUrl(url);}catch{continue;}
    seen.add(url);
    results.push({title:plainText(field("title")),url,snippet:plainText(field("description"))});
    if(results.length>=count)break;
  }
  return results;
}
function parseDDG(html, count) {
  const results = [], seen = new Set();
  const pattern = /<a\b(?=[^>]*\bclass=["'][^"']*\bresult__a\b[^"']*["'])([^>]*)>([\s\S]*?)<\/a>/gi;
  for (const hit of html.matchAll(pattern)) {
    const href = hit[1].match(/\bhref=["']([^"']+)["']/i)?.[1];
    const url = searchLink(href);
    if (!url || seen.has(url)) continue;
    seen.add(url);
    const after = html.slice(hit.index + hit[0].length, hit.index + hit[0].length + 3200);
    const snippet = after.match(/<(?:a|div)\b[^>]*class=["'][^"']*\bresult__snippet\b[^"']*["'][^>]*>([\s\S]*?)<\/(?:a|div)>/i)?.[1] ?? "";
    results.push({title: plainText(hit[2]), url, snippet: plainText(snippet)});
    if (results.length >= count) break;
  }
  return results;
}
async function download(url, options = {}) {
  const response = await fetch(url, {
    headers: {"User-Agent": UA, "Accept":"text/html,application/json;q=0.9,*/*;q=0.5", ...options.headers},
    redirect: "follow",
    signal: AbortSignal.timeout(options.timeout || 15000)
  });
  if (!response.ok) throw Error("HTTP " + response.status + " from " + url);
  const contentType = response.headers.get("content-type") ?? "";
  if (options.types && !options.types.some(t => contentType.includes(t))) {
    throw Error("Unsupported content type: " + (contentType || "unknown"));
  }
  const reader = response.body?.getReader();
  const chunks = [];
  let bytes = 0, truncated = false;
  if (reader) {
    while (true) {
      const {done, value} = await reader.read();
      if (done) break;
      bytes += value.length;
      if (bytes > (options.bytes ?? 600000)) {
        truncated = true;
        await reader.cancel();
        break;
      }
      chunks.push(Buffer.from(value));
    }
  }
  return {text: Buffer.concat(chunks).toString("utf8"), contentType, truncated, url: response.url};
}
async function search(query, config = {}, wanted) {
  if (typeof query !== "string" || !query.trim()) throw Error("query must be a nonempty string");
  if (query.length > 600) throw Error("query exceeds 600 characters");
  const setting = {...DEFAULTS, ...config};
  const count = bounded(wanted ?? setting.max_results, 6, 1, 10);
  const timeout = bounded(setting.timeout_ms, 15000, 2000, 60000);
  const provider = String(setting.provider).toLowerCase();
  let results;
  if (provider === "brave") {
    const key = String(setting.brave_api_key || process.env.BRAVE_SEARCH_API_KEY || "");
    if (!key) throw Error("Set BRAVE_SEARCH_API_KEY or plugin setting brave_api_key for Brave");
    const url = new URL("https://api.search.brave.com/res/v1/web/search");
    url.searchParams.set("q", query);
    url.searchParams.set("count", String(count));
    const response = await download(url.href, {headers: {"X-Subscription-Token":key},timeout,types:["application/json"],bytes:900000});
    results = (JSON.parse(response.text).web?.results ?? []).slice(0,count).map(x => ({
      title:x.title ?? "", url:x.url ?? "",snippet:x.description ?? ""
    }));
  } else if (provider === "searxng") {
    if (!setting.searxng_url) throw Error("Configure searxng_url (with JSON search enabled)");
    const url = httpUrl(setting.searxng_url);
    url.pathname = url.pathname.replace(/\/$/, "") + "/search";
    url.searchParams.set("q", query);
    url.searchParams.set("format", "json");
    const response = await download(url.href, {timeout,types:["application/json"],bytes:900000});
    results = (JSON.parse(response.text).results ?? []).slice(0,count).map(x => ({
      title:x.title ?? "",url:x.url ?? "",snippet:x.content ?? ""
    }));
  } else if (provider === "bing") {
    const url = new URL("https://www.bing.com/search");
    url.searchParams.set("q",query);
    url.searchParams.set("format","rss");
    const response = await download(url.href,{timeout,types:["xml","rss","text/html"],bytes:500000});
    results = parseBingRSS(response.text,count);
    if(!results.length)throw Error("Bing RSS returned no results; check network access or choose another provider");
  } else if (provider === "duckduckgo") {
    const url = new URL("https://html.duckduckgo.com/html/");
    url.searchParams.set("q",query);
    const response = await download(url.href,{timeout,types:["text/html"],bytes:500000});
    results = parseDDG(response.text,count);
    if (!results.length) throw Error("No parseable DuckDuckGo results. The engine may be rate-limiting; use Brave or SearXNG.");
  } else throw Error("Unknown provider: " + provider);
  return JSON.stringify({query,provider,results:results.filter(r => {
    try { httpUrl(r.url); return true; } catch { return false; }
  })},null,2);
}
async function fetchPage(address, config = {}, requestedChars) {
  const url = httpUrl(address);
  const settings = {...DEFAULTS,...config};
  const maxChars = bounded(requestedChars ?? settings.max_chars,12000,1000,30000);
  const timeout = bounded(settings.timeout_ms,15000,2000,60000);
  const response = await download(url.href,{
    timeout,bytes:750000,types:["text/html","text/plain","application/json","application/xhtml+xml","application/xml","text/xml"]
  });
  let text = response.text;
  let title = response.url;
  if (response.contentType.includes("html") || response.contentType.includes("xml")) {
    const match = text.match(/<title\b[^>]*>([\s\S]*?)<\/title>/i);
    if (match) title = plainText(match[1]);
    const main = text.match(/<main\b[^>]*>([\s\S]*?)<\/main>/i)
      || text.match(/<article\b[^>]*>([\s\S]*?)<\/article>/i);
    text = plainText(main?.[1] ?? text);
  }
  return JSON.stringify({url:response.url,title,text:text.slice(0,maxChars),
    truncated:response.truncated || text.length>maxChars, chars:Math.min(maxChars,text.length)},null,2);
}
async function dispatch(req) {
  if (req.type !== "tool") throw Error("Only tool requests are supported");
  if (req.name === "web_search") return search(req.arguments?.query,req.config ?? {},req.arguments?.count);
  if (req.name === "web_fetch") return fetchPage(req.arguments?.url,req.config ?? {},req.arguments?.max_chars);
  throw Error("Unknown tool " + req.name);
}
module.exports={dispatch,search,fetchPage,parseBingRSS,parseDDG,plainText,entities,httpUrl};
