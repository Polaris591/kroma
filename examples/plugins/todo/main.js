"use strict";

// Todo plugin: one checklist per conversation.
//
// Kroma's core knows nothing about todos. This plugin uses generic seams only:
//   * `todo_write` — the model writes the whole list (plan, then status);
//   * `/todo ...` — the person lists and edits the same list;
//   * `ui.status` / `ui.widget` — the live summary line and the checklist card;
//   * a dynamic `ui` panel for `/plugins view todo`;
//   * `before_turn_end` — optional `finish_guard`, which only refuses a finish
//     when the model wrote a checklist in this very turn and left items open.
// State is one file per session under `data_dir`, so another conversation, a
// fork or a rewind never inherits this checklist.

const fs = require("node:fs");
const path = require("node:path");
const { createInterface } = require("node:readline");

const PENDING = "pending";
const DOING = "doing";
const DONE = "done";
const STATUSES = [PENDING, DOING, DONE];
const MARK = { [PENDING]: "[ ]", [DOING]: "[>]", [DONE]: "[x]" };
const MAX_ITEMS = 50;
const TEXT_CHARS = 200;
const WIDGET_ITEMS = 12;
const WIDGET_ID = "checklist";
const USAGE = [
  "用法：",
  "/todo                查看清单",
  "/todo <文字>          加一项（也可以写 /todo add <文字>）",
  "/todo done <编号…>    标记完成",
  "/todo doing <编号…>   标记进行中",
  "/todo undo <编号…>    退回待办",
  "/todo rm <编号…>      删除",
  "/todo clear          清空清单",
  "/todo plan <目标>     让 AI 把目标拆成清单并开干",
].join("\n");

/** Last status line pushed per session, so an unchanged line is not re-sent. */
const pushed = new Map();
/** Host calls waiting for their `host_call_result`. */
const waiting = new Map();
let hostCalls = 0;

const clip = (value, max) => {
  const text = String(value ?? "").replace(/\s+/g, " ").trim();
  return text.length <= max ? text : `${text.slice(0, Math.max(0, max - 1))}…`;
};

function stateFile(request) {
  const id = String(request.session_id || "default")
    .replace(/[^A-Za-z0-9_-]/g, "_")
    .slice(0, 80);
  return path.join(request.data_dir || ".", `todo-${id}.json`);
}

/** Read a list written by an older or a hand-edited version without trusting it. */
function normalize(value) {
  const items = Array.isArray(value?.items) ? value.items : [];
  return {
    items: items
      .map((item) => ({
        text: clip(item?.text, TEXT_CHARS),
        status: STATUSES.includes(item?.status) ? item.status : PENDING,
      }))
      .filter((item) => item.text)
      .slice(0, MAX_ITEMS),
  };
}

function readState(request) {
  try {
    return normalize(JSON.parse(fs.readFileSync(stateFile(request), "utf8")));
  } catch {
    return { items: [] };
  }
}

function writeState(request, state) {
  const target = stateFile(request);
  const dir = path.dirname(target);
  fs.mkdirSync(dir, { recursive: true });
  const body = { items: state.items, updated_at: new Date().toISOString() };
  const temp = path.join(dir, `.todo-${process.pid}-${Date.now()}.tmp`);
  fs.writeFileSync(temp, JSON.stringify(body, null, 2));
  fs.renameSync(temp, target);
}

const doneCount = (state) => state.items.filter((item) => item.status === DONE).length;

function listLines(state) {
  return state.items.map((item, index) => `${index + 1}. ${MARK[item.status]} ${item.text}`);
}

function statusLine(state) {
  if (!state.items.length) return "";
  const done = doneCount(state);
  const head =
    done === state.items.length ? `✅ 清单完成 ${done}/${state.items.length}` : `📋 清单 ${done}/${state.items.length}`;
  const doing = state.items.find((item) => item.status === DOING);
  return doing ? `${head} · 进行中：${clip(doing.text, 40)}` : head;
}

function describe(state) {
  if (!state.items.length) return `当前对话还没有任务清单。\n${USAGE}`;
  return [
    `任务清单（${doneCount(state)}/${state.items.length} 完成）`,
    ...listLines(state),
    "",
    "模型用 todo_write 整体更新；你也可以用 /todo 改，编号就是上面的序号。",
  ].join("\n");
}

function widgetArgs(state) {
  const shown = state.items.slice(0, WIDGET_ITEMS);
  const blocks = [
    { type: "progress", label: "完成", value: doneCount(state), max: state.items.length },
    { type: "list", items: shown.map((item) => `${MARK[item.status]} ${item.text}`) },
  ];
  const hidden = state.items.length - shown.length;
  if (hidden > 0) blocks.push({ type: "text", text: `…还有 ${hidden} 项，/todo 查看全部` });
  return {
    id: WIDGET_ID,
    placement: "above_composer",
    title: "任务清单",
    blocks,
    actions: [
      { id: "clear_done", label: "清除已完成" },
      { id: "clear_all", label: "清空", danger: true },
    ],
  };
}

function writeFrame(frame) {
  process.stdout.write(`${JSON.stringify(frame)}\n`);
}

/** One host call, resolved by the matching `host_call_result` line. */
function hostCall(name, args) {
  return new Promise((resolve) => {
    const id = `todo-${++hostCalls}`;
    waiting.set(id, resolve);
    writeFrame({ type: "host_call", request_id: id, name, arguments: args });
  });
}

async function publish(request, state, changed) {
  if (request.config?.live_status !== false) {
    const key = String(request.session_id || "default");
    const line = statusLine(state);
    if (pushed.get(key) !== line) {
      const ack = await hostCall("ui.status", { text: line });
      if (ack && ack.ok !== false && !ack.error) {
        pushed.set(key, line);
      }
    }
  }
  if (changed && request.config?.widget !== false) {
    await hostCall("ui.widget", state.items.length ? widgetArgs(state) : { id: WIDGET_ID, clear: true });
  }
}

function parseIndexes(tokens, total) {
  const indexes = [];
  for (const token of tokens.join(" ").split(/[\s,，]+/).filter(Boolean)) {
    const number = Number(token);
    if (!Number.isInteger(number) || number < 1 || number > total) {
      return { error: `编号 ${token} 不在这张清单里（1..${total}）。`, indexes: [] };
    }
    indexes.push(number - 1);
  }
  return { error: "", indexes: [...new Set(indexes)].sort((a, b) => a - b) };
}

function command(request) {
  const args = String(request.arguments ?? "").trim();
  const state = readState(request);
  const [verb, ...rest] = args.split(/\s+/).filter(Boolean);
  const tail = rest.join(" ");

  if (!args || verb === "status" || verb === "list") {
    return { content: describe(state) };
  }

  if (verb === "plan") {
    if (!tail) return { content: "用法：/todo plan <目标>" };
    return {
      content: `已派发清单规划：${tail}`,
      task: [
        "把下面这个目标拆成一份可执行的待办清单，用 todo_write 一次写入（每项动词开头、一句话，最多 8 项，状态 pending）。",
        "写完按顺序开始第一项：推进中把当前项标 doing，做完标 done，全部完成再收尾。",
        `目标：${tail}`,
      ].join("\n"),
    };
  }

  if (verb === "add" || !["done", "doing", "undo", "rm", "clear"].includes(verb)) {
    // Bare text is an item, so `/todo 买牛奶` does what it looks like.
    const text = clip(verb === "add" ? tail : args, TEXT_CHARS);
    if (!text) return { content: "用法：/todo add <文字>" };
    if (state.items.length >= MAX_ITEMS) {
      return { content: `清单最多 ${MAX_ITEMS} 项，先删掉一些。\n${describe(state)}` };
    }
    state.items.push({ text, status: PENDING });
    writeState(request, state);
    return { content: `已加入：${text}\n${describe(state)}`, changed: true };
  }

  if (verb === "clear") {
    if (!state.items.length) return { content: "清单已经是空的。" };
    state.items = [];
    writeState(request, state);
    return { content: "清单已清空。", changed: true };
  }

  const picked = parseIndexes(rest, state.items.length);
  if (picked.error) return { content: `${picked.error}\n${describe(state)}` };
  if (!picked.indexes.length) {
    return { content: `用法：/todo ${verb} <编号>（1..${state.items.length}）\n${describe(state)}` };
  }

  if (verb === "rm") {
    const removed = picked.indexes.map((index) => state.items[index].text);
    state.items = state.items.filter((_, index) => !picked.indexes.includes(index));
    writeState(request, state);
    return {
      content: `已删除：\n${removed.map((text) => `- ${text}`).join("\n")}\n${describe(state)}`,
      changed: true,
    };
  }

  const status = verb === "done" ? DONE : verb === "doing" ? DOING : PENDING;
  for (const index of picked.indexes) state.items[index].status = status;
  writeState(request, state);
  return {
    content: `已把 ${picked.indexes.map((index) => index + 1).join("、")} 标为 ${status}。\n${describe(state)}`,
    changed: true,
  };
}

/** `todo_write`: the whole list, validated before anything is stored. */
function writeTool(request) {
  const raw = request.arguments?.items;
  if (!Array.isArray(raw)) return { content: "todo_write 需要 items 数组。", is_error: true };
  if (raw.length > MAX_ITEMS) return { content: `清单最多 ${MAX_ITEMS} 项。`, is_error: true };
  const items = [];
  const seen = new Set();
  for (const [index, entry] of raw.entries()) {
    const text = clip(entry?.text, TEXT_CHARS);
    if (!text) return { content: `第 ${index + 1} 项缺少 text。`, is_error: true };
    const status = entry?.status === undefined ? PENDING : String(entry.status);
    if (!STATUSES.includes(status)) {
      return { content: `第 ${index + 1} 项的 status 只能是 ${STATUSES.join("、")}。`, is_error: true };
    }
    if (seen.has(text)) return { content: `第 ${index + 1} 项与前面重复：${text}`, is_error: true };
    seen.add(text);
    items.push({ text, status });
  }
  const state = { items };
  writeState(request, state);
  if (!items.length) return { content: "清单已清空。", changed: true };
  return {
    content: `清单已更新（${doneCount(state)}/${items.length} 完成）：\n${listLines(state).join("\n")}`,
    changed: true,
  };
}

/** `/plugins view todo`: structured blocks, laid out by the host. */
function panel(request) {
  const state = readState(request);
  if (!state.items.length) {
    return JSON.stringify({
      blocks: [
        { type: "heading", text: "任务清单" },
        { type: "text", text: "当前对话还没有清单。让模型用 todo_write 记录计划，或用 /todo add <文字> 手动加一项。" },
        { type: "divider" },
        { type: "text", text: USAGE },
      ],
    });
  }
  return JSON.stringify({
    blocks: [
      { type: "heading", text: `任务清单 ${doneCount(state)}/${state.items.length}` },
      { type: "progress", label: "完成", value: doneCount(state), max: state.items.length },
      { type: "list", items: listLines(state) },
      { type: "divider" },
      { type: "text", text: "/todo done|doing|undo|rm <编号> 直接改；todo_write 由模型整体更新。" },
    ],
  });
}

/** Optional `finish_guard`: only a checklist written in this turn can hold it. */
function gate(request) {
  const state = readState(request);
  const tools = Array.isArray(request.arguments?.tools) ? request.arguments.tools : [];
  const wrote = tools.some((tool) => tool && tool.name === "todo_write");
  const remaining = state.items.filter((item) => item.status !== DONE);
  if (request.config?.finish_guard !== true || !wrote || !remaining.length) {
    return JSON.stringify({ decision: "finish" });
  }
  const feedback = [
    "本轮刚写过任务清单，但还有没做完的项：",
    ...remaining.slice(0, 8).map((item) => `${MARK[item.status]} ${item.text}`),
    remaining.length > 8 ? `…还有 ${remaining.length - 8} 项，用 /todo 查看。` : "",
    "继续做完并同步状态；确实不做就用 todo_write 移除或标为 done，别让清单停在半路。",
  ]
    .filter(Boolean)
    .join("\n");
  return JSON.stringify({ decision: "continue", feedback });
}

/** Checklist card buttons (terminal and desktop). */
async function action(request) {
  const state = readState(request);
  if (request.action_id === "clear_done") {
    state.items = state.items.filter((item) => item.status !== DONE);
    writeState(request, state);
    await publish(request, state, true);
    return state.items.length ? `已清除完成项，还剩 ${state.items.length} 项。` : "已清除完成项，清单空了。";
  }
  if (request.action_id === "clear_all") {
    const empty = { items: [] };
    writeState(request, empty);
    await publish(request, empty, true);
    return "清单已清空。";
  }
  return "未知操作。";
}

function handleEvent(request) {
  if (request.name !== "session_cleared" && request.name !== "session_rewound") {
    return { content: "{}" };
  }
  const state = readState(request);
  if (!state.items.length) return { content: "{}" };
  writeState(request, { items: [] });
  return { content: "{}", changed: true };
}

async function run(request) {
  let out = { content: "" };
  try {
    if (request.type === "ping") {
      out = { content: "pong" };
    } else if (request.type === "command") {
      out = command(request);
    } else if (request.type === "tool") {
      out =
        request.name === "todo_write"
          ? writeTool(request)
          : { content: `未知工具 ${request.name}`, is_error: true };
    } else if (request.type === "ui") {
      out = { content: panel(request) };
    } else if (request.type === "hook") {
      out = { content: request.name === "before_turn_end" ? gate(request) : "{}" };
    } else if (request.type === "event") {
      out = handleEvent(request);
    } else if (request.type === "ui_action") {
      out = { content: await action(request) };
    }
    if (!out.is_error && (request.type === "command" || request.type === "tool" || request.type === "event")) {
      // Live UI is optional: a failed host call must never fail the call itself.
      try {
        await publish(request, readState(request), out.changed === true);
      } catch {
        /* no interface, or the UI bus is gone; the answer still stands */
      }
    }
  } catch (error) {
    out = { content: String(error?.message ?? error), is_error: true };
  }
  const reply = {
    protocol_version: request.protocol_version,
    request_id: request.request_id,
    content: out.content,
  };
  if (out.task) reply.task = out.task;
  if (out.is_error) reply.is_error = true;
  writeFrame(reply);
}

let chain = Promise.resolve();
createInterface({ input: process.stdin, crlfDelay: Infinity }).on("line", (line) => {
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    return;
  }
  if (request.type === "host_call_result") {
    const resolve = waiting.get(request.request_id);
    if (resolve) {
      waiting.delete(request.request_id);
      resolve(request.result ?? {});
    }
    return;
  }
  chain = chain.then(() => run(request)).catch(() => {});
});
