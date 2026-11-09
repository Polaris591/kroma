"use strict";
const {test, beforeEach, afterEach} = require("node:test");
const assert = require("node:assert/strict");
const {EventEmitter} = require("node:events");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const {dispatch, startBatch, LIMITS, pruneRuns} = require("./subagent");

const tick = () => new Promise(resolve => setImmediate(resolve));

// These variables belong to the plugin worker environment. A parent that
// exports them (for example a subagent running this suite) must not change
// every test outcome.
const INHERITED = {};
for (const name of ["KROMA_SUBAGENT_DEPTH", "KROMA_PLUGIN_TOOL_TIMEOUT_SECS", "KROMA_MODEL"]) {
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

// Windows may still hold a handle briefly after the last write.
function cleanup(dir) {
  try { fs.rmSync(dir, {recursive: true, force: true, maxRetries: 20, retryDelay: 25}); } catch {}
}

function read(dir, file) {
  return JSON.parse(fs.readFileSync(path.join(dir, file), "utf8"));
}

async function start(h, request, extra = {}) {
  const dir = extra.dataDir ?? request.workdir;
  return startBatch(request, {spawn: h.spawn, terminate: child => child.kill(),
    dataDir: dir, killGraceMs: 20, ...extra});
}

test("evidence retention prunes aged runs but preserves active and resumed work", () => {
  const root = workspace();
  const runs = path.join(root, "runs");
  fs.mkdirSync(runs);
  const old = Date.now() - 40 * 24 * 60 * 60 * 1000;
  const createRun = (id, status, stale) => {
    const dir = path.join(runs, id);
    fs.mkdirSync(dir);
    if (status) fs.writeFileSync(path.join(dir, "batch.json"), JSON.stringify({status}));
    fs.writeFileSync(path.join(dir, "answer.txt"), "keep until retention period");
    if (stale) fs.utimesSync(dir, old / 1000, old / 1000);
    return dir;
  };
  try {
    const aged = createRun("aged", "completed", true);
    const protectedRun = createRun("protected", "failed", true);
    const interrupted = createRun("interrupted", null, true);
    const recent = createRun("recent", "partial", false);
    pruneRuns(root, new Set(["protected"]));
    assert.equal(fs.existsSync(aged), false);
    assert.equal(fs.existsSync(protectedRun), true);
    // An interrupted archive has no summary; only age retires it.
    assert.equal(fs.existsSync(interrupted), false);
    assert.equal(fs.existsSync(recent), true);
  } finally { fs.rmSync(root, {recursive: true, force: true}); }
});

test("the tool returns before any child finishes and the answer lands on disk", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const started = await start(h, batch([{task: "do the thing", label: "thing"}], {workdir: dir}));
    assert.equal(h.calls.length, 1);
    assert.match(started.runId, /^\d{14}-[0-9a-f]{8}$/);
    assert.deepEqual(started.tasks.map(task => task.index), [0]);
    // Nothing has finished yet: the caller already has its acknowledgement.
    assert.equal(read(started.tasks[0].artifacts, "state.json").status, "running");
    const call = h.calls[0];
    assert.deepEqual(call.args, ["--json"]);
    assert.equal(call.options.cwd, dir);
    assert.equal(call.stdin.text, "do the thing");
    assert.equal(call.options.env.KROMA_SESSION_FILE,
      path.join(started.tasks[0].artifacts, "session.json"));
    assert.equal(call.options.env.KROMA_WORKDIR, dir);
    assert.equal(call.options.env.KROMA_SUBAGENT_DEPTH, "1");
    const policy = read(started.tasks[0].artifacts, "policy.json");
    assert.deepEqual(policy.hostDeniedTools, ["subagent", "memory_remember", "memory_forget"]);
    call.finish({frames: [{type: "result", text: "the answer"}]});
    const summary = await started.done;
    assert.equal(summary.status, "completed");
    assert.equal(fs.readFileSync(path.join(started.tasks[0].artifacts, "answer.txt"), "utf8"),
      "the answer");
    const state = read(started.tasks[0].artifacts, "state.json");
    assert.equal(state.status, "completed");
    assert.equal(state.label, "thing");
  } finally { cleanup(dir); }
});

test("dispatch waits for every child and returns its answer", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const progress = [];
    const pending = dispatch(
      {...batch([{task: "slow work", label: "slow"}], {workdir: dir}), data_dir: dir},
      {spawn: h.spawn, terminate: child => child.kill(), dataDir: dir, killGraceMs: 20,
        onProgress: line => progress.push(line)});
    await tick();
    assert.equal(h.calls.length, 1);
    // The answer is only produced once the child really finishes.
    h.calls[0].finish({frames: [{type: "result", text: "later"}]});
    const response = await pending;
    assert.equal(response.is_error, false);
    assert.match(response.content, /later/);
    assert.match(response.content, /untrusted data/);
    assert.ok(progress.some(line => /slow/.test(line)), progress.join("\n"));
  } finally { cleanup(dir); }
});

test("a command request is no longer part of this plugin", async () => {
  await assert.rejects(dispatch({type: "command", name: "/subagents", user_input: "list"}),
    /only tool requests are supported/);
});

test("a bad task rejects the whole call before any child starts", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const bad = [
      [[], /nonempty array/],
      [new Array(9).fill({task: "x"}), /at most 8 tasks/],
      [[{task: "ok"}, {task: "bad", profile: "explore"}], /not supported/],
      [[{task: "   "}], /nonempty string/],
      [[{task: "x".repeat(LIMITS.taskBytes + 1)}], /exceeds/],
      [[{task: "x", cwd: path.join(dir, "missing")}], /not an existing directory/],
      [[{task: "x", resume: "../../etc/passwd"}], /task token/],
      [[{task: "x", cwd: 7}], /nonempty string/],
    ];
    for (const [tasks, pattern] of bad) {
      await assert.rejects(start(h, batch(tasks, {workdir: dir})), pattern);
    }
    assert.equal(h.calls.length, 0);
  } finally { cleanup(dir); }
});

test("delegation depth and settings are validated before spawning", async () => {
  const dir = workspace();
  const h = harness();
  try {
    process.env.KROMA_SUBAGENT_DEPTH = "1";
    await assert.rejects(start(h, batch([{task: "x"}], {workdir: dir})), /cannot delegate further/);
    process.env.KROMA_SUBAGENT_DEPTH = "deep";
    await assert.rejects(start(h, batch([{task: "x"}], {workdir: dir})), /must be an integer/);
    delete process.env.KROMA_SUBAGENT_DEPTH;
    await assert.rejects(start(h, batch([{task: "x"}], {workdir: dir, config: {max_parallel: 9}})),
      /max_parallel must be an integer/);
    await assert.rejects(start(h, batch([{task: "x"}], {workdir: dir, config: {task_timeout_secs: "soon"}})),
      /task_timeout_secs must be an integer/);
    assert.equal(h.calls.length, 0);
  } finally { cleanup(dir); }
});

test("a batch never runs more children than max_parallel", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const started = await start(h,
      batch([{task: "a"}, {task: "b"}, {task: "c"}], {workdir: dir, config: {max_parallel: 2}}));
    assert.equal(h.calls.length, 2, "the third task waits for a free slot");
    assert.equal(h.peak(), 2);
    h.calls[0].finish({frames: [{type: "result", text: "a"}]});
    await tick();
    assert.equal(h.calls.length, 3);
    h.calls[1].finish({frames: [{type: "result", text: "b"}]});
    h.calls[2].finish({frames: [{type: "result", text: "c"}]});
    const summary = await started.done;
    assert.equal(summary.status, "completed");
    assert.deepEqual(summary.results.map(result => result.index), [0, 1, 2]);
  } finally { cleanup(dir); }
});

test("one failed child does not spoil its siblings and keeps its error code", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const started = await start(h, batch([{task: "a"}, {task: "b"}], {workdir: dir}));
    h.calls[0].finish({code: 4, stderr: "invalid api key\n"});
    h.calls[1].finish({frames: [{type: "result", text: "fine"}]});
    const summary = await started.done;
    assert.equal(summary.status, "partial");
    assert.deepEqual(summary.results.map(result => result.code), ["AUTH_FAILED", "COMPLETED"]);
    const failed = read(started.tasks[0].artifacts, "state.json");
    assert.equal(failed.status, "failed");
    assert.match(failed.code, /AUTH_FAILED/);
  } finally { cleanup(dir); }
});

test("exit without a final result and an unusable stdout line are failures, not success", async () => {
  const dir = workspace();
  try {
    const h = harness();
    const started = await start(h, batch([{task: "a"}], {workdir: dir}));
    h.calls[0].finish({code: 0});
    const summary = await started.done;
    assert.equal(summary.results[0].code, "CHILD_PROTOCOL");
    assert.match(read(started.tasks[0].artifacts, "state.json").code, /CHILD_PROTOCOL/);
  } finally { cleanup(dir); }
  const dir2 = workspace();
  try {
    const h = harness();
    const started = await start(h, batch([{task: "a"}], {workdir: dir2}));
    const child = h.calls[0].child;
    setImmediate(() => {
      child.stdout.emit("data", Buffer.from("this is not json\n"));
      setImmediate(() => {
        child.exitCode = 0;
        child.emit("close", 0, null);
      });
    });
    const summary = await started.done;
    assert.equal(summary.status, "failed");
    assert.equal(summary.results[0].code, "CHILD_PROTOCOL");
  } finally { cleanup(dir2); }
});

test("a duplicate result and an oversized line abort the run", async () => {
  const dir = workspace();
  try {
    const h = harness();
    const started = await start(h, batch([{task: "a"}, {task: "b"}], {workdir: dir}));
    const first = h.calls[0].child;
    setImmediate(() => first.stdout.emit("data",
      Buffer.from(`${JSON.stringify({type: "result", text: "one"})}\n${JSON.stringify({type: "result", text: "two"})}\n`)));
    const summary = await started.done;
    assert.equal(summary.status, "failed");
    assert.equal(summary.results[0].code, "CHILD_PROTOCOL");
    // The sibling is stopped by the same fault instead of running unobserved.
    assert.equal(summary.results[1].code, "BATCH_ABORTED");
  } finally { cleanup(dir); }
  const dir2 = workspace();
  try {
    const h = harness();
    const started = await start(h, batch([{task: "a"}], {workdir: dir2}));
    const child = h.calls[0].child;
    setImmediate(() => child.stdout.emit("data", Buffer.from("x".repeat(LIMITS.frameBytes + 2) + "\n")));
    const summary = await started.done;
    assert.equal(summary.results[0].code, "CHILD_PROTOCOL");
  } finally { cleanup(dir2); }
});

test("a startup failure names the last stderr line", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const started = await start(h, batch([{task: "a"}], {workdir: dir}));
    h.calls[0].finish({code: 5, stderr: "warming up\nconnection refused\n"});
    const summary = await started.done;
    assert.equal(summary.results[0].code, "MODEL_REQUEST_FAILED");
    assert.match(read(started.tasks[0].artifacts, "state.json").code, /MODEL_REQUEST_FAILED/);
    assert.match(fs.readFileSync(path.join(started.tasks[0].artifacts, "stderr.log"), "utf8"),
      /connection refused/);
  } finally { cleanup(dir); }
});

test("a child that runs past its deadline is killed and keeps what it streamed", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const started = await start(h, batch([{task: "a"}], {workdir: dir}), {taskTimeoutMs: 15});
    const child = h.calls[0].child;
    child.stdout.emit("data", Buffer.from(JSON.stringify({type: "text_delta", text: "half an answer"}) + "\n"));
    const summary = await started.done;
    assert.equal(summary.results[0].code, "CHILD_TIMEOUT");
    const artifacts = started.tasks[0].artifacts;
    assert.equal(fs.readFileSync(path.join(artifacts, "partial.txt"), "utf8"), "half an answer");
    const state = read(artifacts, "state.json");
    assert.equal(state.status, "failed");
    assert.equal(state.code, "CHILD_TIMEOUT");
  } finally { cleanup(dir); }
});


function resumeFixture(dir, token, state) {
  const folder = path.join(dir, "runs", token.split("/")[0], token.split("/")[1]);
  fs.mkdirSync(folder, {recursive: true});
  fs.writeFileSync(path.join(folder, "state.json"), JSON.stringify(state));
  fs.writeFileSync(path.join(folder, "session.json"), JSON.stringify({messages: ["earlier"]}));
  return folder;
}

test("a resume copies the saved session into a new run and keeps the source intact", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const source = resumeFixture(dir, "src001/0", {status: "completed", cwd: dir});
    const started = await start(h, batch([{task: "keep going", resume: "src001/0"}], {workdir: dir}));
    const artifacts = started.tasks[0].artifacts;
    assert.equal(fs.readFileSync(path.join(artifacts, "session.json"), "utf8"),
      JSON.stringify({messages: ["earlier"]}));
    assert.equal(h.calls[0].options.cwd, dir);
    assert.equal(h.calls[0].options.env.KROMA_SESSION_FILE, path.join(artifacts, "session.json"));
    assert.equal(h.calls[0].stdin.text, "keep going");
    assert.equal(read(artifacts, "state.json").resumed_from, "src001/0");
    assert.equal(fs.existsSync(path.join(source, "answer.txt")), false,
      "the source run stays untouched");
    h.calls[0].finish({frames: [{type: "result", text: "continued"}]});
    await started.done;
  } finally { cleanup(dir); }
});

test("resume tokens are validated before any child starts", async () => {
  const dir = workspace();
  const h = harness();
  try {
    await assert.rejects(start(h, batch([{task: "x", resume: "gone/0"}], {workdir: dir})),
      /RESUME_MISSING|no evidence|pruned/);
    resumeFixture(dir, "busy/0", {status: "running", cwd: dir, child_pid: process.pid});
    await assert.rejects(start(h, batch([{task: "x", resume: "busy/0"}], {workdir: dir})),
      /child process that has not exited/);
    fs.mkdirSync(path.join(dir, "elsewhere"));
    resumeFixture(dir, "older/0", {status: "completed", cwd: path.join(dir, "elsewhere")});
    await assert.rejects(start(h, batch([{task: "x", resume: "older/0", cwd: dir}], {workdir: dir})),
      /must match the resumed task directory/);
    assert.equal(h.calls.length, 0);
  } finally { cleanup(dir); }
});

test("the label defaults to the first task line and is bounded", async () => {
  const dir = workspace();
  const h = harness();
  try {
    const started = await start(h, batch([
      {task: "first line\nsecond line"},
      {task: "explicit", label: "my label"},
    ], {workdir: dir}));
    assert.deepEqual(started.tasks.map(task => task.label), ["first line", "my label"]);
    await assert.rejects(start(h, batch([{task: "x", label: "bad\nlabel"}], {workdir: dir})),
      /printable characters/);
    h.calls.forEach(call => call.finish({frames: [{type: "result", text: "ok"}]}));
    await started.done;
  } finally { cleanup(dir); }
});
