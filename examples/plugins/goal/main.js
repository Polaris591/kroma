"use strict";

// Goal plugin, self-contained.
//
// Kroma's core knows nothing about goals. It only offers generic seams:
//   * `/goal ...` may return a `task`, which starts one real user task;
//   * the `before_turn_end` hook may refuse to finish a turn and hand the model
//     one short piece of feedback;
//   * `ui.status` repaints this plugin's own status line.
// All goal state lives in this plugin's per-session file under `data_dir`, so a
// different conversation (or a forked archive) never inherits another goal.

const fs = require("node:fs");
const path = require("node:path");
const { createInterface } = require("node:readline");

const RUNNING = "running";
const PAUSED = "paused";
const COMPLETED = "completed";
const CANCELLED = "cancelled";
const CLAIMS = ["working", "done", "blocked"];
const SUMMARY_CHARS = 2000;
const NEXT_STEP_CHARS = 1000;
const USAGE = [
  "用法：",
  "/goal <目标正文>     开始目标并直接开干",
  "/goal [status]       查看当前目标",
  "/goal cancel         放弃当前目标（不删除已完成的工作）",
].join("\n");

/** A reply held back until the host acknowledges one `ui.status` call. */
let pending = null;
/** Last status line pushed per session, so an unchanged line is not re-sent. */
const pushed = new Map();

const clip = (value, max) => {
  const text = String(value ?? "").replace(/\s+/g, " ").trim();
  return text.length <= max ? text : `${text.slice(0, Math.max(0, max - 1))}…`;
};

function stateFile(request) {
  const id = String(request.session_id || "default")
    .replace(/[^A-Za-z0-9_-]/g, "_")
    .slice(0, 80);
  return path.join(request.data_dir || ".", `goal-${id}.json`);
}

function readState(request) {
  try {
    const value = JSON.parse(fs.readFileSync(stateFile(request), "utf8"));
    return value && typeof value === "object" ? value : { status: "none" };
  } catch {
    return { status: "none" };
  }
}

function writeState(request, state) {
  fs.mkdirSync(path.dirname(stateFile(request)), { recursive: true });
  fs.writeFileSync(stateFile(request), JSON.stringify(state, null, 2));
}

function statusLine(state) {
  if (state.status === RUNNING) return `🎯 目标进行中：${clip(state.text, 60)}`;
  if (state.status === PAUSED) return `⏸ 目标已停（blocked）：${clip(state.text, 60)}`;
  return "";
}

function describe(state) {
  if (state.status === "none" || !state.text) {
    return `当前没有活动目标。\n${USAGE}`;
  }
  const lines = [`目标（${state.status}）：${state.text}`];
  if (state.summary) lines.push(`最近进展：${state.summary}`);
  if (state.next_step) lines.push(`下一步：${state.next_step}`);
  if (state.block_reason) lines.push(`阻塞：${state.block_reason}`);
  if (state.revision) lines.push(`记录轮次：${state.revision}`);
  return lines.join("\n");
}

/** `/goal ...` one-shot: text reply, or a task the host must run. */
function command(request) {
  const args = String(request.arguments ?? "").trim();
  const state = readState(request);
  const verb = args.split(/\s+/)[0].toLowerCase();

  if (!args || verb === "status") {
    return { content: describe(state), status: statusLine(state) };
  }
  if (verb === "cancel") {
    if (state.status === "none" || state.status === CANCELLED) {
      return { content: "没有可取消的目标。", status: "" };
    }
    state.status = CANCELLED;
    state.summary = state.summary || "";
    writeState(request, state);
    return { content: `已取消：${state.text}。已完成的工作不会删除。`, status: "" };
  }
  if (verb === "pause" || verb === "resume") {
    // Kept as an explicit answer instead of becoming a goal named "pause":
    // stopping a turn is the terminal's Ctrl+C, and cancel ends a goal.
    return {
      content: "pause/resume 已移除。停手用 Ctrl+C；结束目标用 /goal cancel；想继续直接说要求，或重新 /goal。",
      status: statusLine(state),
    };
  }

  // A new goal. The text becomes the user turn, so the model starts at once.
  const text = clip(args, SUMMARY_CHARS);
  const next = {
    status: RUNNING,
    text,
    summary: "",
    next_step: "",
    block_reason: "",
    revision: 1,
    updated_at: new Date().toISOString(),
  };
  writeState(request, next);
  return { content: `开始目标：${text}`, task: text, status: statusLine(next) };
}

/** `goal_checkpoint`: the only way to end or block a goal. */
function checkpoint(request) {
  const state = readState(request);
  if (state.status !== RUNNING && state.status !== PAUSED) {
    return { content: "当前没有活动目标，先 /goal <目标> 开始。", status: statusLine(state) };
  }
  const args = request.arguments || {};
  const summary = clip(args.summary, SUMMARY_CHARS);
  const claim = String(args.claim ?? "").trim();
  if (!summary) return { content: "goal_checkpoint 需要 summary。", status: statusLine(state) };
  if (!CLAIMS.includes(claim)) {
    return { content: `claim 只能是 ${CLAIMS.join("、")}。`, status: statusLine(state) };
  }
  state.summary = summary;
  state.next_step = clip(args.next_step, NEXT_STEP_CHARS);
  state.block_reason = clip(args.block_reason, NEXT_STEP_CHARS);
  state.revision = (Number(state.revision) || 1) + 1;
  state.updated_at = new Date().toISOString();
  if (claim === "done") state.status = COMPLETED;
  else if (claim === "blocked") state.status = PAUSED;
  else state.status = RUNNING;
  writeState(request, state);
  const lines = [`已记录（${state.status}）：${summary}`];
  if (state.next_step) lines.push(`下一步：${state.next_step}`);
  if (state.block_reason) lines.push(`阻塞：${state.block_reason}`);
  return { content: lines.join("\n"), status: statusLine(state) };
}

/** `before_turn_end`: refuse to finish while the goal is still running. */
function gate(request) {
  const state = readState(request);
  if (state.status !== RUNNING) {
    return { content: JSON.stringify({ decision: "finish" }), status: statusLine(state) };
  }
  const tools = Array.isArray(request.arguments?.tools) ? request.arguments.tools : [];
  const ran = tools
    .map((tool) => tool && tool.name)
    .filter(Boolean)
    .slice(-5);
  const feedback = [
    `目标还没结束：${state.text}`,
    state.next_step ? `上一步留下的下一步：${state.next_step}` : "",
    ran.length ? `这一轮实际调用过：${ran.join("、")}` : "这一轮没有调用任何工具",
    "继续推进；用 goal_checkpoint 记录（claim=done 表示完成，claim=blocked 表示卡住并暂停目标）。",
  ]
    .filter(Boolean)
    .join("\n");
  return { content: JSON.stringify({ decision: "continue", feedback }), status: statusLine(state) };
}

function handle(request) {
  switch (request.type) {
    case "ping":
      return { content: "ok" };
    case "command":
      return command(request);
    case "tool": {
      if (request.name && request.name !== "goal_checkpoint") {
        return { content: `未知工具 ${request.name}` };
      }
      return checkpoint(request);
    }
    case "hook":
      if (request.name === "before_turn_end") return gate(request);
      return { content: "{}" };
    default:
      return { content: "{}" };
  }
}

function send(reply) {
  process.stdout.write(`${JSON.stringify(reply)}\n`);
}

createInterface({ input: process.stdin }).on("line", (line) => {
  const request = JSON.parse(line);
  if (request.type === "host_call_result") {
    const held = pending;
    pending = null;
    if (held) send(held);
    return;
  }
  const out = handle(request);
  const reply = {
    protocol_version: request.protocol_version,
    request_id: request.request_id,
    content: out.content,
  };
  if (out.task) reply.task = out.task;
  const live = request.config?.live_status !== false;
  const key = String(request.session_id || "default");
  const status = typeof out.status === "string" ? out.status : "";
  if (!live || pushed.get(key) === status) {
    send(reply);
    return;
  }
  pushed.set(key, status);
  pending = reply;
  send({
    type: "host_call",
    request_id: request.request_id,
    name: "ui.status",
    arguments: { text: status },
  });
});
