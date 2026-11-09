"use strict";
const {dispatch} = require("./subagent");

const PROTOCOL_VERSION = 1;

// One JSON line in, one correlated frame out. Fatal execution errors use the
// stream `error` frame so the host discards this worker; task failures stay
// `complete` with is_error so one bad task does not poison later calls.
function createHandler({run = dispatch, write = frame => process.stdout.write(JSON.stringify(frame) + "\n")} = {}) {
  let queue = Promise.resolve();
  function handle(line) {
    queue = queue.then(() => respond(line));
    return queue;
  }

  async function respond(line) {
    let request = null;
    const base = {protocol_version: PROTOCOL_VERSION};
    try {
      request = JSON.parse(line);
    } catch (error) {
      write({...base, content: `invalid JSON request: ${error.message}`, is_error: true});
      return;
    }
    if (request?.request_id !== undefined) base.request_id = request.request_id;
    const streaming = request?.type === "tool_stream";
    // One line per progress message: the host appends deltas verbatim.
    const emit = text => {
      if (streaming) {
        write({...base, event: "output_delta", stream: "stdout", delta: `${text}
`});
      }
    };
    // A child can think for minutes without a progress line. Empty deltas keep
    // the host's silent-tool guard from treating a working child as hung.
    const heartbeat = streaming ? setInterval(() => {
      write({...base, event: "output_delta", stream: "stdout", delta: ""});
    }, 30000) : null;
    heartbeat?.unref();
    try {
      const outcome = await run(request, {onProgress: emit});
      const content = outcome?.content ?? "";
      const isError = Boolean(outcome?.is_error);
      if (streaming) write({...base, event: "complete", result: {content, is_error: isError}});
      else write({...base, content, ...(isError ? {is_error: true} : {})});
    } catch (error) {
      const content = error?.message ?? String(error);
      if (streaming && error?.fatal) write({...base, event: "error", message: content});
      else if (streaming) write({...base, event: "complete", result: {content, is_error: true}});
      else write({...base, content, is_error: true});
    } finally {
      if (heartbeat) clearInterval(heartbeat);
    }
  }

  return handle;
}

function start() {
  const readline = require("node:readline");
  const input = readline.createInterface({input: process.stdin, crlfDelay: Infinity});
  const handle = createHandler();
  input.on("line", line => {
    handle(line).catch(() => {
      console.error("subagent protocol stream failed.");
      process.exitCode = 1;
    });
  });
}

if (require.main === module) start();

module.exports = {createHandler, start};
