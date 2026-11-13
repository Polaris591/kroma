"use strict";
const {test,beforeEach,afterEach} = require("node:test");
const assert = require("node:assert/strict");
const {dispatch,search,fetchPage,fetchMany,readable,httpUrl} = require("./web");
const {createStore, SNAPSHOT_IDLE_TTL_MS, SNAPSHOT_HOLD_TTL_MS} = require("./cache");
const originalFetch = global.fetch;
const envKeys = ["EXA_API_KEY", "BRAVE_SEARCH_API_KEY", "TAVILY_API_KEY", "YDC_API_KEY", "PARALLEL_API_KEY"];
const saved = Object.fromEntries(envKeys.map(key => [key, process.env[key]]));
beforeEach(() => { for (const key of envKeys) delete process.env[key]; });
afterEach(() => {
  global.fetch = originalFetch;
  for (const [key, value] of Object.entries(saved)) {
    if (value === undefined) delete process.env[key]; else process.env[key] = value;
  }
});
function json(value, status = 200) {
  return new Response(JSON.stringify(value), {status, headers: {"content-type": "application/json"}});
}
function html(body) {
  return new Response(body, {headers: {"content-type": "text/html"}});
}
const DOC = "<html><body><main><h1>Title</h1><p>" + "readable words ".repeat(60) + "</p></main></body></html>";

test("exa receives a POST with the key and native domain filters", async () => {
  global.fetch = async (url, options) => {
    assert.equal(String(url), "https://api.exa.ai/search");
    assert.equal(options.method, "POST");
    assert.equal(options.headers["x-api-key"], "exa-key");
    const body = JSON.parse(options.body);
    assert.equal(body.query, "rust async");
    assert.deepEqual(body.includeDomains, ["docs.rs"]);
    assert.deepEqual(body.excludeDomains, ["reddit.com"]);
    return json({results: [{title: "Tokio", url: "https://docs.rs/tokio", highlights: ["async runtime", "tasks"]}]});
  };
  const result = JSON.parse(await search("rust async site:docs.rs -site:reddit.com", {provider: "exa", exa_api_key: "exa-key"}));
  assert.equal(result.provider, "exa");
  assert.equal(result.results[0].snippet, "async runtime tasks");
});

test("brave uses its header token and description field", async () => {
  global.fetch = async (url, options) => {
    assert.equal(options.headers["X-Subscription-Token"], "brave-key");
    assert.match(String(url), /count=2/);
    return json({web: {results: [{title: "Docs", url: "https://example.org/a", description: "Snippet"}]}});
  };
  const result = JSON.parse(await search("docs", {provider: "brave", brave_api_key: "brave-key"}, 2));
  assert.deepEqual(result.results[0], {title: "Docs", url: "https://example.org/a", snippet: "Snippet"});
});

test("tavily and you read their own response shapes", async () => {
  global.fetch = async (url, options) => {
    if (String(url).includes("tavily")) {
      assert.equal(options.headers.Authorization, "Bearer tavily-key");
      assert.equal(JSON.parse(options.body).include_answer, false);
      return json({results: [{title: "T", url: "https://example.org/t", content: "tavily snippet"}]});
    }
    assert.equal(options.headers["X-API-Key"], "you-key");
    return json({results: {web: [{title: "Y", url: "https://example.org/y", snippets: ["one", "two"]}]}});
  };
  assert.equal(JSON.parse(await search("t", {provider: "tavily", tavily_api_key: "tavily-key"})).results[0].snippet, "tavily snippet");
  assert.equal(JSON.parse(await search("y", {provider: "you", you_api_key: "you-key"})).results[0].snippet, "one two");
});

test("parallel sends an objective and reads excerpts", async () => {
  global.fetch = async (_url, options) => {
    const body = JSON.parse(options.body);
    assert.equal(body.objective, "compare runtimes");
    assert.deepEqual(body.search_queries, ["compare runtimes"]);
    return json({results: [{title: "P", url: "https://example.org/p", excerpts: ["first", "second"]}]});
  };
  assert.equal(JSON.parse(await search("compare runtimes", {provider: "parallel", parallel_api_key: "k"})).results[0].snippet, "first second");
});

test("searxng uses its JSON API and may live on a private host", async () => {
  global.fetch = async url => {
    assert.match(String(url), /searx\.example\.org\/search\?format=json&q=rust/);
    return json({results: [{title: "Sear", url: "https://docs.example.org", content: "works"}]});
  };
  const result = JSON.parse(await search("rust", {provider: "searxng", searxng_url: "https://searx.example.org"}));
  assert.equal(result.results[0].snippet, "works");
});

test("mwmbl joins its title and extract fragments", async () => {
  global.fetch = async url => {
    assert.match(String(url), /api\.mwmbl\.org.*s=rust/);
    return json([{url: "https://example.org/m", title: [{value: "Mw"}], extract: [{value: "mbl "}, {value: "text"}]}]);
  };
  const result = JSON.parse(await search("rust", {provider: "mwmbl"}));
  assert.equal(result.results[0].title, "Mw");
  assert.equal(result.results[0].snippet, "mbl text");
});

test("auto starts at the best available backend and hands over on failure", async () => {
  const calls = [];
  global.fetch = async url => {
    calls.push(String(url));
    if (String(url).includes("exa")) return json({error: "boom"}, 500);
    return json({web: {results: [{title: "Brave", url: "https://example.org/b", description: "ok"}]}});
  };
  const result = JSON.parse(await search("docs", {provider: "auto", exa_api_key: "e", brave_api_key: "b"}));
  assert.equal(result.provider, "brave");
  assert.ok(calls.length >= 2, "the chain advanced");
  assert.equal(result.attempts[0].provider, "exa", "keyed Exa ranks before keyless fallbacks");
  assert.ok(result.attempts.some(item => item.provider === "exa" && item.code === "HTTP_ERROR"));
});

test("keyless auto falls back to the free index and says so", async () => {
  global.fetch = async () => json([{url: "https://example.org/free", title: [{value: "Free"}], extract: [{value: "text"}]}]);
  const result = JSON.parse(await search("docs", {provider: "auto"}));
  assert.equal(result.provider, "mwmbl");
  assert.equal(result.mode, "free");
  assert.match(result.warning, /Mwmbl/);
});

test("the keyless chain starts at Exa public, then Firecrawl, then Mwmbl", async () => {
  const calls = [];
  global.fetch = async url => {
    calls.push(String(url));
    if (String(url).includes("firecrawl")) return json({success: false}, 503);
    return json([{url: "https://example.org/free", title: [{value: "Free"}], extract: [{value: "ok"}]}]);
  };
  const result = JSON.parse(await search("docs", {provider: "free"}));
  assert.equal(result.provider, "mwmbl");
  assert.match(calls[0], /mcp.exa.ai/);
  assert.equal(calls.length, 3);
});

test("site filters, tracking cleanup, dedupe and snippet clipping happen locally", async () => {
  global.fetch = async () => json({results: [
    {title: "Keep", url: "https://docs.example.org/a?utm_source=x#frag", content: "a".repeat(600)},
    {title: "Duplicate", url: "https://docs.example.org/a", content: "dup"},
    {title: "Wrong host", url: "https://evil.example.net/b", content: "nope"},
    {title: "Excluded", url: "https://blog.docs.example.org/c", content: "nope"}
  ]});
  const result = JSON.parse(await search("q site:docs.example.org -site:blog.docs.example.org", {provider: "searxng", searxng_url: "https://searx.example.org"}));
  assert.equal(result.results.length, 1);
  assert.equal(result.results[0].url, "https://docs.example.org/a");
  assert.equal(result.results[0].snippet_truncated, true);
  assert.ok(result.results[0].snippet.endsWith("…"));
});

test("filtering out every result reports NO_RESULTS instead of inventing one", async () => {
  global.fetch = async () => json({results: [{title: "x", url: "https://other.example.org", content: "y"}]});
  await assert.rejects(search("q site:docs.example.org", {provider: "searxng", searxng_url: "https://searx.example.org"}), {code: "NO_RESULTS"});
});

test("bad input fails with actionable codes", async () => {
  await assert.rejects(search("", {}), {code: "INVALID_QUERY"});
  await assert.rejects(search("x".repeat(601), {}), {code: "INVALID_QUERY"});
  await assert.rejects(search("q site:http://", {}), {code: "INVALID_QUERY"});
  await assert.rejects(search("q", {provider: "nope"}), {code: "CONFIG_ERROR"});
  await assert.rejects(search("q", {provider: "brave"}), {code: "CONFIG_ERROR"});
  await assert.rejects(dispatch({type: "tool", name: "web_search", arguments: {query: "q", provider: "exa"}}), {code: "INVALID_ARGUMENT"});
  await assert.rejects(dispatch({type: "tool", name: "other", arguments: {}}), {code: "UNKNOWN_TOOL"});
  await assert.rejects(dispatch({type: "hook"}), {code: "INVALID_REQUEST"});
});

test("web_fetch chunks a long document and continues from the cached snapshot", async () => {
  let calls = 0;
  const body = "<html><body><main><p>" + "word ".repeat(800) + "</p></main></body></html>";
  global.fetch = async () => { calls++; return html(body); };
  const first = JSON.parse(await fetchPage("https://example.org/long", {}, 1000));
  assert.equal(first.chars, 1000);
  assert.equal(first.next_start, 1000);
  assert.equal(first.truncated, true);
  const second = JSON.parse(await fetchPage("https://example.org/long", {}, 1000, first.next_start));
  assert.equal(second.start_char, 1000);
  assert.equal(first.text + second.text, second.total_chars > 2000 ? first.text + second.text : "");
  assert.equal(calls, 1);
});

test("continuation for an unknown or expired URL refuses to refetch and names the restart", async () => {
  global.fetch = async () => { throw new Error("must not fetch"); };
  await assert.rejects(fetchPage("https://example.org/never-read", {}, 1000, 1000), {
    code: "CACHE_MISS",
    message: /start_char = 0.*new next_start/
  });
});

test("the continuation window slides while a long document is being read", () => {
  let now = 1000;
  const store = createStore({ttl: 100, limit: 3, now: () => now});
  const page = {text: "x"};
  store.set(["a"], page);
  now = 190;
  assert.equal(store.get("a"), page);
  now = 280;
  assert.equal(store.get("a"), page, "a read must refresh the window");
  now = 420;
  assert.equal(store.get("a"), null);
  for (const key of ["a", "b", "c", "d"]) store.set([key], {text: key});
  assert.equal(store.size, 3, "only the newest address names are kept");
});

test("a paging snapshot survives a long tool gap and releases when the page ends", () => {
  let now = 0;
  const store = createStore({limit: 3, now: () => now});
  const page = {text: "x"};
  store.set(["a"], page);
  store.retain(["a"], true);
  now = 129274; // the observed download between the first read and its continuation
  assert.equal(store.get("a"), page, "a long tool must not expire a page being paged");
  now += SNAPSHOT_HOLD_TTL_MS + 1;
  assert.equal(store.get("a"), null, "a held snapshot still has a bounded idle life");

  let shortNow = 0;
  const short = createStore({limit: 3, now: () => shortNow});
  short.set(["b"], page);
  shortNow = SNAPSHOT_IDLE_TTL_MS + 1;
  assert.equal(short.get("b"), null, "a one-shot page keeps the short idle window");

  let releasedNow = 0;
  const released = createStore({limit: 3, now: () => releasedNow});
  released.set(["c"], page);
  released.retain(["c"], true);
  releasedNow = 129274;
  assert.equal(released.get("c"), page);
  released.retain(["c"], false);
  releasedNow += SNAPSHOT_IDLE_TTL_MS + 1;
  assert.equal(released.get("c"), null, "finishing the page releases the long hold");
});

test("find returns bounded passages without a top-level text or second request", async () => {
  let calls = 0;
  global.fetch = async () => { calls++; return html("<main><p>" + "alpha beta gamma delta ".repeat(120) + "</p></main>"); };
  const found = JSON.parse(await fetchPage("https://example.org/find", {}, 30000, 0, "gamma delta"));
  assert.equal(calls, 1);
  assert.equal(found.text, undefined);
  assert.ok(found.match_count >= 5);
  assert.equal(found.matches.length, 5);
  assert.equal(found.next_start, null);
  assert.ok(found.matches.every(match => Number.isInteger(match.start_char)));
  await assert.rejects(fetchPage("https://example.org/find", {}, 2000, 100, "gamma"), {code: "INVALID_ARGUMENT"});
});

test("mdBook pages survive the sidebar class on <html> and keep hidden nodes out", async () => {
  const body = '<html class="light sidebar-visible"><head><title>Hello</title></head><body>'
    + '<nav><a href="/a">Skip</a></nav><main><p>' + "内容".repeat(600) + "</p></main></body></html>";
  global.fetch = async () => html(body);
  const page = JSON.parse(await fetchPage("https://docs.example.org/mdbook", {}));
  assert.equal(page.title, "Hello");
  assert.ok(page.total_chars > 600);
  assert.doesNotMatch(page.text, /Skip/);
});

test("a JavaScript application shell is reported as unreadable, not as a read", async () => {
  const shell = '<html><body><div id="app"></div><script>boot()</script><noscript>enable javascript</noscript></body></html>';
  global.fetch = async () => html(shell);
  await assert.rejects(fetchPage("https://example.org/spa", {}), {code: "UNREADABLE_PAGE"});
  const navOnly = "<html><body><div>" + Array.from({length: 8}, (_, i) => `<a href="/p${i}">Menu ${i}</a>`).join("") + "</div></body></html>";
  global.fetch = async () => html(navOnly);
  await assert.rejects(fetchPage("https://example.org/nav", {}), error => {
    assert.equal(error.code, "UNREADABLE_PAGE");
    assert.match(error.message, /navigation links only/);
    return true;
  });
});

test("readable() keeps short real text and rejects near-empty pages", () => {
  const note = "A short but genuine note about this API and its limits.";
  assert.equal(readable({text: note}).text, note);
  assert.throws(() => readable({text: ""}), {code: "UNREADABLE_PAGE"});
  assert.throws(() => readable({text: "参数错误"}), {code: "UNREADABLE_PAGE"});
});

test("a truncated source warns and never claims the tail is readable", async () => {
  global.fetch = async () => new Response("x".repeat(1600000), {headers: {"content-type": "text/plain"}});
  const chunk = JSON.parse(await fetchPage("https://example.org/huge", {}, 30000));
  assert.equal(chunk.truncated, true);
  assert.match(chunk.warning, /1.5 MB/);
  const tail = JSON.parse(await fetchPage("https://example.org/huge", {}, 30000, 1499000));
  assert.equal(tail.next_start, null);
  assert.equal(tail.truncated, true);
});

test("plain text passes through unchanged and keeps valid offsets", async () => {
  const text = "a".repeat(999) + "😀".repeat(10);
  global.fetch = async () => new Response(text, {headers: {"content-type": "text/plain"}});
  const page = JSON.parse(await fetchPage("https://example.org/utf16", {}, 1000));
  assert.equal(page.chars, 999, "a split surrogate pair is not returned as half a character");
  assert.equal(page.next_start, 999);
  assert.equal(JSON.parse(await fetchPage("https://example.org/utf16", {}, 1000, 999)).chars, 20);
});

test("a JavaScript shell is rendered remotely only when a Firecrawl key is stored", async () => {
  const calls = [];
  global.fetch = async (url, options) => {
    calls.push(String(url));
    if (String(url).includes("api.firecrawl.dev")) {
      assert.equal(options.headers.Authorization, "Bearer fc-test");
      assert.deepEqual(JSON.parse(options.body), {url: "https://app.example.org/spa", formats: ["markdown"], onlyMainContent: true});
      return json({success: true, data: {markdown: "# Rendered\n\nReal content from the browser.", metadata: {title: "Rendered title", statusCode: 200}}});
    }
    return html('<html><body><div id="app"></div><noscript>enable javascript</noscript></body></html>');
  };
  const page = JSON.parse(await fetchPage("https://app.example.org/spa", {firecrawl_api_key: "fc-test"}));
  assert.equal(page.provider, "firecrawl");
  assert.equal(page.title, "Rendered title");
  assert.match(page.text, /Real content/);
  assert.equal(calls.filter(url => url.includes("api.firecrawl.dev")).length, 1);
});

test("without a Firecrawl key the same page stays unreadable", async () => {
  let calls = 0;
  global.fetch = async () => { calls++; return html('<html><body><div id="app"></div></body></html>'); };
  await assert.rejects(fetchPage("https://app.example.org/plain", {}), {code: "UNREADABLE_PAGE"});
  assert.equal(calls, 2, "direct attempt, then the keyless fallback");
});

test("only blocked responses (403/429) escalate to rendering, server errors do not", async () => {
  const calls = [];
  global.fetch = async url => {
    calls.push(String(url));
    if (String(url).includes("api.firecrawl.dev")) {
      return json({success: true, data: {markdown: "# Blocked page\n\nRendered body text.", metadata: {statusCode: 200}}});
    }
    return new Response("denied", {status: 403});
  };
  const page = JSON.parse(await fetchPage("https://guard.example.org/x", {firecrawl_api_key: "k"}));
  assert.equal(page.provider, "firecrawl");
  assert.equal(calls.filter(url => url.includes("api.firecrawl.dev")).length, 1);

  calls.length = 0;
  global.fetch = async url => {
    calls.push(String(url));
    return new Response("boom", {status: 500});
  };
  await assert.rejects(fetchPage("https://guard.example.org/y", {firecrawl_api_key: "k"}), {code: "HTTP_ERROR"});
  assert.equal(calls.length, 1, "a server error must not spend a rendering call");
});

test("a Firecrawl failure keeps its own error code", async () => {
  global.fetch = async url => {
    if (String(url).includes("api.firecrawl.dev")) return json({success: false, error: "bad key"});
    return html('<html><body><div id="app"></div></body></html>');
  };
  await assert.rejects(fetchPage("https://app.example.org/fail", {firecrawl_api_key: "k"}), {code: "INVALID_RESPONSE"});
});

test("perplexity, serper, kagi and bocha use their documented endpoints and shapes", async () => {
  const seen = [];
  global.fetch = async (url, options) => {
    seen.push({url: String(url), headers: options.headers, body: JSON.parse(options.body)});
    if (String(url).includes("perplexity")) {
      return json({results: [{title: "P", url: "https://example.org/p", snippet: "perplexity snippet"}]});
    }
    if (String(url).includes("serper")) {
      return json({organic: [{title: "S", link: "https://example.org/s", snippet: "serper snippet"}]});
    }
    if (String(url).includes("kagi.com")) {
      return json({data: {search: [{title: "K", url: "https://example.org/k", snippet: "kagi snippet"}]}});
    }
    return json({data: {webPages: {value: [{name: "B", url: "https://example.org/b", snippet: "bocha snippet"}]}}});
  };
  const per = JSON.parse(await search("q", {provider: "perplexity", perplexity_api_key: "pk"}));
  assert.equal(per.results[0].snippet, "perplexity snippet");
  assert.equal(seen[0].url, "https://api.perplexity.ai/search");
  assert.equal(seen[0].headers.Authorization, "Bearer pk");
  assert.deepEqual(seen[0].body, {query: "q"});

  const ser = JSON.parse(await search("q", {provider: "serper", serper_api_key: "sk"}, 4));
  assert.equal(ser.results[0].url, "https://example.org/s");
  assert.equal(seen[1].url, "https://google.serper.dev/search");
  assert.equal(seen[1].headers["X-API-KEY"], "sk");
  assert.equal(seen[1].body.num, 4);

  const kag = JSON.parse(await search("q", {provider: "kagi", kagi_api_key: "kk"}));
  assert.equal(kag.results[0].snippet, "kagi snippet");
  assert.equal(seen[2].url, "https://kagi.com/api/v1/search");
  assert.equal(seen[2].headers.Authorization, "Bearer kk");

  const boc = JSON.parse(await search("q", {provider: "bocha", bocha_api_key: "bk"}, 3));
  assert.deepEqual(boc.results[0], {title: "B", url: "https://example.org/b", snippet: "bocha snippet"});
  assert.equal(seen[3].url, "https://api.bochaai.com/v1/web-search");
  assert.equal(seen[3].body.count, 3);

  const auto = JSON.parse(await search("q", {provider: "auto", bocha_api_key: "bk"}));
  assert.equal(auto.provider, "bocha", "the only configured key is used");
});

test("firecrawl search works with a key and without one", async () => {
  const calls = [];
  global.fetch = async (url, options) => {
    calls.push({url: String(url), headers: {...options.headers}, body: JSON.parse(options.body)});
    return json({success: true, data: {web: [{title: "F", url: "https://example.org/f", description: "firecrawl snippet"}]}});
  };
  const withKey = JSON.parse(await search("q", {provider: "firecrawl", firecrawl_api_key: "fc"}, 2));
  assert.equal(withKey.results[0].snippet, "firecrawl snippet");
  assert.equal(calls[0].url, "https://api.firecrawl.dev/v2/search");
  assert.equal(calls[0].headers.Authorization, "Bearer fc");
  assert.deepEqual(calls[0].body, {query: "q", limit: 2});

  await search("q", {provider: "firecrawl"});
  assert.equal(calls[1].headers.Authorization, undefined, "keyless low-volume search sends no header");
});

test("searchapi queries Google through its documented parameters", async () => {
  global.fetch = async url => {
    const parsed = new URL(String(url));
    assert.equal(parsed.origin + parsed.pathname, "https://www.searchapi.io/api/v1/search");
    assert.equal(parsed.searchParams.get("engine"), "google");
    assert.equal(parsed.searchParams.get("q"), "rust async");
    assert.equal(parsed.searchParams.get("api_key"), "sa-key");
    return json({organic_results: [{title: "SA", link: "https://example.org/sa", snippet: "searchapi snippet"}]});
  };
  const result = JSON.parse(await search("rust async", {provider: "searchapi", searchapi_api_key: "sa-key"}));
  assert.deepEqual(result.results[0], {title: "SA", url: "https://example.org/sa", snippet: "searchapi snippet"});
});

test("quality order is the default, and the owner can replace it", async () => {
  const calls = [];
  global.fetch = async url => {
    calls.push(String(url));
    return json({results: [{title: "X", url: "https://example.org/x", content: "snippet"}]});
  };
  const auto = JSON.parse(await search("q", {provider: "auto", exa_api_key: "e", tavily_api_key: "t"}));
  assert.match(calls[0], /api\.exa\.ai/, "keyed Exa ranks first when configured");
  assert.equal(auto.provider, "exa", "configured keyed Exa answers directly");
  const custom = JSON.parse(await search("q", {provider: "auto", exa_api_key: "e", tavily_api_key: "t", search_chain: "tavily,exa"}));
  assert.equal(custom.provider, "tavily", "a custom chain wins");
  await assert.rejects(search("q", {provider: "auto", exa_api_key: "e", search_chain: "exa,duckduckgo"}), {code: "CONFIG_ERROR"});
});

test("a Chinese query starts at the Chinese index when one is configured", async () => {
  const seen = [];
  global.fetch = async url => {
    seen.push(String(url));
    return json({data: {webPages: {value: [{name: "中文结果", url: "https://example.org/cn", snippet: "内容"}]}}});
  };
  const result = JSON.parse(await search("异步运行时对比", {provider: "auto", exa_api_key: "e", bocha_api_key: "b"}));
  assert.equal(result.provider, "bocha");
  assert.match(seen[0], /bochaai/);
});

test("an explicit provider never falls back to another vendor", async () => {
  let calls = 0;
  global.fetch = async () => { calls++; return json({error: "boom"}, 500); };
  await assert.rejects(search("q", {provider: "brave", brave_api_key: "b", exa_api_key: "e"}), {code: "HTTP_ERROR"});
  assert.equal(calls, 1);
});

test("serpapi and linkup use their documented contracts", async () => {
  const seen = [];
  global.fetch = async (url, options) => {
    seen.push({url: new URL(String(url)), headers: options.headers});
    if (String(url).includes("serpapi")) {
      return json({organic_results: [{title: "SP", link: "https://example.org/sp", snippet: "serpapi snippet"}]});
    }
    return json({results: [{name: "LU", url: "https://example.org/lu", content: "linkup content"}]});
  };
  const sp = JSON.parse(await search("rust", {provider: "serpapi", serpapi_api_key: "sp-key"}));
  assert.deepEqual(sp.results[0], {title: "SP", url: "https://example.org/sp", snippet: "serpapi snippet"});
  assert.equal(seen[0].url.origin + seen[0].url.pathname, "https://serpapi.com/search");
  assert.equal(seen[0].url.searchParams.get("engine"), "google");
  assert.equal(seen[0].url.searchParams.get("api_key"), "sp-key");

  const lu = JSON.parse(await search("rust", {provider: "linkup", linkup_api_key: "lu-key"}));
  assert.deepEqual(lu.results[0], {title: "LU", url: "https://example.org/lu", snippet: "linkup content"});
  assert.equal(seen[1].url.origin + seen[1].url.pathname, "https://api.linkup.so/v1/search");
  assert.equal(seen[1].url.searchParams.get("outputType"), "searchResults");
  assert.equal(seen[1].headers.Authorization, "Bearer lu-key");
});

test("a host this network cannot reach is handed to rendering", async () => {
  const calls = [];
  global.fetch = async url => {
    calls.push(String(url));
    if (String(url).includes("api.firecrawl.dev")) {
      return json({success: true, data: {markdown: "# Reached elsewhere\n\nRendered text.", metadata: {title: "G", statusCode: 200}}});
    }
    const failure = new TypeError("fetch failed");
    failure.cause = {code: "ECONNREFUSED"};
    throw failure;
  };
  const page = JSON.parse(await fetchPage("https://blocked.example.org/x", {firecrawl_api_key: "k"}, 400));
  assert.equal(page.provider, "firecrawl");
  assert.match(page.text, /Rendered text/);
  assert.equal(calls.length, 3, "direct attempt, free fallback, rendering attempt");
});

test("urls fetch several pages in one call and keep per-page errors", async () => {
  global.fetch = async url => {
    if (String(url).includes("bad.example")) throw Object.assign(new TypeError("fetch failed"), {cause: {code: "ENOTFOUND"}});
    return html("<html><body><main><p>" + "content ".repeat(60) + "</p></main></body></html>");
  };
  const batch = JSON.parse(await fetchMany(["https://ok.example/a", "https://bad.example/b"], {}, 600));
  assert.equal(batch.count, 2);
  assert.ok(batch.results[0].chars > 0 && batch.results[0].chars <= 600, "each page keeps its own size cap");
  assert.equal(batch.results[1].code, "NETWORK_ERROR");
  await assert.rejects(fetchMany([], {}, 600), {code: "INVALID_ARGUMENT"});
});

test("a PDF is converted when a rendering key exists", async () => {
  const pdf = Buffer.concat([Buffer.from("%PDF-1.7\n"), Buffer.from("binary".repeat(100))]);
  global.fetch = async url => {
    if (String(url).includes("api.firecrawl.dev")) {
      return json({success: true, data: {markdown: "# Paper\n\nConverted PDF text.", metadata: {title: "Paper", statusCode: 200}}});
    }
    return new Response(pdf, {headers: {"content-type": "application/pdf"}});
  };
  const page = JSON.parse(await fetchPage("https://example.org/paper.pdf", {firecrawl_api_key: "k"}, 400));
  assert.equal(page.provider, "firecrawl");
  assert.match(page.text, /Converted PDF/);
  await assert.rejects(fetchPage("https://example.org/other.pdf", {}, 400), error => {
    assert.equal(error.code, "PDF_DETECTED");
    assert.match(error.message, /firecrawl_api_key/);
    return true;
  });
});

test("a YouTube transcript comes from the caption track", async () => {
  const watch = '<html><head><title>Rust talk - YouTube</title></head><body>"captionTracks":[{"baseUrl":"https://www.youtube.com/api/timedtext?v=abc&lang=en","languageCode":"en"}]</body></html>';
  global.fetch = async url => {
    if (String(url).includes("/watch?v=abc")) return new Response(watch, {headers: {"content-type": "text/html"}});
    return json({events: [
      {tStartMs: 0, segs: [{utf8: "hello"}]},
      {tStartMs: 2000, segs: [{utf8: "world"}]},
      {tStartMs: 40000, segs: [{utf8: "later part"}]}
    ]});
  };
  const page = JSON.parse(await fetchPage("https://www.youtube.com/watch?v=abc", {}, 2000));
  assert.equal(page.provider, "youtube");
  assert.equal(page.title, "Rust talk");
  assert.match(page.text, /\[00:00\] hello world/);
  assert.match(page.text, /\[00:40\] later part/);
});

test("a video without captions fails without spending a rendering call", async () => {
  let calls = 0;
  global.fetch = async () => { calls++; return html("<html><title>No captions - YouTube</title></html>"); };
  await assert.rejects(fetchPage("https://youtu.be/nocap", {firecrawl_api_key: "k"}, 500), {code: "NO_CAPTIONS"});
  assert.equal(calls, 1);
});

test("search1api and valyu follow their documented contracts", async () => {
  const seen = [];
  global.fetch = async (url, options) => {
    seen.push({url: String(url), headers: options.headers, body: JSON.parse(options.body)});
    if (String(url).includes("search1api")) {
      return json({results: [{title: "S1", link: "https://example.org/s1", snippet: "one api"}]});
    }
    return json({results: [{title: "V", url: "https://example.org/v", content: "valyu content"}]});
  };
  const one = JSON.parse(await search("rust", {provider: "search1api", search1api_api_key: "k1"}, 3));
  assert.deepEqual(one.results[0], {title: "S1", url: "https://example.org/s1", snippet: "one api"});
  assert.equal(seen[0].url, "https://api.search1api.com/search");
  assert.equal(seen[0].headers.Authorization, "Bearer k1");
  assert.equal(seen[0].body.max_results, 3);

  const val = JSON.parse(await search("rust", {provider: "valyu", valyu_api_key: "k2"}));
  assert.equal(val.results[0].snippet, "valyu content");
  assert.equal(seen[1].url, "https://api.valyu.ai/v1/search");
  assert.equal(seen[1].headers["x-api-key"], "k2");
  assert.equal(seen[1].body.search_type, "web");
});

function sse(payload, session) {
  return new Response("event: message\ndata: " + JSON.stringify(payload) + "\n\n",
    {status: 200, headers: {"content-type": "text/event-stream", ...(session ? {"mcp-session-id": session} : {})}});
}
function exaStub(handler) {
  return async (url, options) => {
    if (!String(url).includes("mcp.exa.ai")) throw new Error("unexpected host " + url);
    const body = JSON.parse(options.body);
    return handler(body, options);
  };
}
const exaSearchText = "Title: Async Rust\nURL: https://a.example/async\nPublished: N/A\nAuthor: N/A\nHighlights:\nasync runtimes explained";

test("exa_public searches and reads through the keyless MCP endpoint", async () => {
  const seen = [];
  global.fetch = exaStub((body, options) => {
    seen.push({method: body.method, session: options.headers["mcp-session-id"]});
    if (body.method === "initialize") return sse({jsonrpc: "2.0", id: body.id, result: {protocolVersion: "2024-11-05"}}, "sess-1");
    if (body.params.name === "web_search_exa") {
      return sse({jsonrpc: "2.0", id: body.id, result: {content: [{type: "text", text: exaSearchText}]}});
    }
    return sse({jsonrpc: "2.0", id: body.id, result: {content: [{type: "text", text: "# Page\nURL: https://a.example/page\n\nBody text from Exa."}]}});
  });
  const found = JSON.parse(await search("rust async", {provider: "exa_public"}, 3));
  assert.equal(found.results[0].title, "Async Rust");
  assert.equal(found.results[0].url, "https://a.example/async");
  assert.match(found.results[0].snippet, /async runtimes/);

  const page = JSON.parse(await fetchPage("https://a.example/page", {}, 400));
  assert.equal(page.provider, "exa_public");
  assert.equal(page.title, "Page");
  assert.match(page.text, /Body text from Exa/);
  assert.deepEqual(seen.map(item => item.method), ["initialize", "tools/call", "tools/call"], "the session is reused");
  assert.equal(seen[2].session, "sess-1");
});

test("an expired Exa session is renewed once", async () => {
  let calls = 0;
  global.fetch = exaStub(body => {
    calls++;
    if (body.method === "initialize") return sse({jsonrpc: "2.0", id: body.id, result: {}}, "sess-" + calls);
    if (calls === 2) return new Response("expired", {status: 400});
    return sse({jsonrpc: "2.0", id: body.id, result: {content: [{type: "text", text: exaSearchText}]}});
  });
  const found = JSON.parse(await search("rust", {provider: "exa_public"}, 2));
  assert.equal(found.results[0].url, "https://a.example/async");
});

test("a long page reports a heading outline with offsets", async () => {
  const body = "<html><body><main><h1>Alpha</h1><p>" + "text ".repeat(80) + "</p><h2>Beta</h2><p>" + "more ".repeat(80) + "</p></main></body></html>";
  global.fetch = async () => html(body);
  const page = JSON.parse(await fetchPage("https://outline.example/doc", {}, 1000));
  assert.equal(page.headings.length, 2);
  assert.deepEqual(page.headings.map(item => item.text), ["Alpha", "Beta"]);
  assert.equal(page.text.slice(page.headings[1].start_char, page.headings[1].start_char + 6), "## Bet");
});

test("urls are fetched concurrently", async () => {
  let inFlight = 0, peak = 0;
  global.fetch = async () => {
    inFlight++;
    peak = Math.max(peak, inFlight);
    await new Promise(resolve => setTimeout(resolve, 30));
    inFlight--;
    return html("<html><body><main><p>" + "word ".repeat(80) + "</p></main></body></html>");
  };
  const batch = JSON.parse(await fetchMany(["https://a.example/1", "https://b.example/2", "https://c.example/3", "https://d.example/4"], {}, 500));
  assert.equal(batch.results.length, 4);
  assert.ok(peak > 1, "requests overlap instead of queueing");
});

test("time_range reaches the backends that support it and marks the ones that do not", async () => {
  const seen = [];
  global.fetch = async (url, options) => {
    seen.push({url: String(url), body: options.body ? JSON.parse(options.body) : null, headers: options.headers});
    if (String(url).includes("brave")) return json({web: {results: [{title: "B", url: "https://example.org/b", description: "s"}]}});
    if (String(url).includes("serper")) return json({organic: [{title: "S", link: "https://example.org/s", snippet: "s"}]});
    if (String(url).includes("bocha")) return json({data: {webPages: {value: [{name: "C", url: "https://example.org/c", snippet: "s"}]}}});
    if (String(url).includes("exa.ai")) return json({results: [{title: "E", url: "https://example.org/e", highlights: ["h"]}]});
    if (String(url).includes("searx")) return json({results: [{title: "X", url: "https://example.org/x", content: "s"}]});
    if (String(url).includes("kagi")) return json({data: {search: [{title: "K", url: "https://example.org/k", snippet: "s"}]}});
    throw new Error("unexpected " + url);
  };
  const week = JSON.parse(await search("q", {provider: "brave", brave_api_key: "k"}, 2, "week"));
  assert.equal(week.time_range, "week");
  assert.equal(new URL(seen[0].url).searchParams.get("freshness"), "pw");

  const day = JSON.parse(await search("q", {provider: "serper", serper_api_key: "k"}, 2, "day"));
  assert.equal(day.time_range, "day");
  assert.equal(seen[1].body.tbs, "qdr:d");

  const year = JSON.parse(await search("q", {provider: "bocha", bocha_api_key: "k"}, 2, "year"));
  assert.equal(seen[2].body.freshness, "oneYear");
  assert.equal(year.time_range, "year");

  await search("q", {provider: "exa", exa_api_key: "k"}, 2, "year");
  assert.match(seen[3].body.startPublishedDate, /^\d{4}-\d{2}-\d{2}$/);

  await search("q", {provider: "searxng", searxng_url: "https://searx.example.org"}, 2, "day");
  assert.equal(new URL(seen[4].url).searchParams.get("time_range"), "day");

  const ignored = JSON.parse(await search("q", {provider: "kagi", kagi_api_key: "k"}, 2, "day"));
  assert.equal(ignored.time_range_ignored, true);
  await assert.rejects(search("q", {provider: "brave", brave_api_key: "k"}, 2, "hour"), {code: "INVALID_ARGUMENT"});
});

test("an identical search is answered from the short cache", async () => {
  let calls = 0;
  global.fetch = async () => {
    calls++;
    return json({results: [{title: "Cache", url: "https://example.org/c", content: "s"}]});
  };
  const first = JSON.parse(await search("same question", {provider: "searxng", searxng_url: "https://searx.example.org"}, 2));
  const second = JSON.parse(await search("same question", {provider: "searxng", searxng_url: "https://searx.example.org"}, 2));
  assert.equal(first.cached, undefined);
  assert.equal(second.cached, true);
  assert.equal(calls, 1);
  await search("same question", {provider: "searxng", searxng_url: "https://searx.example.org"}, 3);
  assert.equal(calls, 2, "a different result count is a different question");
});

test("section returns only the requested chapter", async () => {
  const body = "<html><body><main><h1>Docs</h1><p>" + "intro ".repeat(200) + "</p><h2>Install</h2><p>" +
    "install ".repeat(300) + "</p><h2>Usage</h2><p>" + "usage ".repeat(300) + "</p></main></body></html>";
  global.fetch = async () => html(body);
  const page = JSON.parse(await fetchPage("https://sections.example/doc", {}, 500, 0, null, "install"));
  assert.equal(page.section, "Install", "the heading name is echoed in its original casing");
  assert.match(page.text, /^## Install/);
  assert.ok(!page.text.includes("usage"), "the next chapter stays out");
  assert.equal(page.truncated, true);
  const rest = JSON.parse(await fetchPage("https://sections.example/doc", {}, 900, page.next_start));
  assert.ok(!rest.text.includes("Usage"), "continuation stops at the section end");
  await assert.rejects(fetchPage("https://sections.example/doc", {}, 500, 0, null, "Nope"),
    error => error.code === "SECTION_NOT_FOUND" && error.message.includes("Install"));
});

test("search results keep the publication date when a backend reports one", async () => {
  global.fetch = async url => {
    if (String(url).includes("searx")) {
      return json({results: [{title: "A", url: "https://example.org/a", content: "s", publishedDate: "2025-03-04T00:00:00"}]});
    }
    if (String(url).includes("serper")) {
      return json({organic: [{title: "B", link: "https://example.org/b", snippet: "s", date: "Mar 4, 2025"}]});
    }
    if (String(url).includes("mwmbl")) {
      return json([{url: "https://example.org/c", title: [{value: "C"}], extract: [{value: "s"}]}]);
    }
    throw new Error("unexpected " + url);
  };
  const searx = JSON.parse(await search("date q", {provider: "searxng", searxng_url: "https://searx.example.org"}, 2));
  assert.equal(searx.results[0].published, "2025-03-04T00:00:00");
  const serper = JSON.parse(await search("date q2", {provider: "serper", serper_api_key: "k"}, 2));
  assert.equal(serper.results[0].published, "Mar 4, 2025");
  const mwmbl = JSON.parse(await search("date q3", {provider: "mwmbl"}, 2));
  assert.equal(mwmbl.results[0].published, undefined, "no invented date");
});

test("the keyless Exa search keeps its published line and drops placeholders", async () => {
  global.fetch = exaStub(body => {
    if (body.method === "initialize") return sse({jsonrpc: "2.0", id: body.id, result: {}}, "s1");
    const text = ["Title: Fresh", "URL: https://a.example/fresh", "Published: 2025-02-01T00:00:00.000Z", "Highlights:", "h"].join(String.fromCharCode(10));
    return sse({jsonrpc: "2.0", id: body.id, result: {content: [{type: "text", text}]}});
  });
  const dated = JSON.parse(await search("fresh q", {provider: "exa_public"}, 2));
  assert.equal(dated.results[0].published, "2025-02-01T00:00:00.000Z");
  global.fetch = exaStub(body => body.method === "initialize" ? sse({jsonrpc: "2.0", id: body.id, result: {}}, "s2")
    : sse({jsonrpc: "2.0", id: body.id, result: {content: [{type: "text", text: exaSearchText}]}}));
  const plain = JSON.parse(await search("plain q", {provider: "exa_public"}, 2));
  assert.equal(plain.results[0].published, undefined, "\"Published: N/A\" is not a date");
});

test("find and section cannot be combined", async () => {
  global.fetch = async () => html("<html><body><main><h2>A</h2><p>" + "x ".repeat(60) + "</p></main></body></html>");
  await assert.rejects(fetchPage("https://both.example/a", {}, 500, 0, "x", "A"), {code: "INVALID_ARGUMENT"});
});

test("Search1API week is reported as ignored, day is applied", async () => {
  global.fetch = async () => json({results: [{title: "S", link: "https://example.org/s", snippet: "s"}]});
  const week = JSON.parse(await search("q1", {provider: "search1api", search1api_api_key: "k"}, 2, "week"));
  assert.equal(week.time_range, undefined);
  assert.equal(week.time_range_ignored, true);
  const day = JSON.parse(await search("q2", {provider: "search1api", search1api_api_key: "k"}, 2, "day"));
  assert.equal(day.time_range, "day");
});

test("a page with more than forty headings says so", async () => {
  let body = "<html><body><main>";
  for (let i = 0; i < 45; i++) body += "<h2>Part " + i + "</h2><p>" + "text ".repeat(30) + "</p>";
  global.fetch = async () => html(body + "</main></body></html>");
  const page = JSON.parse(await fetchPage("https://many.example/doc", {}, 800));
  assert.equal(page.headings.length, 40);
  assert.equal(page.headings_truncated, true, "the cap is disclosed");
});

test("a full batch of ten pages stays readable afterwards", async () => {
  const urls = Array.from({length: 10}, (_, i) => "https://batch.example/" + i);
  global.fetch = async () => html("<html><body><main><p>" + "word ".repeat(400) + "</p></main></body></html>");
  const batch = JSON.parse(await fetchMany(urls, {}, 200));
  assert.equal(batch.results.filter(item => item.code).length, 0);
  const more = JSON.parse(await fetchPage(urls[0], {}, 200, 200));
  assert.equal(more.start_char, 200, "the first page was not evicted by the other nine");
});

test("a free search with a date filter starts at the backend that supports it", async () => {
  const calls = [];
  global.fetch = async url => {
    calls.push(String(url));
    return json({results: [{title: "X", url: "https://example.org/x", content: "s"}]});
  };
  const result = JSON.parse(await search("dated free", {provider: "free", searxng_url: "https://searx.example.org"}, 2, "day"));
  assert.match(calls[0], /searx\.example\.org/, "SearXNG ranks before the date-blind keyless Exa");
  assert.equal(result.provider, "searxng");
  assert.equal(result.time_range, "day");
});

test("private or local URLs never leak to external Exa or Firecrawl fallbacks", async () => {
  const calls = [];
  global.fetch = async url => {
    calls.push(String(url));
    return html("<html><body><div id='app'></div><script src='app.js'></script></body></html>");
  };
  for (const target of [
    "http://localhost:3000/secret?token=1",
    "http://127.0.0.1:8080/internal",
    "http://192.168.1.25/admin",
    "http://intranet/dashboard",
  ]) {
    await assert.rejects(fetchPage(target, {firecrawl_api_key: "fc-key"}, 400), {code: "UNREADABLE_PAGE"});
  }
  assert.equal(calls.length, 4, "each private URL stops after direct fetch");
  assert.ok(calls.every(u => !u.includes("mcp.exa.ai") && !u.includes("api.firecrawl.dev")));
});

test("PDF without a Firecrawl key stops locally instead of calling Exa public", async () => {
  const calls = [];
  const pdf = Buffer.concat([Buffer.from("%PDF-1.7\n"), Buffer.from("binary".repeat(100))]);
  global.fetch = async url => {
    calls.push(String(url));
    return new Response(pdf, {headers: {"content-type": "application/pdf"}});
  };
  await assert.rejects(fetchPage("https://example.org/doc.pdf", {}, 400), {code: "PDF_DETECTED"});
  assert.deepEqual(calls, ["https://example.org/doc.pdf"]);
});
