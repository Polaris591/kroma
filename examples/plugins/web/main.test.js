"use strict";
process.env.KROMA_WEB_ALLOW_PRIVATE = "1";
const {test} = require("node:test");
const assert = require("node:assert/strict");
const {spawn} = require("node:child_process");
const {createServer} = require("node:http");
const path = require("node:path");

test("JSONL process correlates success/errors, handles malformed input and reads continuation", {timeout: 10000}, async t => {
  let requests = 0;
  const content = "document ".repeat(300);
  const server = createServer((req, res) => {
    requests++;
    if (req.url.startsWith("/search")) {
      res.writeHead(200, {"Content-Type": "application/json"});
      res.end(JSON.stringify({results: [{title: "Local search", url: "https://example.org/doc", content: "Original snippet"}]}));
      return;
    }
    res.writeHead(200, {"Content-Type": "text/html"});
    res.end(`<title>Local document</title><main>${content}</main>`);
  });
  t.after(() => server.close());
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const url = `http://127.0.0.1:${server.address().port}/page`;
  const child = spawn(process.execPath, [path.join(__dirname, "main.js")], {stdio: ["pipe", "pipe", "pipe"], env: {...process.env, KROMA_WEB_CREDENTIALS_FILE: path.join(__dirname, "missing-test-credentials.json")}});
  t.after(() => child.kill());
  let stdout = "", stderr = "";
  child.stdout.setEncoding("utf8").on("data", chunk => { stdout += chunk; });
  child.stderr.setEncoding("utf8").on("data", chunk => { stderr += chunk; });
  const exited = new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("exit", (code, signal) => code === 0 ? resolve() : reject(Error(`exit ${code}/${signal}: ${stderr}`)));
  });
  const request = (id, args) => ({protocol_version: 1, request_id: id, type: "tool", name: "web_fetch", arguments: args});
  child.stdin.end([
    "{bad json",
    JSON.stringify(request(1, {url, max_chars: 1000})),
    JSON.stringify(request(2, {url, start_char: 1000, max_chars: 30000})),
    JSON.stringify(request(3, {url: "file:///secret"})),
    JSON.stringify(request(4, {url: url + "?not-cached", start_char: 1000})),
    JSON.stringify({protocol_version: 1, request_id: 5, type: "tool", name: "web_search", arguments: {query: "test"},
      config: {provider: "searxng", searxng_url: new URL(url).origin}}),
    JSON.stringify({protocol_version: 1, request_id: 6, type: "tool", name: "web_search", arguments: {query: "test"},
      config: {provider: "auto", search_priority: ["invalid"], exa_api_key: "key", brave_api_key: "other-key"}})
  ].join("\n") + "\n");
  await exited;
  assert.equal(stderr, "");
  const lines = stdout.trim().split("\n").map(line => JSON.parse(line));
  assert.equal(lines.length, 7);
  assert.equal(lines[0].is_error, true);
  assert.equal(lines[1].request_id, 1);
  assert.equal(lines[2].request_id, 2);
  assert.equal(lines[3].request_id, 3);
  assert.equal(lines[1].protocol_version, 1);
  assert.equal(lines[3].code, "INVALID_URL");
  const first = JSON.parse(lines[1].content), second = JSON.parse(lines[2].content);
  assert.equal(first.text + second.text, content.trim());
  assert.equal(first.next_start, 1000);
  assert.equal(second.next_start, null);
  assert.equal(lines[4].request_id, 4);
  assert.equal(lines[4].code, "CACHE_MISS");
  assert.equal(lines[5].request_id, 5);
  assert.equal(JSON.parse(lines[5].content).provider, "searxng");
  assert.equal(JSON.parse(lines[5].content).results[0].snippet, "Original snippet");
  assert.equal(lines[6].request_id, 6);
  assert.equal(lines[6].code, "CONFIG_ERROR");
  assert.equal(requests, 2);
});

async function runWorker(t, credentials, requests, env = {}) {
  const fs = require("node:fs");
  const os = require("node:os");
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-web-credentials-"));
  const file = path.join(dir, "credentials.json");
  fs.writeFileSync(file, typeof credentials === "string" ? credentials : JSON.stringify(credentials));
  t.after(() => fs.rmSync(dir, {recursive: true, force: true}));
  const child = spawn(process.execPath, [path.join(__dirname, "main.js")], {
    stdio: ["pipe", "pipe", "pipe"], env: {...process.env, ...env, KROMA_WEB_CREDENTIALS_FILE: file}
  });
  t.after(() => child.kill());
  let stdout = "", stderr = "";
  child.stdout.on("data", chunk => { stdout += chunk; });
  child.stderr.on("data", chunk => { stderr += chunk; });
  child.stdin.end(requests.map(x => JSON.stringify(x)).join("\n") + "\n");
  const code = await new Promise((resolve, reject) => { child.on("error", reject); child.on("close", resolve); });
  return {code, stdout, stderr};
}

test("local credentials load outside source, host settings override them and parse errors never echo secrets", {timeout: 6000}, async t => {
  let calls = 0;
  const server = createServer((_req, res) => {
    calls++;
    res.writeHead(200, {"content-type": "application/json"});
    res.end('{"results":[{"title":"Local","url":"https://example.org/doc","content":"Source"}]}');
  });
  t.after(() => { server.closeAllConnections(); server.close(); });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const local = {provider: "searxng", searxng_url: `http://127.0.0.1:${server.address().port}`, tavily_api_key: "not-for-output"};
  const req = {type: "tool", name: "web_search", request_id: 1, arguments: {query: "test"}};
  const result = await runWorker(t, local, [req, {...req, request_id: 2, config: {provider: "unknown"}}]);
  assert.equal(result.code, 0);
  const lines = result.stdout.trim().split("\n").map(JSON.parse);
  assert.equal(JSON.parse(lines[0].content).provider, "searxng");
  assert.equal(lines[1].code, "CONFIG_ERROR");
  assert.equal(calls, 1);
  assert.doesNotMatch(result.stdout + result.stderr, /not-for-output/);
  const bad = await runWorker(t, '{"tavily_api_key":"not-for-output",invalid', [req]);
  assert.equal(bad.code, 1);
  assert.equal(bad.stdout, "");
  assert.match(bad.stderr, /Cannot read web credentials/);
  assert.doesNotMatch(bad.stderr, /not-for-output/);
});

test("configured proxy re-execs before JSONL input and tunnels an actual request", {timeout: 8000}, async t => {
  if (!process.allowedNodeEnvironmentFlags.has("--use-env-proxy")) return t.skip("Node lacks native env proxy support");
  let connections = 0;
  const sockets = new Set();
  const proxy = createServer();
  proxy.on("connect", (_req, socket) => {
    connections++;
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
    socket.once("data", () => {
      const body = '{"results":[{"title":"Proxied","url":"https://example.org/doc","content":"Original"}]}';
      socket.end(`HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ${Buffer.byteLength(body)}\r\nConnection: close\r\n\r\n${body}`);
    });
  });
  t.after(() => { for (const socket of sockets) socket.destroy(); proxy.close(); });
  await new Promise(resolve => proxy.listen(0, "127.0.0.1", resolve));
  const result = await runWorker(t, {
    proxy: `http://127.0.0.1:${proxy.address().port}`, provider: "searxng", searxng_url: "http://kroma-proxy-test.invalid"
  }, [{type: "tool", name: "web_search", request_id: 10, arguments: {query: "test"}}]);
  assert.equal(result.code, 0, result.stderr);
  const response = JSON.parse(result.stdout);
  assert.equal(response.request_id, 10);
  assert.equal(JSON.parse(response.content).results[0].title, "Proxied");
  assert.equal(connections, 1);
});

test("disabled imported proxy stays direct and an invalid active proxy fails without exposing its password", {timeout: 6000}, async t => {
  const unknown = {type: "tool", name: "web_search", arguments: {query: "test"}, config: {provider: "unknown"}};
  const disabled = await runWorker(t, {proxy: "invalid-password-secret", proxy_enabled: false}, [unknown]);
  assert.equal(disabled.code, 0);
  assert.equal(JSON.parse(disabled.stdout).code, "CONFIG_ERROR");
  const invalid = await runWorker(t, {proxy: "socks5://user:password-secret@localhost:1"}, [unknown]);
  assert.equal(invalid.code, 1);
  assert.equal(invalid.stdout, "");
  assert.doesNotMatch(invalid.stderr, /password-secret/);
  assert.match(invalid.stderr, /proxy requires/);
});

test("JSONL priority failover reports actual provider and preserves diagnostics on terminal failure", {timeout: 6000}, async t => {
  const calls = [];
  const server = createServer((req, res) => {
    calls.push(req.url.split("?")[0]);
    if (req.url.startsWith("/search")) { res.writeHead(503); res.end("never-echo-secret"); return; }
    let body = "";
    req.on("data", chunk => { body += chunk; });
    req.on("end", () => {
      assert.equal(req.headers.authorization, "Bearer fake-local-key");
      if (JSON.parse(body).query === "fail") { res.writeHead(429); res.end("never-echo-secret"); return; }
      res.writeHead(200, {"content-type": "application/json"});
      res.end('{"success":true,"data":{"web":[{"title":"Fallback","url":"https://example.org/doc","description":"Source"}]}}');
    });
  });
  t.after(() => { server.closeAllConnections(); server.close(); });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const req = {type: "tool", name: "web_search", request_id: 11, arguments: {query: "ok"}};
  const result = await runWorker(t, {provider: "auto", search_priority: ["searxng", "firecrawl"],
    searxng_url: origin, firecrawl_base_url: origin, firecrawl_api_key: "fake-local-key"}, [req,
    {...req, request_id: 12, arguments: {query: "fail"}},
    {...req, request_id: 13, arguments: {query: "ok", provider: "searxng"}}]);
  assert.equal(result.code, 0);
  const lines = result.stdout.trim().split("\n").map(JSON.parse);
  const success = JSON.parse(lines[0].content);
  assert.equal(success.provider, "firecrawl");
  assert.equal(success.attempts[0].status, 503);
  assert.equal(lines[1].request_id, 12);
  assert.equal(lines[1].code, "HTTP_ERROR");
  assert.deepEqual(lines[1].attempts.map(x => x.provider), ["searxng", "firecrawl"]);
  assert.equal(lines[1].attempts[1].status, 429);
  assert.equal(lines[2].attempts, undefined);
  assert.deepEqual(calls, ["/search", "/v2/search", "/search", "/v2/search", "/search"]);
  assert.doesNotMatch(result.stdout + result.stderr, /fake-local-key|never-echo-secret/);
});

test("a clean no-key JSONL worker automatically returns free results and never sends credentials", {timeout: 6000}, async t => {
  const fs = require("node:fs"), os = require("node:os");
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-free-worker-"));
  t.after(() => fs.rmSync(dir, {recursive: true, force: true}));
  const preload = path.join(dir, "mock.cjs");
  // Test-only transport fixture; production worker has no endpoint/test override.
  fs.writeFileSync(preload, `
    global.fetch = async (url, options) => {
      const assert = require("node:assert/strict");
      const host = new URL(url).hostname;
      assert.doesNotMatch(JSON.stringify(options.headers), /authorization|api.key|subscription.token/i);
      if (host === "api.mwmbl.org") return new Response("[]", {headers:{"content-type":"application/json"}});
      if (host === "html.duckduckgo.com") return new Response('<form id="challenge-form"></form>', {headers:{"content-type":"text/html"}});
      assert.equal(host, "www.bing.com");
      return new Response('<rss><item><title>Free source</title><link>https://example.org/doc</link><description>Original text</description></item></rss>', {headers:{"content-type":"application/rss+xml"}});
    };
  `);
  const env = {NODE_OPTIONS: `--require "${preload.split(path.sep).join("/")}"`};
  for (const key of ["PARALLEL_API_KEY", "YDC_API_KEY", "EXA_API_KEY", "TAVILY_API_KEY", "BRAVE_SEARCH_API_KEY", "FIRECRAWL_API_KEY"]) env[key] = "";
  const result = await runWorker(t, {}, [{type: "tool", name: "web_search", request_id: 150, arguments: {query: "test site:example.org"}}], env);
  assert.equal(result.code, 0, result.stderr);
  const response = JSON.parse(result.stdout);
  assert.equal(response.request_id, 150);
  const content = JSON.parse(response.content);
  assert.equal(content.provider, "bing");
  assert.equal(content.mode, "free");
  assert.deepEqual(content.attempts.map(x => [x.provider, x.code]), [["mwmbl", "NO_RESULTS"], ["duckduckgo", "SEARCH_BLOCKED"]]);
  assert.equal(content.results[0].title, "Free source");
});
