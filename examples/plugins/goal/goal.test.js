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
  await createHandler({write})(JSON.stringify({protocol_version: PROTOCOL_VERSION, request_id: 11, ...request}));
  assert.equal(frames.length, 1, "one request produces exactly one frame");
  return frames[0];
}

async function rejects(request, pattern) {
  const frame = await reply(request);
  assert.equal(frame.is_error, true, `expected a rejection, got ${JSON.stringify(frame).slice(0, 300)}`);
  assert.equal(frame.control, undefined, "a rejected request never carries a control suggestion");
  assert.match(frame.content, pattern);
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

const command = (input, overrides = {}) => ({
  type: "command", name: "/goal",
  arguments: input.split(" ").filter(Boolean), user_input: input,
  control_context: context(overrides),
});

const checkpoint = (args, overrides = {}) => ({
  type: "tool", name: "goal_checkpoint", arguments: args, call_id: "call-1",
  control_context: context(overrides),
});

const emptyGoal = {status: "none", id: undefined, revision: undefined, seq: 0, checks: [], data: undefined};

const baseCheckpoint = {
  revision: 1,
  summary: "已定位登录失败原因",
  claim: "working",
  next_step: "运行已登记的检查并更新验收点",
  checks: [{id: "T1", command: "node --test tests/login.test.js", source: "user", source_ref: CHECK_REF}],
  criteria: [{
    id: "C1", text: "登录恢复", source: "user",
    source_ref: {message_index: 12, quote: "修复登录失败"},
    kind: "judgement", assessment: "pending",
  }],
};

test("start proposes one run without inventing an ID or revision", async () => {
  const frame = await reply(command("修复登录失败，补上回归测试", emptyGoal));
  assert.equal(frame.request_id, 11);
  assert.equal(frame.is_error, undefined);
  assert.equal(frame.control.op, "start");
  assert.equal(frame.control.base_seq, 0);
  assert.equal("id" in frame.control, false);
  assert.equal("revision" in frame.control, false);
  assert.deepEqual(frame.control.checks, []);
  assert.equal(frame.control.data.claim, "working");
  assert.deepEqual(frame.control.data.criteria, [], "starting a goal requires no acceptance form");
  assert.match(frame.content, /目标已准备/);
});

test("goal text is not restricted by a completion-phrase blacklist", async () => {
  const frame = await reply(command("完成", emptyGoal));
  assert.deepEqual(frame.control.data.criteria, []);
  assert.match(frame.content, /直接开始工作/);
});

test("a running goal cannot be silently replaced", async () => {
  await rejects(command("改成邮箱登录"), /不能静默覆盖/);
});

test("empty input shows usage and never creates a goal", async () => {
  const frame = await reply(command("", emptyGoal));
  assert.equal(frame.control, undefined);
  assert.match(frame.content, /当前没有活动目标/);
  assert.match(frame.content, /用法/);
  await rejects(command("resume", emptyGoal), /没有可恢复的目标/);
});

test("pause and cancel stay host-owned and never produce a control effect", async () => {
  await rejects(command("pause"), /宿主直接执行/);
  await rejects(command("cancel"), /宿主直接执行/);
  await rejects(command("pause now"), /不接受额外参数/);
  await rejects(command("-f"), /未知选项/);
});

test("explicit resume reopens stopped goals and clears completion claims", async () => {
  const paused = context({
    status: "paused",
    data: {
      summary: "登录修复完成",
      claim: "done",
      criteria: [{
        id: "C1", text: "登录恢复", source: "user",
        source_ref: {message_index: 12, quote: "修复登录失败"},
        kind: "judgement", assessment: "satisfied",
        evidence: [{kind: "tool", call_id: "call-test"}], notes: "回归用例通过",
      }],
      reconciled_revision: 1,
    },
  });
  const frame = await reply(command("resume", paused));
  assert.equal(frame.control.op, "resume");
  assert.equal(frame.control.id, "goal-1");
  assert.equal(frame.control.revision, 1);
  assert.equal(frame.control.data.claim, "working");
  assert.equal(frame.control.data.criteria[0].assessment, "pending");
  assert.equal(frame.control.data.criteria[0].evidence, undefined);
  assert.equal("checks" in frame.control, false, "the registered plan is inherited");

  for (const status of ["completed", "cancelled"]) {
    assert.equal((await reply(command("resume", context({status})))).control.op, "resume");
  }
  await rejects(command("resume"), /正在运行/);
});

test("status view reports the goal, plan and unverified items without fake progress", async () => {
  const frame = await reply(command("", {
    data: {...context().data, unverified: ["生产环境凭据"]},
  }));
  assert.equal(frame.control, undefined);
  assert.match(frame.content, /目标 goal-1/);
  assert.match(frame.content, /运行中/);
  assert.match(frame.content, /revision 1/);
  assert.match(frame.content, /C1 \[pending\] judgement 登录恢复/);
  assert.match(frame.content, /T1 \[user\] node --test/);
  assert.match(frame.content, /未验证：生产环境凭据/);
  assert.doesNotMatch(frame.content, /100%|完成度/);
});

test("a checkpoint proposes one update bound to the request snapshot", async () => {
  const frame = await reply(checkpoint(baseCheckpoint));
  assert.equal(frame.is_error, undefined);
  assert.equal(frame.control.op, "update");
  assert.equal(frame.control.base_seq, 4);
  assert.equal(frame.control.id, "goal-1");
  assert.equal(frame.control.revision, 1);
  assert.deepEqual(frame.control.checks, baseCheckpoint.checks);
  assert.equal(frame.control.data.reconciled_revision, undefined);
  const content = JSON.parse(frame.content);
  assert.equal(content.status, "accepted");
  assert.deepEqual(content.gaps, ["C1: pending"]);
});

test("omitted criteria and checks are inherited, not deleted", async () => {
  const frame = await reply(checkpoint({
    revision: 1, summary: "检查仍未运行", claim: "working", next_step: "运行 T1",
  }));
  assert.equal(frame.control.data.criteria[0].id, "C1");
  assert.equal("checks" in frame.control, false);
});

test("checkpoints reject stale revisions, inactive goals and unknown fields", async () => {
  await rejects(checkpoint({...baseCheckpoint, revision: 2}), /不是当前修订/);
  await rejects(checkpoint(baseCheckpoint, {status: "paused"}), /只在运行中登记/);
  await rejects(checkpoint(baseCheckpoint, emptyGoal), /没有活动目标/);
  await rejects({type: "tool", name: "goal_checkpoint", arguments: baseCheckpoint}, /缺少根级 control_context/);
  await rejects(checkpoint({...baseCheckpoint, extra: 1}), /不支持字段 extra/);
  await rejects(checkpoint({...baseCheckpoint, claim: "ready"}), /claim 非法/);
  await rejects(checkpoint({...baseCheckpoint, claim: "done"}), /未满足的验收点/);
  assert.equal((await reply(checkpoint({...baseCheckpoint, next_step: ""}))).is_error, undefined);
  assert.equal((await reply(checkpoint({...baseCheckpoint, next_step: null}))).is_error, undefined);
  await rejects(checkpoint({...baseCheckpoint, claim: "blocked"}), /block_reason/);
  await rejects(checkpoint({...baseCheckpoint, summary: "x".repeat(2049)}), /超过 2048/);
});

test("optional criteria validate ids and evidence shape, not model phrasing", async () => {
  const criterion = {...baseCheckpoint.criteria[0]};
  assert.equal((await reply(checkpoint({...baseCheckpoint, criteria: [{...criterion, text: "完成目标"}]}))).is_error, undefined);
  await rejects(checkpoint({
    ...baseCheckpoint,
    criteria: [criterion, {...criterion, text: "另一个条件"}],
  }), /重复的验收点/);
  await rejects(checkpoint({
    ...baseCheckpoint,
    criteria: [{...criterion, id: "has space"}],
  }), /1–32 位/);
  await rejects(checkpoint({
    ...baseCheckpoint,
    criteria: [{...criterion, evidence: [{kind: "tool", call_id: "c1", extra: true}]}],
  }), /不支持字段 extra/);
  assert.equal((await reply(checkpoint({...baseCheckpoint, criteria: [{...criterion, kind: "deliverable", assessment: "satisfied"}]}))).is_error, undefined);
  const satisfied = {...criterion, kind: "deliverable", assessment: "satisfied", notes: "交付文件已保存"};
  const frame = await reply(checkpoint({...baseCheckpoint, criteria: [satisfied]}));
  assert.equal(frame.control.data.criteria[0].notes, "交付文件已保存");
});

test("user references must exist in this goal's own user input", async () => {
  const criterion = baseCheckpoint.criteria[0];
  await rejects(checkpoint({
    ...baseCheckpoint,
    criteria: [{...criterion, source_ref: {message_index: 12, quote: "部署到生产"}}],
  }), /连续片段/);
  await rejects(checkpoint({
    ...baseCheckpoint,
    criteria: [{...criterion, source_ref: {message_index: 99, quote: "修复登录失败"}}],
  }), /不在该目标的用户来源/);
  await rejects(checkpoint({
    ...baseCheckpoint,
    criteria: [{...criterion, source_ref: {message_index: 12, quote: "修复登录失败", extra: 1}}],
  }), /不支持字段 extra/);
  assert.equal((await reply(checkpoint({...baseCheckpoint, criteria: [{...criterion, source: "derived"}]}))).is_error, undefined);
});

test("check plans are optional annotations, but references and ids remain valid", async () => {
  const frame = await reply(checkpoint({...baseCheckpoint, checks: [{id: "T1", command: "npm test"}]}));
  assert.equal(frame.is_error, undefined);
  assert.equal(frame.control.checks[0].source, "derived");
  await rejects(checkpoint({...baseCheckpoint, checks: [{id: "T1", command: "npm test", extra: 1}]}), /不支持字段 extra/);
  await rejects(checkpoint({...baseCheckpoint, criteria: [{...baseCheckpoint.criteria[0], check_ids: ["T9"]}]}), /不存在的检查/);
  assert.equal((await reply(checkpoint({...baseCheckpoint, criteria: [{...baseCheckpoint.criteria[0], kind: "command"}]}))).is_error, undefined);
});

test("models can correct their own lists without another user revision", async () => {
  const frame = await reply(checkpoint({...baseCheckpoint, checks: [], criteria: []}));
  assert.equal(frame.is_error, undefined);
  assert.deepEqual(frame.control.checks, []);
  assert.deepEqual(frame.control.data.criteria, []);
  const minimal = await reply(checkpoint({summary: "完成文档交付", claim: "done", criteria: [], checks: []}));
  assert.equal(minimal.control.revision, 1);
});

test("checklist and command lengths have no arbitrary item limits", async () => {
  const criteria = Array.from({length: 40}, (_, i) => ({id: 'C' + i, text: "回归用例通过", assessment: "satisfied"}));
  const checks = Array.from({length: 20}, (_, i) => ({id: 'T' + i, command: 'echo ' + i}));
  const frame = await reply(checkpoint({...baseCheckpoint, criteria, checks}));
  assert.equal(frame.is_error, undefined);
  assert.equal(frame.control.data.criteria.length, 40);
  assert.equal(frame.control.checks.length, 20);
  const large = await reply(command("目标正文" + "x".repeat(20 * 1024), emptyGoal));
  assert.equal(large.control.op, "start");
  const longCommand = await reply(checkpoint({...baseCheckpoint, checks: [{id: "T1", command: "echo " + "中".repeat(3000)}]}));
  assert.equal(longCommand.is_error, undefined);
});

test("evidence ids are sent to the host for factual validation", async () => {
  const frame = await reply(checkpoint({...baseCheckpoint, criteria: [{...baseCheckpoint.criteria[0], evidence: [{kind: "tool", call_id: "call-test"}]}]}));
  assert.deepEqual(frame.control.evidence, ["call-test"]);
});

test("a bad previous record does not block status or a new goal", async () => {
  const bad = {data: {summary: "旧记录", criteria: [{check_ids: ["deleted"]}]}, checks: []};
  assert.equal((await reply(command("新的目标", {...bad, status: "paused"}))).control.op, "start");
  assert.equal((await reply(command("status", {...bad, status: "paused"}))).control, undefined);
});
