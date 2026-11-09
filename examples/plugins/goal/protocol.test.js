"use strict";
const {test} = require("node:test");
const assert = require("node:assert/strict");
const {createHandler, PROTOCOL_VERSION} = require("./main");

function sink() {
  const frames = [];
  return {frames, write: frame => frames.push(frame)};
}

async function reply(request) {
  const {frames, write} = sink();
  const line = typeof request === "string" ? request : JSON.stringify({protocol_version: PROTOCOL_VERSION, request_id: 11, ...request});
  await createHandler({write})(line);
  assert.equal(frames.length, 1, "one request produces exactly one frame");
  return frames[0];
}

const USER_INPUT = "修复登录失败，补上回归测试：node --test tests/login.test.js，并通过相关检查";
const CHECK_REF = {message_index: 12, quote: "node --test tests/login.test.js"};

function context(overrides = {}) {
  return {
    status: "running",
    id: "goal-1",
    revision: 1,
    seq: 4,
    user_sources: [{message_index: 12, content: USER_INPUT}],
    checks: [{id: "T1", command: "node --test tests/login.test.js", source: "user", source_ref: CHECK_REF}],
    data: {
      summary: "登录修复进行中",
      claim: "working",
      criteria: [{
        id: "C1", text: "登录恢复", source: "user",
        source_ref: {message_index: 12, quote: "修复登录失败"},
        kind: "judgement", assessment: "pending",
      }],
      next_step: "运行已登记的检查",
      reconciled_revision: 1,
    },
    ...overrides,
  };
}

const doneData = {
  summary: "登录修复与回归测试完成",
  claim: "done",
  criteria: [{
    id: "C1", text: "登录恢复", source: "user",
    source_ref: {message_index: 12, quote: "修复登录失败"},
    kind: "judgement", assessment: "satisfied",
    evidence: [{kind: "tool", call_id: "call-test"}], notes: "回归用例通过",
  }],
  reconciled_revision: 1,
};

const startData = {
  summary: "修复登录失败，补上回归测试",
  claim: "working",
  criteria: [],
  next_step: "先查阅项目说明、测试入口与既有实现，登记验收点与检查计划。",
  reconciled_revision: 0,
};

const finish = (facts = {}, candidate = {has_content: true}, overrides = {}) => ({
  type: "task_control",
  name: "before_task_finish",
  control_context: context(overrides),
  arguments: {
    facts: {checks_satisfied: true, missing: [], finish_rejections: 0, checkpoint_basis_valid: true, ...facts},
    candidate,
  },
});

const decide = frame => JSON.parse(frame.content);

test("ping and ordinary replies stay correlated protocol frames", async () => {
  const ping = await reply(`{"type":"ping","name":"ping","request_id":9}`);
  assert.deepEqual(ping, {protocol_version: 2, request_id: 9, content: "goal plugin ready"});

  const command = await reply({
    type: "command", name: "/goal", arguments: ["修复登录失败"], user_input: "修复登录失败",
    control_context: {status: "none", seq: 0},
  });
  assert.equal(command.protocol_version, 2);
  assert.equal(command.request_id, 11);
  assert.equal(command.control.op, "start");
  assert.equal(command.is_error, undefined);
});

test("malformed lines and unknown requests are bounded errors, not crashes", async () => {
  const broken = await reply("{not json");
  assert.equal(broken.protocol_version, 2);
  assert.equal(broken.is_error, true);
  assert.match(broken.content, /invalid JSON request/);

  const type = await reply({type: "model_stream", request_id: 5});
  assert.equal(type.request_id, 5);
  assert.equal(type.is_error, true);
  assert.match(type.content, /不支持的请求类型/);

  const tool = await reply({type: "tool", name: "shell", arguments: {}, control_context: context()});
  assert.equal(tool.is_error, true);
  assert.match(tool.content, /不支持的 Goal 工具/);

  const callback = await reply({type: "task_control", name: "before_tool_call", control_context: context(), arguments: {}});
  assert.equal(callback.is_error, true);
  assert.match(callback.content, /不支持的结束回调/);

  const command = await reply({type: "command", name: "/other", arguments: [], control_context: context()});
  assert.equal(command.is_error, true);
  assert.match(command.content, /不支持的 Goal 命令/);

  const next = await reply(`{"type":"ping","request_id":10}`);
  assert.equal(next.content, "goal plugin ready", "the worker stays reusable after a rejection");
});

test("a normal-finish callback without active goal is a host error", async () => {
  const frame = await reply({
    type: "task_control", name: "before_task_finish",
    control_context: {status: "none", seq: 3},
    arguments: {facts: {checks_satisfied: true}, candidate: {has_content: true}},
  });
  assert.equal(frame.is_error, true);
  assert.match(frame.content, /没有活动目标/);
});

test("the finish callback keeps working until the checkpoint is deliverable", async () => {
  const start = decide(await reply(finish({}, {has_content: true}, {data: startData})));
  assert.deepEqual([start.decision, start.reason_code], ["continue", "CLAIM_WORKING"]);

  const working = decide(await reply(finish()));
  assert.deepEqual([working.decision, working.reason_code], ["continue", "CLAIM_WORKING"]);
  assert.match(working.feedback, /下一步：运行已登记的检查/);

  const empty = decide(await reply(finish({}, {has_content: false})));
  assert.deepEqual([empty.decision, empty.reason_code], ["continue", "NO_DELIVERY"]);

  const checks = decide(await reply(finish({checks_satisfied: false, missing: ["T1"]})));
  assert.deepEqual([checks.decision, checks.reason_code], ["continue", "MISSING_CHECK"]);
  assert.match(checks.feedback, /T1/);

  const stale = decide(await reply(finish({checkpoint_basis_valid: false})));
  assert.deepEqual([stale.decision, stale.reason_code], ["continue", "STALE_CHECKPOINT"]);

  const phrase = decide(await reply(finish({}, {has_content: true}, {data: {...startData, summary: "已完成全部工作"}})));
  assert.equal(phrase.decision, "continue", "a completion phrase alone never finishes a goal");
});

test("done and blocked endings stay distinct, regardless of prior finish attempts", async () => {
  const ready = await reply(finish({}, {has_content: true}, {data: doneData}));
  assert.deepEqual(decide(ready), {decision: "finish", reason_code: "READY", feedback: ""});

  const blocked = decide(await reply(finish({}, {has_content: true}, {
    data: {summary: "缺测试环境", claim: "blocked", block_reason: "需要测试环境密码", criteria: [], reconciled_revision: 1},
  })));
  assert.deepEqual([blocked.decision, blocked.reason_code], ["pause", "BLOCKED"]);
  assert.match(blocked.feedback, /需要测试环境密码/);

  const limited = decide(await reply(finish({finish_rejections: 3})));
  assert.deepEqual([limited.decision, limited.reason_code], ["continue", "CLAIM_WORKING"]);
  assert.match(limited.feedback, /下一步/);

  const paused = decide(await reply(finish({}, {has_content: true}, {status: "paused"})));
  assert.deepEqual([paused.decision, paused.reason_code], ["pause", "NOT_RUNNING"]);
});

test("inconsistent or malformed control facts never release a finish", async () => {
  const lying = decide(await reply(finish({}, {has_content: true}, {
    data: {...doneData, criteria: [{...doneData.criteria[0], assessment: "pending"}]},
  })));
  assert.deepEqual([lying.decision, lying.reason_code], ["pause", "INVALID_STATE"]);
  assert.match(lying.feedback, /不能据此完成/);

  const badFacts = await reply(finish({checks_satisfied: "yes"}));
  assert.equal(badFacts.is_error, true);
  assert.match(badFacts.content, /checks_satisfied/);

  const longBlock = decide(await reply(finish({}, {has_content: true}, {
    data: {summary: "受阻", claim: "blocked", block_reason: "阻".repeat(1024), criteria: [], reconciled_revision: 1},
  })));
  assert.equal(longBlock.decision, "pause");
  assert.ok(Buffer.byteLength(JSON.stringify(longBlock), "utf8") <= 4096, "a decision stays within 4 KiB");
});

test("concurrent lines are serialized and answered in order", async () => {
  const {frames, write} = sink();
  const handle = createHandler({write});
  const order = [];
  await Promise.all([
    handle(JSON.stringify({type: "ping", request_id: 1})).then(() => order.push(1)),
    handle(JSON.stringify({type: "ping", request_id: 2})).then(() => order.push(2)),
  ]);
  assert.deepEqual(order, [1, 2]);
  assert.deepEqual(frames.map(frame => frame.request_id), [1, 2]);
});

const blockedData = reason => ({
  summary: "缺测试环境", claim: "blocked", block_reason: reason, criteria: [], reconciled_revision: 1,
});

test("a real block pauses even when checks, basis or delivery are missing", async () => {
  const cases = [
    {facts: {checks_satisfied: false, missing: ["T1"]}, candidate: {has_content: true}},
    {facts: {checkpoint_basis_valid: false}, candidate: {has_content: true}},
    {facts: {}, candidate: {has_content: false}},
  ];
  for (const item of cases) {
    const decision = decide(await reply(finish(item.facts, item.candidate, {data: blockedData("需要测试环境密码")})));
    assert.deepEqual([decision.decision, decision.reason_code], ["pause", "BLOCKED"], JSON.stringify(item));
    assert.match(decision.feedback, /需要测试环境密码/);
  }
});

test("an empty block reason is an invalid state, not a valid block", async () => {
  const decision = decide(await reply(finish({checks_satisfied: false}, {has_content: false}, {
    data: {summary: "缺测试环境", claim: "blocked", criteria: [], reconciled_revision: 1},
  })));
  assert.deepEqual([decision.decision, decision.reason_code], ["pause", "INVALID_STATE"]);
  assert.match(decision.feedback, /block_reason/);
});

test("repeated finish attempts never impose a hidden pause limit", async () => {
  for (const facts of [{finish_rejections: 100}, {finish_rejections: 100, checkpoint_basis_valid: false}, {finish_rejections: 100, checks_satisfied: false, missing: ["T1"]}]) {
    const decision = decide(await reply(finish(facts)));
    assert.equal(decision.decision, "continue");
  }
  const finished = decide(await reply(finish({finish_rejections: 100}, {has_content: true}, {data: doneData})));
  assert.equal(finished.decision, "finish");
});

test("the callback requires an explicit checkpoint basis", async () => {
  const frame = await reply({
    type: "task_control", name: "before_task_finish",
    control_context: context({data: doneData}),
    arguments: {facts: {checks_satisfied: true, missing: []}, candidate: {has_content: true}},
  });
  assert.equal(frame.is_error, true);
  assert.match(frame.content, /checkpoint_basis_valid/);
  assert.equal(frame.control, undefined, "a bad callback never carries a control suggestion");
});
