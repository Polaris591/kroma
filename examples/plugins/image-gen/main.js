"use strict";
const readline = require("node:readline");
const {dispatch} = require("./image");

function createHandler({run = dispatch, write = frame => process.stdout.write(JSON.stringify(frame) + "\n")} = {}) {
  let queue = Promise.resolve();
  const handle = line => {
    queue = queue.then(async () => {
      let request;
      try { request = JSON.parse(line); }
      catch { write({content: "Invalid image plugin request", is_error: true}); return; }
      const base = {protocol_version: request.protocol_version ?? 1, request_id: request.request_id};
      const streaming = request.type === "tool_stream";
      // The host's frame-idle guard is shorter than a legitimate image job.
      // Empty progress frames keep transport alive without flooding the TUI.
      const heartbeat = streaming ? setInterval(() => {
        write({...base, event: "output_delta", stream: "stdout", delta: ""});
      }, 30000) : null;
      heartbeat?.unref();
      const progress = text => {
        if (streaming) write({...base, event: "output_delta", stream: "stdout", delta: text + "\n"});
      };
      try {
        const answer = await run(request, {onProgress: progress});
        if (streaming) write({...base, event: "complete", result: answer});
        else write({...base, ...answer});
      } catch (error) {
        const content = error.message || String(error);
        if (streaming) write({...base, event: "complete", result: {content, is_error: true}});
        else write({...base, content, is_error: true});
      } finally {
        if (heartbeat) clearInterval(heartbeat);
      }
    });
    return queue;
  };
  return handle;
}

if (require.main === module) {
  const stream = readline.createInterface({input: process.stdin, crlfDelay: Infinity});
  const handle = createHandler();
  stream.on("line", line => { handle(line).catch(error => {
    console.error("image-gen protocol failed:", error.message); process.exitCode = 1;
  }); });
}
module.exports = {createHandler};
