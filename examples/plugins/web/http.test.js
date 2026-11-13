"use strict";
const {test, afterEach} = require("node:test");
const assert = require("node:assert/strict");
const {createServer} = require("node:http");
const zlib = require("node:zlib");
const {download, isPrivateAddress, isPublicDestination} = require("./http");
const {fetchPage} = require("./web");
const originalFetch = global.fetch;
afterEach(() => { global.fetch = originalFetch; });

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
  const long = Buffer.concat(new Array(40).fill(chinese));
  global.fetch = async () => new Response(Buffer.concat([
    Buffer.from('<meta charset="gbk"><main>'), long, Buffer.from("</main>")
  ]), {headers: {"content-type": "text/html"}});
  assert.match(JSON.parse(await fetchPage("https://example.org/meta-gbk")).text, /中文/);
});

test("a real gzip response is decoded by the transport", async t => {
  const body = "<html><body><main><p>" + "hello world ".repeat(50) + "</p></main></body></html>";
  const server = createServer((req, res) => {
    res.writeHead(200, {"content-type": "text/html", "content-encoding": "gzip"});
    res.end(zlib.gzipSync(Buffer.from(body)));
  });
  t.after(() => server.close());
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const response = await download(`http://127.0.0.1:${server.address().port}`);
  assert.equal(response.text, body);
});

test("isPrivateAddress and isPublicDestination classify local vs public hosts", async () => {
  assert.equal(isPrivateAddress("127.0.0.1"), true);
  assert.equal(isPrivateAddress("10.0.0.1"), true);
  assert.equal(isPrivateAddress("169.254.169.254"), true);
  assert.equal(isPrivateAddress("::ffff:127.0.0.1"), true);
  assert.equal(isPrivateAddress("8.8.8.8"), false);
  assert.equal(await isPublicDestination("http://localhost:3000/x"), false);
  assert.equal(await isPublicDestination("http://service.internal/x"), false);
  assert.equal(await isPublicDestination("https://example.org/docs"), true);
});
