"use strict";

// Goal plugin worker. The host owns the single agent loop, the authoritative
// control snapshot, cancellation and every execution fact. This process
// only parses `/goal`, validates acceptance points and the check plan, and
// answers the normal-finish callback with a bounded decision. It never executes
// a command, spawns a process, calls a model or writes authoritative state.

const PROTOCOL_VERSION = 2;

const LIMITS = {
  id: 128,
  decision: 4096,
  feedback: 2048,
  content: 4000,
  summary: 2048,
  next_step: 1024,
  block_reason: 1024,
  rationale: 1024,
  notes: 1024,
  text: Infinity,
  quote: 500,
  command: Infinity,
  call_id: 128,
  unverified_item: 200,
};

const IDENTIFIER = /^[A-Za-z0-9_-]{1,32}$/;
const STATUSES = ["none", "running", "paused", "completed", "cancelled"];
const CLAIMS = ["working", "done", "blocked"];
const SOURCES = ["user", "derived"];
const KINDS = ["command", "deliverable", "judgement"];
const ASSESSMENTS = ["pending", "satisfied", "blocked"];
const DATA_FIELDS = [
  "summary", "claim", "criteria", "next_step", "block_reason", "unverified", "reconciled_revision",
];
const CRITERION_FIELDS = [
  "id", "text", "source", "source_ref", "kind", "assessment", "check_ids", "evidence", "notes",
];
const PLAN_FIELDS = ["id", "command", "source", "source_ref", "rationale"];
const CHECKPOINT_FIELDS = [
  "revision", "summary", "claim", "criteria", "checks", "next_step", "block_reason", "unverified",
];
const USAGE = [
  "用法：",
  "/goal <目标正文>        开始目标，创建后直接执行",
  "/goal [status]          查看当前目标与进展",
  "/goal pause             暂停，不回滚已做的修改（宿主直接执行）",
  "/goal resume [补充内容] 核对现场后继续；补充内容由宿主提升修订号",
  "/goal cancel            放弃当前目标，不删除已做的工作（宿主直接执行）",
].join("\n");

class GoalError extends Error {}

function fail(message) {
  throw new GoalError(message);
}

const blank = value => value === undefined || value === null
  || (typeof value === "string" && value.trim() === "");
const collapsed = value => String(value ?? "").replace(/\s+/g, " ").trim();
const bytes = value => Buffer.byteLength(String(value), "utf8");
function clip(value, max) {
  const text = String(value ?? "");
  return text.length <= max ? text : `${text.slice(0, Math.max(0, max - 1))}…`;
}

const object = (value, where) => {
  if (value === undefined || value === null || typeof value !== "object" || Array.isArray(value)) {
    fail(`${where} 必须是对象`);
  }
  return value;
};

function rejectUnknown(value, allowed, where) {
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) fail(`${where} 不支持字段 ${key}`);
  }
}

function requiredString(value, max, where) {
  if (blank(value)) fail(`${where} 不能为空`);
  if (typeof value !== "string") fail(`${where} 必须是字符串`);
  const text = value.trim();
  if (text.length > max) fail(`${where} 超过 ${max} 字符`);
  return text;
}

function optionalString(value, max, where) {
  return blank(value) ? undefined : requiredString(value, max, where);
}

function integer(value, where, min, max) {
  if (!Number.isInteger(value)) fail(`${where} 必须是整数`);
  if (value < min || value > max) fail(`${where} 必须在 ${min}..${max} 之间`);
  return value;
}

function enumeration(value, allowed, where) {
  if (typeof value !== "string" || !allowed.includes(value)) {
    fail(`${where} 非法：${JSON.stringify(value)}（允许 ${allowed.join("/")}）`);
  }
  return value;
}

function identifier(value, where) {
  if (typeof value !== "string" || !IDENTIFIER.test(value)) {
    fail(`${where} 必须是 1–32 位字母、数字、下划线或短横线`);
  }
  return value;
}

function booleanField(value, where) {
  if (typeof value !== "boolean") fail(`${where} 必须是布尔值`);
  return value;
}

function stringList(value, maxLength, where) {
  if (blank(value)) return [];
  if (!Array.isArray(value)) fail(`${where} 必须是数组`);
  return value.map((item, index) => requiredString(item, maxLength, `${where}[${index}]`));
}

function reference(value, where) {
  object(value, where);
  rejectUnknown(value, ["message_index", "quote"], where);
  return {
    message_index: integer(value.message_index, `${where}.message_index`, 0, Number.MAX_SAFE_INTEGER),
    quote: requiredString(value.quote, LIMITS.quote, `${where}.quote`),
  };
}

// A user reference is only valid inside the goal's own user input, matched as a
// whitespace-collapsed contiguous excerpt. Tool and plugin output never count.
function verifyReference(value, where, sources) {
  const ref = reference(value, where);
  const source = sources.find(item => item.message_index === ref.message_index);
  if (!source) {
    fail(`${where}.message_index ${ref.message_index} 不在该目标的用户来源中；不能引用工具或插件输出`);
  }
  if (!collapsed(source.content).includes(collapsed(ref.quote))) {
    fail(`${where}.quote 不是 message_index ${ref.message_index} 用户原文的连续片段`);
  }
  return {ref, source};
}

function evidenceList(value, where) {
  if (blank(value)) return [];
  if (!Array.isArray(value)) fail(`${where} 必须是数组`);
  return value.map((item, index) => {
    const at = `${where}[${index}]`;
    object(item, at);
    const kind = enumeration(item.kind, ["tool", "final_answer"], `${at}.kind`);
    if (kind === "tool") {
      rejectUnknown(item, ["kind", "call_id"], at);
      return {kind, call_id: requiredString(item.call_id, LIMITS.call_id, `${at}.call_id`)};
    }
    rejectUnknown(item, ["kind"], at);
    return {kind};
  });
}

function userSources(value) {
  if (blank(value)) return [];
  if (!Array.isArray(value)) fail("control_context.user_sources 必须是数组");
  const seen = new Set();
  const list = [];
  value.forEach((item, index) => {
    const at = `control_context.user_sources[${index}]`;
    object(item, at);
    const messageIndex = integer(item.message_index, `${at}.message_index`, 0, Number.MAX_SAFE_INTEGER);
    if (seen.has(messageIndex)) fail(`${at}.message_index ${messageIndex} 重复`);
    seen.add(messageIndex);
    if (item.role !== undefined && item.role !== "user") fail(`${at}.role 必须是 user`);
    if (typeof item.content !== "string") fail(`${at}.content 必须是字符串`);
    list.push({message_index: messageIndex, content: item.content});
  });
  return list;
}

// Check entries stored by the host are trusted; only the plugin-owned fields are
// read, so a host may attach its own execution facts without breaking us.
function storedPlan(value) {
  if (blank(value)) return [];
  if (!Array.isArray(value)) fail("control_context.checks 必须是数组");
  return value.map((item, index) => {
    const at = `control_context.checks[${index}]`;
    object(item, at);
    const entry = {
      id: identifier(item.id, `${at}.id`),
      command: requiredString(item.command, LIMITS.command, `${at}.command`),
      source: enumeration(item.source, SOURCES, `${at}.source`),
    };
    if (!blank(item.source_ref)) entry.source_ref = reference(item.source_ref, `${at}.source_ref`);
    const rationale = optionalString(item.rationale, LIMITS.rationale, `${at}.rationale`);
    if (rationale !== undefined) entry.rationale = rationale;
    return entry;
  });
}

function controlContext(request, {validateData = true} = {}) {
  const raw = request.control_context;
  if (raw === undefined || raw === null) {
    fail("缺少根级 control_context：宿主必须提供 status/id/revision/seq/checks/data 与只读用户来源");
  }
  object(raw, "control_context");
  const status = blank(raw.status) ? "none" : raw.status;
  enumeration(status, STATUSES, "control_context.status");
  const context = {
    status,
    seq: blank(raw.seq) ? 0 : integer(raw.seq, "control_context.seq", 0, Number.MAX_SAFE_INTEGER),
    id: undefined,
    revision: undefined,
    checks: [],
    data: {},
    sources: userSources(raw.user_sources),
  };
  if (status !== "none") {
    if (typeof raw.id !== "string" || blank(raw.id) || raw.id.length > LIMITS.id) {
      fail(`control_context.id 必须是 ≤${LIMITS.id} 字符的非空字符串`);
    }
    context.id = raw.id;
    context.revision = integer(raw.revision, "control_context.revision", 1, Number.MAX_SAFE_INTEGER);
    if (blank(raw.data)) fail("control_context.data 缺失：活动目标必须携带插件 Goal 数据");
    context.checks = storedPlan(raw.checks);
    object(raw.data, "control_context.data");
    context.data = validateData
      ? validateGoalData(raw.data, {checks: context.checks, sources: context.sources})
      : raw.data;
  }
  return context;
}

function criterion(value, index, env) {
  const where = `criteria[${index}]`;
  object(value, where);
  rejectUnknown(value, CRITERION_FIELDS, where);
  const id = identifier(value.id, `${where}.id`);
  const text = requiredString(value.text, LIMITS.text, `${where}.text`);
  const source = enumeration(value.source ?? "derived", SOURCES, `${where}.source`);
  const kind = enumeration(value.kind ?? "judgement", KINDS, `${where}.kind`);
  const assessment = enumeration(value.assessment ?? "pending", ASSESSMENTS, `${where}.assessment`);
  let sourceRef;
  if (!blank(value.source_ref)) {
    sourceRef = verifyReference(value.source_ref, `${where}.source_ref`, env.sources).ref;
  }
  const checkIds = stringList(value.check_ids, 64, `${where}.check_ids`);
  if (new Set(checkIds).size !== checkIds.length) fail(`${where}.check_ids 不能重复`);
  for (const checkId of checkIds) {
    if (!env.checks.some(entry => entry.id === checkId)) {
      fail(`${where}.check_ids 引用了不存在的检查 ${checkId}`);
    }
  }
  const evidence = evidenceList(value.evidence, `${where}.evidence`);
  const notes = optionalString(value.notes, LIMITS.notes, `${where}.notes`);
  const result = {id, text, source};
  if (sourceRef) result.source_ref = sourceRef;
  result.kind = kind;
  result.assessment = assessment;
  if (checkIds.length) result.check_ids = checkIds;
  if (evidence.length) result.evidence = evidence;
  if (notes !== undefined) result.notes = notes;
  return result;
}

function validateGoalData(value, env) {
  object(value, "data");
  rejectUnknown(value, DATA_FIELDS, "data");
  const summary = requiredString(value.summary, LIMITS.summary, "data.summary");
  const claim = enumeration(value.claim, CLAIMS, "data.claim");
  const nextStep = optionalString(value.next_step, LIMITS.next_step, "data.next_step");
  const blockReason = optionalString(value.block_reason, LIMITS.block_reason, "data.block_reason");
  const unverified = stringList(value.unverified, LIMITS.unverified_item, "data.unverified");
  if (!blank(value.criteria) && !Array.isArray(value.criteria)) fail("data.criteria 必须是数组");
  const criteria = (blank(value.criteria) ? [] : value.criteria)
    .map((item, index) => criterion(item, index, {checks: env.checks, sources: env.sources}));
  const seen = new Set();
  for (const item of criteria) {
    if (seen.has(item.id)) fail(`重复的验收点 ID：${item.id}`);
    seen.add(item.id);
  }
  if (claim === "blocked" && blockReason === undefined) fail("claim=blocked 必须说明 block_reason");
  if (claim === "done" && blockReason !== undefined) fail("claim=done 不能同时声明 block_reason");
  if (claim === "done") {
    const open = criteria.filter(item => item.assessment !== "satisfied").map(item => item.id);
    if (open.length) fail(`claim=done 仍有未满足的验收点：${open.join("、")}；完成短语不能代替验收`);
  }
  const data = {summary, claim, criteria};
  if (nextStep !== undefined) data.next_step = nextStep;
  if (blockReason !== undefined) data.block_reason = blockReason;
  if (unverified.length) data.unverified = unverified;
  return data;
}

function planEntry(value, index, sources) {
  const where = `checks[${index}]`;
  object(value, where);
  rejectUnknown(value, PLAN_FIELDS, where);
  const id = identifier(value.id, `${where}.id`);
  const command = requiredString(value.command, LIMITS.command, `${where}.command`);
  const source = enumeration(value.source ?? "derived", SOURCES, `${where}.source`);
  const rationale = optionalString(value.rationale, LIMITS.rationale, `${where}.rationale`);
  const entry = {id, command, source};
  if (!blank(value.source_ref)) {
    entry.source_ref = verifyReference(value.source_ref, `${where}.source_ref`, sources).ref;
  }
  if (rationale !== undefined) entry.rationale = rationale;
  return entry;
}

function planFromArgs(value, context) {
  if (!Array.isArray(value)) fail("checks 必须是数组；省略字段表示继承，空数组表示显式清空");
  const entries = value.map((item, index) => planEntry(item, index, context.sources));
  const seen = new Set();
  for (const entry of entries) {
    if (seen.has(entry.id)) fail(`重复的检查 ID：${entry.id}`);
    seen.add(entry.id);
  }
  return entries;
}

function resumeData(data) {
  const criteria = (data.criteria ?? []).map(item => {
    const next = {id: item.id, text: item.text, source: item.source, kind: item.kind, assessment: "pending"};
    if (item.source_ref) next.source_ref = item.source_ref;
    if (item.check_ids) next.check_ids = item.check_ids;
    return next;
  });
  const next = {
    summary: typeof data.summary === "string" && data.summary.trim() ? data.summary : "（恢复目标）",
    claim: "working",
    criteria,
    next_step: "核对现场和用户最新要求，按需继续工作与检查。",
  };
  if (data.unverified?.length) next.unverified = data.unverified;
  return next;
}

function statusLabel(status) {
  return ({running: "运行中", paused: "已暂停", completed: "已完成", cancelled: "已取消"})[status] ?? status;
}

function render(context) {
  if (context.status === "none") return `当前没有活动目标。\n${USAGE}`;
  const data = context.data;
  const lines = [`目标 ${context.id}  ·  ${statusLabel(context.status)}  ·  revision ${context.revision}`];
  lines.push(`摘要：${data.summary ?? "（等待继续）"}`);
  if (data.claim === "blocked") lines.push(`模型声明：blocked — ${data.block_reason}`);
  else if (data.claim === "done") lines.push("模型声明：done（是否放行完成由收尾回调与宿主事实校验决定）");
  else lines.push(`模型声明：working — 下一步：${data.next_step ?? "（未登记）"}`);
  lines.push("验收点：");
  if (!data.criteria?.length) lines.push("  （尚未登记）");
  for (const item of data.criteria ?? []) {
    const refs = item.check_ids ? ` ⇢ ${item.check_ids.join(", ")}` : "";
    const notes = item.notes ? ` — ${clip(item.notes, 100)}` : "";
    lines.push(`  - ${item.id} [${item.assessment}] ${item.kind} ${clip(item.text, 120)}${refs}${notes}`);
  }
  lines.push("检查计划（执行事实由宿主校验）：");
  if (!context.checks.length) lines.push("  （尚未登记）");
  for (const entry of context.checks) lines.push(`  - ${entry.id} [${entry.source}] ${clip(entry.command, 160)}`);
  if (data.unverified?.length) lines.push(`未验证：${data.unverified.join("；")}`);
  if (context.status === "paused") lines.push("继续：/goal resume　放弃：/goal cancel");
  else if (context.status === "running") lines.push("暂停：/goal pause　放弃：/goal cancel");
  else lines.push("新目标：/goal <正文>");
  return clip(lines.join("\n"), LIMITS.content);
}

function commandText(request) {
  if (!blank(request.user_input)) {
    if (typeof request.user_input !== "string") fail("user_input 必须是字符串");
    return request.user_input;
  }
  const args = request.arguments;
  if (blank(args)) return "";
  if (!Array.isArray(args)) fail("command.arguments 必须是字符串数组");
  for (const item of args) if (typeof item !== "string") fail("command.arguments 必须是字符串数组");
  return args.join(" ");
}

function startGoal(body, context) {
  if (context.status === "running") {
    fail("当前目标正在运行，不能静默覆盖；先 /goal pause 或 /goal cancel");
  }
  const data = {summary: clip(collapsed(body), LIMITS.summary), claim: "working", criteria: []};
  return {
    content: `目标已准备：${data.summary}。直接开始工作；只在重要进展、完成或受阻时记录检查点。`,
    control: {op: "start", base_seq: context.seq, data, checks: []},
  };
}

function resumeGoal(context, supplement) {
  if (context.status === "none") fail(`没有可恢复的目标；用 /goal <目标正文> 创建。\n${USAGE}`);
  if (context.status === "running") fail("目标正在运行，不需要 resume");
  const data = resumeData(context.data);
  const note = supplement ? `（补充：${clip(collapsed(supplement), 200)}；修订号由宿主提升）` : "";
  return {
    content: `已恢复目标 ${context.id}（revision ${context.revision}）${note}。先核对现场和最新要求；已有结果保留作参考，按需继续工作与检查。`,
    control: {op: "resume", base_seq: context.seq, id: context.id, revision: context.revision, data},
  };
}

function runCommand(request, context) {
  if (request.name !== "/goal") fail(`不支持的 Goal 命令：${String(request.name)}`);
  const input = commandText(request);
  const trimmed = input.trim();
  if (trimmed === "" || trimmed.toLowerCase() === "status") return {content: render(context)};
  const words = trimmed.split(/\s+/);
  const head = words[0].toLowerCase();
  if (head === "pause" || head === "cancel") {
    if (words.length > 1) fail(`/goal ${head} 不接受额外参数`);
    fail(`/goal ${head} 由宿主直接执行；本次没有产生控制效果，目标状态未改变`);
  }
  if (head === "resume") return resumeGoal(context, trimmed.slice(words[0].length).trim());
  if (head.startsWith("-")) fail(`未知选项 ${words[0]}。\n${USAGE}`);
  return startGoal(trimmed, context);
}

function runCheckpoint(request, context) {
  if (request.name !== "goal_checkpoint") fail(`不支持的 Goal 工具：${String(request.name)}`);
  object(request.arguments, "goal_checkpoint 参数");
  const args = request.arguments;
  if (context.status === "none") fail("没有活动目标；goal_checkpoint 只在活动目标内使用");
  if (context.status !== "running") {
    fail(`目标状态为 ${statusLabel(context.status)}；检查点只在运行中登记，继续请先 /goal resume`);
  }
  rejectUnknown(args, CHECKPOINT_FIELDS, "goal_checkpoint 参数");
  const revision = blank(args.revision) ? context.revision : integer(args.revision, "revision", 1, Number.MAX_SAFE_INTEGER);
  if (revision !== context.revision) {
    fail(`revision ${revision} 不是当前修订 ${context.revision}；模型不能改写用户修订号`);
  }
  const summary = requiredString(args.summary, LIMITS.summary, "summary");
  const claim = enumeration(args.claim, CLAIMS, "claim");
  const nextStep = optionalString(args.next_step, LIMITS.next_step, "next_step");
  const blockReason = optionalString(args.block_reason, LIMITS.block_reason, "block_reason");
  const unverified = stringList(args.unverified ?? context.data.unverified, LIMITS.unverified_item, "unverified");
  const planProvided = args.checks !== undefined && args.checks !== null;
  const checks = planProvided ? planFromArgs(args.checks, context) : context.checks;
  const criteriaProvided = args.criteria !== undefined && args.criteria !== null;
  if (criteriaProvided && !Array.isArray(args.criteria)) {
    fail("criteria 必须是数组；省略表示继承，空数组表示显式清空");
  }
  const criteria = criteriaProvided ? args.criteria : (context.data.criteria ?? []);
  const data = {summary, claim, criteria};
  if (nextStep !== undefined) data.next_step = nextStep;
  if (blockReason !== undefined) data.block_reason = blockReason;
  if (unverified.length) data.unverified = unverified;
  const validated = validateGoalData(data, {checks, sources: context.sources});
  const gaps = validated.criteria
    .filter(item => item.assessment !== "satisfied")
    .map(item => `${item.id}: ${item.assessment}`);
  const evidence = validated.criteria.flatMap(item => item.evidence ?? []).filter(item => item.kind === "tool").map(item => item.call_id);
  const control = {op: "update", base_seq: context.seq, id: context.id, revision: context.revision, data: validated, evidence};
  if (planProvided) control.checks = checks;
  return {
    content: clip(JSON.stringify({
      status: "accepted",
      goal_id: context.id,
      revision: context.revision,
      claim: validated.claim,
      gaps,
    }), LIMITS.content),
    control,
  };
}

function decisionFrame(decision) {
  let feedback = clip(decision.feedback ?? "", LIMITS.feedback);
  let payload = JSON.stringify({...decision, feedback});
  while (bytes(payload) > LIMITS.decision && feedback.length > 0) {
    feedback = feedback.slice(0, Math.floor(feedback.length * 0.8));
    payload = JSON.stringify({...decision, feedback});
  }
  return payload;
}

function finishDecision(request) {
  if (request.name !== "before_task_finish") fail(`不支持的结束回调：${String(request.name)}`);
  const context = controlContext(request, {validateData: false});
  if (context.status === "none") {
    fail("before_task_finish 收到时没有活动目标；宿主不应在普通任务中调用该回调");
  }
  const args = object(request.arguments, "arguments");
  const facts = object(args.facts, "arguments.facts");
  const candidate = object(args.candidate, "arguments.candidate");
  const checksSatisfied = booleanField(facts.checks_satisfied, "facts.checks_satisfied");
  // Required: a missing basis must never be read as "valid" and release a finish.
  const basis = booleanField(facts.checkpoint_basis_valid, "facts.checkpoint_basis_valid");
  const missing = stringList(facts.missing, 64, "facts.missing");
  const hasContent = booleanField(candidate.has_content, "candidate.has_content");
  const decide = (decision, reason_code, feedback = "") => ({decision, reason_code, feedback});
  if (context.status !== "running") {
    return decide("pause", "NOT_RUNNING", `目标状态为 ${statusLabel(context.status)}，不应请求结束判断；等待用户处理。`);
  }
  let data;
  try {
    data = validateGoalData(context.data, {checks: context.checks, sources: context.sources});
  } catch (error) {
    return decide("pause", "INVALID_STATE", `目标控制数据无效，不能据此完成：${error.message}`);
  }
  let decision;
  // A valid blocked claim pauses regardless of failed checks, a stale basis or
  // a candidate without content: the goal really waits on the user.
  if (data.claim === "blocked") {
    decision = decide("pause", "BLOCKED", `目标受阻：${data.block_reason}。需要用户补充条件后用 /goal resume 继续。`);
  } else if (!hasContent) {
    decision = decide("continue", "NO_DELIVERY", "本轮还没有可交付内容；先给出实际结果，或明确说明受阻原因。");
  } else if (!basis) {
    decision = decide("continue", "STALE_CHECKPOINT", "检查点依据已失效（修订号或检查轮次变化）；重新核验后调用 goal_checkpoint。");
  } else if (!checksSatisfied) {
    const detail = missing.length ? `尚未通过：${missing.join("、")}。` : "尚未全部通过。";
    decision = decide("continue", "MISSING_CHECK", `已登记的检查${detail}运行它们，再更新检查点。`);
  } else if (data.claim === "working") {
    decision = decide("continue", "CLAIM_WORKING", `当前检查点仍是 working；下一步：${data.next_step ?? "按用户目标继续工作，完成或受阻时更新检查点"}`);
  } else {
    const open = data.criteria.filter(item => item.assessment !== "satisfied");
    decision = open.length
      ? decide("continue", "CRITERIA_NOT_SATISFIED", `验收点尚未满足：${open.map(item => item.id).join("、")}。完成短语不能代替验收。`)
      : decide("finish", "READY");
  }
  return decision;
}

// One JSONL request in, one correlated frame out. Every frame echoes
// protocol_version and the request id, including ping; a rejected request is an
// ordinary is_error frame so the worker stays reusable.
function createHandler({write = frame => process.stdout.write(`${JSON.stringify(frame)}\n`)} = {}) {
  let queue = Promise.resolve();
  function handle(line) {
    queue = queue.then(() => respond(line));
    return queue;
  }

  async function respond(line) {
    const base = {protocol_version: PROTOCOL_VERSION};
    let request;
    try {
      request = JSON.parse(line);
    } catch (error) {
      write({...base, content: `invalid JSON request: ${error.message}`, is_error: true});
      return;
    }
    if (request && typeof request === "object" && !Array.isArray(request) && request.request_id !== undefined) {
      base.request_id = request.request_id;
    }
    try {
      object(request, "request");
      if (request.type === "ping") {
        write({...base, content: "goal plugin ready"});
        return;
      }
      if (request.type === "command") {
        const reply = runCommand(request, controlContext(request, {validateData: false}));
        write({
          ...base,
          content: reply.content,
          ...(reply.control ? {control: reply.control} : {}),
          ...(reply.is_error ? {is_error: true} : {}),
        });
        return;
      }
      if (request.type === "tool") {
        const reply = runCheckpoint(request, controlContext(request, {validateData: false}));
        write({
          ...base,
          content: reply.content,
          ...(reply.control ? {control: reply.control} : {}),
          ...(reply.is_error ? {is_error: true} : {}),
        });
        return;
      }
      if (request.type === "task_control") {
        write({...base, content: decisionFrame(finishDecision(request))});
        return;
      }
      fail(`不支持的请求类型：${String(request.type)}`);
    } catch (error) {
      write({...base, content: clip(error?.message ?? String(error), LIMITS.content), is_error: true});
    }
  }

  return handle;
}

function start() {
  const readline = require("node:readline");
  const input = readline.createInterface({input: process.stdin, crlfDelay: Infinity});
  const handle = createHandler();
  input.on("line", line => {
    handle(line).catch(() => {
      console.error("goal protocol stream failed.");
      process.exitCode = 1;
    });
  });
}

if (require.main === module) start();

module.exports = {createHandler, start, PROTOCOL_VERSION};
