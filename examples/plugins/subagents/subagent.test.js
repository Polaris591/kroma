"use strict";
const {test, beforeEach, afterEach} = require("node:test");
const assert = require("node:assert/strict");
const {EventEmitter} = require("node:events");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const {dispatch, runBatch, LIMITS} = require("./subagent");

const tick = () => new Promise(resolve => setImmediate(resolve));

// These variables belong to the plugin worker environment. A parent that
// exports them (for example a subagent running this suite) must not change
// every test outcome.
const INHERITED = {};
for (const name of ["KROMA_SUBAGENT_DEPTH", "KROMA_PLUGIN_TOOL_TIMEOUT_SECS"]) {
  INHERITED[name] = process.env[name];
}
beforeEach(() => {
  for (const name of Object.keys(INHERITED)) delete process.env[name];
});
afterEach(() => {
  for (const [name, value] of Object.entries(INHERITED)) {
    if (value === undefined) delete process.env[name];
    else process.env[name] = value;
  }
});

function workspace() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "kroma-subagent-test-"));
}

// Deterministic child processes: tests decide when and how each one finishes.
function harness() {
  const calls = [];
  let running = 0;
  let peak = 0;
  function spawn(command, args, options) {
    const child = new EventEmitter();
    child.stdout = new EventEmitter();
    child.stderr = new EventEmitter();
    child.pid = 9000 + calls.length;
    child.exitCode = null;
    const stdin = {text: "", write(value) { this.text += value; }, end() {}, on() {}};
    child.stdin = stdin;
    const call = {command, args, options, child, stdin, finish: null};
    calls.push(call);
    running++;
    peak = Math.max(peak, running);
    child.kill = () => {
      if (child.exitCode !== null) return;
      child.exitCode = 1;
      running--;
      setImmediate(() => child.emit("close", null, "SIGTERM"));
    };
    call.finish = ({frames = [], code = 0, signal = null, stderr = ""} = {}) => {
      setImmediate(() => {
        if (child.exitCode === null) {
          child.exitCode = code;
          running--;
        }
        if (stderr) child.stderr.emit("data", Buffer.from(stderr));
        for (const frame of frames) {
          child.stdout.emit("data", Buffer.from(JSON.stringify(frame) + "\n"));
        }
        child.emit("close", code, signal);
      });
    };
    return child;
  }
  return {spawn, calls, peak: () => peak};
}

function batch(tasks, options = {}) {
  return {
    type: "tool_stream",
    name: "subagent",
    workdir: options.workdir,
    call_id: "call-1",
    arguments: {tasks},
    config: options.config ?? {},
  };
}

test("a task gets its own session, working directory and parsed result", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "do the thing"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    assert.equal(h.calls.length, 1);
    const call = h.calls[0];
    assert.deepEqual(call.args, ["--json"]);
    assert.equal(call.options.cwd, fs.realpathSync(dir));
    assert.equal(call.options.env.KROMA_WORKDIR, fs.realpathSync(dir));
    assert.equal(call.options.env.KROMA_SUBAGENT_DEPTH, "1");
    const session = call.options.env.KROMA_SESSION_FILE;
    assert.ok(session.includes(`${path.sep}runs${path.sep}`), session);
    assert.ok(session.endsWith(`${path.sep}0${path.sep}session.json`), session);
    assert.equal(call.stdin.text, "do the thing");
    call.finish({frames: [
      {type: "task_started"},
      {type: "model_finished", request: 1, tools: 0, input: 10, output: 5,
        cached: 2, reasoning_tokens: 1},
      {type: "tool_started", name: "shell"},
      {type: "task_finished", status: "completed"},
      {type: "result", text: "the answer"},
    ]});
    const response = await run;
    assert.equal(response.schema_version, 1);
    assert.equal(response.status, "completed");
    const [result] = response.results;
    assert.equal(result.index, 0);
    assert.equal(result.status, "completed");
    assert.equal(result.output, "the answer");
    assert.equal(result.output_truncated, false);
    assert.deepEqual(result.usage,
      {requests: 1, input: 10, output: 5, cached: 2, reasoning: 1});
    for (const file of ["state.json", "events.ndjson", "stderr.log", "answer.txt"]) {
      assert.ok(fs.existsSync(path.join(result.artifacts, file)), file);
    }
    const events = fs.readFileSync(path.join(result.artifacts, "events.ndjson"), "utf8");
    assert.match(events, /model_finished/);
    assert.match(events, /task_finished/);
    assert.doesNotMatch(events, /"text":"the answer"/);
    assert.ok(fs.existsSync(path.join(dir, "runs", response.run_id, "batch.json")));
    const request = JSON.parse(fs.readFileSync(
      path.join(dir, "runs", response.run_id, "request.json"), "utf8"));
    assert.equal(request.call_id, "call-1");
    assert.equal(request.tasks[0].task, "do the thing");
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("the queue caps concurrency and returns results in input order", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}, {task: "b"}, {task: "c"}],
      {workdir: dir, config: {max_parallel: 2}}), {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    assert.equal(h.calls.length, 2, "the third task waits for a free slot");
    assert.equal(h.peak(), 2);
    h.calls[1].finish({frames: [{type: "result", text: "B"}]});
    await tick();
    await tick();
    assert.equal(h.calls.length, 3, "the waiting task starts after a slot frees");
    h.calls[0].finish({frames: [{type: "result", text: "A"}]});
    h.calls[2].finish({frames: [{type: "result", text: "C"}]});
    const response = await run;
    assert.equal(response.status, "completed");
    assert.deepEqual(response.results.map(result => result.output), ["A", "B", "C"]);
    assert.ok(h.peak() <= 2);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("every task is validated before any child starts", async () => {
  const dir = workspace();
  const h = harness();
  try {
    await assert.rejects(runBatch(batch([{task: "fine"}, {task: "   "}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir}), {code: "INVALID_ARGUMENT"});
    await assert.rejects(runBatch(batch([{task: "fine"}, {task: "x", cwd: "missing"}],
      {workdir: dir}), {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir}), {code: "INVALID_ARGUMENT"});
    await assert.rejects(runBatch(batch([], {workdir: dir}), {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir}),
      {code: "INVALID_ARGUMENT"});
    await assert.rejects(runBatch(batch(Array.from({length: 9}, () => ({task: "x"})),
      {workdir: dir}), {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir}), {code: "INVALID_ARGUMENT"});
    assert.equal(h.calls.length, 0);
    assert.equal(fs.existsSync(path.join(dir, "runs")), false);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("an explicit cwd is the child's working directory", async () => {
  const dir = workspace();
  const other = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-subagent-cwd-"));
  const h = harness();
  try {
    const run = runBatch(batch([{task: "x", cwd: other}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    assert.equal(h.calls[0].options.cwd, fs.realpathSync(other));
    assert.equal(h.calls[0].options.env.KROMA_WORKDIR, fs.realpathSync(other));
    h.calls[0].finish({frames: [{type: "result", text: "ok"}]});
    const response = await run;
    assert.equal(response.status, "completed");
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
    fs.rmSync(other, {recursive: true, force: true});
  }
});

test("one failed task does not hide the others", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}, {task: "b"}],
      {workdir: dir, config: {max_parallel: 2}}), {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    h.calls[0].finish({code: 2, stderr: "boom"});
    h.calls[1].finish({frames: [{type: "result", text: "fine"}]});
    const response = await run;
    assert.equal(response.status, "partial");
    assert.equal(response.results[0].status, "failed");
    assert.equal(response.results[0].error.code, "CHILD_EXIT");
    assert.match(response.results[0].error.message, /code 2/);
    assert.equal(response.results[1].output, "fine");
    assert.match(fs.readFileSync(path.join(response.results[0].artifacts, "stderr.log"), "utf8"), /boom/);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("exit code zero without a final result is a protocol failure", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    h.calls[0].finish({frames: [{type: "task_started"}]});
    const response = await run;
    assert.equal(response.status, "failed");
    assert.equal(response.results[0].error.code, "CHILD_PROTOCOL");
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("a protocol fault aborts the batch and names the evidence", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}, {task: "b"}],
      {workdir: dir, config: {max_parallel: 2}}),
    {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    const settled = run.then(() => null, error => error);
    h.calls[0].finish({frames: [
      {type: "result", text: "first"},
      {type: "result", text: "second"},
    ]});
    await tick();
    await tick();
    const error = await settled;
    assert.ok(error, "the batch must reject instead of looking successful");
    assert.equal(error.code, "CHILD_PROTOCOL");
    assert.equal(error.fatal, true);
    assert.match(error.message, /evidence under/);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});
test("non-JSON child output is a protocol fault", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    setImmediate(() => {
      h.calls[0].child.stdout.emit("data", Buffer.from("not json\n"));
    });
    await assert.rejects(run, {code: "CHILD_PROTOCOL"});
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("answers are clipped on a character boundary", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir, config: {max_output_chars: 1000}}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    h.calls[0].finish({frames: [{type: "result", text: "🙂".repeat(1001)}]});
    const response = await run;
    assert.equal(response.results[0].output_truncated, true);
    assert.equal(Array.from(response.results[0].output).length, 1000);
    assert.equal(response.results[0].output, "🙂".repeat(1000));
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("usage counters stay unknown per field, never as zero", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    h.calls[0].finish({frames: [
      {type: "model_finished", request: 1, input: 7, output: 3, cached: null, reasoning_tokens: null},
      {type: "model_finished", request: 2, input: 1, output: 2},
      {type: "result", text: "ok"},
    ]});
    const response = await run;
    assert.deepEqual(response.results[0].usage,
      {requests: 2, input: 8, output: 5, cached: null, reasoning: null});
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("a subagent cannot delegate further", async () => {
  const dir = workspace();
  const h = harness();
  const previous = process.env.KROMA_SUBAGENT_DEPTH;
  process.env.KROMA_SUBAGENT_DEPTH = "1";
  try {
    await assert.rejects(runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir}), {code: "DEPTH_LIMIT"});
    assert.equal(h.calls.length, 0);
  } finally {
    if (previous === undefined) delete process.env.KROMA_SUBAGENT_DEPTH;
    else process.env.KROMA_SUBAGENT_DEPTH = previous;
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("unknown tools and requests are rejected", async () => {
  const dir = workspace();
  try {
    await assert.rejects(dispatch({type: "tool", name: "nope"}, {dataDir: dir}),
      {code: "UNKNOWN_TOOL"});
    await assert.rejects(dispatch({type: "tool", name: "subagent_run",
      arguments: {task: "x"}, workdir: dir}, {dataDir: dir}), {code: "UNKNOWN_TOOL"});
    await assert.rejects(dispatch({type: "command", name: "/x"}, {dataDir: dir}),
      {code: "INVALID_REQUEST"});
    const ping = await dispatch({type: "ping"});
    assert.deepEqual(ping, {content: "subagent plugin ready", is_error: false});
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("progress is best-effort and never changes the outcome", async () => {
  const dir = workspace();
  const h = harness();
  const seen = [];
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir, onProgress: text => {
        seen.push(text);
        throw new Error("listener failed");
      }});
    h.calls[0].finish({frames: [{type: "result", text: "ok"}]});
    const response = await run;
    assert.equal(response.status, "completed");
    assert.ok(seen.length > 0);
    assert.ok(seen[0].startsWith("▷ [1/1] started"), seen[0]);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("limits stay small and documented", () => {
  assert.equal(LIMITS.tasks, 8);
  assert.equal(LIMITS.maxParallel, 3);
  assert.equal(LIMITS.outputChars, 12000);
  assert.equal(LIMITS.frameBytes, 2 * 1024 * 1024);
  // The child CLI inlines piped stdin only up to 16 KiB.
  assert.equal(LIMITS.taskBytes, 16 * 1024);
});

test("each task gets a distinct session file and is never retried", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}, {task: "b"}],
      {workdir: dir, config: {max_parallel: 2}}), {spawn: h.spawn, dataDir: dir});
    const sessions = h.calls.map(call => call.options.env.KROMA_SESSION_FILE);
    assert.equal(new Set(sessions).size, 2);
    h.calls[0].finish({code: 1});
    h.calls[1].finish({code: 1});
    await run;
    assert.equal(h.calls.length, 2, "a failed task must not be retried automatically");
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("start failures and signal exits are classified", async () => {
  const dir = workspace();
  const first = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: first.spawn, terminate: child => child.kill(), dataDir: dir});
    setImmediate(() => first.calls[0].child.emit("error", new Error("ENOENT")));
    const response = await run;
    assert.equal(response.results[0].error.code, "CHILD_START");
    assert.match(response.results[0].error.message, /ENOENT/);

    const second = harness();
    const killed = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: second.spawn, terminate: child => child.kill(), dataDir: dir});
    second.calls[0].finish({signal: "SIGKILL", code: null});
    const signalled = await killed;
    assert.equal(signalled.results[0].error.code, "CHILD_SIGNAL");
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("chunked stdout is reassembled across UTF-8 and line boundaries", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    const payload = Buffer.from(JSON.stringify({type: "result", text: "完成 🙂 ok"}) + "\n");
    const emoji = payload.indexOf(Buffer.from("🙂"));
    setImmediate(() => {
      const stdout = h.calls[0].child.stdout;
      stdout.emit("data", payload.subarray(0, emoji + 2));
      stdout.emit("data", payload.subarray(emoji + 2, emoji + 3));
      stdout.emit("data", payload.subarray(emoji + 3, 40));
      stdout.emit("data", payload.subarray(40));
    });
    h.calls[0].finish({});
    const response = await run;
    assert.equal(response.results[0].status, "completed");
    assert.equal(response.results[0].output, "完成 🙂 ok");
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("an oversized line is rejected by bytes, not characters", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    // One complete line of 700k three-byte characters exceeds 2 MiB of UTF-8.
    const oversized = "中".repeat(700000) + "\n";
    setImmediate(() => {
      h.calls[0].child.stdout.emit("data", Buffer.from(oversized, "utf8"));
    });
    await assert.rejects(run, {code: "CHILD_PROTOCOL"});
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("a result without text is a protocol fault", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    h.calls[0].finish({frames: [{type: "result", text: null}]});
    await assert.rejects(run, {code: "CHILD_PROTOCOL"});
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("a task is locatable before its child starts", async () => {
  const dir = workspace();
  const h = harness();
  const observed = [];
  const spawn = (command, args, options) => {
    const statePath = path.join(path.dirname(options.env.KROMA_SESSION_FILE), "state.json");
    observed.push(fs.readFileSync(statePath, "utf8"));
    return h.spawn(command, args, options);
  };
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn, terminate: child => child.kill(), dataDir: dir});
    h.calls[0].finish({frames: [{type: "result", text: "ok"}]});
    await run;
    assert.match(observed[0], /"status":"running"/);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("large stderr is drained and bounded", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    setImmediate(() => {
      h.calls[0].child.stderr.emit("data", Buffer.alloc(300 * 1024, 46));
    });
    h.calls[0].finish({frames: [{type: "result", text: "ok"}]});
    const response = await run;
    assert.equal(response.status, "completed");
    const log = fs.statSync(path.join(response.results[0].artifacts, "stderr.log"));
    assert.ok(log.size <= 256 * 1024, `stderr.log is ${log.size} bytes`);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("the whole response stays bounded even for large outputs", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const tasks = Array.from({length: 8}, (_, index) => ({task: "t" + index}));
    const run = runBatch(batch(tasks, {workdir: dir,
      config: {max_parallel: 8, max_output_chars: 100000}}),
    {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    h.calls.forEach(call => call.finish({frames: [{type: "result", text: "x".repeat(100000)}]}));
    const response = await run;
    assert.equal(response.status, "completed");
    assert.ok(Buffer.byteLength(JSON.stringify(response), "utf8") <= 512 * 1024);
    assert.ok(response.results.every(result => result.output_truncated));
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("the tool result is an error exactly when a task fails", async () => {
  const dir = workspace();
  try {
    const passing = harness();
    const ok = dispatch({type: "tool_stream", name: "subagent",
      arguments: {tasks: [{task: "a"}]}, workdir: dir, config: {}},
    {spawn: passing.spawn, terminate: child => child.kill(), dataDir: dir});
    passing.calls[0].finish({frames: [{type: "result", text: "fine"}]});
    const okOutcome = await ok;
    assert.equal(okOutcome.is_error, false);
    assert.equal(JSON.parse(okOutcome.content).status, "completed");

    const failing = harness();
    const partial = dispatch({type: "tool_stream", name: "subagent",
      arguments: {tasks: [{task: "a"}, {task: "b"}]}, workdir: dir,
      config: {max_parallel: 2}},
    {spawn: failing.spawn, terminate: child => child.kill(), dataDir: dir});
    failing.calls[0].finish({code: 1});
    failing.calls[1].finish({frames: [{type: "result", text: "fine"}]});
    const partialOutcome = await partial;
    assert.equal(partialOutcome.is_error, true);
    const response = JSON.parse(partialOutcome.content);
    assert.equal(response.status, "partial");
    assert.equal(response.results[0].status, "failed");
    assert.equal(response.results[1].output, "fine");
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("an extended-length workdir is normalized before use", async () => {
  if (process.platform !== "win32") return;
  const dir = workspace();
  const bs = String.fromCharCode(92);
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: bs + bs + "?" + bs + dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    assert.equal(h.calls[0].options.cwd, fs.realpathSync(dir));
    h.calls[0].finish({frames: [{type: "result", text: "ok"}]});
    const response = await run;
    assert.equal(response.status, "completed");
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("progress reports running count, elapsed, limit and rolling tokens", async () => {
  const dir = workspace();
  const h = harness();
  const seen = [];
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir,
        heartbeatMs: 10, onProgress: text => seen.push(text)});
    await new Promise(resolve => setTimeout(resolve, 60));
    h.calls[0].finish({frames: [
      {type: "model_finished", input: 5200, output: 300, cached: 0, reasoning_tokens: 0},
      {type: "result", text: "ok"},
    ]});
    const response = await run;
    assert.equal(response.status, "completed");
    const beat = seen.find(line => line.includes("running"));
    assert.ok(beat, JSON.stringify(seen));
    assert.ok(beat.startsWith("0/1 done · 1 running · "), beat);
    assert.ok(beat.endsWith("s/600s · in 0 / out 0"), beat);
    const done = seen.find(line => line.startsWith("✓ [1/1] "));
    assert.ok(done.startsWith("✓ [1/1] "), done);
    assert.ok(done.endsWith("s · in 5.2k / out 300"), done);
    assert.ok(seen.at(-1).startsWith("✓ 1/1 done · "), seen.at(-1));
    assert.ok(seen.at(-1).endsWith("s · in 5.2k / out 300"), seen.at(-1));
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("the advertised limits match the manifest", () => {
  const manifest = fs.readFileSync(path.join(__dirname, "plugin.toml"), "utf8");
  assert.ok(manifest.includes("timeout_secs = " + LIMITS.timeoutSecs), manifest);
  assert.ok(manifest.includes("max_parallel = " + LIMITS.maxParallel), manifest);
  assert.ok(manifest.includes("max_output_chars = " + LIMITS.outputChars), manifest);
});

test("a JSON line that is not an object is a protocol fault, not a crash", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    const settled = run.then(() => null, error => error);
    assert.doesNotThrow(() => {
      h.calls[0].child.stdout.emit("data", Buffer.from("null\n"));
    });
    const error = await settled;
    assert.equal(error.code, "CHILD_PROTOCOL");
    assert.equal(error.fatal, true);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("evidence reaches disk while the task is still running", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    const taskDir = path.dirname(h.calls[0].options.env.KROMA_SESSION_FILE);
    h.calls[0].child.stdout.emit("data", Buffer.from(
      JSON.stringify({type: "model_started", request: 1}) + "\n"));
    h.calls[0].child.stderr.emit("data", Buffer.from("[model] provider rejected the key\n"));
    assert.match(fs.readFileSync(path.join(taskDir, "events.ndjson"), "utf8"), /model_started/);
    assert.match(fs.readFileSync(path.join(taskDir, "stderr.log"), "utf8"), /rejected the key/);
    h.calls[0].finish({frames: [{type: "result", text: "ok"}]});
    const response = await run;
    assert.equal(response.status, "completed");
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("evidence keeps provider failures and never the answer text", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    h.calls[0].finish({frames: [
      {type: "model_started", request: 1},
      {type: "model_failed"},
      {type: "text_delta", text: "private draft"},
      {type: "result", text: "the answer"},
    ]});
    const response = await run;
    const events = fs.readFileSync(
      path.join(response.results[0].artifacts, "events.ndjson"), "utf8");
    assert.match(events, /model_failed/);
    assert.doesNotMatch(events, /private draft/);
    assert.doesNotMatch(events, /the answer/);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("a failed exit names the last stderr line", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    h.calls[0].finish({code: 1,
      stderr: "warning: noisy startup notes\nError: KROMA_MODEL is required\n"});
    const response = await run;
    assert.equal(response.results[0].error.code, "CHILD_EXIT");
    assert.match(response.results[0].error.message, /KROMA_MODEL is required/);
    assert.doesNotMatch(response.results[0].error.message, /noisy/);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});
test("a task the child cannot inline is refused before spawning", async () => {
  const dir = workspace();
  const h = harness();
  try {
    await assert.rejects(runBatch(batch([{task: "x".repeat(LIMITS.taskBytes + 1)}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir}),
    {code: "INVALID_ARGUMENT"});
    assert.equal(h.calls.length, 0, "nothing may start for a rejected batch");
    const accepted = runBatch(batch([{task: "x".repeat(LIMITS.taskBytes)}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    assert.equal(h.calls.length, 1);
    assert.equal(h.calls[0].stdin.text.length, LIMITS.taskBytes);
    h.calls[0].finish({frames: [{type: "result", text: "ok"}]});
    assert.equal((await accepted).status, "completed");
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("cancellation asks the child to stop itself before escalating", async () => {
  if (process.platform === "win32") return;
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, dataDir: dir, killGraceMs: 10});
    const settled = run.then(() => null, error => error);
    const child = h.calls[0].child;
    const signals = [];
    child.kill = signal => {
      signals.push(signal);
      if (signal === "SIGKILL") {
        child.exitCode = 1;
        setImmediate(() => child.emit("close", null, "SIGKILL"));
      }
    };
    child.stdout.emit("data", Buffer.from("not json\n"));
    const error = await settled;
    assert.equal(error.code, "CHILD_PROTOCOL");
    assert.deepEqual(signals, ["SIGINT", "SIGKILL"]);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("a malformed nesting depth is refused, not read as top level", async () => {
  const dir = workspace();
  const h = harness();
  try {
    process.env.KROMA_SUBAGENT_DEPTH = "abc";
    await assert.rejects(runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir}), {code: "CONFIG_ERROR"});
    assert.equal(h.calls.length, 0);
  } finally {
    delete process.env.KROMA_SUBAGENT_DEPTH;
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("batch evidence records the tasks a fatal fault never started", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const run = runBatch(batch([{task: "a"}, {task: "b"}],
      {workdir: dir, config: {max_parallel: 1}}),
    {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir});
    const settled = run.then(() => null, error => error);
    h.calls[0].child.stdout.emit("data", Buffer.from("not json\n"));
    const error = await settled;
    assert.equal(error.fatal, true);
    const runs = fs.readdirSync(path.join(dir, "runs"));
    assert.equal(runs.length, 1);
    const written = JSON.parse(
      fs.readFileSync(path.join(dir, "runs", runs[0], "batch.json"), "utf8"));
    assert.equal(written.results[1].status, "not_started");
    assert.equal(written.results[1].error.code, "BATCH_ABORTED");
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("progress names the deadline the host will enforce", async () => {
  const dir = workspace();
  const h = harness();
  const seen = [];
  const previous = process.env.KROMA_PLUGIN_TOOL_TIMEOUT_SECS;
  process.env.KROMA_PLUGIN_TOOL_TIMEOUT_SECS = "120";
  try {
    const run = runBatch(batch([{task: "a"}], {workdir: dir}),
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir,
        heartbeatMs: 10, onProgress: text => seen.push(text)});
    await new Promise(resolve => setTimeout(resolve, 40));
    h.calls[0].finish({frames: [{type: "result", text: "ok"}]});
    await run;
    const beat = seen.find(line => line.includes("running"));
    assert.ok(beat, JSON.stringify(seen));
    assert.ok(beat.endsWith("/120s · in 0 / out 0"), beat);
  } finally {
    if (previous === undefined) delete process.env.KROMA_PLUGIN_TOOL_TIMEOUT_SECS;
    else process.env.KROMA_PLUGIN_TOOL_TIMEOUT_SECS = previous;
    fs.rmSync(dir, {recursive: true, force: true});
  }
});
