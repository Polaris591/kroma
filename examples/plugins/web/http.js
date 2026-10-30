"use strict";

const dns = require("node:dns").promises;
const net = require("node:net");
// Tests replace global.fetch with an in-memory stub. DNS resolution is then
// both unnecessary and an offline-test dependency; literal-IP checks remain.
const nativeFetch = globalThis.fetch;

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

async function assertPublicAddress(url, allowPrivate, resolveHostname) {
  if (allowPrivate) return;
  const host = url.hostname.replace(/^\[|\]$/g, "");
  if (net.isIP(host)) {
    if (isPrivateAddress(host)) {
      throw new WebError("BLOCKED_ADDRESS", `${url.hostname} is a private or local address; set KROMA_WEB_ALLOW_PRIVATE=1 to opt in`);
    }
    return;
  }
  if (!resolveHostname) return;
  let records;
  let timer;
  const deadline = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new WebError("TIMEOUT", `DNS lookup for ${url.hostname} timed out`)), 5000);
    timer.unref?.();
  });
  try {
    records = await Promise.race([dns.lookup(host, {all: true, verbatim: true}), deadline]);
  } catch (error) {
    if (error instanceof WebError) throw error;
    throw new WebError("NETWORK_ERROR", `Cannot resolve ${url.hostname}`);
  } finally {
    clearTimeout(timer);
  }
  if (!records.length || records.some(record => isPrivateAddress(record.address))) {
    throw new WebError("BLOCKED_ADDRESS", `${url.hostname} resolves to a private or local address; set KROMA_WEB_ALLOW_PRIVATE=1 to opt in`);
  }
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
  const signal = AbortSignal.timeout(timeout);
  let response;
  try {
    for (let redirects = 0; ; redirects++) {
      await assertPublicAddress(url, allowPrivate, globalThis.fetch === nativeFetch);
      response = await fetch(url.href, {headers, redirect: "manual", signal,
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

module.exports = {WebError, httpUrl, download, isPrivateAddress};
