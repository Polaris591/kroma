"use strict";
const {test,beforeEach,afterEach} = require("node:test");
const assert = require("node:assert/strict");
const {dispatch,parseDDG,parseBingRSS,plainText,fetchPage,search} = require("./web");
const originalFetch=global.fetch;
const credentials = Object.fromEntries(["EXA_API_KEY", "BRAVE_SEARCH_API_KEY", "TAVILY_API_KEY", "YDC_API_KEY", "PARALLEL_API_KEY", "FIRECRAWL_API_KEY", "FIRECRAWL_BASE_URL"].map(key => [key, process.env[key]]));
beforeEach(() => { for (const key of Object.keys(credentials)) delete process.env[key]; });
afterEach(() => {
  global.fetch = originalFetch;
  for (const [key, value] of Object.entries(credentials)) {
    if (value === undefined) delete process.env[key]; else process.env[key] = value;
  }
});
function mock(body,type="text/html") {
  return new Response(body,{status:200,headers:{"content-type":type}});
}
test("DDG HTML results decode redirect and snippet",()=>{
  const html='<div class="result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fa">Sample &amp; Example</a><a class="result__snippet">A <b>test</b> description</a></div>';
  assert.deepEqual(parseDDG(html,4),[{title:"Sample & Example",url:"https://example.org/a",snippet:"A test description"}]);
});
test("Bing RSS search works without API key",async()=>{
  const rss='<rss><channel><item><title>Topic &amp; Title</title><link>https://example.org/topic</link><description>Found &amp; read</description></item></channel></rss>';
  assert.deepEqual(parseBingRSS(rss,5),[{title:"Topic & Title",url:"https://example.org/topic",snippet:"Found & read"}]);
  global.fetch=async url=>{
    assert.match(String(url),/format=rss/);
    return mock(rss,"application/rss+xml");
  };
  const result=JSON.parse(await search("topic", {provider: "bing"}));
  assert.equal(result.provider,"bing");
  assert.equal(result.results[0].url,"https://example.org/topic");
});
test("DDG web_search and web_fetch extract page",async()=>{
  global.fetch=async url=>{
    if(String(url).includes("duckduckgo")) return mock('<a class="result__a" href="https://example.org/a">A test</a>');
    return mock('<html><head><title>Page &amp; topic</title></head><nav>Skip</nav><main><h1>Hello</h1><p>Readable <strong>text</strong>.</p></main><footer>Noise</footer></html>');
  };
  const searched=JSON.parse(await dispatch({type:"tool",name:"web_search",arguments:{query:"hello"},config:{provider:"duckduckgo"}}));
  assert.equal(searched.results[0].url,"https://example.org/a");
  const fetched=JSON.parse(await dispatch({type:"tool",name:"web_fetch",arguments:{url:"https://example.org"},config:{}}));
  assert.equal(fetched.title,"Page & topic");
  assert.match(fetched.text,/Hello/);
  assert.doesNotMatch(fetched.text,/Skip|Noise/);
});
test("Brave uses header token and structured web results",async()=>{
  global.fetch=async (url,opts)=>{
    assert.match(String(url),/api.search.brave.com/);
    assert.equal(opts.headers["X-Subscription-Token"],"test-key");
    return mock(JSON.stringify({web:{results:[{title:"Brave",url:"https://example.org",description:"Snippet"}]}}),"application/json");
  };
  const result=JSON.parse(await search("test",{provider:"brave",brave_api_key:"test-key"}));
  assert.equal(result.results[0].title,"Brave");
});
test("SearXNG uses JSON search API",async()=>{
  global.fetch=async url=>{
    assert.match(String(url),/format=json/);
    return mock(JSON.stringify({results:[{title:"Sear",url:"https://docs.example.org",content:"works"}]}),"application/json");
  };
  const result=JSON.parse(await search("rust",{provider:"searxng",searxng_url:"https://searx.example.org"}));
  assert.equal(result.results[0].snippet,"works");
});
test("fetch truncation returns bounded text",async()=>{
  global.fetch=async()=>mock("<main>"+"word ".repeat(500)+"</main>");
  const result=JSON.parse(await fetchPage("https://example.org",{},1000));
  assert.equal(result.truncated,true);
  assert.equal(result.text.length,1000);
});
test("rejects unsupported URL and missing key",async()=>{
  await assert.rejects(fetchPage("file:///etc/passwd"),/HTTP/);
  await assert.rejects(search("x",{provider:"brave",brave_api_key:""}),/BRAVE_SEARCH_API_KEY/);
  assert.equal(plainText("<b>a</b> &amp; <p>b</p>"),"a & b");
});


test("site filters reject unrelated hosts, deduplicate tracking URLs and fill the requested count", async () => {
  global.fetch = async () => mock(JSON.stringify({results: [
    {title: "Wrong", url: "https://notexample.org/a"},
    {title: "Wrong suffix", url: "https://example.org.evil.test/a"},
    {title: "A", url: "https://docs.example.org/a?utm_source=search#anchor", content: "<b>Useful</b>"},
    {title: "Duplicate", url: "https://docs.example.org/a"},
    {title: "Excluded", url: "https://private.example.org/a"},
    {title: "Invalid", url: "javascript:alert(1)"},
    {title: "B", url: "https://example.org/b"}
  ]}), "application/json");
  const result = JSON.parse(await search("docs site:example.org -site:private.example.org",
    {provider: "searxng", searxng_url: "https://search.example.org"}, 2));
  assert.deepEqual(result.results.map(x => x.url), ["https://docs.example.org/a", "https://example.org/b"]);
  assert.equal(result.results[0].snippet, "Useful");
});



test("SearXNG search endpoint is not duplicated and invalid JSON is reported", async () => {
  global.fetch = async url => {
    assert.equal(new URL(url).pathname, "/instance/search");
    return mock("not json", "application/json");
  };
  await assert.rejects(search("test", {provider: "searxng", searxng_url: "https://search.example.org/instance/search"}), {code: "INVALID_RESPONSE"});
});

test("RSS CDATA titles, descriptions and URLs are extracted", () => {
  const results = parseBingRSS('<rss><item><title><![CDATA[Title &amp; More]]></title><link><![CDATA[https://example.org/cdata]]></link><description><![CDATA[<b>Snippet</b>]]></description></item></rss>', 1);
  assert.deepEqual(results, [{title: "Title & More", url: "https://example.org/cdata", snippet: "Snippet"}]);
});

test("DDG snippets do not leak from the next result and missing href is ignored", () => {
  const results = parseDDG('<a class="result__a">Invalid</a><a class="result__a" href="https://a.example.org">A</a><a class="result__a" href="https://b.example.org">B</a><div class="result__snippet">B snippet</div>', 5);
  assert.equal(results.length, 2);
  assert.equal(results[0].snippet, "");
  assert.equal(results[1].snippet, "B snippet");
});

test("extracts documentation body instead of sidebar and preserves code, links and tables", async () => {
  global.fetch = async () => mock(`<title>Docs &amp; API</title><body>
    <div id="column2"><div id="toc">Huge navigation</div></div>
    <div id="apicontent"><header><h1>API reference</h1></header>
      <p>Read <a href="../guide?q=1&amp;x=2">the guide</a>.</p>
      <div><p>Nested text</p></div><p>After nested div</p>
      <pre><code>if (a &lt; b) {\n  run();\n}</code></pre>
      <table><tr><th>Name</th><th>Type</th></tr><tr><td>count</td><td>number</td></tr></table>
      <p hidden>Hidden noise</p><div aria-hidden="true">Not visible</div>
      <script>malicious()</script><footer>Footer noise</footer>
    </div></body>`);
  const page = JSON.parse(await fetchPage("https://docs.example.org/api/reference"));
  assert.equal(page.title, "Docs & API");
  assert.match(page.text, /# API reference/);
  assert.match(page.text, /\[the guide\]\(https:\/\/docs.example.org\/guide\?q=1&x=2\)/);
  assert.match(page.text, /After nested div/);
  assert.match(page.text, /```\nif \(a < b\) {\n  run\(\);\n}\n```/);
  assert.match(page.text, /\| Name \| Type \|/);
  assert.doesNotMatch(page.text, /Huge navigation|Hidden noise|Not visible|malicious|Footer noise/);
});

test("selects complete main rather than the first tiny article", async () => {
  global.fetch = async () => mock('<body><article>Ad</article><main><div>First</div><p>Complete documentation body.</p></main></body>');
  const page = JSON.parse(await fetchPage("https://example.org/multiple"));
  assert.match(page.text, /Complete documentation/);
  assert.doesNotMatch(page.text, /Ad/);
});

test("long pages can be continued without refetching or losing characters", async () => {
  let calls = 0;
  const source = "A".repeat(999) + "😀" + "end ".repeat(600);
  global.fetch = async () => { calls++; return mock(source, "text/plain"); };
  const first = JSON.parse(await fetchPage("https://example.org/continuation", {}, 1000));
  assert.equal(first.next_start, 999);
  assert.equal(first.total_chars, source.length);
  const next = JSON.parse(await dispatch({type: "tool", name: "web_fetch", arguments: {
    url: "https://example.org/continuation", start_char: first.next_start, max_chars: 30000
  }}));
  assert.equal(first.text + next.text, source);
  assert.equal(next.next_start, null);
  assert.equal(next.truncated, false);
  assert.equal(calls, 1);
});

test("a new read from zero refreshes a cached page and offsets are validated", async () => {
  global.fetch = async () => mock("old", "text/plain");
  await fetchPage("https://example.org/refresh");
  global.fetch = async () => mock("new", "text/plain");
  assert.equal(JSON.parse(await fetchPage("https://example.org/refresh")).text, "new");
  await assert.rejects(fetchPage("https://example.org/refresh", {}, 1000, -1), {code: "INVALID_OFFSET"});
  await assert.rejects(fetchPage("https://example.org/refresh", {}, 1000, 100), {code: "INVALID_OFFSET"});
});

test("specific main content wins over a larger generic wrapper with unquoted HTML attributes", async () => {
  global.fetch = async () => mock(`<div id=content><div role=navigation>${"Sidebar ".repeat(500)}</div>
    <header>Site branding</header><div id=gtoc>${"Other API links ".repeat(500)}</div>
    <div role=main id=apicontent><h1>Global objects</h1><p>Actual API documentation.</p></div></div>`);
  const page = JSON.parse(await fetchPage("https://example.org/wrapped"));
  assert.match(page.text, /Actual API documentation/);
  assert.doesNotMatch(page.text, /Sidebar|Site branding|Other API/);
});

test("script markup cannot close the surrounding main or leak script text", async () => {
  global.fetch = async () => mock('<main><p>Before</p><script>const html="</main><p>Injected</p>";</script><p>After</p></main>');
  const page = JSON.parse(await fetchPage("https://example.org/script"));
  assert.match(page.text, /Before/);
  assert.match(page.text, /After/);
  assert.doesNotMatch(page.text, /Injected|const html/);
});



test("extraction limit is explicit, output whitespace is compact and code whitespace survives", async () => {
  global.fetch = async () => mock('<main><div><p>First</p></div>\n<div><p>Second</p></div><pre>line 1\n\n\nline 4</pre></main>');
  const page = JSON.parse(await fetchPage("https://example.org/spacing"));
  assert.match(page.text, /First\n\nSecond/);
  assert.match(page.text, /```\nline 1\n\n\nline 4\n```/);
  global.fetch = async () => mock("<main>" + "<span>x</span>".repeat(50001) + "</main>");
  await assert.rejects(fetchPage("https://example.org/too-many-elements"), error => {
    assert.equal(error.code, "UNREADABLE_PAGE");
    assert.match(error.message, /50,000/);
    return true;
  });
});

test("hidden main candidates cannot replace visible content", async () => {
  global.fetch = async () => mock('<body><main hidden>Hidden main</main><div><p>Visible body</p></div></body>');
  const page = JSON.parse(await fetchPage("https://example.org/hidden-main"));
  assert.equal(page.text, "Visible body");
});

test("auto rejects invalid API priority configuration before network access", async () => {
  let calls = 0;
  global.fetch = async () => { calls++; throw Error("must not fetch"); };
  for (const search_priority of [[], ["auto"], ["bing"], ["duckduckgo"], ["exa", "exa"], "exa,", {}, ["secret-key"]]) {
    await assert.rejects(search("guide", {exa_api_key: "secret", search_priority}), error => {
      assert.equal(error.code, "CONFIG_ERROR");
      assert.doesNotMatch(error.message, /secret/);
      return true;
    });
  }
  assert.equal(calls, 0);
});

test("unrelated results fail domain checks without switching providers", async () => {
  let calls = 0;
  global.fetch = async () => {
    calls++;
    return mock('<rss><item><title>Wrong</title><link>https://unrelated.org</link></item></rss>', "application/rss+xml");
  };
  await assert.rejects(search("fetch site:nodejs.org", {provider: "bing"}), {code: "NO_RESULTS"});
  assert.equal(calls, 1);
});

test("explicit provider overrides multiple configured backends and never retries", async () => {
  let calls = 0;
  global.fetch = async url => {
    calls++;
    assert.equal(new URL(url).hostname, "api.search.brave.com");
    return new Response("denied", {status: 403});
  };
  await assert.rejects(search("test", {provider: "brave", brave_api_key: "key", exa_api_key: "other", searxng_url: "https://search.example.org"}), {code: "HTTP_ERROR"});
  assert.equal(calls, 1);
});

test("evicted, missing and expired continuation snapshots fail without refetching", async () => {
  let calls = 0;
  global.fetch = async () => { calls++; return mock("content ".repeat(200), "text/plain"); };
  for (let i = 0; i < 7; i++) await fetchPage(`https://example.org/eviction-${i}`);
  await assert.rejects(fetchPage("https://example.org/eviction-0", {}, 1000, 100), {code: "CACHE_MISS"});
  await assert.rejects(fetchPage("https://example.org/never-read", {}, 1000, 100), {code: "CACHE_MISS"});
  assert.equal(calls, 7);
  const now = Date.now;
  try {
    Date.now = () => now() + 120000;
    await assert.rejects(fetchPage("https://example.org/eviction-6", {}, 1000, 100), error => {
      assert.equal(error.code, "CACHE_MISS");
      assert.match(error.message, /start_char = 0/);
      return true;
    });
  } finally { Date.now = now; }
  assert.equal(calls, 7);
  const fresh = JSON.parse(await fetchPage("https://example.org/eviction-6", {}, 1000));
  const next = JSON.parse(await fetchPage("https://example.org/eviction-6", {}, 1000, fresh.next_start));
  assert.equal(next.next_start, null);
  assert.equal(calls, 8);
});

test("a single configured backend is selected and gets no fallback on failure", async () => {
  let calls = 0;
  global.fetch = async url => {
    calls++;
    assert.equal(new URL(url).hostname, "search.example.org");
    return new Response("bad gateway", {status: 502});
  };
  await assert.rejects(search("test", {searxng_url: "https://search.example.org", search_priority: ["searxng"]}), {code: "HTTP_ERROR"});
  assert.equal(calls, 1);
  global.fetch = async () => mock(JSON.stringify({results: [{title: "Doc", url: "https://example.org/doc"}]}), "application/json");
  assert.equal(JSON.parse(await search("test", {searxng_url: "https://search.example.org", search_priority: ["searxng"]})).provider, "searxng");
});

test("Exa POST uses native domain filters and original highlights, not generated summaries", async () => {
  process.env.EXA_API_KEY = "env-key";
  let calls = 0;
  global.fetch = async (url, options) => {
    calls++;
    assert.equal(String(url), "https://api.exa.ai/search");
    assert.equal(options.method, "POST");
    assert.equal(options.headers["x-api-key"], "env-key");
    assert.equal(options.headers["Content-Type"], "application/json");
    assert.equal(options.redirect, "manual");
    assert.deepEqual(JSON.parse(options.body), {
      query: "Node fetch", type: "auto", numResults: 2, contents: {highlights: true},
      includeDomains: ["nodejs.org"], excludeDomains: ["private.nodejs.org"]
    });
    return mock(JSON.stringify({results: [
      {title: "Wrong host", url: "https://nodejs.org.evil.test"},
      {title: "Excluded", url: "https://private.nodejs.org/doc"},
      {title: "Fetch", url: "https://nodejs.org/api/globals.html?utm_source=exa", highlights: ["First <b>passage</b>", "Second passage", null], summary: "Do not return this"},
      {title: "More", url: "https://nodejs.org/api/http.html", summary: "Not an original passage"}
    ], output: {content: "Generated answer"}}), "application/json");
  };
  const result = JSON.parse(await search("Node fetch site:nodejs.org -site:private.nodejs.org", {}, 2));
  assert.equal(result.provider, "exa");
  assert.equal(result.results.length, 2);
  assert.equal(result.results[0].url, "https://nodejs.org/api/globals.html");
  assert.equal(result.results[0].snippet, "First passage Second passage");
  assert.equal(result.results[1].snippet, "");
  assert.doesNotMatch(JSON.stringify(result), /env-key|Generated|Do not return/);
  assert.equal(calls, 1);
});

test("Exa configuration overrides environment and sends only requested bounded results", async () => {
  process.env.EXA_API_KEY = "env-key";
  global.fetch = async (_url, options) => {
    assert.equal(options.headers["x-api-key"], "configured-key");
    assert.deepEqual(JSON.parse(options.body), {
      query: "plain query", type: "auto", numResults: 10, contents: {highlights: true}
    });
    return mock(JSON.stringify({results: [{title: "Found", url: "https://example.org"}]}), "application/json");
  };
  const result = JSON.parse(await search("plain query", {provider: "exa", exa_api_key: "configured-key"}, 100));
  assert.equal(result.results.length, 1);
});

test("JSON providers distinguish malformed responses from empty results", async () => {
  for (const config of [
    {provider: "exa", exa_api_key: "key"},
    {provider: "brave", brave_api_key: "key"},
    {provider: "searxng", searxng_url: "https://search.example.org"}
  ]) {
    for (const body of ["not-json", "null", "{}", '{"results":{}}', '{"web":{"results":{}}}']) {
      global.fetch = async () => mock(body, "application/json");
      await assert.rejects(search("test", config), {code: "INVALID_RESPONSE"});
    }
    global.fetch = async () => mock(JSON.stringify(config.provider === "brave" ? {web: {results: []}} : {results: []}), "application/json");
    await assert.rejects(search("test", config), {code: "NO_RESULTS"});
  }
});

test("Exa errors do not leak credentials, retry or fall back", async () => {
  let calls = 0;
  for (const status of [401, 403, 429, 500]) {
    global.fetch = async () => { calls++; return new Response("secret-key", {status}); };
    await assert.rejects(search("test", {provider: "exa", exa_api_key: "secret-key", searxng_url: "https://search.example.org"}), error => {
      assert.equal(error.code, "HTTP_ERROR");
      assert.match(error.message, new RegExp(String(status)));
      assert.doesNotMatch(error.message, /secret-key/);
      return true;
    });
  }
  assert.equal(calls, 4);
  await assert.rejects(search("test", {provider: "exa"}), {code: "CONFIG_ERROR"});
});

test("invalid queries, sites and provider settings do not make requests", async () => {
  global.fetch = async () => { throw Error("must not fetch"); };
  for (const query of ["", " ", null, "x".repeat(601), "test site:!@#$"]) {
    await assert.rejects(search(query, {provider: "exa", exa_api_key: "key"}), {code: "INVALID_QUERY"});
  }
  await assert.rejects(search("test", {provider: "unknown"}), {code: "CONFIG_ERROR"});
});

test("table captions, section headings, headers and code survive one complete read", async () => {
  global.fetch = async () => mock('<title>Versioned API</title><main><h2>Node 22 options</h2><table><caption>Request settings</caption><thead><tr><th>Option</th><th>Default</th></tr></thead><tbody><tr><td>timeout</td><td>15000 ms</td></tr><tr><td>signal</td><td>undefined</td></tr></tbody></table><pre><code>if (a &lt; b) {\n  fetch(url);\n}</code></pre></main>');
  const page = JSON.parse(await fetchPage("https://example.org/table-context"));
  assert.match(page.text, /## Node 22 options/);
  assert.match(page.text, /Request settings/);
  assert.match(page.text, /\| Option \| Default \|/);
  assert.match(page.text, /\| timeout \| 15000 ms \|/);
  assert.match(page.text, /\| signal \| undefined \|/);
  assert.match(page.text, /```\nif \(a < b\) {\n  fetch\(url\);\n}\n```/);
  assert.equal(page.next_start, null);
});

for (const provider of ["tavily", "you", "parallel", "firecrawl"]) {
  test(`${provider} authenticates, bounds results, preserves source snippets and enforces domains`, async () => {
    let calls = 0;
    global.fetch = async (url, options) => {
      calls++;
      const expected = {tavily: "api.tavily.com", you: "ydc-index.io", parallel: "api.parallel.ai", firecrawl: "api.firecrawl.dev"};
      assert.equal(new URL(url).hostname, expected[provider]);
      assert.equal(options.method, "POST");
      assert.equal(options.redirect, "manual");
      const body = JSON.parse(options.body);
      const entries = [
        {title: "Wrong", url: "https://example.org.evil.test/doc"},
        {title: "Private", url: "https://private.example.org/doc"},
        {title: "Good", url: "https://example.org/doc?utm_source=test", content: "Source snippet",
          snippets: ["First", "Second"], excerpts: ["First", "Second"], description: "Source snippet"}
      ];
      let response;
      if (provider === "tavily") {
        assert.equal(options.headers.Authorization, "Bearer fake-secret");
        assert.deepEqual(body, {query: "API docs", search_depth: "basic", max_results: 2,
          include_answer: false, include_raw_content: false, include_domains: ["example.org"], exclude_domains: ["private.example.org"]});
        response = {results: entries, answer: "Ignore generated answer"};
      } else if (provider === "you") {
        assert.equal(options.headers["X-API-Key"], "fake-secret");
        assert.deepEqual(body, {query: "API docs site:example.org -site:private.example.org", count: 2});
        response = {results: {web: entries}};
      } else if (provider === "parallel") {
        assert.equal(options.headers["x-api-key"], "fake-secret");
        assert.deepEqual(body, {objective: "API docs site:example.org -site:private.example.org", search_queries: ["API docs"],
          mode: "basic", max_chars_total: 3600, advanced_settings: {max_results: 2,
            source_policy: {include_domains: ["example.org"], exclude_domains: ["private.example.org"]}}});
        response = {results: entries};
      } else {
        assert.equal(options.headers.Authorization, "Bearer fake-secret");
        assert.equal(new URL(url).pathname, "/v2/search");
        assert.deepEqual(body, {query: "API docs site:example.org -site:private.example.org", limit: 2});
        response = {success: true, data: {web: entries}};
      }
      return mock(JSON.stringify(response), "application/json");
    };
    const result = JSON.parse(await search("API docs site:example.org -site:private.example.org", {
      provider, [provider + "_api_key"]: "fake-secret"
    }, 2));
    assert.equal(calls, 1);
    assert.equal(result.provider, provider);
    assert.equal(result.results.length, 1);
    assert.equal(result.results[0].url, "https://example.org/doc");
    assert.equal(result.results[0].snippet, ["you", "parallel"].includes(provider) ? "First Second" : "Source snippet");
    assert.doesNotMatch(JSON.stringify(result), /fake-secret|generated answer/);
  });

  test(`${provider} errors do not retry, echo bodies or fall back`, async () => {
    const config = {provider, [provider + "_api_key"]: "fake-secret"};
    await assert.rejects(search("test", {provider}), {code: "CONFIG_ERROR"});
    for (const body of ["null", "not-json", "{}", '{"results":{}}', '{"success":false,"error":"fake-secret"}']) {
      global.fetch = async () => mock(body, "application/json");
      await assert.rejects(search("test", config), {code: "INVALID_RESPONSE"});
    }
    const empty = provider === "you" ? {results: {web: []}} : provider === "firecrawl" ? {success: true, data: {web: []}} : {results: []};
    global.fetch = async () => mock(JSON.stringify(empty), "application/json");
    await assert.rejects(search("test", config), {code: "NO_RESULTS"});
    let calls = 0;
    for (const status of [401, 403, 429, 500]) {
      global.fetch = async () => { calls++; return new Response("fake-secret", {status}); };
      await assert.rejects(search("test", config), error => {
        assert.equal(error.code, "HTTP_ERROR");
        assert.doesNotMatch(error.message, /fake-secret/);
        return true;
      });
    }
    assert.equal(calls, 4);
  });
}

test("all new credentials participate in auto and per-call provider overrides config", async () => {
  for (const [provider, env] of Object.entries({tavily: "TAVILY_API_KEY", you: "YDC_API_KEY", parallel: "PARALLEL_API_KEY", firecrawl: "FIRECRAWL_API_KEY"})) {
    process.env[env] = "fake-env-key";
    global.fetch = async () => new Response(null, {status: 401});
    await assert.rejects(search("test", {search_priority: [provider]}), {code: "HTTP_ERROR"});
    await assert.rejects(search("test", {exa_api_key: "other-key", search_priority: [provider, "exa"]}), {code: "HTTP_ERROR"});
    delete process.env[env];
    global.fetch = async () => mock(JSON.stringify({results: [{title: "Found", url: "https://example.org"}]}), "application/json");
    const result = JSON.parse(await dispatch({type: "tool", name: "web_search", config: {provider, exa_api_key: "key"},
      arguments: {query: "test", provider: "exa"}}));
    assert.equal(result.provider, "exa");
  }
});

test("Firecrawl extraction validates source status and reuses its own continuation snapshot", async () => {
  let calls = 0;
  const content = "Markdown paragraph.\n\n".repeat(100);
  global.fetch = async (url, options) => {
    calls++;
    assert.equal(url, "https://api.firecrawl.dev/v2/scrape");
    assert.equal(options.headers.Authorization, "Bearer fake-key");
    assert.deepEqual(JSON.parse(options.body), {url: "https://example.org/firecrawl-doc", formats: ["markdown"], onlyMainContent: true, maxAge: 0});
    return mock(JSON.stringify({success: true, data: {markdown: content, metadata: {title: "Doc", statusCode: 200}}}), "application/json");
  };
  const config = {fetch_provider: "firecrawl", firecrawl_api_key: "fake-key", firecrawl_fresh_scrape: true};
  const first = JSON.parse(await fetchPage("https://example.org/firecrawl-doc", config, 1000));
  const second = JSON.parse(await fetchPage("https://example.org/firecrawl-doc", config, 30000, first.next_start));
  assert.equal(first.provider, "firecrawl");
  assert.equal(first.text + second.text, content);
  assert.equal(calls, 1);
  await assert.rejects(fetchPage("https://example.org/firecrawl-doc", {}, 1000, 1000), {code: "CACHE_MISS"});
  for (const data of [{markdown: "failure", metadata: {statusCode: 403}}, {markdown: "failure", metadata: {error: "fake-key"}}]) {
    global.fetch = async () => mock(JSON.stringify({success: true, data}), "application/json");
    await assert.rejects(fetchPage("https://example.org/firecrawl-doc", config), {code: "HTTP_ERROR"});
  }
  global.fetch = async () => mock('{"success":true,"data":{"markdown":""}}', "application/json");
  await assert.rejects(fetchPage("https://example.org/firecrawl-doc", config), {code: "UNREADABLE_PAGE"});
  await assert.rejects(search("test", {provider: "firecrawl", firecrawl_api_key: "key", firecrawl_base_url: "http://remote.test"}), {code: "CONFIG_ERROR"});
  await assert.rejects(search("test", {provider: "firecrawl", firecrawl_api_key: "key", firecrawl_api_version: "v3"}), {code: "CONFIG_ERROR"});
});

const priorityKeys = {parallel_api_key: "fake-p", you_api_key: "fake-y", exa_api_key: "fake-e", tavily_api_key: "fake-t"};
const found = {title: "Docs", url: "https://nodejs.org/api/globals.html", snippets: ["Source"], highlights: ["Source"], content: "Source", excerpts: ["Source"]};
function priorityResponse(host) {
  return mock(JSON.stringify(host === "ydc-index.io" ? {results: {web: [found]}} : {results: [found]}), "application/json");
}

test("auto follows Pi priority, stops at first success and reports only actual failed attempts", async () => {
  const calls = [];
  global.fetch = async url => {
    const host = new URL(url).hostname;
    calls.push(host);
    if (host === "api.parallel.ai") return new Response("not logged", {status: 503});
    return priorityResponse(host);
  };
  const result = JSON.parse(await search("timeout site:nodejs.org", priorityKeys));
  assert.deepEqual(calls, ["api.parallel.ai", "ydc-index.io"]);
  assert.equal(result.provider, "you");
  assert.equal(result.attempts.length, 1);
  assert.equal(result.attempts[0].provider, "parallel");
  assert.equal(result.attempts[0].code, "HTTP_ERROR");
  assert.equal(result.attempts[0].status, 503);
  assert.doesNotMatch(JSON.stringify(result), /fake-|not logged/);
  calls.length = 0;
  global.fetch = async url => { calls.push(new URL(url).hostname); return priorityResponse(calls[0]); };
  const first = JSON.parse(await search("timeout", priorityKeys));
  assert.equal(first.provider, "parallel");
  assert.deepEqual(first.attempts, []);
  assert.deepEqual(calls, ["api.parallel.ai"]);
});

test("auto skips missing keys, respects custom array/comma priority and never adds unlisted paid providers", async () => {
  const calls = [];
  global.fetch = async url => { const host = new URL(url).hostname; calls.push(host); return priorityResponse(host); };
  for (const search_priority of [["tavily", "exa", "you"], "tavily, exa, you"]) {
    const result = JSON.parse(await search("timeout", {...priorityKeys, search_priority}));
    assert.equal(result.provider, "tavily");
  }
  const result = JSON.parse(await search("timeout", {exa_api_key: "key", firecrawl_api_key: "unlisted-key"}));
  assert.equal(result.provider, "exa");
  assert.deepEqual(calls, ["api.tavily.com", "api.tavily.com", "api.exa.ai"]);
});

test("auto switches on network errors and 429/5xx only, retaining the full ordered failure chain", async () => {
  const calls = [];
  global.fetch = async url => {
    const host = new URL(url).hostname;
    calls.push(host);
    if (host === "api.parallel.ai") throw Error("fake-secret");
    if (host === "ydc-index.io") return new Response("fake-secret", {status: 429});
    if (host === "api.exa.ai") return new Response("fake-secret", {status: 500});
    return priorityResponse(host);
  };
  const result = JSON.parse(await search("timeout", priorityKeys));
  assert.equal(result.provider, "tavily");
  assert.deepEqual(result.attempts.map(x => x.code), ["NETWORK_ERROR", "HTTP_ERROR", "HTTP_ERROR"]);
  assert.deepEqual(calls, ["api.parallel.ai", "ydc-index.io", "api.exa.ai", "api.tavily.com"]);
  assert.doesNotMatch(JSON.stringify(result), /fake-secret/);
});

test("auto stops on auth, query/config errors, no results, invalid data and redirects", async () => {
  for (const status of [400, 401, 402, 403, 404, 408, 422]) {
    let calls = 0;
    global.fetch = async () => { calls++; return new Response("fake-secret", {status}); };
    await assert.rejects(search("timeout", priorityKeys), error => {
      assert.equal(error.code, "HTTP_ERROR");
      assert.equal(error.attempts.length, 1);
      assert.equal(error.attempts[0].status, status);
      return true;
    });
    assert.equal(calls, 1);
  }
  for (const [response, code] of [
    [() => mock('{"results":[]}', "application/json"), "NO_RESULTS"],
    [() => mock('{"results":[{"url":"https://wrong.org"}]}', "application/json"), "NO_RESULTS"],
    [() => mock("not JSON", "application/json"), "INVALID_RESPONSE"],
    [() => new Response(null, {status: 307, headers: {location: "https://other.org"}}), "REDIRECT_ERROR"]
  ]) {
    let calls = 0;
    global.fetch = async () => { calls++; return response(); };
    await assert.rejects(search("timeout site:nodejs.org", priorityKeys), {code});
    assert.equal(calls, 1);
  }
  let calls = 0;
  global.fetch = async () => { calls++; throw Error(); };
  await assert.rejects(search("timeout site:!@#$", priorityKeys), {code: "INVALID_QUERY"});
  assert.equal(calls, 0);
  await assert.rejects(search("timeout", {...priorityKeys, parallel_api_key: 123}), {code: "CONFIG_ERROR"});
  assert.equal(calls, 0);
});

test("explicit provider or per-call override never falls back even when priority and keys are present", async () => {
  const calls = [];
  global.fetch = async url => { calls.push(new URL(url).hostname); return new Response(null, {status: 503}); };
  await assert.rejects(dispatch({type: "tool", name: "web_search", config: priorityKeys,
    arguments: {query: "timeout", provider: "exa"}}), error => {
    assert.equal(error.code, "HTTP_ERROR");
    assert.equal(error.attempts, undefined);
    return true;
  });
  assert.deepEqual(calls, ["api.exa.ai"]);
});

test("auto timeout advances to next backend without starting requests concurrently", async () => {
  let calls = 0, aborted = false;
  const keepAlive = setTimeout(() => {}, 3000);
  try {
    global.fetch = async (url, {signal}) => {
      calls++;
      if (new URL(url).hostname === "api.parallel.ai") {
        return new Promise((_resolve, reject) => signal.addEventListener("abort", () => {
          aborted = true; reject(signal.reason);
        }, {once: true}));
      }
      assert.equal(aborted, true);
      return priorityResponse(new URL(url).hostname);
    };
    const result = JSON.parse(await search("timeout", {...priorityKeys, search_attempt_timeout_ms: 500, timeout_ms: 2000}));
    assert.equal(result.provider, "you");
    assert.equal(result.attempts[0].code, "TIMEOUT");
    assert.equal(calls, 2);
  } finally { clearTimeout(keepAlive); }
});

test("auto shares one total deadline and does not call providers after budget exhaustion", async () => {
  const now = Date.now;
  let clock = now(), calls = 0;
  Date.now = () => clock;
  try {
    global.fetch = async () => { calls++; clock += 2000; throw Error("unreachable"); };
    await assert.rejects(search("timeout", {...priorityKeys, timeout_ms: 2000}), error => {
      assert.equal(error.code, "TIMEOUT");
      assert.deepEqual(error.attempts.map(x => x.provider), ["parallel"]);
      return true;
    });
    assert.equal(calls, 1);
  } finally { Date.now = now; }
});

test("auto retains all diagnostics when every configured provider fails", async () => {
  let calls = 0;
  global.fetch = async () => { calls++; return new Response(null, {status: 503}); };
  await assert.rejects(search("timeout", priorityKeys), error => {
    assert.equal(error.code, "HTTP_ERROR");
    assert.deepEqual(error.attempts.map(x => x.provider), ["parallel", "you", "exa", "tavily"]);
    return true;
  });
  assert.equal(calls, 4);
});

test("default auto attempt cap is eight seconds and the next request receives only remaining total budget", async () => {
  const now = Date.now, timeout = AbortSignal.timeout;
  let clock = now(), calls = 0;
  const budgets = [];
  Date.now = () => clock;
  AbortSignal.timeout = ms => { budgets.push(ms); return timeout(ms); };
  try {
    global.fetch = async url => {
      calls++;
      if (calls === 1) { clock += 8000; throw Error("unreachable"); }
      return priorityResponse(new URL(url).hostname);
    };
    const result = JSON.parse(await search("timeout", priorityKeys));
    assert.equal(result.provider, "you");
    assert.deepEqual(budgets, [8000, 7000]);
    assert.equal(calls, 2);
  } finally { Date.now = now; AbortSignal.timeout = timeout; }
});

const freeHtml = '<a class="result__a" href="https://nodejs.org/api/globals.html?utm_source=ddg">Node globals</a><a class="result__snippet">AbortSignal <b>timeout</b> source</a>';
const freeLite = '<tr><td><a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fnodejs.org%2Fapi%2Fglobals.html" class="result-link">Node &amp; globals</a></td></tr><tr><td class="result-snippet">Original <b>timeout</b> text.</td></tr>';
const freeRss = '<rss><channel><item><title>Node globals</title><link>https://nodejs.org/api/globals.html</link><description>AbortSignal timeout source</description></item></channel></rss>';
const freeMwmbl = JSON.stringify([{url: "https://nodejs.org/api/globals.html",
  title: [{value: "Node ", is_bold: false}, {value: "globals", is_bold: true}],
  extract: [{value: "AbortSignal ", is_bold: false}, {value: "timeout", is_bold: true}, {value: " source", is_bold: false}]}]);
const emptyMwmbl = JSON.stringify([]);

test("Mwmbl joins split title/extract spans and rejects a non-array document", async () => {
  global.fetch = async url => {
    const target = new URL(url);
    assert.equal(target.hostname, "api.mwmbl.org");
    assert.equal(target.searchParams.get("s"), "timeout");
    return mock(freeMwmbl, "application/json");
  };
  const ok = JSON.parse(await search("timeout", {provider: "mwmbl"}));
  assert.deepEqual(ok.results, [{title: "Node globals", url: "https://nodejs.org/api/globals.html", snippet: "AbortSignal timeout source"}]);
  global.fetch = async () => mock('{"results":[]}', "application/json");
  await assert.rejects(search("timeout", {provider: "mwmbl"}), {code: "INVALID_RESPONSE"});
});

test("no-key auto prefers the real free API and warns about its limited corpus", async () => {
  process.env.EXA_API_KEY = "";
  const calls = [];
  global.fetch = async url => {
    calls.push(new URL(url).hostname);
    assert.doesNotMatch(JSON.stringify(url), /api.key|authorization|subscription.token/i);
    return mock(freeMwmbl, "application/json");
  };
  const result = JSON.parse(await search("Node globals", {provider: "auto"}));
  assert.deepEqual(calls, ["api.mwmbl.org"]);
  assert.equal(result.mode, "free");
  assert.equal(result.provider, "mwmbl");
  assert.match(result.warning, /small community corpus/);
  assert.deepEqual(result.attempts, []);
});

test("free ignores configured and environment API keys and never calls paid hosts", async () => {
  process.env.EXA_API_KEY = "fake-env-paid-key";
  const calls = [];
  global.fetch = async (url, options) => {
    calls.push(new URL(url).hostname);
    assert.doesNotMatch(JSON.stringify(options), /fake-env-paid-key|fake-[pyet]/);
    const first = calls.length === 1;
    return first ? mock(emptyMwmbl, "application/json") : mock(freeHtml);
  };
  try {
    const result = JSON.parse(await dispatch({type: "tool", name: "web_search",
      config: {...priorityKeys, provider: "parallel"}, arguments: {query: "test", provider: "free"}}));
    assert.equal(result.provider, "duckduckgo");
    assert.equal(result.mode, "free");
    assert.deepEqual(calls, ["api.mwmbl.org", "html.duckduckgo.com"]);
  } finally { process.env.EXA_API_KEY = ""; }
});

test("DDG Lite parser preserves source snippets, entities and redirects, and cannot take the next snippet", () => {
  const results = parseDDG(freeLite + '<a class="result-link" href="https://example.org/second">Second</a>', 10, true);
  assert.deepEqual(results, [
    {title: "Node & globals", url: "https://nodejs.org/api/globals.html", snippet: "Original timeout text."},
    {title: "Second", url: "https://example.org/second", snippet: ""}
  ]);
});

test("free walks Mwmbl -> DuckDuckGo -> Lite -> Bing on empty/network/markup failures", async () => {
  const calls = [];
  global.fetch = async url => {
    const host = new URL(url).hostname; calls.push(host);
    if (host === "api.mwmbl.org") return mock(emptyMwmbl, "application/json");
    if (host === "html.duckduckgo.com") throw Error("offline");
    if (host === "lite.duckduckgo.com") return mock('<p class="no-results">No results found</p>');
    return mock(freeRss, "application/rss+xml");
  };
  const result = JSON.parse(await search("timeout site:nodejs.org", {provider: "free"}));
  assert.equal(result.provider, "bing");
  assert.match(result.warning, /generic popular pages/);
  // An unreachable DDG edge skips the sibling Lite host to protect the budget.
  assert.deepEqual(calls, ["api.mwmbl.org", "html.duckduckgo.com", "www.bing.com"]);
  assert.deepEqual(result.attempts.map(x => x.code), ["NO_RESULTS", "NETWORK_ERROR"]);
  calls.length = 0;
  global.fetch = async url => {
    const host = new URL(url).hostname; calls.push(host);
    if (host === "api.mwmbl.org") return mock(emptyMwmbl, "application/json");
    if (host === "html.duckduckgo.com") return new Response(null, {status: 500});
    return host === "lite.duckduckgo.com" ? mock(freeLite) : mock(freeRss, "application/rss+xml");
  };
  const viaLite = JSON.parse(await search("timeout", {provider: "free"}));
  // A redesign or server error can still be worked around by the sibling host.
  assert.equal(viaLite.provider, "duckduckgo_lite");
  assert.deepEqual(calls, ["api.mwmbl.org", "html.duckduckgo.com", "lite.duckduckgo.com"]);
  global.fetch = async url => {
    const host = new URL(url).hostname, mwmbl = host === "api.mwmbl.org";
    return mock(mwmbl ? "changed markup" : freeLite, mwmbl ? "application/json" : "text/html");
  };
  const lite = JSON.parse(await search("timeout", {provider: "free"}));
  assert.equal(lite.provider, "duckduckgo_lite");
  assert.equal(lite.attempts[0].code, "INVALID_RESPONSE");
  assert.equal(lite.results[0].snippet, "Original timeout text.");
});

test("CAPTCHA/403/429 or an unreachable DDG edge skips the same-vendor Lite host", async () => {
  const blocked = [() => mock('<form id="challenge-form" action="/anomaly.js"><a class="result__a" href="https://evil.test">Not evidence</a></form>'),
    () => new Response(null, {status: 403}), () => new Response(null, {status: 429}), () => { throw Error("offline"); }];
  for (const fail of blocked) {
    const calls = [];
    global.fetch = async url => {
      const host = new URL(url).hostname; calls.push(host);
      if (host === "api.mwmbl.org") return mock(emptyMwmbl, "application/json");
      return host === "html.duckduckgo.com" ? fail() : mock(freeRss, "application/rss+xml");
    };
    const result = JSON.parse(await search("timeout", {provider: "free"}));
    assert.equal(result.provider, "bing");
    assert.deepEqual(calls, ["api.mwmbl.org", "html.duckduckgo.com", "www.bing.com"]);
    assert.equal(result.attempts.length, 2);
  }
});

test("free prefers a configured SearXNG and degrades to Mwmbl, never to an unconfigured public instance", async () => {
  const calls = [];
  global.fetch = async url => {
    const host = new URL(url).hostname; calls.push(host);
    return host === "search.example.org" ? mock('{"results":[]}', "application/json") : mock(freeMwmbl, "application/json");
  };
  const result = JSON.parse(await search("timeout", {provider: "free", searxng_url: "https://search.example.org", ...priorityKeys}));
  assert.equal(result.provider, "mwmbl");
  assert.deepEqual(calls, ["search.example.org", "api.mwmbl.org"]);
  assert.equal(result.attempts[0].provider, "searxng");
});

test("free enforces hard domain filters on every fallback and reports exhaustion without inventing results", async () => {
  let calls = 0;
  global.fetch = async url => {
    calls++;
    const host = new URL(url).hostname;
    if (host === "api.mwmbl.org") return mock(freeMwmbl, "application/json");
    if (host === "html.duckduckgo.com") return mock(freeHtml);
    if (host === "lite.duckduckgo.com") return mock(freeLite);
    return mock(freeRss, "application/rss+xml");
  };
  await assert.rejects(search("timeout -site:nodejs.org", {provider: "free", ...priorityKeys}), error => {
    assert.equal(error.code, "NO_RESULTS");
    assert.deepEqual(error.attempts.map(x => x.provider), ["mwmbl", "duckduckgo", "duckduckgo_lite", "bing"]);
    return true;
  });
  assert.equal(calls, 4);
});

test("free obeys the total deadline and explicit engines never gain automatic failover", async () => {
  const now = Date.now;
  let clock = now(), calls = 0;
  Date.now = () => clock;
  try {
    global.fetch = async () => { calls++; clock += 2000; throw Error("offline"); };
    await assert.rejects(search("timeout", {provider: "free", timeout_ms: 2000}), error => {
      assert.equal(error.code, "TIMEOUT");
      assert.equal(error.attempts.length, 1);
      return true;
    });
    assert.equal(calls, 1);
  } finally { Date.now = now; }
  calls = 0;
  global.fetch = async () => { calls++; return mock(emptyMwmbl, "application/json"); };
  await assert.rejects(search("test", {provider: "mwmbl"}), error => {
    assert.equal(error.code, "NO_RESULTS");
    assert.equal(error.attempts, undefined);
    return true;
  });
  assert.equal(calls, 1);
});

test("paid auto retains its priority order and does not silently spill into keyless search", async () => {
  const calls = [];
  global.fetch = async url => { calls.push(new URL(url).hostname); return new Response(null, {status: 503}); };
  await assert.rejects(search("timeout", priorityKeys), {code: "HTTP_ERROR"});
  assert.deepEqual(calls, ["api.parallel.ai", "ydc-index.io", "api.exa.ai", "api.tavily.com"]);
});
