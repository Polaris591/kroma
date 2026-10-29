"use strict";
const {test,afterEach} = require("node:test");
const assert = require("node:assert/strict");
const {dispatch,parseDDG,parseBingRSS,plainText,fetchPage,search} = require("./web");
const originalFetch=global.fetch;
afterEach(()=>{global.fetch=originalFetch;});
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
  const result=JSON.parse(await search("topic"));
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
