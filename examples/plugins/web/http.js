"use strict";

class WebError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

// Sites that block obvious bots answer a browser-shaped request far more often
// than a bare API client. This is a header profile, not a browser: no JS, no
// cookies, no fingerprinting tricks.
const BROWSER_HEADERS = Object.freeze({
  "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36",
  Accept: "text/html,application/xhtml+xml,application/xml;q=0.9,application/json;q=0.8,*/*;q=0.7",
  "Accept-Language": "zh-CN,zh;q=0.9,en;q=0.8",
  "Cache-Control": "no-cache",
  "Upgrade-Insecure-Requests": "1",
  "Sec-Fetch-Dest": "document",
  "Sec-Fetch-Mode": "navigate",
  "Sec-Fetch-Site": "none"
});

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

function stripCredentials(headers) {
  for (const key of Object.keys(headers)) {
    if (/authorization|cookie|token|api[-_]?key/i.test(key)) delete headers[key];
  }
}

// The system fetch supplies compression handling, HTTP/2 and connection reuse;
// redirects stay manual so every hop is checked and POST bodies are never
// replayed to another origin.
async function download(address, options = {}) {
  let url = httpUrl(address);
  const headers = {...BROWSER_HEADERS, ...options.headers};
  const timeout = options.timeout ?? 15000;
  const signal = options.signal ?? AbortSignal.timeout(timeout);
  try {
    let response;
    for (let redirects = 0; ; redirects++) {
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
      if (next.origin !== url.origin) stripCredentials(headers);
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
      truncated, url: response.url || url.href, headers: response.headers};
  } catch (error) {
    if (error instanceof WebError) throw error;
    if (signal.aborted) throw new WebError("TIMEOUT", `Request to ${url.hostname} timed out after ${timeout} ms`);
    throw new WebError("NETWORK_ERROR", `Cannot reach ${url.hostname}; check network, DNS, proxy and TLS settings, then retry or use another provider (kroma plugins set web provider <name>)`);
  }
}

module.exports = {WebError, httpUrl, download};
