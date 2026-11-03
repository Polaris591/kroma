"use strict";
const {spawn} = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const {StringDecoder} = require("node:string_decoder");

// One executor, one deadline. The host owns time (tool timeout_secs), this
// module owns state: validation, bounded queues, child processes and evidence.
const LIMITS = Object.freeze({
  tasks: 8,
  // `kroma --json` inlines piped stdin only up to 16 KiB and otherwise hands
  // the model a file pointer, so a longer task would arrive as a path instead
  // of an instruction. Refuse it instead of degrading silently.
  taskBytes: 16 * 1024,
  frameBytes: 2 * 1024 * 1024,
  outputChars: 12000,
  maxParallel: 3,
  responseBytes: 512 * 1024,
  stderrBytes: 256 * 1024,
  eventBytes: 2 * 1024 * 1024,
  evidenceChars: 2000,
  timeoutSecs: 600,
});

// Time a cancelled child Kroma gets to run its own cancellation path before
// the plugin escalates to SIGKILL.
const DEFAULT_KILL_GRACE_MS = 2000;

class SubagentError extends Error {
  // fatal errors abort the whole batch and are reported as a stream error, so
  // the host discards the worker instead of reusing a possibly corrupt one.
  constructor(code, message, fatal = false) {
    super(message);
    this.code = code;
    this.fatal = fatal;
  }
}

function clip(text, limit) {
  const chars = Array.from(text);
  return chars.length <= limit
    ? {text, truncated: false}
    : {text: chars.slice(0, limit).join(""), truncated: true};
}

function integer(value, name, min, max, fallback) {
  if (value === undefined || value === null || value === "") return fallback;
  const parsed = typeof value === "string" ? Number(value.trim()) : value;
  if (!Number.isInteger(parsed) || parsed < min || parsed > max) {
    throw new SubagentError("CONFIG_ERROR", `${name} must be an integer in ${min}..${max}`);
  }
  return parsed;
}

// Windows canonicalization (and therefore the host) can hand out extended-length
// paths; Node's path/realpath helpers mishandle the prefix.
function localPath(value) {
  if (process.platform !== "win32" || typeof value !== "string") return value;
  const bs = String.fromCharCode(92);
  const prefix = bs + bs + "?" + bs;
  if (value.startsWith(prefix + "UNC" + bs)) return bs + bs + value.slice(8);
  if (value.startsWith(prefix)) return value.slice(4);
  return value;
}

function directory(value, workdir) {
  const target = path.resolve(localPath(workdir), localPath(value ?? "."));
  let info;
  try {
    info = fs.statSync(target);
  } catch {
    throw new SubagentError("INVALID_ARGUMENT", `cwd is not an existing directory: ${target}`);
  }
  if (!info.isDirectory()) {
    throw new SubagentError("INVALID_ARGUMENT", `cwd is not a directory: ${target}`);
  }
  try {
    return fs.realpathSync(target);
  } catch {
    return target;
  }
}

// Validate the whole batch before spawning anything: a bad task later in the
// list must not leave earlier child agents running.
function tasks(request, workdir) {
  const value = request?.arguments?.tasks;
  if (!Array.isArray(value) || value.length === 0) {
    throw new SubagentError("INVALID_ARGUMENT", "tasks must be a nonempty array");
  }
  if (value.length > LIMITS.tasks) {
    throw new SubagentError("INVALID_ARGUMENT", `at most ${LIMITS.tasks} tasks per call`);
  }
  return value.map((entry, index) => {
    const task = entry?.task;
    if (typeof task !== "string" || !task.trim()) {
      throw new SubagentError("INVALID_ARGUMENT", `tasks[${index}].task must be a nonempty string`);
    }
    if (Buffer.byteLength(task, "utf8") > LIMITS.taskBytes) {
      throw new SubagentError("INVALID_ARGUMENT", `tasks[${index}].task exceeds ${LIMITS.taskBytes} bytes`);
    }
    if (entry.cwd !== undefined && (typeof entry.cwd !== "string" || !entry.cwd.trim())) {
      throw new SubagentError("INVALID_ARGUMENT", `tasks[${index}].cwd must be a nonempty string`);
    }
    return {index, task, cwd: directory(entry.cwd, workdir)};
  });
}

function writeFile(file, text) {
  const temp = `${file}.${process.pid}.${crypto.randomUUID().slice(0, 8)}.tmp`;
  fs.writeFileSync(temp, text);
  fs.renameSync(temp, file);
}

function newRunId() {
  const stamp = new Date().toISOString().replace(/[-:.TZ]/g, "").slice(0, 14);
  return `${stamp}-${crypto.randomUUID().slice(0, 8)}`;
}

const USAGE_FIELDS = [["input", "input"], ["output", "output"],
  ["cached", "cached"], ["reasoning", "reasoning_tokens"]];

function emptyUsage() {
  const total = {requests: 0, input: 0, output: 0, cached: 0, reasoning: 0, known: {}};
  for (const [field] of USAGE_FIELDS) total.known[field] = true;
  return total;
}

// Each counter stands on its own: a provider that omits one optional field
// must not erase the input and output it did report.
function recordUsage(total, frame) {
  total.requests++;
  for (const [field, key] of USAGE_FIELDS) {
    if (typeof frame[key] === "number") {
      if (total.known[field]) total[field] += frame[key];
    } else {
      total.known[field] = false;
    }
  }
}

function usageReport(total) {
  const known = total.requests > 0;
  const value = field => (known && total.known[field] ? total[field] : null);
  return {requests: total.requests, input: value("input"), output: value("output"),
    cached: value("cached"), reasoning: value("reasoning")};
}

function compactTokens(value) {
  if (typeof value !== "number") return "?";
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}m`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}k`;
  return String(value);
}

function usageLine(usage) {
  // No request yet means literally nothing spent, not unknown.
  if (!usage.requests) return "in 0 / out 0";
  const report = usageReport(usage);
  return `in ${compactTokens(report.input)} / out ${compactTokens(report.output)}`;
}

function createTrace(eventsPath) {
  return {answer: null, results: 0, reported: null, fault: null, taskStatus: null, usage: emptyUsage(),
    eventsPath, eventBytes: 0, eventsTruncated: false};
}

const EVIDENCE_EVENTS = new Set(["task_started", "model_started", "model_finished", "model_failed",
  "tool_started", "tool_finished", "task_finished", "error", "failed_request_usage", "apec_usage",
  "context"]);

// Evidence is appended while the task runs, so a host deadline or Ctrl+C still
// leaves a diagnosable trace. A write failure is fatal: the result would then
// be untrustworthy.
function traceEvent(trace, frame) {
  if (!EVIDENCE_EVENTS.has(frame.type) || trace.eventsTruncated) return;
  const line = JSON.stringify(frame) + "\n";
  trace.eventBytes += Buffer.byteLength(line, "utf8");
  if (trace.eventBytes > LIMITS.eventBytes) {
    trace.eventsTruncated = true;
    fs.appendFileSync(trace.eventsPath, "{\"type\":\"events_truncated\"}\n");
    return;
  }
  fs.appendFileSync(trace.eventsPath, line);
}

function record(trace, frame) {
  if (frame.type === "result") {
    trace.results++;
    if (trace.results === 1) trace.answer = typeof frame.text === "string" ? frame.text : "";
    return;
  }
  if (frame.type === "model_finished" || frame.type === "failed_request_usage") {
    recordUsage(trace.usage, frame);
  }
  if (frame.type === "error" && trace.reported === null) {
    trace.reported = typeof frame.message === "string" ? frame.message : "child reported an error";
  }
  if (frame.type === "task_finished") trace.taskStatus = frame.status ?? null;
  traceEvent(trace, frame);
}

const terminated = new WeakSet();

// A protocol fault is fatal for the batch. Kill managed children; deeper
// descendants follow the same OS limits as the host's own cancellation.
function terminate(child, graceMs = DEFAULT_KILL_GRACE_MS) {
  if (!child || terminated.has(child)) return;
  terminated.add(child);
  try {
    if (process.platform === "win32" && child.pid) {
      spawn("taskkill.exe", ["/PID", String(child.pid), "/T", "/F"],
        {stdio: "ignore", windowsHide: true}).on("error", () => {});
      return;
    }
    if (child.exitCode !== null) return;
    // SIGINT lets the child Kroma cancel its own tools, including the Shell
    // process group it owns. Killing the child outright would orphan that
    // group, so SIGKILL is only the bounded fallback.
    child.kill("SIGINT");
    const escalate = setTimeout(() => {
      try {
        child.kill("SIGKILL");
      } catch {
        // The child may already be gone.
      }
    }, graceMs);
    escalate.unref?.();
    child.once("close", () => clearTimeout(escalate));
  } catch {
    // The child may already be gone.
  }
}

// Startup failures happen before the CLI can emit a JSON error frame, so the
// exit message falls back to the last stderr line.
function lastStderrLine(buffer) {
  const text = buffer.toString("utf8").trim();
  if (!text) return "";
  const line = text.slice(text.lastIndexOf("\n") + 1).trim();
  return line.length > 300 ? `${line.slice(0, 300)}…` : line;
}

// The host enforces the smaller of the manifest deadline and its own
// administrator cap, so the progress line must not promise the larger one.
function hostDeadlineSecs() {
  const admin = Number(process.env.KROMA_PLUGIN_TOOL_TIMEOUT_SECS);
  return Number.isInteger(admin) && admin > 0
    ? Math.min(admin, LIMITS.timeoutSecs) : LIMITS.timeoutSecs;
}

function progress(context, text) {
  if (!context.onProgress) return;
  try {
    context.onProgress(text);
  } catch {
    // Progress is best-effort; it never changes the task outcome.
  }
}

async function runTask(entry, context) {
  const dir = path.join(context.runDir, String(entry.index));
  const session = path.join(dir, "session.json");
  const stderrPath = path.join(dir, "stderr.log");
  const eventsPath = path.join(dir, "events.ndjson");
  const started = Date.now();
  const trace = createTrace(eventsPath);
  // Bounded tail of stderr, mirrored to stderr.log as it arrives.
  let stderr = Buffer.alloc(0);
  let finished = false;
  let pending = "";
  const decoder = new StringDecoder("utf8");
  try {
    fs.mkdirSync(dir, {recursive: true});
    // Locatable before it starts: a host kill cannot skip this record.
    writeFile(path.join(dir, "state.json"), JSON.stringify({
      index: entry.index, status: "running", session, cwd: entry.cwd,
      started_at: new Date().toISOString(),
    }));
  } catch (error) {
    const failure = new SubagentError("ARCHIVE_ERROR",
      `cannot write evidence: ${error.message}`, true);
    context.run.abort(failure);
    return {index: entry.index, status: "failed", exit_code: null,
      error: {code: "ARCHIVE_ERROR", message: failure.message},
      duration_ms: 0, usage: usageReport(trace.usage)};
  }
  progress(context, `▷ [${entry.index + 1}/${context.count}] started`);

  function appendStderr(chunk) {
    stderr = Buffer.concat([stderr, chunk]);
    if (stderr.length <= LIMITS.stderrBytes) {
      fs.appendFileSync(stderrPath, chunk);
      return;
    }
    stderr = stderr.subarray(stderr.length - Math.floor(LIMITS.stderrBytes / 2));
    // Never begin inside a multi-byte UTF-8 sequence.
    while (stderr.length && (stderr[0] & 0xc0) === 0x80) stderr = stderr.subarray(1);
    // Trimming the head cannot be appended: rewrite the bounded tail.
    writeFile(stderrPath, stderr.toString("utf8"));
  }

  return new Promise(resolve => {
    let child;
    try {
      child = context.run.spawn(context.executable, ["--json"], {
        cwd: entry.cwd,
        env: {
          ...process.env,
          KROMA_WORKDIR: entry.cwd,
          KROMA_SESSION_FILE: session,
          KROMA_SUBAGENT_DEPTH: String(context.depth),
        },
        windowsHide: true,
        stdio: ["pipe", "pipe", "pipe"],
      });
    } catch (error) {
      // The directory can disappear between validation and spawn.
      finish({status: "failed", code: "CHILD_START",
        message: `cannot start '${context.executable}': ${error.message}`});
      return;
    }
    context.run.children.add(child);

    const fault = error => {
      if (trace.fault) return;
      trace.fault = error;
      context.run.abort(error);
    };

    const consume = line => {
      if (!line.trim() || trace.fault) return;
      let frame;
      try {
        frame = JSON.parse(line);
      } catch {
        fault(new SubagentError("CHILD_PROTOCOL", "child emitted a non-JSON stdout line", true));
        return;
      }
      // `null`, arrays and scalars are valid JSON but not frames; treating one
      // as an object would throw out of the read handler and skip teardown.
      if (!frame || typeof frame !== "object" || Array.isArray(frame)) {
        fault(new SubagentError("CHILD_PROTOCOL",
          "child emitted a JSON line that is not an object", true));
        return;
      }
      try {
        record(trace, frame);
      } catch (error) {
        fault(new SubagentError("ARCHIVE_ERROR", `cannot write evidence: ${error.message}`, true));
        return;
      }
      if (frame.type === "model_finished" || frame.type === "failed_request_usage") {
        recordUsage(context.run.usage, frame);
      }
      if (trace.results > 1) {
        fault(new SubagentError("CHILD_PROTOCOL", "child returned more than one final result", true));
      } else if (trace.results === 1 && typeof frame.text !== "string") {
        fault(new SubagentError("CHILD_PROTOCOL", "child returned a result without text", true));
      }
    };

    child.stdout?.on("data", chunk => {
      pending += decoder.write(chunk);
      let at;
      while ((at = pending.indexOf("\n")) >= 0) {
        const line = pending.slice(0, at);
        pending = pending.slice(at + 1);
        if (Buffer.byteLength(line, "utf8") > LIMITS.frameBytes) {
          fault(new SubagentError("CHILD_PROTOCOL", "child emitted an oversized stdout line", true));
          return;
        }
        consume(line);
      }
      if (Buffer.byteLength(pending, "utf8") > LIMITS.frameBytes) {
        fault(new SubagentError("CHILD_PROTOCOL", "child emitted an oversized stdout line", true));
      }
    });
    child.stderr?.on("data", chunk => {
      try {
        appendStderr(Buffer.from(chunk));
      } catch (error) {
        fault(new SubagentError("ARCHIVE_ERROR", `cannot write evidence: ${error.message}`, true));
      }
    });
    child.stdin?.on("error", () => {});
    child.on("error", error => finish({
      status: "failed", code: "CHILD_START",
      message: `cannot start '${context.executable}': ${error.message}`,
    }));
    child.on("close", (code, signal) => {
      if (finished) return;
      if (pending.trim()) consume(pending);
      pending = "";
      if (trace.fault) finish({status: "failed", code: trace.fault.code, exit_code: null, message: trace.fault.message});
      else if (context.run.aborted) finish({status: "failed", code: "BATCH_ABORTED", exit_code: null,
        message: context.run.reason?.message ?? "batch aborted"});
      else if (signal) finish({status: "failed", code: "CHILD_SIGNAL", exit_code: null,
        message: `child was terminated by ${signal}`});
      else if (code !== 0) {
        // Startup failures happen before the child can emit an error frame.
        const reason = trace.reported ?? lastStderrLine(stderr);
        finish({status: "failed", code: "CHILD_EXIT", exit_code: code,
          message: `child exited with code ${code}${reason ? `: ${reason}` : ""}`});
      }
      else if (trace.results === 0) finish({status: "failed", code: "CHILD_PROTOCOL", exit_code: 0,
        message: "child exited without a final result"});
      else finish({status: "completed", code: "COMPLETED", exit_code: 0});
    });

    try {
      child.stdin?.write(entry.task);
      child.stdin?.end();
    } catch {
      // A dead child surfaces through close/error.
    }

    function finish(outcome) {
      if (finished) return;
      finished = true;
      context.run.children.delete(child);
      const answer = trace.answer ?? "";
      const clipped = clip(answer, context.maxOutput);
      try {
        if (answer) writeFile(path.join(dir, "answer.txt"), answer);
        // The logs are already on disk; only guarantee the artifacts exist.
        for (const file of [stderrPath, eventsPath]) {
          if (!fs.existsSync(file)) writeFile(file, "");
        }
        writeFile(path.join(dir, "state.json"), JSON.stringify({
          index: entry.index, status: outcome.status, code: outcome.code,
          session, cwd: entry.cwd, task_status: trace.taskStatus,
          duration_ms: Date.now() - started,
        }));
      } catch (error) {
        // Evidence is part of the contract: without it the result is untrusted.
        const failure = new SubagentError("ARCHIVE_ERROR",
          `cannot write evidence: ${error.message}`, true);
        context.run.abort(failure);
        outcome = {status: "failed", code: "ARCHIVE_ERROR", exit_code: null, message: failure.message};
      }
      const result = {index: entry.index, status: outcome.status,
        exit_code: outcome.exit_code ?? null};
      if (outcome.status === "completed") {
        result.output = clipped.text;
        result.output_truncated = clipped.truncated;
      } else {
        result.error = {code: outcome.code, message: outcome.message};
      }
      result.duration_ms = Date.now() - started;
      result.usage = usageReport(trace.usage);
      result.artifacts = dir;
      const seconds = ((Date.now() - started) / 1000).toFixed(1).padStart(5);
      const mark = outcome.status === "completed" ? "✓" : "✗";
      const detail = outcome.status === "completed"
        ? usageLine(trace.usage) : (outcome.code ?? outcome.status);
      progress(context, `${mark} [${entry.index + 1}/${context.count}] ${seconds}s · ${detail}`);
      resolve(result);
    }
  });
}

function fit(response) {
  const size = text => Buffer.byteLength(text, "utf8");
  if (size(JSON.stringify(response)) <= LIMITS.responseBytes) return response;
  for (const result of response.results) {
    if (typeof result.output === "string") {
      result.output = clip(result.output, LIMITS.evidenceChars).text;
      result.output_truncated = true;
    }
    if (result.error && typeof result.error.message === "string") {
      result.error.message = clip(result.error.message, LIMITS.evidenceChars).text;
    }
  }
  // Per-task fields are already clipped, so clipping is the final stage.
  return response;
}

async function runBatch(request, options = {}) {
  const workdir = options.workdir ?? request?.workdir ?? process.cwd();
  const config = {...(request?.config ?? {}), ...(options.config ?? {})};
  // A malformed depth is refused rather than read as zero: reading it as
  // "top level" is what would allow unbounded nesting.
  const depth = integer(process.env.KROMA_SUBAGENT_DEPTH, "KROMA_SUBAGENT_DEPTH", 0, 8, 0);
  if (depth >= 1) {
    throw new SubagentError("DEPTH_LIMIT", "a subagent cannot delegate further; do the work directly");
  }
  const maxParallel = integer(config.max_parallel, "max_parallel", 1, LIMITS.tasks,
    LIMITS.maxParallel);
  const maxOutput = integer(config.max_output_chars, "max_output_chars", 1000, 100000,
    LIMITS.outputChars);
  const killGraceMs = integer(options.killGraceMs, "killGraceMs", 1, 60_000,
    DEFAULT_KILL_GRACE_MS);
  const deadlineSecs = hostDeadlineSecs();
  const executable = String(config.kroma_bin || process.env.KROMA_BIN || "kroma");
  const entries = tasks(request, workdir);
  const rawDataDir = options.dataDir ?? request?.data_dir;
  const dataDir = rawDataDir ? localPath(rawDataDir) : null;
  if (!dataDir) throw new SubagentError("ARCHIVE_ERROR", "plugin data directory is unavailable", true);

  const id = newRunId();
  const runDir = path.join(dataDir, "runs", id);
  try {
    fs.mkdirSync(runDir, {recursive: true});
    writeFile(path.join(runDir, "request.json"), JSON.stringify({
      call_id: request?.call_id ?? null, archive: request?.archive ?? null, workdir,
      max_parallel: maxParallel, max_output_chars: maxOutput,
      tasks: entries.map(entry => ({index: entry.index, task: entry.task, cwd: entry.cwd})),
    }, null, 2));
  } catch (error) {
    throw new SubagentError("ARCHIVE_ERROR", `cannot create the run directory: ${error.message}`, true);
  }

  const terminateChild = options.terminate ?? terminate;
  const run = {aborted: false, reason: null, children: new Set(), usage: emptyUsage(),
    spawn: options.spawn ?? spawn, terminate: child => terminateChild(child, killGraceMs)};
  run.abort = error => {
    if (run.aborted) return;
    run.aborted = true;
    run.reason = error;
    for (const child of run.children) run.terminate(child);
  };
  const context = {runDir, executable, maxOutput, count: entries.length, depth: depth + 1,
    deadlineSecs, run, onProgress: options.onProgress};

  const results = new Array(entries.length);
  let next = 0;
  let done = 0;
  let started = 0;
  const startedAt = Date.now();
  // Live status for the tool card: what runs, how long, against which limit,
  // and what it has cost so far. Best effort, never part of the result.
  const heartbeat = options.onProgress ? setInterval(() => {
    const elapsed = Math.round((Date.now() - startedAt) / 1000);
    progress(context, `${done}/${entries.length} done · ${started - done} running · `
      + `${elapsed}s/${deadlineSecs}s · ${usageLine(run.usage)}`);
  }, options.heartbeatMs ?? 5000) : null;
  heartbeat?.unref?.();

  async function worker() {
    while (!run.aborted && next < entries.length) {
      const entry = entries[next++];
      started++;
      results[entry.index] = await runTask(entry, context);
      done++;
    }
  }
  try {
    await Promise.all(Array.from({length: Math.min(maxParallel, entries.length)}, worker));
  } finally {
    clearInterval(heartbeat);
  }

  // Only batch.json shows this: a fatal fault rejects the tool call itself,
  // so these entries record which tasks never began.
  for (let index = 0; index < entries.length; index++) {
    if (!results[index]) {
      results[index] = {index, status: "not_started", error: {
        code: "BATCH_ABORTED",
        message: run.reason?.message ?? "batch aborted",
      }};
    }
  }
  const completed = results.filter(result => result.status === "completed").length;
  const status = completed === results.length ? "completed" : completed > 0 ? "partial" : "failed";
  const mark = status === "completed" ? "✓" : status === "partial" ? "△" : "✗";
  progress(context, `${mark} ${completed}/${results.length} done · `
    + `${((Date.now() - startedAt) / 1000).toFixed(1)}s · ${usageLine(run.usage)}`);
  const response = {schema_version: 1, run_id: id, status, results};
  try {
    writeFile(path.join(runDir, "batch.json"),
      JSON.stringify({...response, tasks: entries.length}, null, 2));
  } catch {
    // Per-task evidence is already durable; batch.json is a convenience.
  }
  if (run.reason?.fatal) {
    // A fatal fault invalidates the worker instead of looking like a normal
    // result, so the host discards it and the next call starts clean.
    throw new SubagentError(run.reason.code,
      `${run.reason.message}; ${completed}/${entries.length} task(s) completed, evidence under ${runDir}`,
      true);
  }
  return fit(response);
}

async function dispatch(request, options = {}) {
  if (request?.type === "ping") return {content: "subagent plugin ready", is_error: false};
  if (request?.type !== "tool" && request?.type !== "tool_stream") {
    throw new SubagentError("INVALID_REQUEST", "only tool requests are supported");
  }
  if (request.name === "subagent") {
    const response = await runBatch(request, options);
    return {content: JSON.stringify(response, null, 2),
      is_error: response.status !== "completed"};
  }
  throw new SubagentError("UNKNOWN_TOOL", `unknown tool '${request.name}'`);
}

module.exports = {dispatch, runBatch, SubagentError, LIMITS};
