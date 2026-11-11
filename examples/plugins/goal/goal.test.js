"use strict";

// Drives the real plugin process over JSONL, exactly like Kroma does.
const { test } = require("node:test");
const assert = require("node:assert/strict");
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

function harness() {
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-goal-"));
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
  /** Answer any host call in the reply stream, then return the final frame. */
  const call = async (message) => {
    const request = { protocol_version: 2, request_id: ++id, data_dir: dataDir, session_id: "s1", ...message };
    let frame = await send(request);
    while (frame.type === "host_call") {
      frame = await send({
        type: "host_call_result",
        request_id: frame.request_id,
        result: { ok: true },
      });
    }
    return frame;
  };
  const close = () => {
    child.kill();
    fs.rmSync(dataDir, { recursive: true, force: true });
  };
  return { call, close };
}

test("a new goal becomes one real task and refuses to finish until done", async () => {
  const goal = harness();
  try {
    const started = await goal.call({ type: "command", name: "/goal", arguments: "fix login" });
    assert.equal(started.task, "fix login");
    assert.match(started.content, /开始目标/);

    const gate = await goal.call({
      type: "hook",
      name: "before_turn_end",
      arguments: { final_text: "done", tools: [{ name: "shell" }] },
    });
    const decision = JSON.parse(gate.content);
    assert.equal(decision.decision, "continue");
    assert.match(decision.feedback, /fix login/);

    const recorded = await goal.call({
      type: "tool",
      name: "goal_checkpoint",
      arguments: { summary: "tests pass", claim: "done" },
    });
    assert.match(recorded.content, /已记录（completed）/);

    const after = await goal.call({
      type: "hook",
      name: "before_turn_end",
      arguments: { final_text: "done", tools: [] },
    });
    assert.equal(JSON.parse(after.content).decision, "finish");
  } finally {
    goal.close();
  }
});

test("pause and resume are gone: stopping is Ctrl+C, ending is /goal cancel", async () => {
  const goal = harness();
  try {
    await goal.call({ type: "command", name: "/goal", arguments: "ship it" });
    const paused = await goal.call({ type: "command", name: "/goal", arguments: "pause" });
    assert.match(paused.content, /已移除/);
    assert.equal(paused.task, undefined, "an old habit must not start a goal called 'pause'");
    const gate = await goal.call({
      type: "hook",
      name: "before_turn_end",
      arguments: { final_text: "ok", tools: [] },
    });
    assert.equal(
      JSON.parse(gate.content).decision,
      "continue",
      "the goal is still running after Ctrl+C stops one turn",
    );

    const resumed = await goal.call({ type: "command", name: "/goal", arguments: "resume 加上回归测试" });
    assert.match(resumed.content, /已移除/);
    const cancelled = await goal.call({ type: "command", name: "/goal", arguments: "cancel" });
    assert.match(cancelled.content, /已取消/);
  } finally {
    goal.close();
  }
});

test("blocked records a pause instead of pretending the goal finished", async () => {
  const goal = harness();
  try {
    await goal.call({ type: "command", name: "/goal", arguments: "deploy" });
    const blocked = await goal.call({
      type: "tool",
      name: "goal_checkpoint",
      arguments: { summary: "缺少凭证", claim: "blocked", block_reason: "no token" },
    });
    assert.match(blocked.content, /已记录（paused）/);
    const status = await goal.call({ type: "command", name: "/goal", arguments: "status" });
    assert.match(status.content, /阻塞：no token/);
  } finally {
    goal.close();
  }
});

test("an invalid claim is refused without changing the goal", async () => {
  const goal = harness();
  try {
    await goal.call({ type: "command", name: "/goal", arguments: "audit" });
    const bad = await goal.call({
      type: "tool",
      name: "goal_checkpoint",
      arguments: { summary: "x", claim: "maybe" },
    });
    assert.match(bad.content, /claim 只能是/);
    const status = await goal.call({ type: "command", name: "/goal", arguments: "status" });
    assert.match(status.content, /running/);
  } finally {
    goal.close();
  }
});
