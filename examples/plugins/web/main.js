"use strict";
// Credentials come from plugin settings or the environment; there is no second
// configuration file. Node's fetch honours HTTP(S)_PROXY only when started with
// --use-env-proxy (Node 22.21+/24.5+), so restart once with that flag instead of
// writing a CONNECT tunnel. Older Node keeps working and says what is missing.
function proxyConfigured() {
  const env = process.env;
  return !!(env.HTTPS_PROXY || env.https_proxy || env.HTTP_PROXY || env.http_proxy);
}

function worker() {
  const {dispatch} = require("./web");
  const input = require("node:readline").createInterface({input: process.stdin, crlfDelay: Infinity});
  (async () => {
    for await (const line of input) {
      let request, response;
      try {
        request = JSON.parse(line);
        response = {content: await dispatch(request)};
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

if (!process.env.KROMA_WEB_PROXY_READY && proxyConfigured()) {
  if (!process.allowedNodeEnvironmentFlags.has("--use-env-proxy")) {
    console.error("Proxy variables are set but this Node cannot apply them to fetch; install Node 22.21+ or route requests directly.");
    worker();
  } else {
    const child = require("node:child_process").spawn(process.execPath, ["--use-env-proxy", __filename], {
      // Do not open a second console in Windows Desktop when the proxy worker
      // re-executes Node; its stdio remains inherited from the plugin worker.
      stdio: "inherit", windowsHide: true, env: {...process.env, KROMA_WEB_PROXY_READY: "1"}
    });
    child.on("error", () => { console.error("Could not start the web proxy worker."); process.exitCode = 1; });
    child.on("exit", code => { process.exitCode = code ?? 1; });
  }
} else {
  worker();
}
