"use strict";
process.env.KROMA_WEB_ALLOW_PRIVATE = "1";
const {test, afterEach} = require("node:test");
const assert = require("node:assert/strict");
const {download, isPrivateAddress} = require("./http");
const {fetchPage} = require("./web");
const originalFetch = global.fetch;
afterEach(() => { global.fetch = originalFetch; });

test("private-network detection covers loopback, RFC1918 and mapped IPv6", () => {
  assert.equal(isPrivateAddress("127.0.0.1"), true);
  assert.equal(isPrivateAddress("10.1.2.3"), true);
  assert.equal(isPrivateAddress("172.20.0.1"), true);
  assert.equal(isPrivateAddress("192.168.1.1"), true);
  assert.equal(isPrivateAddress("169.254.169.254"), true);
  assert.equal(isPrivateAddress("::1"), true);
  assert.equal(isPrivateAddress("fd00::1"), true);
  assert.equal(isPrivateAddress("::ffff:127.0.0.1"), true);
  assert.equal(isPrivateAddress("::ffff:7f00:1"), true);
  assert.equal(isPrivateAddress("::ffff:c0a8:101"), true);
  assert.equal(isPrivateAddress("8.8.8.8"), false);
});

test("private URLs are blocked unless the caller explicitly opts in", async () => {
  await assert.rejects(download("http://127.0.0.1:9/private", {allowPrivateNetwork: false}), {code: "BLOCKED_ADDRESS"});
});

test("download keeps the prefix of an oversized first chunk and cancels the stream", async () => {
  let cancelled = false;
  global.fetch = async () => new Response(new ReadableStream({
    start(controller) { controller.enqueue(new TextEncoder().encode("abcdefghijk")); },
    cancel() { cancelled = true; }
  }), {headers: {"content-type": "text/plain"}});
  const response = await download("https://example.org/large", {bytes: 5});
  assert.equal(response.text, "abcde");
  assert.equal(response.truncated, true);
  assert.equal(cancelled, true);
});

test("an exact-limit response is complete and a chunked response keeps the full prefix", async () => {
  global.fetch = async () => new Response("abcde");
  assert.equal((await download("https://example.org/exact", {bytes: 5})).truncated, false);
  global.fetch = async () => new Response(new ReadableStream({
    start(controller) {
      controller.enqueue(new TextEncoder().encode("abc"));
      controller.enqueue(new TextEncoder().encode("def"));
      controller.close();
    }
  }));
  const response = await download("https://example.org/chunked", {bytes: 5});
  assert.equal(response.text, "abcde");
  assert.equal(response.truncated, true);
});

test("decodes Chinese GBK pages declared by header or meta charset", async () => {
  // GBK bytes for 中文, independent of third-party encoders.
  const chinese = Buffer.from([0xd6, 0xd0, 0xce, 0xc4]);
  global.fetch = async () => new Response(chinese, {headers: {"content-type": "text/plain; charset=gb2312"}});
  assert.equal((await download("https://example.org/gbk")).text, "中文");
  global.fetch = async () => new Response(Buffer.concat([
    Buffer.from('<meta charset="gbk"><main>'), chinese, Buffer.from("</main>")
  ]), {headers: {"content-type": "text/html"}});
  assert.equal(JSON.parse(await fetchPage("https://example.org/meta-gbk")).text, "中文");
});

test("follows relative redirects and strips API credentials across origins", async () => {
  const calls = [];
  global.fetch = async (url, options) => {
    calls.push({url, headers: {...options.headers}});
    if (calls.length === 1) return new Response(null, {status: 302, headers: {location: "/moved"}});
    if (calls.length === 2) return new Response(null, {status: 307, headers: {location: "https://other.example.org/page"}});
    return new Response("final");
  };
  const response = await download("https://example.org/start", {headers: {"X-Subscription-Token": "secret"}});
  assert.equal(response.url, "https://other.example.org/page");
  assert.equal(calls[1].url, "https://example.org/moved");
  assert.equal(calls[1].headers["X-Subscription-Token"], "secret");
  assert.equal(calls[2].headers["X-Subscription-Token"], undefined);
});

test("redirect loops and unsafe redirect schemes are rejected", async () => {
  let calls = 0;
  global.fetch = async () => { calls++; return new Response(null, {status: 302, headers: {location: "/loop"}}); };
  await assert.rejects(download("https://example.org/loop"), {code: "REDIRECT_ERROR"});
  assert.equal(calls, 6);
  global.fetch = async () => new Response(null, {status: 302, headers: {location: "file:///secret"}});
  await assert.rejects(download("https://example.org/unsafe"), {code: "INVALID_URL"});
  await assert.rejects(download("https://user:secret@example.org"), {code: "INVALID_URL"});
});

test("timeouts include a useful error code without leaking URL secrets", async () => {
  global.fetch = async (_url, {signal}) => new Promise((resolve, reject) => {
    const timer = setTimeout(resolve, 1000);
    signal.addEventListener("abort", () => { clearTimeout(timer); reject(signal.reason); }, {once: true});
  });
  await assert.rejects(download("https://example.org/?token=secret", {timeout: 20}), error => {
    assert.equal(error.code, "TIMEOUT");
    assert.match(error.message, /example.org.*20 ms/);
    assert.doesNotMatch(error.message, /secret/);
    return true;
  });
});

test("HTTP and unsupported-content errors cancel the unused response body", async () => {
  let cancellations = 0;
  const body = () => new ReadableStream({cancel() { cancellations++; }});
  global.fetch = async () => new Response(body(), {status: 403});
  await assert.rejects(download("https://example.org/private?token=secret"), error => {
    assert.equal(error.code, "HTTP_ERROR");
    assert.doesNotMatch(error.message, /secret/);
    return true;
  });
  global.fetch = async () => new Response(body(), {headers: {"content-type": "application/pdf"}});
  await assert.rejects(fetchPage("https://example.org/pdf"), {code: "UNSUPPORTED_CONTENT"});
  assert.equal(cancellations, 2);
});

test("plain text is preserved, and headerless HTML is extracted", async () => {
  global.fetch = async () => new Response("a < b && c > d", {headers: {"content-type": "text/plain"}});
  assert.equal(JSON.parse(await fetchPage("https://example.org/plain")).text, "a < b && c > d");
  global.fetch = async () => {
    const response = new Response("<html><main>Readable</main></html>");
    response.headers.delete("content-type");
    return response;
  };
  assert.equal(JSON.parse(await fetchPage("https://example.org/no-type")).text, "Readable");
});

test("empty pages and bot challenges fail instead of reporting success", async () => {
  for (const html of ["<html><body><script>start()</script></body></html>",
    "<title>Just a moment...</title><main>Checking your browser</main>",
    "<main>Please enable JavaScript to view this page</main>"]) {
    global.fetch = async () => new Response(html, {headers: {"content-type": "text/html"}});
    await assert.rejects(fetchPage("https://example.org/challenge"), {code: "UNREADABLE_PAGE"});
  }
});

test("declared plain text containing HTML-looking examples is not interpreted as HTML", async () => {
  const source = '<main><p>This is a source-code example.</p></main>';
  global.fetch = async () => new Response(source, {headers: {"content-type": "text/plain"}});
  assert.equal(JSON.parse(await fetchPage("https://example.org/raw-html.txt")).text, source);
});

test("source download limit is distinct from continuation and never drops the whole first chunk", async () => {
  global.fetch = async () => new Response("x".repeat(1500001), {headers: {"content-type": "text/plain"}});
  const first = JSON.parse(await fetchPage("https://example.org/oversized", {}, 1000));
  assert.equal(first.text.length, 1000);
  assert.equal(first.total_chars, 1500000);
  assert.match(first.warning, /1.5 MB/);
  const last = JSON.parse(await fetchPage("https://example.org/oversized", {}, 1000, 1499000));
  assert.equal(last.next_start, null);
  assert.equal(last.truncated, true);
  assert.match(last.warning, /unavailable/);
});

test("search POST redirects cancel the body and never forward the query or API key", async () => {
  for (const status of [301, 302, 303, 307, 308]) {
    let calls = 0, cancellations = 0;
    global.fetch = async (_url, options) => {
      calls++;
      assert.equal(options.method, "POST");
      assert.equal(options.body, '{"query":"private query"}');
      return new Response(new ReadableStream({cancel() { cancellations++; }}), {
        status, headers: {location: "https://other.example.org/collect"}
      });
    };
    await assert.rejects(download("https://api.exa.ai/search", {
      method: "POST", body: '{"query":"private query"}', headers: {"x-api-key": "secret"}
    }), {code: "REDIRECT_ERROR"});
    assert.equal(calls, 1);
    assert.equal(cancellations, 1);
  }
});

test("the same deadline covers the response body of a search POST", async () => {
  global.fetch = async (_url, {signal}) => new Response(new ReadableStream({
    start(controller) {
      signal.addEventListener("abort", () => controller.error(signal.reason), {once: true});
    }
  }), {headers: {"content-type": "application/json"}});
  // AbortSignal timers are unref'ed; keep the synthetic connection alive until abort.
  const keepAlive = setTimeout(() => {}, 1000);
  try {
    await assert.rejects(download("https://api.exa.ai/search", {
      method: "POST", body: "{}", timeout: 20
    }), {code: "TIMEOUT"});
  } finally { clearTimeout(keepAlive); }
});

test("HTTP transport sends JSON POST bodies through a real local connection", {timeout: 5000}, async t => {
  const {createServer} = require("node:http");
  let received;
  const server = createServer(async (req, res) => {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    received = {method: req.method, key: req.headers["x-api-key"],
      type: req.headers["content-type"], body: JSON.parse(Buffer.concat(chunks).toString())};
    res.writeHead(200, {"content-type": "application/json"});
    res.end('{"results":[]}');
  });
  t.after(() => { server.closeAllConnections(); server.close(); });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const response = await download(`http://127.0.0.1:${server.address().port}/search`, {
    method: "POST", body: '{"query":"中文 query"}',
    headers: {"x-api-key": "local-test-key", "Content-Type": "application/json"}, types: ["application/json"]
  });
  assert.deepEqual(received, {method: "POST", key: "local-test-key", type: "application/json", body: {query: "中文 query"}});
  assert.deepEqual(JSON.parse(response.text), {results: []});
});
