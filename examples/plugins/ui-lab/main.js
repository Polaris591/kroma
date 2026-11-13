"use strict";

const fs = require("node:fs");
const path = require("node:path");
const readline = require("node:readline");

let customIndex = 0;
const customChoices = ["刷新看板", "填充输入栏", "发送通知"];

function stateFile(req) {
  const id = String(req?.session_id || "default")
    .replace(/[^A-Za-z0-9_-]/g, "_")
    .slice(0, 80);
  return path.join(req?.data_dir || ".", `ui-lab-${id}.json`);
}

function readState(req) {
  try {
    const parsed = JSON.parse(fs.readFileSync(stateFile(req), "utf8"));
    return {
      counter: Number(parsed?.counter) || 0,
      lastTool: String(parsed?.lastTool || "—"),
    };
  } catch {
    return { counter: 0, lastTool: "—" };
  }
}

function writeState(req, next) {
  fs.mkdirSync(path.dirname(stateFile(req)), { recursive: true });
  fs.writeFileSync(stateFile(req), JSON.stringify(next));
}

const pendingHostCalls = new Map();
let nextHostCallId = 1;
const queue = [];
let processing = false;

function writeFrame(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function reply(req, content, extra = {}) {
  writeFrame({
    protocol_version: 2,
    request_id: req.request_id,
    content,
    ...extra,
  });
}

function hostCall(name, args) {
  return new Promise((resolve) => {
    const id = `hc-${nextHostCallId++}`;
    pendingHostCalls.set(id, resolve);
    writeFrame({
      type: "host_call",
      request_id: id,
      name,
      arguments: args,
    });
  });
}

function buildWidget(state, title = "UI Lab", label = "进度", value = state.counter, max = 5) {
  return {
    id: "hud",
    placement: "sidebar",
    title,
    blocks: [
      { type: "keyvalue", rows: [{ key: "点击次数", value: String(state.counter) }, { key: "最近工具", value: state.lastTool }] },
      { type: "progress", label, value, max },
    ],
    actions: [
      { id: "inc", label: "+1 计数", primary: true },
      { id: "prefill", label: "填入输入框" },
      { id: "clear", label: "隐藏", danger: true },
    ],
  };
}

function renderCustomLines(width, height) {
  const w = Math.max(20, Number(width) || 40);
  const lines = [
    [{ text: "UI Lab 自由终端画布", fg: "accent", bold: true }],
    [{ text: "↑/↓ 选择，Enter 确认，Esc 退出", fg: "muted" }],
    [],
    ...customChoices.map((item, idx) => {
      const selected = idx === customIndex;
      return [{
        text: `${selected ? "› " : "  "}${item}`.slice(0, w),
        fg: selected ? "accent" : "text",
        bold: selected,
        reverse: selected,
      }];
    }),
  ];
  return lines.slice(0, Math.max(4, Number(height) || 12));
}

async function handleRequest(req) {
  if (req.type === "ping") {
    return reply(req, "pong");
  }
  if (req.type === "ui_render") {
    customIndex = 0;
    return writeFrame({ lines: renderCustomLines(req.width, req.height), done: false });
  }
  if (req.type === "ui_input") {
    if (req.key === "Up") {
      customIndex = (customIndex - 1 + customChoices.length) % customChoices.length;
      return writeFrame({ lines: renderCustomLines(req.width, req.height), done: false });
    }
    if (req.key === "Down") {
      customIndex = (customIndex + 1) % customChoices.length;
      return writeFrame({ lines: renderCustomLines(req.width, req.height), done: false });
    }
    if (req.key === "Esc") {
      return writeFrame({ done: true, result: null });
    }
    if (req.key === "Enter") {
      return writeFrame({ done: true, result: { choice: customChoices[customIndex] } });
    }
    return writeFrame({ lines: renderCustomLines(req.width, req.height), done: false });
  }
  if (req.type === "ui_close") {
    return;
  }
  const state = readState(req);
  if (req.type === "ui") {
    return reply(req, JSON.stringify({
      blocks: [
        { type: "heading", text: "UI Lab 实时状态" },
        { type: "keyvalue", rows: [{ key: "计数", value: String(state.counter) }, { key: "最近工具", value: state.lastTool }] },
      ],
    }));
  }
  if (req.type === "event") {
    const args = req.arguments || {};
    if (req.name === "tool_started") {
      state.lastTool = String(args.name || "tool");
      writeState(req, state);
    }
    if (req.name === "task_started") {
      await hostCall("ui.status", { text: "UI Lab · 任务执行中" });
    } else if (req.name === "task_finished") {
      await hostCall("ui.status", { text: `UI Lab · 完成 (${state.counter})` });
      await hostCall("ui.desktop_emit", { kind: "state", counter: state.counter, lastTool: state.lastTool });
    }
    return reply(req, "");
  }
  if (req.type === "ui_action") {
    if (req.action_id === "inc") {
      state.counter += 1;
      writeState(req, state);
      await hostCall("ui.widget", buildWidget(state));
      await hostCall("ui.desktop_emit", { kind: "state", counter: state.counter, lastTool: state.lastTool });
      return reply(req, `计数已更新为 ${state.counter}`);
    }
    if (req.action_id === "prefill") {
      await hostCall("ui.composer", { text: `当前 UI Lab 计数是 ${state.counter}`, mode: "replace" });
      return reply(req, "已填入输入框");
    }
    if (req.action_id === "clear") {
      await hostCall("ui.widget", { id: "hud", clear: true });
      return reply(req, "已隐藏挂件");
    }
    return reply(req, "未知操作");
  }
  if (req.type === "ui_message") {
    const p = req.payload || {};
    if (p.action === "increment") {
      state.counter += 1;
      writeState(req, state);
      await hostCall("ui.widget", buildWidget(state));
      await hostCall("ui.status", { text: `UI Lab · 计数 ${state.counter}` });
    }
    return reply(req, JSON.stringify(state));
  }
  if (req.type === "tool") {
    const args = req.arguments || {};
    if (args.clear) {
      await hostCall("ui.widget", { id: "hud", clear: true });
      return reply(req, "cleared");
    }
    await hostCall("ui.widget", buildWidget(state, args.title || "UI Lab", args.label || "进度", Number(args.value ?? state.counter), Number(args.max ?? 5)));
    return reply(req, "widget updated");
  }
  if (req.type === "command") {
    const raw = String(req.arguments || "").trim();
    const [sub, ...rest] = raw.split(/\s+/).filter(Boolean);
    const tail = rest.join(" ");
    if (!sub || sub === "widget") {
      await hostCall("ui.widget", buildWidget(state));
      return reply(req, "已展示 UI Lab 侧边挂件");
    }
    if (sub === "clear") {
      await hostCall("ui.widget", { id: "hud", clear: true });
      await hostCall("ui.status", { text: "" });
      return reply(req, "已清理 UI Lab 挂件与状态");
    }
    if (sub === "status") {
      await hostCall("ui.status", { text: tail });
      return reply(req, tail ? `状态已设为：${tail}` : "已清空状态");
    }
    if (sub === "notify") {
      await hostCall("ui.notify", { message: tail || "来自 UI Lab 的实时通知", level: "info" });
      return reply(req, "通知已发送");
    }
    if (sub === "composer") {
      await hostCall("ui.composer", { text: tail || "请总结当前改动", mode: "replace" });
      return reply(req, "已设置输入框");
    }
    if (sub === "panel") {
      await hostCall("ui.panel", {
        title: "UI Lab 实时面板",
        blocks: [
          { type: "heading", text: "实时数据" },
          { type: "keyvalue", rows: [{ key: "计数", value: String(state.counter) }, { key: "最近工具", value: state.lastTool }] },
        ],
        actions: [{ id: "inc", label: "+1 计数", primary: true }],
      });
      return reply(req, "已打开实时面板");
    }
    if (sub === "custom") {
      const res = await hostCall("ui.custom", { title: "UI Lab 自定义画布", width: 48, height: 12 });
      if (!res.ok) return reply(req, `无法打开自定义画布：${res.error}`);
      if (res.cancelled) return reply(req, "已取消自定义画布");
      return reply(req, `你选择了：${res.result?.choice ?? "无"}`);
    }
    return reply(req, "用法：/uilab [widget|clear|panel|custom|status <字>|notify <字>|composer <字>]");
  }
  return reply(req, "");
}

async function pump() {
  if (processing) return;
  processing = true;
  while (queue.length > 0) {
    const req = queue.shift();
    try {
      await handleRequest(req);
    } catch (err) {
      if (req.request_id !== undefined) {
        writeFrame({ protocol_version: 2, request_id: req.request_id, is_error: true, content: String(err) });
      }
    }
  }
  processing = false;
}

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
rl.on("line", (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.type === "host_call_result") {
    const resolve = pendingHostCalls.get(msg.request_id);
    if (resolve) {
      pendingHostCalls.delete(msg.request_id);
      resolve(msg.result || {});
    }
    return;
  }
  if (msg.type === "ui_render" || msg.type === "ui_input" || msg.type === "ui_close") {
    void handleRequest(msg);
    return;
  }
  queue.push(msg);
  void pump();
});

