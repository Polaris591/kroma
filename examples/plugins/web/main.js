"use strict";
const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");

// Credentials live outside the installed/source code and never enter tool schemas.
const stateRoot = process.env.KROMA_PLUGIN_DIR
  ? path.dirname(path.resolve(process.env.KROMA_PLUGIN_DIR)) : path.join(os.homedir(), ".kroma");
const credentialsPath = process.env.KROMA_WEB_CREDENTIALS_FILE
  || path.join(stateRoot, "plugin-data", "web", "credentials.json");
let local = {};
try {
  if (fs.existsSync(credentialsPath)) {
    local = JSON.parse(fs.readFileSync(credentialsPath, "utf8"));
    if (!local || typeof local !== "object" || Array.isArray(local)) throw Error();
  }
} catch {
  console.error("Cannot read web credentials.json; fix its JSON object/permissions and restart.");
  process.exit(1);
}

// Node reads proxy variables at startup. Re-exec once before reading stdin rather
// than implementing CONNECT/TLS ourselves or installing a networking dependency.
if (local.proxy && local.proxy_enabled !== false && !process.env.KROMA_WEB_PROXY_READY) {
  let proxy;
  try {
    proxy = new URL(local.proxy);
    if (!["http:", "https:"].includes(proxy.protocol)) throw Error();
    if (!process.allowedNodeEnvironmentFlags.has("--use-env-proxy")) throw Error();
  } catch {
    console.error("Web proxy requires an HTTP(S) URL and Node.js 22.21+ or 24.5+.");
    process.exit(1);
  }
  const child = require("node:child_process").spawn(process.execPath, ["--use-env-proxy", __filename], {
    stdio: "inherit", env: {...process.env, KROMA_WEB_PROXY_READY: "1",
      HTTP_PROXY: proxy.href, HTTPS_PROXY: proxy.href, http_proxy: proxy.href, https_proxy: proxy.href,
      NO_PROXY: process.env.NO_PROXY || "localhost,127.0.0.1,::1"}
  });
  child.on("error", () => { console.error("Could not start web proxy worker."); process.exitCode = 1; });
  child.on("exit", code => { process.exitCode = code ?? 1; });
} else {
  const {dispatch} = require("./web");
  const input = require("node:readline").createInterface({input: process.stdin, crlfDelay: Infinity});
  // Web 1.6: keyless auto/free routing; process in order with correlated responses.
  (async () => {
    for await (const line of input) {
      let request, response;
      try {
        request = JSON.parse(line);
        // Host settings override local defaults, but never need to contain secrets.
        response = {content: await dispatch({...request, config: {...local, ...request?.config}})};
      } catch (error) {
        response = {content: error.message || String(error), is_error: true, code: error.code ?? "INVALID_REQUEST",
          ...(error.attempts ? {attempts: error.attempts} : {})};
      }
      if (request?.request_id !== undefined) response.request_id = request.request_id;
      if (request?.protocol_version !== undefined) response.protocol_version = 1;
      process.stdout.write(JSON.stringify(response) + "\n");
    }
  })().catch(() => { console.error("Web protocol stream failed."); process.exitCode = 1; });
}
