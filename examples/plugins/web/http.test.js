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

test("the request deadline also bounds the DNS safety lookup", async () => {
  const dns = require("node:dns");
  const realLookup = dns.promises.lookup;
  const allow = process.env.KROMA_WEB_ALLOW_PRIVATE;
  delete process.env.KROMA_WEB_ALLOW_PRIVATE;
  let lookups = 0;
  dns.promises.lookup = () => {
    lookups++;
    return new Promise(resolve => setTimeout(() => resolve([{address: "203.0.113.10", family: 4}]), 400));
  };
  try {
    const started = Date.now();
    await assert.rejects(download("https://slow.example.org/page", {timeout: 40}), error => {
      assert.equal(error.code, "TIMEOUT");
      assert.match(error.message, /DNS safety check/);
      return true;
    });
    assert.ok(Date.now() - started < 300, "the lookup must not outlive the request deadline");
    assert.equal(lookups, 1);
  } finally {
    dns.promises.lookup = realLookup;
    if (allow === undefined) delete process.env.KROMA_WEB_ALLOW_PRIVATE;
    else process.env.KROMA_WEB_ALLOW_PRIVATE = allow;
  }
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

test("real-socket DNS rebinding cannot redirect the connection to a private address", {timeout: 5000}, async t => {
  const {createServer} = require("node:http");
  const dns = require("node:dns");
  let hits = 0;
  const server = createServer((req, res) => { hits++; res.end("LOCAL_ONLY_MARKER"); });
  t.after(() => { server.closeAllConnections(); server.close(); });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const port = server.address().port;

  // First answer (validation) is public; every later answer is the local server.
  // A transport that resolves again after validation is therefore rebound.
  const hostname = "rebind.example.test";
  let lookups = 0;
  const answer = () => {
    lookups++;
    return lookups === 1
      ? [{address: "203.0.113.10", family: 4}]
      : [{address: "127.0.0.1", family: 4}];
  };
  const realLookup = dns.lookup;
  const realPromiseLookup = dns.promises.lookup;
  dns.lookup = function(host, options, callback) {
    if (typeof options === "function") { callback = options; options = {}; }
    if (host !== hostname) return realLookup.call(dns, host, options, callback);
    const records = answer();
    process.nextTick(() => options?.all ? callback(null, records)
      : callback(null, records[0].address, records[0].family));
  };
  dns.promises.lookup = async (host, options) => {
    if (host !== hostname) return realPromiseLookup.call(dns.promises, host, options);
    return answer();
  };
  t.after(() => { dns.lookup = realLookup; dns.promises.lookup = realPromiseLookup; });

  await download(`http://${hostname}:${port}/secret`, {
    allowPrivateNetwork: false, timeout: 500
  }).catch(error => {
    assert.ok(["TIMEOUT", "NETWORK_ERROR"].includes(error.code), `unexpected error ${error.code}`);
  });
  assert.equal(hits, 0, "the local server must never be reached after rebinding");
  assert.equal(lookups, 1, "the connection must reuse the validated address, not resolve again");
});

test("real network path follows redirects, cancels bodies and strips credentials across origins", {timeout: 5000}, async t => {
  const {createServer} = require("node:http");
  let credential;
  const destination = createServer((req, res) => {
    credential = req.headers.authorization;
    res.writeHead(200, {"content-type": "text/plain"});
    res.end("FINAL_BODY");
  });
  const source = createServer((req, res) => {
    if (req.url === "/start") {
      res.writeHead(302, {location: "/next"});
      res.end("ignored");
      return;
    }
    res.writeHead(302, {location: `http://127.0.0.1:${destination.address().port}/final`});
    res.end("ignored");
  });
  t.after(() => {
    source.closeAllConnections(); source.close();
    destination.closeAllConnections(); destination.close();
  });
  await new Promise(resolve => destination.listen(0, "127.0.0.1", resolve));
  await new Promise(resolve => source.listen(0, "127.0.0.1", resolve));

  const response = await download(`http://127.0.0.1:${source.address().port}/start`, {
    allowPrivateNetwork: true, headers: {Authorization: "Bearer secret"}
  });
  assert.equal(response.text, "FINAL_BODY");
  assert.equal(response.url, `http://127.0.0.1:${destination.address().port}/final`);
  assert.equal(credential, undefined, "credentials must not cross the redirect origin");
});

test("proxy CONNECT uses the validated address and keeps the request Host header", {timeout: 5000}, async t => {
  const {createServer} = require("node:http");
  const dns = require("node:dns");
  const hostname = "proxy-target.example.test";
  let authority, hostHeader, lookups = 0;
  const proxy = createServer();
  proxy.on("connect", (req, socket) => {
    authority = req.url;
    socket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
    let buffered = "";
    socket.on("data", chunk => {
      buffered += chunk.toString("latin1");
      if (!buffered.includes("\r\n\r\n")) return;
      hostHeader = /host: ([^\r\n]+)/i.exec(buffered)?.[1];
      socket.end("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 8\r\nConnection: close\r\n\r\nPROXIED!");
    });
  });
  t.after(() => proxy.close());

  const realPromiseLookup = dns.promises.lookup;
  const realLookup = dns.lookup;
  dns.promises.lookup = async (host, options) => host === hostname
    ? [{address: "203.0.113.10", family: 4}] : realPromiseLookup.call(dns.promises, host, options);
  dns.lookup = function(host, options, callback) {
    if (typeof options === "function") { callback = options; options = {}; }
    if (host !== hostname) return realLookup.call(dns, host, options, callback);
    lookups++;
    process.nextTick(() => callback(null, "127.0.0.1", 4));
  };
  const env = {...process.env};
  t.after(() => {
    dns.promises.lookup = realPromiseLookup;
    dns.lookup = realLookup;
    for (const key of ["KROMA_WEB_PROXY_READY", "HTTP_PROXY", "NO_PROXY"]) {
      if (env[key] === undefined) delete process.env[key]; else process.env[key] = env[key];
    }
  });

  await new Promise(resolve => proxy.listen(0, "127.0.0.1", resolve));
  process.env.KROMA_WEB_PROXY_READY = "1";
  process.env.HTTP_PROXY = `http://127.0.0.1:${proxy.address().port}`;
  delete process.env.NO_PROXY;

  const response = await download(`http://${hostname}/page`, {allowPrivateNetwork: false, timeout: 3000});
  assert.equal(response.text, "PROXIED!");
  assert.equal(authority, "203.0.113.10:80", "CONNECT must target the validated address");
  assert.equal(hostHeader, hostname, "the proxy must still see the original hostname");
  assert.equal(lookups, 0, "the tunnel must not re-resolve the target hostname");
});
