"use strict";

const dns = require("node:dns").promises;
const http = require("node:http");
const https = require("node:https");
const net = require("node:net");
const {Readable} = require("node:stream");
const tls = require("node:tls");
// Tests replace global.fetch with an in-memory stub. DNS resolution is then
// both unnecessary and an offline-test dependency; literal-IP checks remain.
const nativeFetch = globalThis.fetch;
// Node's global fetch is a thin wrapper that lazy-loads undici; a stub installed
// by a preload script does not match it.
const builtinFetch = (() => {
  try { return /internal\/deps\/undici\/undici/.test(Function.prototype.toString.call(nativeFetch)); }
  catch { return false; }
})();

class WebError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

// Loopback, link-local, private and unique-local ranges. These are never
// useful for fetching public documentation and are a classic SSRF target.
function isPrivateAddress(address) {
  const version = net.isIP(address);
  if (version === 4) {
    const [a, b] = address.split(".").map(Number);
    return a === 0 || a === 10 || a === 127 ||
      (a === 100 && b >= 64 && b <= 127) ||
      (a === 169 && b === 254) ||
      (a === 172 && b >= 16 && b <= 31) ||
      (a === 192 && b === 168);
  }
  if (version === 6) {
    const value = address.toLowerCase();
    if (value === "::" || value === "::1") return true;
    if (/^f[cd]/.test(value) || /^fe[89ab]/.test(value)) return true;
    const mapped = value.match(/^::ffff:(\d+\.\d+\.\d+\.\d+)$/);
    if (mapped) return isPrivateAddress(mapped[1]);
    // IPv4-mapped addresses may be rendered in hexadecimal by the OS.
    const hex = value.match(/^::ffff:([0-9a-f]{1,4}):([0-9a-f]{1,4})$/);
    if (hex) {
      const high = parseInt(hex[1], 16);
      const low = parseInt(hex[2], 16);
      return isPrivateAddress(`${high >> 8}.${high & 255}.${low >> 8}.${low & 255}`);
    }
    return false;
  }
  return true;
}

// The address check shares the request deadline: the socket layer must not keep
// waiting on a slow DNS answer after the request itself has expired.
function expired(signal, hostname) {
  return new Promise((_, reject) => {
    if (!signal) return;
    const fail = () => reject(new WebError("TIMEOUT",
      `Request to ${hostname} exceeded its deadline during the DNS safety check`));
    if (signal.aborted) return fail();
    signal.addEventListener("abort", fail, {once: true});
  });
}

// Returns the validated addresses to pin the connection to, or null when no
// pinning is needed (private network allowed, literal IP, or stub transport).
async function assertPublicAddress(url, allowPrivate, resolveHostname, signal) {
  if (allowPrivate) return null;
  const host = url.hostname.replace(/^\[|\]$/g, "");
  if (net.isIP(host)) {
    if (isPrivateAddress(host)) {
      throw new WebError("BLOCKED_ADDRESS", `${url.hostname} is a private or local address; set KROMA_WEB_ALLOW_PRIVATE=1 to opt in`);
    }
    return null;
  }
  if (!resolveHostname) return null;
  let records;
  try {
    records = await Promise.race([dns.lookup(host, {all: true, verbatim: true}), expired(signal, url.hostname)]);
  } catch (error) {
    if (error instanceof WebError) throw error;
    throw new WebError("NETWORK_ERROR", `Cannot resolve ${url.hostname}`);
  }
  if (!records.length || records.some(record => isPrivateAddress(record.address))) {
    throw new WebError("BLOCKED_ADDRESS", `${url.hostname} resolves to a private or local address; set KROMA_WEB_ALLOW_PRIVATE=1 to opt in`);
  }
  return records.map(record => ({address: record.address, family: record.family}));
}

// A remote extraction API reads the page from our behalf, so the target must
// pass the same check here; otherwise Firecrawl reads become an SSRF gap.
async function assertPublicTarget(value, signal) {
  const url = value instanceof URL ? value : httpUrl(value);
  await assertPublicAddress(url, process.env.KROMA_WEB_ALLOW_PRIVATE === "1", !fetchIsStubbed(), signal);
  return url;
}

// The validated addresses are handed to the socket layer so the connection
// cannot resolve the hostname a second time (DNS rebinding) and reach a
// private address after the public answer was approved.
function pinnedLookup(addresses) {
  return (hostname, options, callback) => {
    const family = options?.family;
    const candidates = family === 4 || family === 6
      ? addresses.filter(record => record.family === family)
      : addresses;
    if (!candidates.length) {
      const error = new Error(`Cannot resolve ${hostname}`);
      error.code = "ENOTFOUND";
      return callback(error);
    }
    if (options?.all) return callback(null, candidates);
    callback(null, candidates[0].address, candidates[0].family);
  };
}

// Minimal fetch Response for the Node http/https transport. Redirect and error
// handling above only needs status, ok, url, header lookup and a web stream.
function nodeResponse(url, response) {
  return {
    status: response.statusCode,
    ok: response.statusCode >= 200 && response.statusCode < 300,
    url: url.href,
    headers: {
      get(name) {
        const value = response.headers[String(name).toLowerCase()];
        return Array.isArray(value) ? value.join(", ") : value ?? null;
      }
    },
    body: Readable.toWeb(response)
  };
}

// A preload script can replace fetch before this module is loaded, so identity
// alone cannot tell the stub transport from the built-in one.
function fetchIsStubbed() {
  return globalThis.fetch !== nativeFetch || typeof nativeFetch !== "function"
    || (!builtinFetch && nativeFetch.name !== "fetch");
}

// main.js re-execs with Node's --use-env-proxy after configuring a proxy, and
// fetch/undici would honor those variables; the pinned transport must too.
function envProxyEnabled() {
  return process.env.KROMA_WEB_PROXY_READY === "1"
    || process.execArgv.includes("--use-env-proxy")
    || process.env.NODE_USE_ENV_PROXY === "1"
    || /(?:^|\s)--use-env-proxy(?:\s|$)/.test(process.env.NODE_OPTIONS ?? "");
}

function bypassesProxy(url, noProxy) {
  if (!noProxy) return false;
  const host = url.hostname.replace(/^\[|\]$/g, "").toLowerCase();
  const port = url.port || (url.protocol === "https:" ? "443" : "80");
  return noProxy.split(",").some(raw => {
    let entry = raw.trim().toLowerCase();
    if (!entry) return false;
    if (entry === "*") return true;
    let entryPort = null;
    if (entry.startsWith("[") && entry.includes("]")) {
      const close = entry.indexOf("]");
      if (entry[close + 1] === ":") entryPort = entry.slice(close + 2);
      entry = entry.slice(1, close);
    } else if ((entry.match(/:/g) ?? []).length === 1) {
      const colon = entry.lastIndexOf(":");
      entryPort = entry.slice(colon + 1);
      entry = entry.slice(0, colon);
    }
    if (entry.startsWith("*.")) entry = entry.slice(1);
    if (entry.startsWith(".")) entry = entry.slice(1);
    if (entryPort && entryPort !== port) return false;
    return host === entry || host.endsWith("." + entry);
  });
}

function proxyFor(url) {
  if (!envProxyEnabled()) return null;
  const env = process.env;
  const raw = url.protocol === "https:"
    ? env.HTTPS_PROXY || env.https_proxy
    : env.HTTP_PROXY || env.http_proxy;
  if (!raw || bypassesProxy(url, env.NO_PROXY || env.no_proxy)) return null;
  let proxy;
  try { proxy = new URL(raw); } catch { return null; }
  if (!["http:", "https:"].includes(proxy.protocol)) return null;
  return proxy;
}

function openTunnel(proxy, authority, signal) {
  return new Promise((resolve, reject) => {
    const headers = {Host: authority};
    if (proxy.username || proxy.password) {
      let credentials;
      try { credentials = `${decodeURIComponent(proxy.username)}:${decodeURIComponent(proxy.password)}`; }
      catch { credentials = `${proxy.username}:${proxy.password}`; }
      headers["Proxy-Authorization"] = "Basic " + Buffer.from(credentials).toString("base64");
    }
    const request = (proxy.protocol === "https:" ? https : http).request({
      host: proxy.hostname.replace(/^\[|\]$/g, ""),
      port: proxy.port || (proxy.protocol === "https:" ? 443 : 80),
      method: "CONNECT", path: authority, headers, signal
    });
    request.once("connect", (response, socket, head) => {
      if (response.statusCode !== 200) {
        socket.destroy();
        reject(new Error(`Proxy CONNECT failed with HTTP ${response.statusCode}`));
        return;
      }
      if (head?.length) socket.unshift(head);
      resolve(socket);
    });
    request.once("error", reject);
    request.end();
  });
}

// Production transport: node:http(s) with the pre-validated address list. The
// URL still supplies Host, SNI and certificate hostname verification. Proxy
// tunnels CONNECT to the pinned address while keeping the original hostname.
async function pinnedRequest(url, options, addresses) {
  const headers = {...options.headers};
  // fetch negotiates and decodes compression; node:http does not, so ask for
  // an unencoded body rather than handing compressed bytes to the extractor.
  if (!Object.keys(headers).some(key => key.toLowerCase() === "accept-encoding")) {
    headers["Accept-Encoding"] = "identity";
  }
  const body = options.body;
  if (body != null && !Object.keys(headers).some(key => key.toLowerCase() === "content-length")) {
    headers["Content-Length"] = String(Buffer.byteLength(body));
  }
  const host = url.hostname.replace(/^\[|\]$/g, "");
  const port = url.port || (url.protocol === "https:" ? 443 : 80);
  const proxy = proxyFor(url);
  let socket, agent;
  if (proxy) {
    const connectHost = addresses?.length ? addresses[0].address : host;
    const authority = `${net.isIP(connectHost) === 6 ? `[${connectHost}]` : connectHost}:${port}`;
    socket = await openTunnel(proxy, authority, options.signal);
    agent = new (url.protocol === "https:" ? https.Agent : http.Agent)({keepAlive: false, maxSockets: 1});
    agent.createConnection = url.protocol === "https:"
      ? () => tls.connect({socket, host, servername: net.isIP(host) ? "" : host})
      : () => socket;
  }
  return new Promise((resolve, reject) => {
    let request;
    try {
      request = (url.protocol === "https:" ? https : http).request(url, {
        method: options.method ?? "GET",
        headers,
        signal: options.signal,
        ...(agent ? {agent} : addresses ? {lookup: pinnedLookup(addresses)} : {})
      }, response => resolve(nodeResponse(url, response)));
    } catch (error) {
      socket?.destroy();
      reject(error);
      return;
    }
    request.on("error", error => {
      socket?.destroy();
      reject(error);
    });
    request.end(body ?? undefined);
  });
}

function httpUrl(value) {
  let url;
  try { url = new URL(String(value ?? "")); }
  catch { throw new WebError("INVALID_URL", "Provide a full HTTP(S) URL"); }
  if (!["https:", "http:"].includes(url.protocol)) {
    throw new WebError("INVALID_URL", "Only HTTP(S) URLs are supported");
  }
  if (url.username || url.password) {
    throw new WebError("INVALID_URL", "URLs containing credentials are not supported");
  }
  return url;
}

function decodeBody(buffer, contentType) {
  const charset = contentType.match(/charset\s*=\s*["']?([^\s;"']+)/i)?.[1]
    || buffer.subarray(0, 4096).toString("ascii").match(/<meta\b[^>]*charset\s*=\s*["']?([\w-]+)/i)?.[1]
    || "utf-8";
  try { return new TextDecoder(charset).decode(buffer); }
  catch { return buffer.toString("utf8"); }
}

async function download(address, options = {}) {
  let url = httpUrl(address);
  const allowPrivate = options.allowPrivateNetwork ?? process.env.KROMA_WEB_ALLOW_PRIVATE === "1";
  const headers = {"User-Agent": "Mozilla/5.0 (compatible; Kroma-Web/1.6)",
    Accept: "text/html,application/json;q=0.9,*/*;q=0.5", ...options.headers};
  const timeout = options.timeout ?? 15000;
  const signal = options.signal ?? AbortSignal.timeout(timeout);
  const pinned = !fetchIsStubbed();
  let response;
  try {
    for (let redirects = 0; ; redirects++) {
      const addresses = await assertPublicAddress(url, allowPrivate, pinned, signal);
      response = pinned
        ? await pinnedRequest(url, {headers, method: options.method, body: options.body, signal}, addresses)
        : await fetch(url.href, {headers, redirect: "manual", signal,
          ...(options.method ? {method: options.method, body: options.body} : {})});
      if (![301, 302, 303, 307, 308].includes(response.status)) break;
      const location = response.headers.get("location");
      await response.body?.cancel();
      // Do not replay a search POST or forward its query/body to a redirect target.
      if (options.method === "POST") {
        throw new WebError("REDIRECT_ERROR", "Search POST redirects are not supported");
      }
      if (!location || redirects >= 5) {
        throw new WebError("REDIRECT_ERROR", "Missing redirect destination or more than 5 redirects");
      }
      const next = httpUrl(new URL(location, url).href);
      if (next.origin !== url.origin) {
        for (const key of Object.keys(headers)) {
          if (/authorization|cookie|token|api[-_]?key/i.test(key)) delete headers[key];
        }
      }
      url = next;
    }
    if (!response.ok) {
      await response.body?.cancel();
      const hint = response.status === 429 ? "; rate limited, try later or use another provider"
        : [401, 403].includes(response.status) ? "; access denied, authentication or a browser may be required" : "";
      const error = new WebError("HTTP_ERROR", `HTTP ${response.status} from ${url.origin}${url.pathname}${hint}`);
      error.status = response.status;
      throw error;
    }
    const contentType = (response.headers.get("content-type") ?? "").toLowerCase();
    if (contentType && options.types && !options.types.some(t => contentType.includes(t))) {
      await response.body?.cancel();
      throw new WebError("UNSUPPORTED_CONTENT", `Unsupported content type: ${contentType.slice(0, 120)}. Use a direct HTML or text URL; PDF/binary parsing is not supported.`);
    }
    const reader = response.body?.getReader();
    const chunks = [];
    const limit = options.bytes ?? 1500000;
    let bytes = 0, truncated = false;
    if (reader) {
      try {
        while (true) {
          const {done, value} = await reader.read();
          if (done) break;
          const remaining = limit - bytes;
          chunks.push(Buffer.from(value.subarray(0, remaining)));
          bytes += Math.min(value.length, remaining);
          if (value.length > remaining) {
            truncated = true;
            await reader.cancel();
            break;
          }
        }
      } finally { reader.releaseLock(); }
    }
    return {text: decodeBody(Buffer.concat(chunks), contentType), contentType,
      truncated, url: response.url || url.href};
  } catch (error) {
    if (error instanceof WebError) throw error;
    if (signal.aborted) throw new WebError("TIMEOUT", `Request to ${url.hostname} timed out after ${timeout} ms`);
    throw new WebError("NETWORK_ERROR", `Cannot reach ${url.hostname}; check network, DNS, proxy and TLS settings`);
  }
}

module.exports = {WebError, httpUrl, download, isPrivateAddress, assertPublicTarget};
