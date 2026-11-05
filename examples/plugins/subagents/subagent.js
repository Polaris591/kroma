"use strict";
const {spawn} = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const {StringDecoder} = require("node:string_decoder");
const {loadProfiles} = require("./profiles");

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
  // In-memory tail of streamed assistant text; never written to events.ndjson.
  draftChars: 64 * 1024,
  taskTimeoutSecs: 300,
  timeoutSecs: 600,
});

// Time a cancelled child Kroma gets to run its own cancellation path before
// the plugin escalates to SIGKILL.
const DEFAULT_KILL_GRACE_MS = 2000;
// A running task silent for this long is shown as quiet, so a stuck child
// looks different from one that is thinking hard.
const QUIET_MS = 90_000;
// Show streamed text in a live row only while it is fresh.
const PREVIEW_MS = 30_000;

class SubagentError extends Error {
  // fatal errors abort the whole batch and are reported as a stream error, so
  // the host discards the worker instead of reusing a possibly corrupt one.
  constructor(code, message, fatal = false) {
    super(message);
    this.code = code;
    this.fatal = fatal;
  }
}

// Code-point clipping: callers bound bytes where a transport requires it,
// while answers and evidence fields are bounded in characters.
function sliceChars(text, limit) {
  if (limit <= 0) return {text: "", truncated: text.length > 0};
  if (text.length <= limit) return {text, truncated: false};
  let end = 0;
  let count = 0;
  while (end < text.length && count < limit) {
    const code = text.charCodeAt(end);
    const pair = code >= 0xd800 && code <= 0xdbff && end + 1 < text.length
      && text.charCodeAt(end + 1) >= 0xdc00 && text.charCodeAt(end + 1) <= 0xdfff;
    end += pair ? 2 : 1;
    count++;
  }
  return {text: text.slice(0, end), truncated: end < text.length};
}

function clip(text, limit) {
  return sliceChars(String(text), limit);
}

// Partial answers are the most recent text, so keep the tail rather than the
// beginning when a failed task streamed more than the output budget.
function tailClip(text, limit) {
  if (limit <= 0) return {text: "", truncated: text.length > 0};
  if (text.length <= limit) return {text, truncated: false};
  const chars = Array.from(text);
  return chars.length <= limit
    ? {text, truncated: false}
    : {text: chars.slice(-limit).join(""), truncated: true};
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

// A resume token is `<run_id>/<index>` and resolves only inside this plugin's
// own runs directory. It continues a child that already finished; a running or
// interrupted task is refused because its session may still be in use.
function resumeSource(value, dataDir, workdir) {
  if (value === undefined || value === null) return null;
  if (typeof value !== "string" || !value.trim()) {
    throw new SubagentError("INVALID_ARGUMENT", "resume must be a nonempty string");
  }
  const token = value.trim();
  if (token.length > 80) {
    throw new SubagentError("INVALID_ARGUMENT", "resume is too long");
  }
  const match = /^([A-Za-z0-9][A-Za-z0-9_-]{0,63})\/(0|[1-9][0-9]{0,2})$/.exec(token);
  if (!match) {
    throw new SubagentError("INVALID_ARGUMENT",
      "resume must be a task token such as <run_id>/<index>");
  }
  const runsDir = path.join(dataDir, "runs");
  const source = path.join(runsDir, match[1], match[2]);
  let root;
  let real;
  try {
    root = fs.realpathSync(runsDir);
    real = fs.realpathSync(source);
  } catch {
    throw new SubagentError("RESUME_MISSING", `no evidence for resumed task '${token}'`);
  }
  if (real !== root && !real.startsWith(root + path.sep)) {
    throw new SubagentError("INVALID_ARGUMENT", `resume '${token}' leaves the run directory`);
  }
  let state;
  try {
    state = JSON.parse(fs.readFileSync(path.join(real, "state.json"), "utf8"));
  } catch {
    throw new SubagentError("RESUME_MISSING", `resumed task '${token}' has no state record`);
  }
  if (!state || typeof state !== "object" || Array.isArray(state)) {
    throw new SubagentError("RESUME_MISSING", `resumed task '${token}' has an invalid state record`);
  }
  if (state.status === "running") {
    throw new SubagentError("RESUME_ACTIVE",
      `task '${token}' is still marked running; inspect its evidence instead of resuming it`);
  }
  if (state.status !== "completed" && state.status !== "failed") {
    throw new SubagentError("RESUME_MISSING", `resumed task '${token}' never finished`);
  }
  let cwd;
  try {
    if (typeof state.cwd !== "string" || !state.cwd.trim()) throw new Error("missing cwd");
    cwd = directory(state.cwd, workdir);
  } catch {
    throw new SubagentError("RESUME_STALE", `resumed task '${token}' has no usable working directory`);
  }
  const session = path.join(real, "session.json");
  let info;
  try {
    info = fs.statSync(session);
  } catch {
    throw new SubagentError("RESUME_MISSING", `resumed task '${token}' has no saved session`);
  }
  if (!info.isFile()) {
    throw new SubagentError("RESUME_MISSING", `resumed task '${token}' has no saved session`);
  }
  return {token, source: real, session, cwd, profile: state.profile ?? "general"};
}

// Validate the whole batch before spawning anything: a bad task later in the
// list must not leave earlier child agents running.
function tasks(request, workdir, dataDir, profiles) {
  const value = request?.arguments?.tasks;
  if (!Array.isArray(value) || value.length === 0) {
    throw new SubagentError("INVALID_ARGUMENT", "tasks must be a nonempty array");
  }
  if (value.length > LIMITS.tasks) {
    throw new SubagentError("INVALID_ARGUMENT", `at most ${LIMITS.tasks} tasks per call`);
  }
  return value.map((entry, index) => {
    if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
      throw new SubagentError("INVALID_ARGUMENT", `tasks[${index}] must be an object`);
    }
    for (const key of Object.keys(entry)) {
      if (!new Set(["task", "cwd", "resume", "profile", "label", "complexity"]).has(key)) {
        throw new SubagentError("INVALID_ARGUMENT", `tasks[${index}].${key} is not supported`);
      }
    }
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
    const cwd = entry.cwd === undefined ? null : directory(entry.cwd, workdir);
    const resume = resumeSource(entry.resume, dataDir, workdir);
    const name = entry.profile ?? resume?.profile ?? "general";
    if (typeof name !== "string" || !profiles.has(name)) {
      throw new SubagentError("INVALID_ARGUMENT", `tasks[${index}].profile is unknown`);
    }
    if (resume && entry.profile !== undefined && name !== resume.profile) {
      throw new SubagentError("RESUME_PROFILE", `tasks[${index}].profile differs from resumed task`);
    }
    const complexity = entry.complexity ?? "medium";
    if (!["light", "medium", "heavy"].includes(complexity)) {
      throw new SubagentError("INVALID_ARGUMENT", `tasks[${index}].complexity must be light, medium or heavy`);
    }
    const label = entry.label ?? Array.from(task.split(/\r?\n/).find(line => line.trim())?.trim() ?? "")
      .slice(0, 60).join("");
    if (typeof label !== "string" || !label.trim() || Array.from(label).length > 60
      || /[\x00-\x1f\x7f]/.test(label)) {
      throw new SubagentError("INVALID_ARGUMENT", `tasks[${index}].label must have 1..60 printable characters`);
    }
    if (resume && cwd && cwd !== resume.cwd) {
      throw new SubagentError("INVALID_ARGUMENT",
        `tasks[${index}].cwd must match the resumed task directory: ${resume.cwd}`);
    }
    return {index, task, label, complexity, profile: profiles.get(name),
      cwd: resume ? resume.cwd : (cwd ?? directory(undefined, workdir)), resume};
  });
}

function profileModel(entry, config, legacyModel) {
  const selected = entry.profile.model === "auto" ? entry.complexity : entry.profile.model;
  if (selected === "inherit") return legacyModel;
  // Never invent a model ID: an absent tier mapping uses the configured model.
  return optionalModel(config[`model_${selected}`]) ?? legacyModel;
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
// Each frame reports its own counters. `apec_usage` has no reasoning counter,
// so it must not erase reasoning that a model frame already reported.
const USAGE_FRAME_FIELDS = Object.freeze({
  model_finished: new Set(["input", "output", "cached", "reasoning"]),
  failed_request_usage: new Set(["input", "output", "cached", "reasoning"]),
  apec_usage: new Set(["input", "output", "cached"]),
});

function emptyUsage() {
  const total = {requests: 0, input: 0, output: 0, cached: 0, reasoning: 0,
    seen: {}, known: {}};
  for (const [field] of USAGE_FIELDS) {
    total.seen[field] = false;
    total.known[field] = true;
  }
  return total;
}

// Each counter stands on its own: a provider that omits one optional field
// must not erase the input and output it did report.
function recordUsage(total, frame) {
  const fields = USAGE_FRAME_FIELDS[frame.type];
  if (!fields) return;
  // Compaction frames may cover several requests; model frames cover one.
  const count = Number.isInteger(frame.requests) && frame.requests > 0 ? frame.requests : 1;
  total.requests += count;
  for (const [field, key] of USAGE_FIELDS) {
    if (!fields.has(field)) continue;
    if (typeof frame[key] === "number") {
      total.seen[field] = true;
      if (total.known[field]) total[field] += frame[key];
    } else {
      total.known[field] = false;
    }
  }
}

function usageReport(total) {
  const known = total.requests > 0;
  const value = field => (known && total.known[field] && total.seen[field]
    ? total[field] : null);
  return {requests: total.requests, input: value("input"), output: value("output"),
    cached: value("cached"), reasoning: value("reasoning")};
}

function compactChars(value) {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}m ch`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}k ch`;
  return `${value} ch`;
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
  return {answer: null, results: 0, reported: null, fault: null, taskStatus: null,
    usage: emptyUsage(), tools: new Map(), toolCalls: 0, textChars: 0, draft: "",
    lastTextAt: 0, eventsPath, eventBytes: 0, eventsTruncated: false};
}

// A bounded tail of streamed assistant text. It is never written to
// events.ndjson, but it is what lets a failed task keep the work it produced
// and lets the live card show that a child is writing.
function appendDraft(trace, text) {
  if (!text) return;
  trace.textChars += text.length;
  trace.lastTextAt = Date.now();
  trace.draft += text;
  const cap = LIMITS.draftChars;
  if (trace.draft.length > cap * 2) {
    let start = trace.draft.length - cap;
    const code = trace.draft.charCodeAt(start);
    if (code >= 0xdc00 && code <= 0xdfff) start += 1;
    trace.draft = trace.draft.slice(start);
  }
}

const EVIDENCE_EVENTS = new Set(["task_started", "model_started", "model_finished", "model_failed",
  "tool_started", "tool_finished", "task_finished", "warning", "error", "failed_request_usage",
  "apec_usage", "context"]);
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
  if (USAGE_FRAME_FIELDS[frame.type]) {
    recordUsage(trace.usage, frame);
  }
  if (frame.type === "tool_started") trace.toolCalls++;
  if (frame.type === "text_delta") appendDraft(trace, frame.text);
  if (frame.type === "error" && trace.reported === null) {
    trace.reported = typeof frame.message === "string" ? frame.message : "child reported an error";
  }
  if (frame.type === "task_finished") trace.taskStatus = frame.status ?? null;
  traceEvent(trace, frame);
}

// A resume copies the snapshot and journal into the new task directory: the
// source evidence stays immutable and the child owns its new lock.
function copySession(source, destination) {
  const copy = (from, to) => {
    const temp = `${to}.${process.pid}.${crypto.randomUUID().slice(0, 8)}.tmp`;
    fs.copyFileSync(from, temp);
    fs.renameSync(temp, to);
  };
  copy(source, destination);
  const wal = `${source}.wal`;
  if (fs.existsSync(wal)) copy(wal, `${destination}.wal`);
}

// Exit codes documented by `kroma --reference/cli.md`: a parent can tell an
// auth failure that will fail again from a transient model request failure.
const EXIT_CODES = new Map([
  [3, "MODEL_NOT_CONFIGURED"],
  [4, "AUTH_FAILED"],
  [5, "MODEL_REQUEST_FAILED"],
  [6, "CONTEXT_LIMIT"],
]);

function exitFailure(code, reason) {
  const name = EXIT_CODES.get(code) ?? "CHILD_EXIT";
  return {status: "failed", code: name, exit_code: code,
    message: `child exited with code ${code}${reason ? `: ${reason}` : ""}`};
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
  const line = text.slice(text.lastIndexOf(String.fromCharCode(10)) + 1).trim();
  const clipped = sliceChars(line, 300);
  return clipped.truncated ? `${clipped.text}…` : clipped.text;
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
  const clipped = sliceChars(line, max);
  return clipped.truncated ? `${clipped.text}…` : clipped.text;
}

// Last characters without starting inside a surrogate pair.
function tailChars(text, limit) {
  if (text.length <= limit) return text;
  let start = text.length - limit;
  const code = text.charCodeAt(start);
  if (code >= 0xdc00 && code <= 0xdfff) start += 1;
  return text.slice(start);
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
  const answerPath = path.join(dir, "answer.txt");
  const partialPath = path.join(dir, "partial.txt");
  const started = Date.now();
  const trace = createTrace(eventsPath);
  trace.startedAt = started;
  trace.label = entry.label;
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
      profile: entry.profile.name, label: entry.label,
      resumed_from: entry.resume?.token ?? null,
      started_at: new Date().toISOString(),
    }));
  } catch (error) {
    const failure = new SubagentError("ARCHIVE_ERROR",
      `cannot write evidence: ${error.message}`, true);
    context.run.abort(failure);
    return {index: entry.index, status: "failed", exit_code: null,
      error: {code: "ARCHIVE_ERROR", message: failure.message},
      duration_ms: Date.now() - started, tool_calls: 0, usage: usageReport(trace.usage)};
  }
  context.run.status[entry.index] = "running";
  context.run.live[entry.index] = trace;
  progress(context, `▷ [${entry.index + 1}/${context.count}] ${oneLine(entry.label, 48)}`);

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
    let child = null;
    let timedOut = null;
    let closeTimer = null;
    let taskTimer = null;

    const armCloseFallback = outcome => {
      if (closeTimer || finished) return;
      closeTimer = setTimeout(() => {
        child?.stdout?.destroy?.();
        child?.stderr?.destroy?.();
        finish(outcome);
      }, context.killGraceMs + 100);
    };

    const failByTimeout = message => {
      if (finished || trace.fault || timedOut) return;
      timedOut = {status: "failed", code: "CHILD_TIMEOUT", exit_code: null, message};
      context.run.terminate(child);
      armCloseFallback(timedOut);
    };

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
      if (USAGE_FRAME_FIELDS[frame.type]) {
        recordUsage(context.run.usage, frame);
      }
      // Live state only: the heartbeat renders one row per running task, so
      // every child event must not become a row of its own.
      trace.lastEventAt = Date.now();
      if (frame.type === "model_started") {
        trace.phase = "model";
        trace.turn = frame.request ?? trace.turn;
        trace.lastTextAt = 0;
      } else if (frame.type === "tool_started") {
        const at = Date.now();
        trace.tools.set(frame.id, {name: frame.name ?? "tool", at});
        trace.tool = frame.name ?? "tool";
        trace.toolStartedAt = at;
        trace.phase = "tool";
      } else if (frame.type === "tool_finished") {
        trace.tools.delete(frame.id);
        const pendingTool = [...trace.tools.values()].at(-1);
        trace.tool = pendingTool ? pendingTool.name : null;
        trace.toolStartedAt = pendingTool ? pendingTool.at : 0;
        trace.phase = pendingTool ? "tool" : "model";
      }
      if (trace.results > 1) {
        fault(new SubagentError("CHILD_PROTOCOL", "child returned more than one final result", true));
      } else if (trace.results === 1 && typeof frame.text !== "string") {
        fault(new SubagentError("CHILD_PROTOCOL", "child returned a result without text", true));
      }
    };

    // The source evidence stays immutable: only the snapshot and its journal
    // are copied, so a resumed child owns a fresh lock and its own run files.
    if (entry.resume) {
      try {
        copySession(entry.resume.session, session);
      } catch (error) {
        finish({status: "failed", code: "RESUME_STALE",
          message: `cannot copy the saved session for resume: ${error.message}`});
        return;
      }
    }

    const policyPath = path.join(dir, "policy.json");
    try {
      writeFile(policyPath, JSON.stringify({
        profile: entry.profile.name, prompt: entry.profile.prompt,
        tools: entry.profile.tools, disallowedTools: entry.profile.disallowedTools,
        shell: entry.profile.shell, maxTurns: entry.profile.maxTurns,
        maxTokens: entry.profile.maxTokens,
      }));
    } catch (error) {
      finish({status: "failed", code: "ARCHIVE_ERROR",
        message: `cannot write subagent policy: ${error.message}`});
      return;
    }

    try {
      child = context.run.spawn(context.executable, ["--json"], {
        cwd: entry.cwd,
        env: {
          ...process.env,
          KROMA_WORKDIR: entry.cwd,
          KROMA_SESSION_FILE: session,
          KROMA_SUBAGENT_DEPTH: String(context.depth),
          KROMA_SUBAGENT_POLICY_FILE: policyPath,
          ...(profileModel(entry, context.config, context.model) ?
            {KROMA_MODEL: profileModel(entry, context.config, context.model)} : {}),
          ...((entry.profile.thinking || context.thinking) ?
            {KROMA_DEEPSEEK_THINKING: entry.profile.thinking || context.thinking} : {}),
        },
        windowsHide: true,
        stdio: ["pipe", "pipe", "pipe"],
      });
    } catch (error) {
      // The directory can disappear between validation and spawn.
      finish({status: "failed", code: "CHILD_START",
        message: `cannot start '${context.executable}': ${error.message}; set kroma_bin if Kroma is not on PATH`});
      return;
    }
    context.run.children.add(child);

    taskTimer = setTimeout(
      () => failByTimeout(`child timed out after ${formatTimeout(context.taskTimeoutMs)}`),
      context.taskTimeoutMs,
    );
    context.run.timeouts.add(failByTimeout);

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
      message: `cannot start '${context.executable}': ${error.message}; set kroma_bin if Kroma is not on PATH`,
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
        finish(exitFailure(code, trace.reported ?? lastStderrLine(stderr)));
      } else if (trace.results === 0) finish({status: "failed", code: "CHILD_PROTOCOL", exit_code: 0,
        message: `child exited without a final result${trace.reported ? `: ${trace.reported}` : ""}`});
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
      // A failed child often produced useful text before the deadline. Keep a
      // bounded tail as `output` with `partial: true`; never present it as a
      // verified answer, and never write text deltas into events.ndjson.
      const draft = outcome.status !== "completed" && !answer && trace.draft.trim()
        ? tailClip(trace.draft, context.maxOutput) : null;
      try {
        if (answer) writeFile(answerPath, answer);
        if (draft) writeFile(partialPath, draft.text);
        // The logs are already on disk; only guarantee the artifacts exist.
        for (const file of [stderrPath, eventsPath]) {
          if (!fs.existsSync(file)) writeFile(file, "");
        }
        writeFile(path.join(dir, "state.json"), JSON.stringify({
          index: entry.index, status: outcome.status, code: outcome.code,
          profile: entry.profile.name, label: entry.label,
          session, cwd: entry.cwd, task_status: trace.taskStatus,
          resumed_from: entry.resume?.token ?? null,
          tool_calls: trace.toolCalls, text_chars: trace.textChars,
          partial_chars: draft ? draft.text.length : 0,
          duration_ms: Date.now() - started,
        }));
      } catch (error) {
        // Evidence is part of the contract: without it the result is untrusted.
        const failure = new SubagentError("ARCHIVE_ERROR",
          `cannot write evidence: ${error.message}`, true);
        context.run.abort(failure);
        outcome = {status: "failed", code: "ARCHIVE_ERROR", exit_code: null, message: failure.message};
      }
      const result = {index: entry.index, profile: entry.profile.name, label: entry.label,
        status: outcome.status,
        exit_code: outcome.exit_code ?? null};
      if (outcome.status === "completed") {
        result.output = clipped.text;
        result.output_truncated = clipped.truncated;
      } else {
        if (draft) {
          result.output = draft.text;
          result.output_truncated = draft.truncated;
          result.partial = true;
        } else if (answer) {
          result.output = clipped.text;
          result.output_truncated = clipped.truncated;
        }
        const detail = draft ? `${outcome.message ?? outcome.code}; partial answer kept in output`
          : (outcome.message ?? outcome.code ?? outcome.status);
        result.error = {code: outcome.code ?? "FAILED", message: detail};
      }
      result.duration_ms = Date.now() - started;
      result.usage = usageReport(trace.usage);
      result.tool_calls = trace.toolCalls;
      result.resume = `${context.runId}/${entry.index}`;
      result.artifacts = dir;
      const seconds = ((Date.now() - started) / 1000).toFixed(1).padStart(5);
      const mark = outcome.status === "completed" ? "✓" : "✗";
      const detail = outcome.status === "completed"
        ? usageLine(trace.usage)
        : `${outcome.code ?? outcome.status}${draft ? ` · partial ${compactChars(draft.text.length)}` : ""}`;
      const previewText = outcome.status === "completed" ? answer : (draft?.text ?? "");
      const preview = previewText.trim() ? ` · "${oneLine(previewText, 40)}"` : "";
      context.run.status[entry.index] = outcome.status;
      context.run.live[entry.index] = null;
      progress(context, `${mark} [${entry.index + 1}/${context.count}] ${seconds}s · ${detail}${preview}`);
      resolve(result);
    }
  });
}

// The host reads one bounded response, so a large batch must share the budget
// instead of collapsing every answer to the same fixed floor. Binary-search
// the largest per-field character limit that fits the pretty JSON actually
// sent to the host, then mark exactly the outputs that were shortened.
function fit(response) {
  const target = LIMITS.responseBytes;
  const fields = [];
  const measure = limit => {
    for (const field of fields) {
      const shorten = field.tail ? tailClip : clip;
      field.owner[field.key] = shorten(field.original, limit).text;
    }
    return Buffer.byteLength(JSON.stringify(response, null, 2), "utf8");
  };
  if (Buffer.byteLength(JSON.stringify(response, null, 2), "utf8") <= target) {
    return response;
  }
  for (const result of response.results) {
    if (typeof result.output === "string" && result.output) {
      fields.push({owner: result, key: "output", original: result.output,
        output: true, tail: Boolean(result.partial)});
    }
    if (result.error && typeof result.error.message === "string" && result.error.message) {
      fields.push({owner: result.error, key: "message", original: result.error.message,
        output: false, tail: false});
    }
  }
  if (!fields.length) return response;
  let high = 0;
  for (const field of fields) high = Math.max(high, field.original.length);
  let low = 0;
  if (measure(0) <= target) {
    while (low < high) {
      const mid = Math.ceil((low + high) / 2);
      if (measure(mid) <= target) low = mid;
      else high = mid - 1;
    }
  }
  measure(low);
  for (const field of fields) {
    if (field.output) {
      const shorten = field.tail ? tailClip : clip;
      const shortened = shorten(field.original, low).truncated;
      field.owner.output_truncated = Boolean(field.owner.output_truncated || shortened);
    }
  }
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
  const rawDataDir = options.dataDir ?? request?.data_dir;
  const dataDir = rawDataDir ? localPath(rawDataDir) : null;
  if (!dataDir) throw new SubagentError("ARCHIVE_ERROR", "plugin data directory is unavailable", true);
  // Resolve profiles and validate every task before creating run evidence or
  // starting children. Invalid definitions must not start half a batch.
  let profiles;
  try {
    profiles = loadProfiles(workdir, options.profileDirs);
  } catch (error) {
    throw new SubagentError("PROFILE_INVALID", error.message);
  }
  const entries = tasks(request, workdir, dataDir, profiles);

  const id = newRunId();
  const runDir = path.join(dataDir, "runs", id);
  try {
    fs.mkdirSync(runDir, {recursive: true});
    writeFile(path.join(runDir, "request.json"), JSON.stringify({
      call_id: request?.call_id ?? null, archive: request?.archive ?? null, workdir,
      max_parallel: maxParallel, max_output_chars: maxOutput,
      task_timeout_secs: taskTimeoutSecs, model, thinking,
      tasks: entries.map(entry => ({index: entry.index, task: entry.task, cwd: entry.cwd,
        profile: entry.profile.name, label: entry.label, complexity: entry.complexity,
        resume: entry.resume?.token ?? null})),
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
  const context = {runId: id, runDir, executable, model, thinking, config, maxOutput,
    count: entries.length, depth: depth + 1, deadlineSecs, taskTimeoutMs, killGraceMs, run,
    onProgress: options.onProgress};

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
    const preview = live.phase === "model" && live.lastTextAt && live.draft.trim()
      && now - live.lastTextAt < PREVIEW_MS
      ? ` · "${oneLine(tailChars(live.draft, 120), 36)}"` : "";
    return `[${index + 1}/${context.count}] ${oneLine(live.label, 18)} ${clock(now - live.startedAt)}`
      + ` ▶ ${phase} · ${compactTokens(tokens)} tok${preview}${quiet}`;
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
      results[index] = {index, status: "not_started", exit_code: null, tool_calls: 0, error: {
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
