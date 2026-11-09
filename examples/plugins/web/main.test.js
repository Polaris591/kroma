"use strict";
const {test} = require("node:test");
const assert = require("node:assert/strict");
const {spawn} = require("node:child_process");
const {createServer} = require("node:http");
const path = require("node:path");

function cleanEnv(extra = {}) {
  const env =  {...process.env, ...extra};
  for (const key of ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "NO_PROXY", "no_proxy",
    "KROMA_WEB_PROXY_READY", "KROMA_WEB_CREDENTIALS_FILE"]) delete env[key];
  return {...env, ...extra};
}

async function runWorker(t, requests, env = {}) {
  const child = spawn(process.execPath, [path.join(__dirname, "main.js")],
    {stdio: ["pipe", "pipe", "pipe"], env: cleanEnv(env), windowsHide: true});
  t.after(() => child.kill());
  let stdout = "", stderr = "";
  child.stdout.on("data", chunk => { stdout += chunk; });
  child.stderr.on("data", chunk => { stderr += chunk; });
  child.stdin.end(requests.map(x => JSON.stringify(x)).join("\n") + "\n");
  const code = await new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("close", resolve);
  });
  return {code, stdout, stderr, lines: stdout.trim() ? stdout.trim().split("\n").map(JSON.parse) : []};
}

test("JSONL protocol keeps order, reports errors and serves continuation from memory", {timeout: 15000}, async t => {
  const content = "document ".repeat(300);
  let requests = 0;
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
  t.after(() => { server.closeAllConnections(); server.close(); });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  const fetchRequest = (id, args) => ({protocol_version: 1, request_id: id, type: "tool", name: "web_fetch", arguments: args});
  const result = await runWorker(t, [
    "{bad json",
    fetchRequest(1, {url: `${base}/page`, max_chars: 1000}),
    fetchRequest(2, {url: `${base}/page`, start_char: 1000, max_chars: 30000}),
    fetchRequest(3, {url: "file:///secret"}),
    fetchRequest(4, {url: `${base}/page?not-cached`, start_char: 1000}),
    {protocol_version: 1, request_id: 5, type: "tool", name: "web_search", arguments: {query: "test"},
      config: {provider: "searxng", searxng_url: base}},
    {protocol_version: 1, request_id: 6, type: "tool", name: "web_search", arguments: {query: "test"},
      config: {provider: "nope"}}
  ]);
  assert.equal(result.code, 0);
  assert.equal(result.stderr, "", "no configuration file warning is printed anymore");
  assert.equal(result.lines.length, 7);
  assert.equal(result.lines[0].is_error, true);
  assert.equal(result.lines[1].request_id, 1);
  assert.equal(result.lines[1].protocol_version, 1);
  assert.equal(result.lines[3].code, "INVALID_URL");
  const first = JSON.parse(result.lines[1].content), second = JSON.parse(result.lines[2].content);
  assert.equal(first.text + second.text, content.trim());
  assert.equal(first.next_start, 1000);
  assert.equal(second.next_start, null);
  assert.equal(result.lines[4].code, "CACHE_MISS");
  assert.equal(JSON.parse(result.lines[5].content).results[0].snippet, "Original snippet");
  assert.equal(result.lines[6].code, "CONFIG_ERROR");
  assert.equal(requests, 2);
});

test("configured proxy re-executes Node so fetch honours it", {timeout: 15000}, async t => {
  if (!process.allowedNodeEnvironmentFlags.has("--use-env-proxy")) return t.skip("Node lacks native env proxy support");
  let seen = 0;
  const CRLF = String.fromCharCode(13, 10);
  const body = '{"results":[{"title":"Proxied","url":"https://example.org/doc","content":"Original"}]}';
  // undici tunnels through CONNECT even for http targets, so the fake proxy
  // acknowledges the tunnel and answers the request written inside it.
  const proxy = createServer((_req, res) => {
    seen++;
    res.writeHead(200, {"Content-Type": "application/json", "Content-Length": Buffer.byteLength(body)});
    res.end(body);
  });
  proxy.on("connect", (_req, socket) => {
    seen++;
    socket.write("HTTP/1.1 200 Connection Established" + CRLF + CRLF);
    socket.once("data", () => {
      socket.end("HTTP/1.1 200 OK" + CRLF + "Content-Type: application/json" + CRLF
        + "Content-Length: " + Buffer.byteLength(body) + CRLF + "Connection: close" + CRLF + CRLF + body);
    });
  });
  t.after(() => { proxy.closeAllConnections(); proxy.close(); });
  await new Promise(resolve => proxy.listen(0, "127.0.0.1", resolve));
  const result = await runWorker(t, [
    {type: "tool", request_id: 1, name: "web_search", arguments: {query: "test"},
      config: {provider: "searxng", searxng_url: "http://search.example.org"}}
  ], {HTTP_PROXY: `http://127.0.0.1:${proxy.address().port}`, http_proxy: `http://127.0.0.1:${proxy.address().port}`});
  assert.equal(result.code, 0);
  assert.equal(seen, 1, "the request went through the proxy");
  assert.equal(JSON.parse(result.lines[0].content).provider, "searxng");
});
