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
  taskTimeoutSecs: 300,
  timeoutSecs: 600,
});

// Time a cancelled child Kroma gets to run its own cancellation path before
// the plugin escalates to SIGKILL.
const DEFAULT_KILL_GRACE_MS = 2000;
// A running task silent for this long is shown as quiet, so a stuck child
// looks different from one that is thinking hard.
const QUIET_MS = 90_000;

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
    tools: new Map(), eventsPath, eventBytes: 0, eventsTruncated: false};
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

// Task text and answers are multi-line: a card row must stay one short line.
// A child model id is optional: no value means "inherit the parent's model
// and profile". Control characters are refused because the value reaches a
// child process environment.
function optionalModel(value) {
  if (value === undefined || value === null) return null;
  const text = String(value).trim();
  if (!text) return null;
  if (text.length > 120) {
    throw new SubagentError("CONFIG_ERROR", "model must be at most 120 characters");
  }
  if (/[\u0000-\u001f\u007f]/.test(text)) {
    throw new SubagentError("CONFIG_ERROR", "model must not contain control characters");
  }
  return text;
}

// DeepSeek reasoning tiers understood by the host's KROMA_DEEPSEEK_THINKING.
const THINKING_LEVELS = new Set(["off", "enabled", "high", "max"]);

// Optional like the model: absent means "keep whatever the parent uses".
function optionalThinking(value) {
  if (value === undefined || value === null) return null;
  const text = String(value).trim().toLowerCase();
  if (!text) return null;
  if (!THINKING_LEVELS.has(text)) {
    throw new SubagentError("CONFIG_ERROR", "thinking must be off, enabled, high or max");
  }
  return text;
}

function oneLine(text, max) {
  const line = String(text ?? "").replace(/\s+/g, " ").trim();
  return line.length > max ? `${line.slice(0, max)}…` : line;
}

function clock(ms) {
  const seconds = Math.max(0, Math.round(ms / 1000));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

function formatTimeout(ms) {
  return ms % 1000 === 0 ? `${ms / 1000}s` : `${ms}ms`;
}

async function runTask(entry, context) {
  const dir = path.join(context.runDir, String(entry.index));
  const session = path.join(dir, "session.json");
  const stderrPath = path.join(dir, "stderr.log");
  const eventsPath = path.join(dir, "events.ndjson");
  const started = Date.now();
  const trace = createTrace(eventsPath);
  trace.startedAt = started;
  trace.label = entry.task;
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
  context.run.status[entry.index] = "running";
  context.run.live[entry.index] = trace;
  progress(context, `▷ [${entry.index + 1}/${context.count}] ${oneLine(entry.task, 48)}`);

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
          ...(context.model ? {KROMA_MODEL: context.model} : {}),
          ...(context.thinking ? {KROMA_DEEPSEEK_THINKING: context.thinking} : {}),
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
    let timedOut = null;
    let closeTimer = null;

    const armCloseFallback = outcome => {
      if (closeTimer || finished) return;
      closeTimer = setTimeout(() => {
        child.stdout?.destroy?.();
        child.stderr?.destroy?.();
        finish(outcome);
      }, context.killGraceMs + 100);
    };

    const failByTimeout = message => {
      if (finished || trace.fault || timedOut) return;
      timedOut = {status: "failed", code: "CHILD_TIMEOUT", exit_code: null, message};
      context.run.terminate(child);
      armCloseFallback(timedOut);
    };

    const taskTimer = setTimeout(
      () => failByTimeout(`child timed out after ${formatTimeout(context.taskTimeoutMs)}`),
      context.taskTimeoutMs,
    );
    context.run.timeouts.add(failByTimeout);

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
      // Live state only: the heartbeat renders one row per running task, so
      // every child event must not become a row of its own.
      trace.lastEventAt = Date.now();
      if (frame.type === "model_started") {
        trace.phase = "model";
        trace.turn = frame.request ?? trace.turn;
      } else if (frame.type === "tool_started") {
        const at = Date.now();
        trace.tools.set(frame.id, {name: frame.name ?? "tool", at});
        trace.tool = frame.name ?? "tool";
        trace.toolStartedAt = at;
        trace.phase = "tool";
      } else if (frame.type === "tool_finished") {
        trace.tools.delete(frame.id);
        const pending = [...trace.tools.values()].at(-1);
        trace.tool = pending ? pending.name : null;
        trace.toolStartedAt = pending ? pending.at : 0;
        trace.phase = pending ? "tool" : "model";
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
      else if (timedOut) finish(timedOut);
      else if (context.run.aborted) finish({status: "failed",
        code: context.run.timedOut ? "CHILD_TIMEOUT" : "BATCH_ABORTED", exit_code: null,
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
      clearTimeout(taskTimer);
      clearTimeout(closeTimer);
      context.run.timeouts.delete(failByTimeout);
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
      const preview = outcome.status === "completed" && answer.trim()
        ? ` · "${oneLine(answer, 40)}"` : "";
      context.run.status[entry.index] = outcome.status;
      context.run.live[entry.index] = null;
      progress(context, `${mark} [${entry.index + 1}/${context.count}] ${seconds}s · ${detail}${preview}`);
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
  const taskTimeoutSecs = integer(config.task_timeout_secs, "task_timeout_secs", 1,
    LIMITS.timeoutSecs, LIMITS.taskTimeoutSecs);
  const taskTimeoutMs = integer(options.taskTimeoutMs, "taskTimeoutMs", 1,
    LIMITS.timeoutSecs * 1000, taskTimeoutSecs * 1000);
  const deadlineMs = deadlineSecs * 1000;
  const reserveMs = Math.min(Math.max(killGraceMs + 3000, 5000), Math.floor(deadlineMs / 2));
  const batchTimeoutMs = integer(options.batchTimeoutMs, "batchTimeoutMs", 1,
    deadlineMs, Math.max(1, deadlineMs - reserveMs));
  const executable = String(config.kroma_bin || process.env.KROMA_BIN || "kroma");
  const model = optionalModel(config.model);
  const thinking = optionalThinking(config.thinking);
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
      task_timeout_secs: taskTimeoutSecs, model, thinking,
      tasks: entries.map(entry => ({index: entry.index, task: entry.task, cwd: entry.cwd})),
    }, null, 2));
  } catch (error) {
    throw new SubagentError("ARCHIVE_ERROR", `cannot create the run directory: ${error.message}`, true);
  }

  const terminateChild = options.terminate ?? terminate;
  const run = {aborted: false, timedOut: false, reason: null, children: new Set(),
    timeouts: new Set(), usage: emptyUsage(),
    status: entries.map(() => "queued"), live: entries.map(() => null),
    spawn: options.spawn ?? spawn, terminate: child => terminateChild(child, killGraceMs)};
  run.abort = error => {
    if (run.aborted) return;
    run.aborted = true;
    run.reason = error;
    for (const child of run.children) run.terminate(child);
  };
  run.timeout = message => {
    if (run.aborted) return;
    run.aborted = true;
    run.timedOut = true;
    run.reason = new SubagentError("CHILD_TIMEOUT", message, false);
    for (const stop of [...run.timeouts]) stop(message);
  };
  const context = {runDir, executable, model, thinking, maxOutput, count: entries.length, depth: depth + 1,
    deadlineSecs, taskTimeoutMs, killGraceMs, run, onProgress: options.onProgress};

  const results = new Array(entries.length);
  let next = 0;
  const startedAt = Date.now();
  // Live status for the tool card: what runs, how long, against which limit,
  // and what it has cost so far. Best effort, never part of the result.
  const STATE = {queued: "·", running: "▶", completed: "✓", failed: "✗", not_started: "–"};
  // One row per running task: task elapsed, current activity, tokens, quiet mark.
  const liveRow = (index, live) => {
    const now = Date.now();
    const phase = live.tool
      ? `${live.tool} ${clock(now - live.toolStartedAt)}`
      : live.phase === "model" ? `model turn ${live.turn || 1}` : "starting";
    const quiet = now - live.lastEventAt >= QUIET_MS
      ? ` · quiet ${clock(now - live.lastEventAt)}` : "";
    const report = usageReport(live.usage);
    const tokens = (report.input ?? 0) + (report.output ?? 0);
    return `[${index + 1}/${context.count}] ${oneLine(live.label, 18)} ${clock(now - live.startedAt)}`
      + ` ▶ ${phase} · ${compactTokens(tokens)} tok${quiet}`;
  };
  const beat = () => {
    const elapsed = Math.round((Date.now() - startedAt) / 1000);
    const strip = run.status.map((state, index) => `${index + 1}${STATE[state] ?? "?"}`).join(" ");
    progress(context, `${strip} · ${elapsed}s/${deadlineSecs}s · ${usageLine(run.usage)}`);
    const running = run.live.map((live, index) => ({live, index})).filter(item => item.live);
    for (const {live, index} of running.slice(0, 3)) progress(context, liveRow(index, live));
    if (running.length > 3) progress(context, `+${running.length - 3} running`);
  };
  const heartbeat = options.onProgress ? setInterval(beat, options.heartbeatMs ?? 5000) : null;
  heartbeat?.unref?.();
  const batchTimer = setTimeout(
    () => run.timeout(`batch timed out after ${formatTimeout(batchTimeoutMs)}`),
    batchTimeoutMs,
  );

  async function worker() {
    while (!run.aborted && next < entries.length) {
      const entry = entries[next++];
      results[entry.index] = await runTask(entry, context);
    }
  }
  try {
    await Promise.all(Array.from({length: Math.min(maxParallel, entries.length)}, worker));
  } finally {
    clearInterval(heartbeat);
    clearTimeout(batchTimer);
  }

  // Unstarted tasks occur when a fatal fault or batch deadline stops the queue.
  for (let index = 0; index < entries.length; index++) {
    if (!results[index]) {
      run.status[index] = "not_started";
      results[index] = {index, status: "not_started", exit_code: null, error: {
        code: run.timedOut ? "CHILD_TIMEOUT" : "BATCH_ABORTED",
        message: run.reason?.message ?? "batch aborted",
      }};
    }
  }
  const completed = results.filter(result => result.status === "completed").length;
  const status = completed === results.length ? "completed" : completed > 0 ? "partial" : "failed";
  const mark = status === "completed" ? "✓" : status === "partial" ? "△" : "✗";
  progress(context, `${mark} ${completed}/${results.length} done · `
    + `${((Date.now() - startedAt) / 1000).toFixed(1)}s · ${usageLine(run.usage)}`);
  // The host keeps the last two rows after the call finishes: make them the
  // summary and one line of answers, not whichever task happened to finish last.
  const roster = results.map(result => `${result.index + 1} ${result.status === "completed"
    ? `"${oneLine(result.output ?? "", 24)}"`
    : `✗ ${result.error?.code ?? result.status}`}`).join(" · ");
  progress(context, oneLine(roster, 160));
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
