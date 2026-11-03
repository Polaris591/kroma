"use strict";
const {test} = require("node:test");
const assert = require("node:assert/strict");
const {createHandler} = require("./main");

function sink() {
  const frames = [];
  return {frames, write: frame => frames.push(frame)};
}

function line(overrides = {}) {
  return JSON.stringify({protocol_version: 1, request_id: 7, type: "tool_stream",
    name: "subagent", arguments: {tasks: [{task: "x"}]}, ...overrides});
}

test("streaming progress and completion echo the request id", async () => {
  const {frames, write} = sink();
  const handle = createHandler({
    run: async (request, options) => {
      options.onProgress("▷ [1/1] started");
      return {content: "{\"status\":\"completed\"}", is_error: false};
    },
    write,
  });
  await handle(line());
  assert.deepEqual(frames[0], {protocol_version: 1, request_id: 7,
    event: "output_delta", stream: "stdout", delta: "▷ [1/1] started\n"});
  assert.deepEqual(frames.at(-1), {protocol_version: 1, request_id: 7, event: "complete",
    result: {content: "{\"status\":\"completed\"}", is_error: false}});
});

test("a partial batch is marked as an error at the protocol layer", async () => {
  const streaming = sink();
  await createHandler({
    run: async () => ({content: "{\"status\":\"partial\"}", is_error: true}),
    write: streaming.write,
  })(line());
  assert.deepEqual(streaming.frames.at(-1).result,
    {content: "{\"status\":\"partial\"}", is_error: true});

  const plain = sink();
  await createHandler({
    run: async () => ({content: "failed", is_error: true}),
    write: plain.write,
  })(line({type: "tool"}));
  assert.deepEqual(plain.frames.at(-1),
    {protocol_version: 1, request_id: 7, content: "failed", is_error: true});
});

test("a task failure stays a reusable completion, a fatal one kills the worker", async () => {
  const ordinary = sink();
  await createHandler({
    run: async () => { throw Object.assign(new Error("child exited with code 2"), {code: "CHILD_EXIT"}); },
    write: ordinary.write,
  })(line());
  assert.equal(ordinary.frames.at(-1).event, "complete");
  assert.deepEqual(ordinary.frames.at(-1).result,
    {content: "child exited with code 2", is_error: true});

  const fatal = sink();
  await createHandler({
    run: async () => { throw Object.assign(new Error("corrupt stream"), {fatal: true}); },
    write: fatal.write,
  })(line());
  assert.deepEqual(fatal.frames.at(-1),
    {protocol_version: 1, request_id: 7, event: "error", message: "corrupt stream"});
});

test("plain tool requests and ping keep the single-response shape", async () => {
  const {frames, write} = sink();
  const handle = createHandler({run: async () => ({content: "result", is_error: false}), write});
  await handle(line({type: "tool"}));
  assert.deepEqual(frames[0], {protocol_version: 1, request_id: 7, content: "result"});

  const ping = sink();
  // The real dispatcher answers ping without spawning any child process.
  await createHandler({write: ping.write})(`{"type":"ping","name":"ping","request_id":9}`);
  assert.deepEqual(ping.frames[0], {protocol_version: 1, request_id: 9,
    content: "subagent plugin ready"});
});

test("a malformed line is reported without crashing the worker", async () => {
  const {frames, write} = sink();
  const handle = createHandler({run: async () => ({content: "unused", is_error: false}), write});
  await handle("{not json");
  assert.equal(frames[0].is_error, true);
  assert.match(frames[0].content, /invalid JSON request/);
  await handle(line({type: "tool"}));
  assert.equal(frames.at(-1).content, "unused");
});

test("concurrent lines are serialized and answered in order", async () => {
  const {frames, write} = sink();
  let active = 0;
  let peak = 0;
  const order = [];
  const handle = createHandler({
    run: async request => {
      active++;
      peak = Math.max(peak, active);
      order.push(request.arguments.tasks[0].task);
      await new Promise(resolve => setTimeout(resolve, 5));
      active--;
      return {content: "ok", is_error: false};
    },
    write,
  });
  await Promise.all([handle(line()), handle(line())]);
  assert.equal(peak, 1, "the worker must not interleave two plugin requests");
  assert.deepEqual(order, ["x", "x"]);
  assert.equal(frames.length, 2);
});
