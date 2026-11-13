"use strict";

// Drives the real plugin process over JSONL, exactly like Kroma does.
const { test } = require("node:test");
const assert = require("node:assert/strict");
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const CONFIG = { live_status: true, widget: true, finish_guard: false };

function harness() {
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-todo-"));
  const child = spawn(process.execPath, [path.join(__dirname, "main.js")], {
    stdio: ["pipe", "pipe", "inherit"],
  });
  const waiting = [];
  let buffer = "";
  child.stdout.on("data", (chunk) => {
    buffer += chunk.toString("utf8");
    let at;
    while ((at = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, at);
      buffer = buffer.slice(at + 1);
      if (!line.trim()) continue;
      const frame = JSON.parse(line);
      const next = waiting.shift();
      assert.ok(next, `unexpected frame ${line}`);
      next(frame);
    }
  });
  let id = 0;
  const send = (message) =>
    new Promise((resolve) => {
      waiting.push(resolve);
      child.stdin.write(`${JSON.stringify(message)}\n`);
    });
  /** Answer every host call in the reply stream, then return the final frame. */
  const call = async (message, config = CONFIG) => {
    const request = {
      protocol_version: 2,
      request_id: ++id,
      data_dir: dataDir,
      session_id: "s1",
      config,
      ...message,
    };
    const hostCalls = [];
    let frame = await send(request);
    while (frame.type === "host_call") {
      hostCalls.push(frame);
      frame = await send({
        type: "host_call_result",
        request_id: frame.request_id,
        result: { ok: true },
      });
    }
    frame.hostCalls = hostCalls;
    return frame;
  };
  const close = () => {
    child.kill();
    fs.rmSync(dataDir, { recursive: true, force: true });
  };
  return { call, close };
}

const list = (items) => ({ type: "tool", name: "todo_write", arguments: { items } });

test("the model writes a checklist that the person can then finish by index", async () => {
  const todo = harness();
  try {
    const started = await todo.call(
      list([
        { text: "读 src/agent.rs", status: "doing" },
        { text: "改 todo 插件" },
        { text: "跑测试" },
      ]),
    );
    assert.match(started.content, /清单已更新（0\/3 完成）/);
    assert.match(started.content, /1\. \[>\] 读 src\/agent\.rs/);
    const status = started.hostCalls.find((call) => call.name === "ui.status");
    assert.equal(status.arguments.text, "📋 清单 0/3 · 进行中：读 src/agent.rs");
    const widget = started.hostCalls.find((call) => call.name === "ui.widget");
    assert.equal(widget.arguments.id, "checklist");
    assert.equal(widget.arguments.blocks[0].type, "progress");

    const done = await todo.call({ type: "command", name: "/todo", arguments: "done 1,3" });
    assert.match(done.content, /已把 1、3 标为 done/);
    const line = done.hostCalls.find((call) => call.name === "ui.status");
    assert.equal(line.arguments.text, "📋 清单 2/3");

    const listed = await todo.call({ type: "command", name: "/todo", arguments: "" });
    assert.match(listed.content, /2\. \[ \] 改 todo 插件/);
    assert.match(listed.content, /任务清单（2\/3 完成）/);

    const removed = await todo.call({ type: "command", name: "/todo", arguments: "rm 2" });
    assert.match(removed.content, /已删除：\n- 改 todo 插件/);
    assert.match(removed.content, /任务清单（2\/2 完成）/);
  } finally {
    todo.close();
  }
});

test("state is per session: another conversation starts empty", async () => {
  const todo = harness();
  try {
    await todo.call({ type: "command", name: "/todo", arguments: "买牛奶" });
    const same = await todo.call({ type: "command", name: "/todo", arguments: "" });
    assert.match(same.content, /1\. \[ \] 买牛奶/);
    const other = await todo.call({ type: "command", name: "/todo", arguments: "", session_id: "s2" });
    assert.match(other.content, /还没有任务清单/);
  } finally {
    todo.close();
  }
});

test("bad arguments are refused before anything is stored", async () => {
  const todo = harness();
  try {
    const missing = await todo.call({ type: "tool", name: "todo_write", arguments: {} });
    assert.equal(missing.is_error, true);
    assert.match(missing.content, /需要 items 数组/);

    const status = await todo.call(list([{ text: "x", status: "finished" }]));
    assert.equal(status.is_error, true);
    assert.match(status.content, /status 只能是/);

    const blank = await todo.call(list([{ text: "   " }]));
    assert.equal(blank.is_error, true);

    const duplicate = await todo.call(list([{ text: "同一件事" }, { text: "同一件事" }]));
    assert.equal(duplicate.is_error, true);
    assert.match(duplicate.content, /重复/);

    const empty = await todo.call({ type: "tool", name: "todo_write", arguments: { items: [] } });
    assert.equal(empty.content, "清单已清空。");
  } finally {
    todo.close();
  }
});

test("the panel returns host-drawn blocks and the widget clears when empty", async () => {
  const todo = harness();
  try {
    const blocks = JSON.parse((await todo.call({ type: "ui", name: "panel" })).content).blocks;
    assert.match(blocks[1].text, /还没有清单/);

    await todo.call(list([{ text: "写 README" }, { text: "跑 node --test", status: "done" }]));
    const filled = JSON.parse((await todo.call({ type: "ui", name: "panel" })).content).blocks;
    assert.equal(filled[0].text, "任务清单 1/2");
    assert.deepEqual(filled[2].items, ["1. [ ] 写 README", "2. [x] 跑 node --test"]);

    const cleared = await todo.call({ type: "ui_action", action_id: "clear_all" });
    assert.equal(cleared.content, "清单已清空。");
    const widget = cleared.hostCalls.find((call) => call.name === "ui.widget");
    assert.equal(widget.arguments.clear, true);
  } finally {
    todo.close();
  }
});

test("finish_guard only holds a finish for a list written in this turn", async () => {
  const todo = harness();
  const guard = { ...CONFIG, finish_guard: true };
  const hook = (tools) =>
    todo.call({ type: "hook", name: "before_turn_end", arguments: { final_text: "done", tools } }, guard);
  try {
    await todo.call(list([{ text: "改代码" }, { text: "跑测试" }]));
    const wrote = await hook([{ name: "todo_write" }]);
    const refusal = JSON.parse(wrote.content);
    assert.equal(refusal.decision, "continue");
    assert.match(refusal.feedback, /\[ \] 改代码/);
    assert.match(refusal.feedback, /\[ \] 跑测试/);

    // A stale checklist must not nag every later turn.
    const unrelated = await hook([{ name: "shell" }]);
    assert.equal(JSON.parse(unrelated.content).decision, "finish");

    await todo.call({ type: "command", name: "/todo", arguments: "done 1 2" });
    const finished = await hook([{ name: "todo_write" }]);
    assert.equal(JSON.parse(finished.content).decision, "finish");

    // The default setting is off even with an unfinished list.
    await todo.call({ type: "command", name: "/todo", arguments: "再补一项" });
    const off = await todo.call({
      type: "hook",
      name: "before_turn_end",
      arguments: { final_text: "done", tools: [{ name: "todo_write" }] },
    });
    assert.equal(JSON.parse(off.content).decision, "finish");
  } finally {
    todo.close();
  }
});

test("/todo plan hands one real task to the host instead of starting a second loop", async () => {
  const todo = harness();
  try {
    const planned = await todo.call({ type: "command", name: "/todo", arguments: "plan 把插件装上" });
    assert.match(planned.content, /已派发清单规划/);
    assert.match(planned.task, /todo_write/);
    assert.match(planned.task, /目标：把插件装上/);
    const usage = await todo.call({ type: "command", name: "/todo", arguments: "plan" });
    assert.equal(usage.task, undefined);
  } finally {
    todo.close();
  }
});
