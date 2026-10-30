"use strict";
// Demonstrates the stream contract. Replace the fixture with real provider
// events to support any upstream model protocol.
const readline = require("node:readline");
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
const send = (request, payload) => {
  process.stdout.write(JSON.stringify({
    protocol_version: 1,
    request_id: request.request_id,
    ...payload
  }) + "\n");
};
readline.createInterface({ input: process.stdin, crlfDelay: Infinity })
  .on("line", async line => {
    try {
      const request = JSON.parse(line);
      if (request.type === "model_generate") {
        send(request, { content: JSON.stringify({
          text: "Stream summary", requests: 1, usage: { input: 8, output: 3 }
        }) });
        return;
      }
      if (request.type !== "model_stream") throw Error("Unknown operation");
      const prompt = request.arguments.messages.at(-1)?.content ?? "";
      if (prompt === "bad-id") {
        process.stdout.write(JSON.stringify({
          protocol_version: 1, request_id: request.request_id + 1,
          event: "text_delta", delta: "wrong"
        }) + "\n");
        return;
      }
      if (prompt === "stall") {
        send(request, { event: "text_delta", delta: "started" });
        return; // No terminal frame, for cancellation tests.
      }
      if (prompt === "error") {
        send(request, { event: "error", message: "Simulated provider failure" });
        return;
      }
      if (prompt === "show-level") {
        send(request, { event: "complete", result: {
          message: { role: "assistant", content: `reasoning_level=${request.arguments.reasoning_level}` },
          usage: { input: 4, output: 1 }
        } });
        return;
      }
      if (request.arguments.tools.length) {
        send(request, { event: "tool_call_delta", index: 0,
          id: "call-1", name: "shell", arguments_delta: '{"command":"' });
        await pause(20);
        send(request, { event: "tool_call_delta", index: 0,
          arguments_delta: 'echo hi"}' });
        if (prompt === "tool-mismatch") {
          send(request, { event: "complete", result: { message: {
            role: "assistant", content: null, tool_calls: [{
              id: "call-1", type: "function", function: {name:"shell",arguments:'{"command":"different"}'}
            }] } } });
          return;
        }
        send(request, { event: "complete", result: {
          message: { role: "assistant", content: null },
          usage: { input: 20, output: 7 }
        } });
        return;
      }
      send(request, { event: "reasoning_delta", delta: "Think " });
      await pause(25);
      send(request, { event: "reasoning_delta", delta: "first." });
      await pause(25);
      send(request, { event: "text_delta", delta: "Hello " });
      await pause(25);
      send(request, { event: "text_delta", delta: "from stream." });
      await pause(40);
      send(request, { event: "complete", result: {
        message: { role: "assistant", content: "Hello from stream.",
          reasoning_content: "Think first." },
        usage: { input: 100, output: 12, cached: 75, reasoning: 2 }
      } });
    } catch (error) {
      try { send({ request_id: null }, { event: "error", message: String(error.message) }); }
      catch { process.stderr.write(String(error) + "\n"); }
    }
  });
