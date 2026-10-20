#!/usr/bin/env node
"use strict";

/**
 * Kroma Benchmark Foundation v1: immutable experiment inputs and evidence-only analysis.
 *
 * The AgentRunner, task execution, and scoring remain in kroma-core/src/bench.rs.
 * This Node helper NEVER calls a provider, runs a candidate's code, or asserts that a
 * synthetic score is evidence of model capability. No third-party dependencies.
 *
 * Usage:
 *   node scripts/bench-foundation.js freeze --manifest ... --suite ... --prices ...
 *       --out ... --executor synthetic --budget-tokens 10000 --seed 12345
 *   node scripts/bench-foundation.js evaluate --lock ... --report ... --out ...
 *
 * License: MIT. Run `node --test scripts/test-bench-foundation.js`.
 */

const crypto = require("node:crypto");
const os = require("node:os");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const SCHEMA = "kroma.benchmark.foundation/1";
const MAX_JSON_BYTES = 16 * 1024 * 1024;
const REQUIRED_ARMS = ["single_agent", "adaptive_tournament_3"];
const SUPPORTED_ORACLE = "BenchTask::evaluate:v1";

class BenchFoundationError extends Error {
  constructor(message) {
    super(message);
    this.name = "BenchFoundationError";
  }
}
function fail(message) { throw new BenchFoundationError(message); }
function must(condition, message) { if (!condition) fail(message); }
function own(obj, key) { return Object.prototype.hasOwnProperty.call(obj, key); }
function obj(value) { return value !== null && typeof value === "object" && !Array.isArray(value); }
function sha(bytes) { return crypto.createHash("sha256").update(bytes).digest("hex"); }
function integer(value, label, min = 0) {
  must(Number.isSafeInteger(value) && value >= min, `${label} must be an integer >= ${min}`);
  return value;
}
function readLimited(file) {
  const stat = fs.lstatSync(file);
  must(stat.isFile() && !stat.isSymbolicLink(), `${file} must be a regular file (not a symlink)`);
  must(stat.size <= MAX_JSON_BYTES, `${file} exceeds the ${MAX_JSON_BYTES} byte bound`);
  return fs.readFileSync(file);
}
function parseJsonFile(file) {
  const bytes = readLimited(file);
  let value;
  try { value = JSON.parse(bytes.toString("utf8")); }
  catch (e) { fail(`invalid JSON in ${file}: ${e.message}`); }
  return { bytes, value };
}
function safeLabel(value, label) {
  must(typeof value === "string" && /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,100}$/.test(value),
    `${label} needs a safe, non-empty identifier`);
  return value;
}
function pathOk(name) {
  if (typeof name !== "string" || !name || name.includes("\\") || name.startsWith("/") || name.includes("\0")) return false;
  const parts = name.split("/");
  if (parts.some(p => !p || p === "." || p === ".." || p.includes(":") || /[ .]$/.test(p))) return false;
  if (parts.some(p => /^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i.test(p))) return false;
  return true;
}
function checkKeys(o, allowed, label) {
  must(obj(o), `${label} must be an object`);
  for (const key of Object.keys(o)) must(allowed.includes(key), `${label}: unsupported key ${key}`);
}
function ensureTask(task, index) {
  const label = `suite[${index}]`;
  checkKeys(task, ["id", "prompt", "files", "expect_contains", "expect_absent", "regression_guard", "verify_command"], label);
  safeLabel(task.id, `${label}.id`);
  must(typeof task.prompt === "string" && task.prompt.trim().length > 0, `${label}: prompt required`);
  must(obj(task.files) && Object.keys(task.files).length > 0, `${label}: nonempty initial files required (foundation v1)`);
  for (const [filename, contents] of Object.entries(task.files)) {
    must(pathOk(filename) && typeof contents === "string", `${label}: unsafe initial file ${filename}`);
  }
  const contains = task.expect_contains ?? [];
  const absent = task.expect_absent ?? [];
  const command = task.verify_command;
  must(Array.isArray(contains) && Array.isArray(absent), `${label}: post-conditions must be arrays`);
  must(contains.every(entry => Array.isArray(entry) && entry.length === 2 && pathOk(entry[0]) && typeof entry[1] === "string" && entry[1].length > 0),
    `${label}: unsafe/empty expect_contains`);
  must(absent.every(pathOk), `${label}: unsafe expect_absent`);
  if (command !== undefined && command !== null) {
    must(Array.isArray(command) && command.length > 0 && command.every(v => typeof v === "string" && v.length > 0 && !v.includes("\0")), `${label}: invalid verify_command argv`);
  }
  must(contains.length + absent.length > 0 || (command && command.length > 0), `${label}: no independent oracle`);
  must(task.regression_guard === undefined || typeof task.regression_guard === "boolean", `${label}: regression_guard must be boolean`);
  // A task that passes all static checks before execution is not a meaningful task.
  const alreadySatisfied = contains.every(([name, needle]) => own(task.files, name) && task.files[name].includes(needle)) &&
    absent.every(name => !own(task.files, name));
  if (contains.length + absent.length > 0) {
    must(!alreadySatisfied, `${label}: task already satisfies all static post-conditions before any model runs`);
  }
  return {
    id: task.id,
    prompt: task.prompt,
    files: Object.fromEntries(Object.entries(task.files).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)),
    expect_contains: contains,
    expect_absent: absent,
    regression_guard: task.regression_guard ?? false,
    ...(command !== undefined && command !== null ? { verify_command: command } : {})
  };
}
// Matches BenchTask's serde field order and BTreeMap file order. The Rust runner hashes
// the serde_json encoding, then prints its first 8 digest bytes as a 16-character hex ID.
function rustSuiteFingerprint(tasks) {
  return sha(Buffer.from(JSON.stringify(tasks), "utf8")).slice(0, 16);
}
function validateManifest(manifest, suite, prices, manifestFile, suiteFile, executor, budgetTokens, seed, thresholds, checkSourcePath = true) {
  checkKeys(manifest, ["version", "id", "arms", "tasks", "holdout", "repeats", "sandbox", "oracle", "max_total_tokens"], "manifest");
  must(manifest.version === 1, "manifest version must equal 1");
  safeLabel(manifest.id, "manifest.id");
  must(Array.isArray(manifest.arms) && manifest.arms.length === 2, "v1 supports exactly two paired arms");
  must(Array.isArray(manifest.tasks) && manifest.tasks.length > 0, "manifest needs tasks");
  must(manifest.oracle === SUPPORTED_ORACLE, `manifest.oracle must equal ${SUPPORTED_ORACLE}; do not claim an unimplemented scorer`);
  integer(manifest.repeats, "manifest.repeats", 1);
  must(manifest.repeats <= 100, "repeats limited to 100");
  const seenArms = new Set();
  let baseline = 0;
  for (const arm of manifest.arms) {
    checkKeys(arm, ["id", "kind", "model", "price_table"], "arm");
    safeLabel(arm.id, "arm.id");
    must(!seenArms.has(arm.id), "duplicate arm id");
    seenArms.add(arm.id);
    must(arm.kind === "baseline" || arm.kind === "candidate", "arm.kind invalid");
    baseline += Number(arm.kind === "baseline");
    must(typeof arm.model === "string" && /^[a-zA-Z0-9][a-zA-Z0-9._:/@+\-]{0,127}$/.test(arm.model),
      "arm.model requires a bounded non-control model ID");
    must(arm.price_table === prices.version, "arm price_table must match frozen price table version");
    must(own(prices.models, arm.model), `missing price rates for ${arm.model}`);
  }
  must(baseline === 1, "exactly one baseline required");
  must(manifest.arms[0].id === REQUIRED_ARMS[0] && manifest.arms[0].kind === "baseline" &&
    manifest.arms[1].id === REQUIRED_ARMS[1] && manifest.arms[1].kind === "candidate",
  `Rust CLI currently executes only ${REQUIRED_ARMS.join(" vs ")}; refusing to silently replace manifest arms`);
  must(manifest.arms[0].model === manifest.arms[1].model, "paired comparison requires same model");
  must(manifest.sandbox !== undefined && typeof manifest.sandbox === "string" && manifest.sandbox.trim(),
    "sandbox must be declared for candidate (declaration is not proof of isolation)");
  must(executor === "synthetic" || executor === "provider", "executor must be synthetic or provider; this script only freezes/evaluates, never invokes a model");
  if (executor === "provider") {
    must(!String(manifest.sandbox).startsWith("synthetic-only-") &&
      !String(prices.version).startsWith("demonstration-") &&
      !manifest.arms[0].model.startsWith("demo-only-"),
      "synthetic demonstration fixtures cannot be frozen as real provider evidence");
  }
  checkKeys(prices, ["version", "source", "models"], "prices");
  must(typeof prices.source === "string" && prices.source.trim(), "price table source required");
  must(typeof prices.version === "string" && prices.version.trim(), "price table version required");
  for (const [model, rates] of Object.entries(prices.models)) {
    must(model.length > 0 && obj(rates), "price table models must contain rate objects");
    checkKeys(rates, ["uncached_input_nano_usd", "cached_input_nano_usd", "cache_creation_input_nano_usd", "output_nano_usd"], `prices.models.${model}`);
    for (const field of ["uncached_input_nano_usd", "cached_input_nano_usd", "output_nano_usd"]) integer(rates[field], `prices.models.${model}.${field}`);
    if (rates.cache_creation_input_nano_usd != null) integer(rates.cache_creation_input_nano_usd, `prices.models.${model}.cache_creation_input_nano_usd`);
  }
  const ids = new Set();
  const loadedTasks = suite.map(ensureTask);
  for (const task of loadedTasks) {
    must(!ids.has(task.id), `duplicate suite id ${task.id}`);
    ids.add(task.id);
  }
  must(manifest.tasks.length === loadedTasks.length, "manifest/suite task count mismatch");
  for (let i = 0; i < manifest.tasks.length; i++) {
    const spec = manifest.tasks[i];
    checkKeys(spec, ["id", "source"], `manifest.tasks[${i}]`);
    must(spec.id === loadedTasks[i].id, `manifest.tasks[${i}].id != suite task id`);
    const src = String(spec.source).match(/^(.*?)#([0-9]+)$/);
    must(src && src[1] && Number(src[2]) === i, `manifest task ${spec.id}: source must name its exact suite entry (#${i})`);
    const localSource = path.resolve(path.dirname(manifestFile), src[1]);
    if (checkSourcePath) {
      must(localSource === path.resolve(suiteFile), `manifest task ${spec.id}: source must resolve to the frozen suite`);
    }
  }
  must(Array.isArray(manifest.holdout) && manifest.holdout.length > 0, "v1 requires an explicit nonempty holdout");
  must(new Set(manifest.holdout).size === manifest.holdout.length, "duplicate holdout id");
  must(manifest.holdout.every(id => ids.has(id)), "holdout includes unknown task");
  must(manifest.holdout.length < ids.size, "holdout cannot cover all tasks");
  integer(budgetTokens, "budget_tokens_per_arm_task", 1);
  integer(seed, "seed", 0);
  const jobs = manifest.arms.length * manifest.tasks.length * manifest.repeats;
  must(jobs <= 10000, "matrix exceeds 10000 jobs");
  const plannedTokens = jobs * budgetTokens;
  must(Number.isSafeInteger(plannedTokens), "declared token matrix overflows JS integer");
  must(Number.isSafeInteger(manifest.max_total_tokens) && plannedTokens <= manifest.max_total_tokens,
    `declared maximum ${manifest.max_total_tokens} tokens insufficient for ${plannedTokens} planned tokens`);
  must(thresholds.min_clusters >= 5, "minimum independent tasks for promotion must be at least 5");
  return { jobs, plannedTokens, suite: loadedTasks, baseline: REQUIRED_ARMS[0], candidate: REQUIRED_ARMS[1] };
}
function parseArgs(args) {
  const verb = args.shift();
  const flags = {};
  for (let i = 0; i < args.length; i += 2) {
    const key = args[i];
    must(/^--[a-z][a-z-]*$/.test(key || "") && args[i + 1] !== undefined, `expected --name value, got ${key}`);
    must(!own(flags, key.slice(2)), `duplicate ${key}`);
    flags[key.slice(2)] = args[i + 1];
  }
  return { verb, flags };
}
function requireOptions(flags, allowed, required) {
  for (const key of Object.keys(flags)) must(allowed.includes(key), `unknown option --${key}`);
  for (const key of required) must(typeof flags[key] === "string", `missing --${key}`);
}
function atomicWrite(file, bytes) {
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  const temp = `${file}.${process.pid}.${crypto.randomBytes(8).toString("hex")}.tmp`;
  try {
    fs.writeFileSync(temp, bytes, { mode: 0o600, flag: "wx" });
    fs.renameSync(temp, file);
  } finally {
    try { fs.unlinkSync(temp); } catch (e) { if (e.code !== "ENOENT") throw e; }
  }
}
function freeze(flags) {
  requireOptions(flags,
    ["manifest", "suite", "prices", "out", "executor", "budget-tokens", "seed", "min-effect", "min-clusters", "scorer-key"],
    ["manifest", "suite", "prices", "out", "budget-tokens"]);
  const manifestFile = path.resolve(flags.manifest);
  const suiteFile = path.resolve(flags.suite);
  const pricesFile = path.resolve(flags.prices);
  const outDir = path.resolve(flags.out);
  must(!fs.existsSync(outDir), "freeze output must not already exist (immutable run identity)");
  const m = parseJsonFile(manifestFile);
  const s = parseJsonFile(suiteFile);
  const p = parseJsonFile(pricesFile);
  must(Array.isArray(s.value), "suite must be an array");
  must(obj(p.value) && obj(p.value.models), "prices.models must be an object");
  const thresholds = { min_effect: Number(flags["min-effect"] ?? 0.05), min_clusters: Number(flags["min-clusters"] ?? 5) };
  must(Number.isFinite(thresholds.min_effect) && thresholds.min_effect >= 0 && thresholds.min_effect <= 1,
    "min-effect must be between 0 and 1");
  integer(thresholds.min_clusters, "min-clusters", 5);
  const budget = Number(flags["budget-tokens"]);
  const seed = Number(flags.seed ?? 17);
  const checked = validateManifest(m.value, s.value, p.value, manifestFile, suiteFile,
    flags.executor ?? "synthetic", budget, seed, thresholds);
  const inputs = {
    manifest: { name: "manifest.json", sha256: sha(m.bytes) },
    suite: { name: "suite.json", sha256: sha(s.bytes) },
    prices: { name: "prices.json", sha256: sha(p.bytes) }
  };
  // BF-12: independently managed scorer public key is pinned at freeze time. The private key
  // must never be passed to a model or checked into this frozen task bundle.
  let scorerKeySha256 = null;
  if (flags["scorer-key"] !== undefined) {
    const keyBytes = readLimited(path.resolve(flags["scorer-key"]));
    must(keyBytes.length <= 16 * 1024, "scorer public key is too large");
    let parsedKey;
    try { parsedKey = crypto.createPublicKey(keyBytes); }
    catch (_) { fail("invalid scorer public key"); }
    must(parsedKey.asymmetricKeyType === "ed25519", "scorer key must be Ed25519");
    scorerKeySha256 = sha(keyBytes);
  }
  const lock = {
    schema: SCHEMA,
    experiment_id: m.value.id,
    trusted_scorer_key_sha256: scorerKeySha256,
    executor: flags.executor ?? "synthetic",
    arms: REQUIRED_ARMS,
    model: m.value.arms[0].model,
    budget_tokens_per_arm_task: budget,
    planned_max_total_tokens: checked.plannedTokens,
    matrix_jobs: checked.jobs,
    repeats: m.value.repeats,
    seed,
    oracle: SUPPORTED_ORACLE,
    holdout: m.value.holdout,
    suite_fingerprint_rust: rustSuiteFingerprint(checked.suite),
    price_table_version: p.value.version,
    thresholds,
    inputs,
    caveats: [
      "Synthetic results are infrastructure-only; when executor is provider, require independent scorer and sandbox evidence before promotion.",
      "Declared sandbox is not evidence that a real agent or held-out oracle was isolated.",
      "Input hashes prove equality, not authenticity. Freeze under version control or sign externally."
    ]
  };
  fs.mkdirSync(outDir, { recursive: false, mode: 0o700 });
  for (const [kind, data] of [["manifest", m], ["suite", s], ["prices", p]]) {
    atomicWrite(path.join(outDir, "inputs", inputs[kind].name), data.bytes);
  }
  atomicWrite(path.join(outDir, "lock.json"), Buffer.from(JSON.stringify(lock, null, 2) + "\n"));
  return { lock, lockPath: path.join(outDir, "lock.json") };
}
function loadLock(lockPath) {
  const lockEntry = parseJsonFile(lockPath);
  const lock = lockEntry.value;
  must(obj(lock) && lock.schema === SCHEMA && obj(lock.inputs), "unrecognized lock schema");
  const root = path.dirname(path.resolve(lockPath));
  const bytes = {};
  for (const kind of ["manifest", "suite", "prices"]) {
    const spec = lock.inputs[kind];
    must(obj(spec) && spec.name === `${kind}.json` && /^[0-9a-f]{64}$/.test(spec.sha256), `invalid lock input ${kind}`);
    const file = path.join(root, "inputs", spec.name);
    bytes[kind] = parseJsonFile(file);
    must(sha(bytes[kind].bytes) === spec.sha256, `frozen ${kind} changed since freeze`);
  }
  const manifest = bytes.manifest.value;
  const suite = bytes.suite.value;
  const prices = bytes.prices.value;
  must(Array.isArray(suite) && obj(prices) && obj(prices.models), "corrupt lock inputs");
  const checked = validateManifest(manifest, suite, prices,
    path.join(root, "inputs", "manifest.json"), path.join(root, "inputs", "suite.json"),
    lock.executor, lock.budget_tokens_per_arm_task, lock.seed, lock.thresholds, false);
  // Manifest source locators refer to original input paths; after freeze the task bytes live
  // under inputs/suite.json. Source is checked when freezing, hashes on every later read.
  must(lock.experiment_id === manifest.id && lock.model === manifest.arms[0].model &&
    lock.matrix_jobs === checked.jobs && lock.planned_max_total_tokens === checked.plannedTokens &&
    lock.repeats === manifest.repeats && lock.oracle === manifest.oracle &&
    lock.price_table_version === prices.version &&
    JSON.stringify(lock.arms) === JSON.stringify(REQUIRED_ARMS) &&
    JSON.stringify(lock.holdout) === JSON.stringify(manifest.holdout) &&
    lock.suite_fingerprint_rust === rustSuiteFingerprint(checked.suite),
    "lock metadata does not match its hashed inputs");
  must(lock.trusted_scorer_key_sha256 == null ||
    (typeof lock.trusted_scorer_key_sha256 === "string" && /^[0-9a-f]{64}$/.test(lock.trusted_scorer_key_sha256)),
    "frozen scorer key fingerprint invalid");
  return { lock, lock_sha256: sha(lockEntry.bytes), manifest, tasks: checked.suite, prices, checked, root };
}
function safeReadReport(reportFile) {
  const report = parseJsonFile(reportFile).value;
  must(obj(report) && obj(report.metadata) && Array.isArray(report.results), "not a Kroma JSON benchmark report");
  return report;
}
/** BF-13: independently price EVERY parent and child response using the frozen model-specific
 * rates. This audit never trusts a run-level aggregate as a bill; losing Tournament candidates,
 * retries, timeouts, and missing cache splits remain included or explicitly unpriceable. */
function independentlyPricedCost(result, metadata, ctx) {
  if (ctx.lock.executor !== "provider" ||
      metadata.price_table_version !== ctx.lock.price_table_version ||
      result.usage_basis !== "provider_reported" ||
      !ctx.lock.arms.includes(result.strategy) ||
      result.request_usage_complete !== true ||
      !Array.isArray(result.request_usage) || result.request_usage.length === 0 ||
      result.request_attempts_used !== result.request_usage.length) return null;
  const expected = { input: 0, cached: 0, output: 0 };
  let sum = 0n;
  function priceRequest(request, label) {
    must(obj(request), `${label} must be an object`);
    must(request.usage_basis === "provider_reported", `${label} is not provider-measured`);
    must(Number.isSafeInteger(request.turn) && request.turn >= 0, `${label} missing turn`);
    must(typeof request.request_identity_sha256 === "string" && /^[0-9a-f]{64}$/.test(request.request_identity_sha256),
      `${label} missing wire identity`);
    if (!request.reported_model || !own(ctx.prices.models, request.reported_model)) return null;
    // A missing cache split is never a measured zero, even when the provider reports total input.
    for (const field of ["input_tokens", "cached_input_tokens", "cache_creation_input_tokens", "output_tokens"]) {
      if (!Number.isSafeInteger(request[field]) || request[field] < 0) return null;
    }
    const input = request.input_tokens, cached = request.cached_input_tokens;
    const created = request.cache_creation_input_tokens, output = request.output_tokens;
    must(cached + created <= input, `${label} cached reads+writes exceed input`);
    const rates = ctx.prices.models[request.reported_model];
    if (created > 0 && rates.cache_creation_input_nano_usd == null) return null;
    const value = BigInt(input - cached - created) * BigInt(rates.uncached_input_nano_usd) +
      BigInt(cached) * BigInt(rates.cached_input_nano_usd) +
      BigInt(created) * BigInt(rates.cache_creation_input_nano_usd ?? 0) +
      BigInt(output) * BigInt(rates.output_nano_usd);
    if (value > BigInt(Number.MAX_SAFE_INTEGER)) return null;
    must(request.estimated_cost_nano_usd == null ||
      (Number.isSafeInteger(request.estimated_cost_nano_usd) && BigInt(request.estimated_cost_nano_usd) === value),
      `${label} price estimate does not match frozen rate table`);
    expected.input += input;
    expected.cached += cached;
    expected.output += output;
    for (const field of ["input", "cached", "output"]) {
      if (!Number.isSafeInteger(expected[field])) return null;
    }
    sum += value;
    return true;
  }
  function priceRequests(requests, label) {
    const ids = new Set();
    for (const [i, request] of requests.entries()) {
      const prefix = `${label} ${i}`;
      if (!priceRequest(request, prefix)) return false;
      const identity = `${request.turn}:${request.request_identity_sha256}`;
      // Catch accidentally duplicated evidence for one turn; distinct child scopes may share
      // an identical prompt hash and must not be treated as one request.
      must(!ids.has(identity), `duplicate request identity ${prefix}`);
      ids.add(identity);
    }
    return true;
  }
  if (!priceRequests(result.request_usage, "request")) return null;
  must(expected.input === result.self_input_tokens &&
    expected.cached === result.self_cached_input_tokens &&
    expected.output === result.self_output_tokens,
  `request usage does not reconcile to parent summary for ${result.task_id}/${result.strategy}`);
  must(result.self_tokens === expected.input + expected.output,
    `reported token total does not reconcile to per-request usage for ${result.task_id}`);

  // A legacy report with no child fields can only be priced when it proves no children ran.
  // A present but incomplete child envelope remains unknown even if it reports zero tokens.
  if (result.children_tokens !== 0 && result.child_usage_complete !== true) return null;
  // An old tournament report cannot prove that cancelled children did not issue requests.
  // Keep the legacy single-agent baseline readable without blessing pre-BF13 child accounting.
  if (result.child_usage_complete !== true && result.strategy !== ctx.lock.arms[0]) return null;
  if (result.child_usage_complete === false) return null;
  const children = result.child_request_usage;
  if (children != null && !Array.isArray(children)) return null;
  const childrenObserved = Array.isArray(children) ? children : [];
  if (childrenObserved.length && result.child_usage_complete !== true) return null;
  if (result.children_tokens !== 0 && !childrenObserved.length) return null;
  const names = new Set();
  const childTotal = { input: 0, cached: 0, output: 0 };
  for (const child of childrenObserved) {
    if (!obj(child) || typeof child.subagent_id !== "string" || !child.subagent_id ||
        names.has(child.subagent_id) || child.request_usage_complete !== true ||
        !Array.isArray(child.requests) || !Number.isSafeInteger(child.request_attempts_used) ||
        child.request_attempts_used < 0 || child.requests.length !== child.request_attempts_used) return null;
    names.add(child.subagent_id);
    const before = { ...expected };
    if (!priceRequests(child.requests, `child ${child.subagent_id} request`)) return null;
    if (expected.input - before.input !== child.input_tokens ||
        expected.cached - before.cached !== child.cached_input_tokens ||
        expected.output - before.output !== child.output_tokens) {
      must(false, `child ${child.subagent_id} request usage does not reconcile to its summary`);
    }
    childTotal.input += child.input_tokens;
    childTotal.cached += child.cached_input_tokens;
    childTotal.output += child.output_tokens;
    if (!Object.values(childTotal).every(Number.isSafeInteger)) return null;
  }
  must(childTotal.input === result.children_input_tokens &&
    childTotal.cached === result.children_cached_input_tokens &&
    childTotal.output === result.children_output_tokens &&
    childTotal.input + childTotal.output === result.children_tokens,
    `child/tournament usage does not reconcile to the run tree for ${result.task_id}/${result.strategy}`);
  if (sum > BigInt(Number.MAX_SAFE_INTEGER)) return null;
  must(result.estimated_cost_nano_usd == null ||
    (Number.isSafeInteger(result.estimated_cost_nano_usd) && BigInt(result.estimated_cost_nano_usd) === sum),
    `result ${result.task_id}/${result.strategy}: price estimate does not match frozen operator rate table`);
  return Number(sum);
}

function validateReport(ctx, report) {
  const { lock, manifest, tasks } = ctx;
  must(report.executor === lock.executor, `report executor ${report.executor} != frozen ${lock.executor}`);
  must(report.budget_tokens === lock.budget_tokens_per_arm_task, "report per-arm budget differs from lock");
  must(report.metadata.seed === lock.seed && report.metadata.repetitions === manifest.repeats &&
    report.metadata.randomized_order === true, "seed/repetitions/order do not match freeze");
  must(report.metadata.suite_sha256 === lock.suite_fingerprint_rust,
    "report suite/oracle fingerprint differs from frozen suite");
  must(report.results.length === lock.matrix_jobs, `expected ${lock.matrix_jobs} individual results, got ${report.results.length}`);
  const tasksById = new Map(tasks.map(task => [task.id, task]));
  const seen = new Set();
  const sanitized = [];
  for (const r of report.results) {
    must(obj(r) && tasksById.has(r.task_id) && lock.arms.includes(r.strategy), "unknown task or arm in results");
    integer(r.repetition, "repetition", 0);
    must(r.repetition < lock.repeats, "repetition outside frozen range");
    const key = JSON.stringify([r.task_id, r.strategy, r.repetition]);
    must(!seen.has(key), `duplicate result ${key}`);
    seen.add(key);
    must(typeof r.resolved === "boolean" && (r.gate_status === (r.resolved ? "approved" : "rejected")),
      `gate_status mismatches independent resolved flag for ${key}`);
    for (const field of ["self_tokens", "children_tokens", "total_tokens", "wall_time_ms", "regressions", "budget_tokens", "budget_overage", "verify_time_ms"]) {
      integer(r[field] ?? (field === "verify_time_ms" ? 0 : undefined), `${key}.${field}`);
    }
    must(r.total_tokens === r.self_tokens + r.children_tokens, `token arithmetic mismatch for ${key}`);
    must(r.budget_tokens === lock.budget_tokens_per_arm_task, `per-result cap mismatch for ${key}`);
    must(r.budget_overage === Math.max(0, r.total_tokens - r.budget_tokens), `overage mismatch for ${key}`);
    must(typeof r.cap_ok === "boolean" && typeof r.finished_normally === "boolean", `invalid status flags for ${key}`);
    must(r.cap_ok === (r.budget_overage === 0), `cap_ok disagrees with reported budget overage for ${key}`);
    must(r.budget_exceeded === (r.budget_overage > 0), `budget_exceeded disagrees with overage for ${key}`);
    must(r.regressions === Number(tasksById.get(r.task_id).regression_guard && !r.resolved), `regression-guard counter mismatch for ${key}`);
    must(typeof r.usage_basis === "string", `usage basis absent for ${key}`);
    if (r.request_usage !== undefined) {
      must(Array.isArray(r.request_usage), `request ledger must be an array for ${key}`);
      integer(r.request_attempts_used, `request_attempts_used for ${key}`, 0);
      must(r.request_usage.length <= r.request_attempts_used, `request records exceed transmitted attempts for ${key}`);
      if (r.request_usage_complete === true) {
        must(r.request_usage.length === r.request_attempts_used && r.request_usage.length > 0 &&
          r.request_usage.every(record => obj(record) && record.usage_basis === "provider_reported"),
          `request usage marked complete despite missing/estimated attempts for ${key}`);
      }
    }
    must(r.estimated_cost_nano_usd == null || Number.isSafeInteger(r.estimated_cost_nano_usd), `invalid priced value for ${key}`);
    if (r.child_request_usage !== undefined) {
      must(Array.isArray(r.child_request_usage), `child request ledger must be an array for ${key}`);
    }
    // No tool outputs, final text, prompts, or failure trace are included in the derived evidence.
    const pricedCost = independentlyPricedCost(r, report.metadata, ctx);
    sanitized.push({
      task_id: r.task_id,
      strategy: r.strategy,
      repetition: r.repetition,
      resolved: r.resolved,
      cap_ok: r.cap_ok,
      finished_normally: r.finished_normally,
      regressions: r.regressions,
      total_tokens: r.total_tokens,
      budget_overage: r.budget_overage,
      wall_time_ms: r.wall_time_ms,
      verify_time_ms: r.verify_time_ms ?? 0,
      usage_basis: r.usage_basis,
      cost_estimate_nano_usd: pricedCost,
      reported_model: r.reported_model ?? null
    });
  }
  for (const task of tasks) for (const arm of lock.arms) for (let rep = 0; rep < lock.repeats; rep++) {
    must(seen.has(JSON.stringify([task.id, arm, rep])), `missing result ${task.id}/${arm}/r${rep}`);
  }
  if (lock.executor === "provider") {
    must(report.metadata.model === lock.model, "requested provider model differs from lock");
    must(typeof report.metadata.protocol === "string" && report.metadata.protocol.length > 0,
      "provider protocol identity missing");
  }
  if (Array.isArray(report.arms)) {
    for (const arm of report.arms) {
      const observed = sanitized.filter(r => r.strategy === arm.strategy);
      must(observed.length && arm.tasks === observed.length && arm.resolved === observed.filter(r => r.resolved).length,
        `reported aggregate inconsistent for ${arm.strategy}`);
    }
  }
  return sanitized;
}
function wilson(k, n) {
  if (!n) return [null, null];
  const z = 1.959963984540054;
  const p = k / n, d = 1 + z * z / n;
  const c = (p + z*z/(2*n))/d;
  const h = z * Math.sqrt((p*(1-p) + z*z/(4*n))/n)/d;
  return [Math.max(0,c-h), Math.min(1,c+h)];
}
function bootstrapTasks(byTask, seed, draws = 2000) {
  const clusters = [...byTask.values()];
  const n = clusters.length;
  if (!n) return [null, null];
  let state = (seed >>> 0) || 1;
  function next() { state ^= state << 13; state ^= state >>> 17; state ^= state << 5; return state >>> 0; }
  const means = [];
  for (let b = 0; b < draws; b++) {
    let total = 0, count = 0;
    for (let i = 0; i < n; i++) {
      for (const delta of clusters[next() % n]) { total += delta; count++; }
    }
    means.push(total / count);
  }
  means.sort((a,b) => a-b);
  return [means[Math.floor(draws * 0.025)], means[Math.floor(draws * 0.975)]];
}
function summarize(ctx, results, metadata) {
  const { lock, manifest } = ctx;
  const byKey = new Map(results.map(r => [JSON.stringify([r.task_id,r.strategy,r.repetition]),r]));
  const arms = [];
  for (const strategy of lock.arms) {
    const rows = results.filter(r => r.strategy === strategy);
    const successes = rows.filter(r => r.resolved).length;
    const costComplete = rows.every(r => r.cost_estimate_nano_usd !== null);
    const summedCost = costComplete ? rows.reduce((a,r) => a + BigInt(r.cost_estimate_nano_usd), 0n) : null;
    const totalCost = summedCost !== null && summedCost <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(summedCost) : null;
    arms.push({
      strategy,
      runs: rows.length,
      independent_tasks: manifest.tasks.length,
      successes,
      pass_rate: successes / rows.length,
      pass_rate_95pct_wilson: wilson(successes, rows.length),
      total_tokens: rows.reduce((a,r) => a + r.total_tokens, 0),
      total_wall_ms: rows.reduce((a,r) => a + r.wall_time_ms, 0),
      over_budget_runs: rows.filter(r => r.budget_overage > 0 || !r.cap_ok).length,
      interrupted_runs: rows.filter(r => !r.finished_normally).length,
      regression_failures: rows.reduce((a,r) => a + r.regressions, 0),
      priced_runs: rows.filter(r => r.cost_estimate_nano_usd !== null).length,
      cost_estimate_nano_usd: totalCost,
      cost_per_success_nano_usd: totalCost === null || !successes ? null : totalCost / successes
    });
  }
  function paired(taskIds) {
    const values = new Map();
    for (const id of taskIds) {
      const observations = [];
      for (let rep = 0; rep < lock.repeats; rep++) {
        const base = byKey.get(JSON.stringify([id,lock.arms[0],rep]));
        const cand = byKey.get(JSON.stringify([id,lock.arms[1],rep]));
        observations.push(Number(cand.resolved) - Number(base.resolved));
      }
      values.set(id, observations);
    }
    const all = [...values.values()].flat();
    const delta = all.reduce((a,b) => a+b,0)/all.length;
    return { clusters: taskIds.length, paired_observations: all.length, delta_pass_rate: delta,
      ci_95pct: bootstrapTasks(values, lock.seed + 901) };
  }
  const holdout = new Set(lock.holdout);
  const devIds = manifest.tasks.map(t=>t.id).filter(id=>!holdout.has(id));
  const holdIds = manifest.tasks.map(t=>t.id).filter(id=>holdout.has(id));
  const development = paired(devIds);
  const holdoutStats = paired(holdIds);
  const caveats = [
    ...lock.caveats,
    "Paired bootstrap resamples independent tasks, retaining within-task repetitions.",
    "Cost is a non-invoice operator estimate; only fully itemized provider-measured parent+child requests with matching frozen model rates are priced. Missing retries/candidates/cache splits remain unknown.",
    "Per-arm Wilson intervals are descriptive over run counts; only task-cluster bootstrap is used for paired inference."
  ];
  let promotion = "inconclusive";
  let why = "not enough independent holdout tasks";
  if (lock.executor === "synthetic") {
    promotion = "not_applicable_synthetic";
    why = "synthetic fixes were fabricated by the executor";
  } else if (holdoutStats.clusters < lock.thresholds.min_clusters) {
    promotion = "inconclusive";
  } else if (arms.some(a=>a.over_budget_runs > 0 || a.interrupted_runs > 0)) {
    why = "budget overage or interrupted runs: no valid cap-locked comparison";
  } else if (!metadata || !metadata.source_fingerprint || metadata.source_fingerprint.source_identity_incomplete ||
       !metadata.source_fingerprint.source_sha256) {
    why = "source identity incomplete";
  } else if (arms[1].regression_failures > 0) {
    promotion = "reject";
    why = "at least one guarded regression failed";
  } else if (holdoutStats.ci_95pct[0] > lock.thresholds.min_effect) {
    // This is a statistical *candidate*, not permission to release: external scorer/sandbox
    // integrity is not attested by the BenchComparisonReport schema.
    promotion = "needs_isolation_attestation";
    why = "positive holdout effect, but no verified external sandbox/scorer provenance";
  } else {
    why = "holdout confidence interval does not clear preregistered minimum effect";
  }
  return {
    schema: SCHEMA,
    experiment_id: lock.experiment_id,
    evidence_kind: lock.executor === "synthetic" ? "synthetic_plumbing_only" : "provider_descriptive_not_a_release_gate",
    frozen_suite_sha256: lock.inputs.suite.sha256,
    rust_suite_fingerprint: lock.suite_fingerprint_rust,
    executor: lock.executor,
    model: lock.executor === "provider" ? lock.model : null,
    source_sha256: /^[a-f0-9]{64}$/.test(metadata?.source_fingerprint?.source_sha256 ?? "") ?
      metadata.source_fingerprint.source_sha256 : null,
    arms,
    paired: { development, holdout: holdoutStats },
    preregistered: lock.thresholds,
    promotion,
    promotion_reason: why,
    caveats
  };
}
function renderReport(summary) {
  const pct = value => value == null ? "unknown" : `${(100*value).toFixed(1)}%`;
  const usd = value => value == null ? "unknown" : `$${(value/1e9).toFixed(6)} (estimated)`;
  const lines = [
    `# Kroma Benchmark Foundation v1: ${summary.experiment_id}`,
    "",
    `Evidence: **${summary.evidence_kind}**. No claim of product superiority follows from this report.`,
    "",
    `Frozen suite SHA-256: \`${summary.frozen_suite_sha256}\``,
    `Executor: \`${summary.executor}\`; model: \`${summary.model ?? "not measured"}\``,
    "",
    "| Arm | Successful / total | Pass rate | Token total | Cost per success | Budget overruns |",
    "|---|---:|---:|---:|---:|---:|"
  ];
  for (const arm of summary.arms) lines.push(`| ${arm.strategy} | ${arm.successes}/${arm.runs} | ${pct(arm.pass_rate)} | ${arm.total_tokens} | ${usd(arm.cost_per_success_nano_usd)} | ${arm.over_budget_runs} |`);
  lines.push("", "## Paired delta (candidate - baseline)", "");
  for (const split of ["development", "holdout"]) {
    const stats = summary.paired[split];
    lines.push(`- ${split}: ${stats.clusters} independent tasks, ${stats.paired_observations} pairs, delta ${pct(stats.delta_pass_rate)}, task-cluster 95% bootstrap [${pct(stats.ci_95pct[0])}, ${pct(stats.ci_95pct[1])}].`);
  }
  if (summary.scorer_attestation) {
    lines.push("", `Independent scorer: **${summary.scorer_attestation.status}**`,
      `Scorer key SHA-256: ${summary.scorer_attestation.scorer_key_sha256}`);
  }
  lines.push("", `**Promotion verdict: ${summary.promotion}** — ${summary.promotion_reason}.`, "", "## Evidence caveats", "");
  for (const caveat of summary.caveats) lines.push(`- ${caveat}`);
  return lines.join("\n") + "\n";
}
// Attestation is a statement by a *separate trusted scorer*, not a proof from the harness itself.
// Only a key whose SHA-256 was frozen into the lock may attest. The payload binds the exact
// report bytes, inputs and source. If any field changes, signature verification must fail.
const SCORER_ATTESTATION_SCHEMA = "kroma.scorer-attestation/1";
const ISOLATION_EVIDENCE_SCHEMA = "kroma.benchmark.isolation-evidence/1";
// BF-14B: the signed matrix alone contains only digest strings. A reviewer must possess the
// actual, bounded bytes under a separate evidence root before the result can enter manual review.
// Artifacts are read and hashed only; their raw contents are NEVER included in summary output.
const RAW_JOB_ARTIFACTS = [
  ["sandbox_probe_sha256", "sandbox-probe.json"],
  ["agent_execution_log_sha256", "agent-execution.json"],
  ["scorer_execution_log_sha256", "scorer-execution.json"],
  ["workspace_snapshot_sha256", "workspace-snapshot.json"]
];
function verifyRawJobArtifacts(rootName, jobs) {
  const root = path.resolve(rootName);
  must(fs.existsSync(root), "raw evidence root not found");
  const st = fs.lstatSync(root);
  must(st.isDirectory() && !st.isSymbolicLink(), "raw evidence root must be a real directory, never a symlink");
  const names = fs.readdirSync(root).sort();
  const expectedDirs = jobs.map(j => j.run_id).sort();
  must(JSON.stringify(names) === JSON.stringify(expectedDirs),
    "raw evidence root must contain exactly the signed job directories");
  const expectedFiles = RAW_JOB_ARTIFACTS.map(([,name]) => name).sort();
  let count = 0, totalBytes = 0;
  for (const job of jobs) {
    // run_id was checked against a tight safe ASCII alphabet by verifyIsolationEvidence.
    const dir = path.join(root, job.run_id);
    const jobSt = fs.lstatSync(dir);
    must(jobSt.isDirectory() && !jobSt.isSymbolicLink(), `raw evidence job dir is not a real directory: ${job.run_id}`);
    must(JSON.stringify(fs.readdirSync(dir).sort()) === JSON.stringify(expectedFiles),
      `raw evidence job ${job.run_id} has missing or unexpected artifacts`);
    for (const [digestKey, name] of RAW_JOB_ARTIFACTS) {
      const file = path.join(dir, name);
      const meta = fs.lstatSync(file);
      must(meta.isFile() && !meta.isSymbolicLink() && meta.size <= 1024 * 1024,
        `unsafe or oversized raw evidence file ${job.run_id}/${name}`);
      totalBytes += meta.size;
      must(totalBytes <= 128 * 1024 * 1024,
        "total raw evidence exceeds 128 MiB safety limit");
      const entry = readLimited(file); // never export bytes, reject symlinks / overlarge files
      must(sha(entry) === job[digestKey],
        `raw evidence bytes differ from the signed digest for ${job.run_id}/${name}`);
      count++;
    }
  }
  return count;
}

const REQUIRED_JOB_ISOLATION_ASSERTIONS = [
  "tool_execution_isolated", "network_policy_enforced", "host_secrets_inaccessible",
  "workspace_writes_confined", "timeout_and_cancellation_enforced", "budget_enforced",
  "oracle_unwritable_by_agent", "scorer_unwritable_by_agent"
];
/** BF-14A: require a signed, exhaustive *external* job-by-job account of the execution and
 * independent grading boundary. These are scorer assertions, NOT a substitute for independently
 * inspecting the OS sandbox, scorer logs or runtime denial tests. Do not expose raw log bytes. */
function verifyIsolationEvidence(ctx, flags, report, reportSha256, signedPayload) {
  if (flags["isolation-proof"] === undefined) return null;
  const filename = path.resolve(flags["isolation-proof"]);
  const meta = fs.lstatSync(filename);
  must(meta.isFile() && !meta.isSymbolicLink() && meta.size <= MAX_JSON_BYTES,
    "isolation evidence must be a bounded regular file, never a symlink");
  const entry = parseJsonFile(filename);
  must(sha(entry.bytes) === signedPayload.isolation_proof_sha256,
    "isolation evidence bytes differ from the externally signed SHA-256");
  const proof = entry.value;
  must(obj(proof), "isolation evidence must be an object");
  checkKeys(proof, ["schema", "experiment_id", "lock_sha256", "report_sha256", "suite_sha256",
    "source_sha256", "oracle", "sandbox_declaration", "scorer_binary_sha256",
    "scorer_environment_sha256", "sandbox_policy_sha256", "jobs"], "isolation evidence");
  must(proof.schema === ISOLATION_EVIDENCE_SCHEMA &&
    proof.experiment_id === ctx.lock.experiment_id &&
    proof.lock_sha256 === ctx.lock_sha256 &&
    proof.report_sha256 === reportSha256 &&
    proof.suite_sha256 === ctx.lock.inputs.suite.sha256 &&
    proof.source_sha256 === signedPayload.source_sha256 &&
    proof.oracle === ctx.lock.oracle &&
    proof.sandbox_declaration === ctx.manifest.sandbox &&
    proof.scorer_binary_sha256 === signedPayload.scorer_binary_sha256,
    "isolation evidence does not match the frozen experiment, report, source or scorer");
  for (const field of ["scorer_environment_sha256", "sandbox_policy_sha256"]) {
    must(typeof proof[field] === "string" && /^[0-9a-f]{64}$/.test(proof[field]),
      `isolation evidence missing ${field}`);
  }
  must(Array.isArray(proof.jobs) && proof.jobs.length === ctx.lock.matrix_jobs &&
    proof.jobs.length === report.results.length,
    "isolation evidence must cover every planned and observed job");
  const expected = new Map(report.results.map(r => [JSON.stringify([r.task_id,r.strategy,r.repetition]),r]));
  const seen = new Set(), runIds = new Set();
  for (const [index, job] of proof.jobs.entries()) {
    must(obj(job), `isolation evidence job ${index} is not an object`);
    checkKeys(job, ["task_id", "strategy", "repetition", "run_id", "sandbox_backend",
      "sandbox_probe_sha256", "agent_execution_log_sha256", "scorer_execution_log_sha256",
      "workspace_snapshot_sha256", "oracle_resolved", "oracle_gate_status",
      ...REQUIRED_JOB_ISOLATION_ASSERTIONS], `isolation evidence job ${index}`);
    must(typeof job.task_id === "string" && typeof job.strategy === "string" &&
      Number.isSafeInteger(job.repetition) && job.repetition >= 0,
      `isolation evidence job ${index} has an invalid identity`);
    const key = JSON.stringify([job.task_id, job.strategy, job.repetition]);
    const actual = expected.get(key);
    must(actual && !seen.has(key), `isolation evidence missing, repeated or unexpected job ${key}`);
    seen.add(key);
    must(typeof job.run_id === "string" && /^[a-zA-Z0-9][a-zA-Z0-9._:-]{0,127}$/.test(job.run_id) &&
      !runIds.has(job.run_id), `isolation evidence has a missing or reused run ID for ${key}`);
    runIds.add(job.run_id);
    must(job.sandbox_backend === "bwrap" || job.sandbox_backend === "container",
      `isolation evidence job ${key} lacks an OS sandbox backend`);
    for (const field of ["sandbox_probe_sha256", "agent_execution_log_sha256",
      "scorer_execution_log_sha256", "workspace_snapshot_sha256"]) {
      must(typeof job[field] === "string" && /^[0-9a-f]{64}$/.test(job[field]),
        `isolation evidence job ${key} missing ${field}`);
    }
    for (const field of REQUIRED_JOB_ISOLATION_ASSERTIONS) {
      must(job[field] === true, `isolation evidence job ${key} did not verify ${field}`);
    }
    must(typeof job.oracle_resolved === "boolean" &&
      job.oracle_resolved === actual.resolved &&
      job.oracle_gate_status === actual.gate_status,
      `independent scoring result disagrees with the Rust result for ${key}`);
  }
  must(seen.size === ctx.lock.matrix_jobs, "isolation evidence lacks expected jobs");
  const rawArtifacts = flags["evidence-root"] === undefined ? null :
    verifyRawJobArtifacts(flags["evidence-root"], proof.jobs);
  return {
    status: rawArtifacts === null ? "signed_job_matrix_consistent_raw_missing" :
      "signed_job_matrix_and_raw_digests_consistent",
    raw_artifacts_verified: rawArtifacts !== null,
    raw_artifact_count: rawArtifacts,
    evidence_sha256: sha(entry.bytes),
    job_count: seen.size,
    sandbox_policy_sha256: proof.sandbox_policy_sha256,
    scorer_environment_sha256: proof.scorer_environment_sha256,
    limitation: "Raw digest equality and signed per-job claims are NOT proof of kernel enforcement; independent inspection of actual denial probes, scorer separation and credential broker remains mandatory"
  };
}
function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (obj(value)) return `{${Object.keys(value).sort().map(k => `${JSON.stringify(k)}:${canonicalJson(value[k])}`).join(",")}}`;
  return JSON.stringify(value);
}
function verifyScorerAttestation(ctx, flags, report, reportSha256) {
  const linked = ["attestation", "scorer-key", "expect-lock-sha256"];
  must(flags["evidence-root"] === undefined || typeof flags["isolation-proof"] === "string",
    "raw artifacts require a signed isolation evidence matrix");
  must(flags["isolation-proof"] === undefined || linked.every(key => typeof flags[key] === "string"),
    "isolation evidence must be linked to a signed scorer attestation and externally pinned lock");
  if (!linked.some(key => flags[key] !== undefined)) return null;
  must(linked.every(key => typeof flags[key] === "string"),
    "signed scoring needs --attestation, --scorer-key, and --expect-lock-sha256 together");
  must(ctx.lock.executor === "provider", "synthetic data cannot receive real scoring attestation");
  must(typeof ctx.lock.trusted_scorer_key_sha256 === "string", "scorer public key was not pinned during freeze");
  const lockSha = ctx.lock_sha256;
  must(flags["expect-lock-sha256"] === lockSha,
    "externally pinned lock SHA-256 disagrees with current experiment configuration");
  const pubBytes = readLimited(path.resolve(flags["scorer-key"]));
  must(sha(pubBytes) === ctx.lock.trusted_scorer_key_sha256, "scorer public key is not the frozen key");
  const attestationEntry = parseJsonFile(path.resolve(flags.attestation));
  const envelope = attestationEntry.value;
  checkKeys(envelope, ["schema", "payload", "signature_b64"], "scorer attestation envelope");
  must(envelope.schema === SCORER_ATTESTATION_SCHEMA, "unknown scorer attestation schema");
  checkKeys(envelope.payload, ["experiment_id", "lock_sha256", "report_sha256", "suite_sha256",
    "oracle", "source_sha256", "scorer_binary_sha256", "isolation_proof_sha256", "sandbox_passed",
    "all_jobs_isolated", "scorer_unwritable_by_agent"], "scorer attestation payload");
  const p = envelope.payload;
  must(p.experiment_id === ctx.lock.experiment_id && p.lock_sha256 === lockSha &&
    p.report_sha256 === reportSha256 &&
    p.suite_sha256 === ctx.lock.inputs.suite.sha256 &&
    p.oracle === ctx.lock.oracle &&
    p.source_sha256 === report.metadata?.source_fingerprint?.source_sha256,
    "scorer attestation is not bound to this exact run, source, task set and oracle");
  must([p.source_sha256, p.scorer_binary_sha256, p.isolation_proof_sha256].every(v =>
    typeof v === "string" && /^[0-9a-f]{64}$/.test(v)),
    "scorer or isolation identity missing");
  must(p.sandbox_passed === true && p.all_jobs_isolated === true &&
    p.scorer_unwritable_by_agent === true,
    "external scorer did not attest to passing isolation and immutable oracle");
  must(typeof envelope.signature_b64 === "string" &&
    /^[A-Za-z0-9+/]+={0,2}$/.test(envelope.signature_b64), "malformed scorer signature");
  let key;
  try { key = crypto.createPublicKey(pubBytes); }
  catch (_) { fail("scorer public key is unreadable"); }
  must(key.asymmetricKeyType === "ed25519", "invalid Ed25519 scorer public key");
  const signature = Buffer.from(envelope.signature_b64, "base64");
  must(signature.length === 64 && crypto.verify(null, Buffer.from(canonicalJson(p)), key, signature),
    "scorer signature invalid; refusing untrusted scoring evidence");
  const isolationEvidence = verifyIsolationEvidence(ctx, flags, report, reportSha256, p);
  return {
    status: "verified_by_pre_registered_scorer_key",
    isolation_evidence: isolationEvidence,
    scorer_key_sha256: ctx.lock.trusted_scorer_key_sha256,
    attestation_sha256: sha(attestationEntry.bytes),
    scorer_binary_sha256: p.scorer_binary_sha256,
    isolation_proof_sha256: p.isolation_proof_sha256,
    // Even a valid attestation cannot prove that the signer was honest or that no task leakage
    // occurred before isolation; human review remains mandatory.
    limitation: "cryptographic key verification authenticates the external scorer statement, not its truth"
  };
}
function evaluate(flags) {
  requireOptions(flags, ["lock", "report", "out", "attestation", "scorer-key", "expect-lock-sha256", "isolation-proof", "evidence-root"],
    ["lock", "report", "out"]);
  const ctx = loadLock(flags.lock);
  const reportEntry = parseJsonFile(flags.report);
  const report = reportEntry.value;
  must(obj(report) && obj(report.metadata) && Array.isArray(report.results),
    "not a Kroma JSON benchmark report");
  const results = validateReport(ctx, report);
  const summary = summarize(ctx, results, report.metadata);
  const scoringEvidence = verifyScorerAttestation(ctx, flags, report, sha(reportEntry.bytes));
  if (scoringEvidence) {
    summary.scorer_attestation = scoringEvidence;
    summary.evidence_kind = "provider_scored_with_frozen_external_attestation";
    summary.caveats.push(scoringEvidence.limitation);
    // BF-14A: legacy signed BF-12 boolean statements have no corresponding checked evidence
    // file. Authenticate them for backwards compatibility, but NEVER upgrade a result solely on
    // their word. The signed, complete job matrix remains subject to independent human review.
    if (scoringEvidence.isolation_evidence) {
      summary.evidence_kind = scoringEvidence.isolation_evidence.raw_artifacts_verified ?
        "provider_scored_with_signed_matrix_and_raw_bytes" :
        "provider_scored_with_signed_matrix_raw_missing";
      summary.caveats.push(scoringEvidence.isolation_evidence.limitation);
      if (summary.promotion === "needs_isolation_attestation") {
        if (scoringEvidence.isolation_evidence.raw_artifacts_verified === true) {
          summary.promotion = "eligible_for_manual_review";
          summary.promotion_reason = "statistical thresholds met, signed job matrix and all referenced raw evidence bytes are consistent; actual sandbox enforcement and scorer independence still require human inspection";
        } else {
          summary.promotion_reason = "signed job matrix has no independently supplied raw evidence files; manual review is blocked until every signed artifact digest is checked";
        }
      }
    } else {
      summary.caveats.push("Legacy scorer attestation has no checked per-job isolation evidence; cannot qualify for manual promotion review");
    }
  }
  const outDir = path.resolve(flags.out);
  must(!fs.existsSync(outDir), "analysis output must not already exist (immutable evidence)");
  fs.mkdirSync(outDir, { recursive: false, mode: 0o700 });
  // Do not persist the raw provider report: it may contain tool logs, model replies and secrets.
  atomicWrite(path.join(outDir, "results.redacted.json"), Buffer.from(JSON.stringify(results, null, 2) + "\n"));
  atomicWrite(path.join(outDir, "metrics.json"), Buffer.from(JSON.stringify(summary, null, 2) + "\n"));
  atomicWrite(path.join(outDir, "report.md"), Buffer.from(renderReport(summary)));
  return { summary, outDir };
}
/** BF-10: frozen Manifest -> existing Rust CLI -> independent evaluation.
 * This is not a new agent executor. We intentionally do NOT pass --experiment to Rust: in the
 * current Rust CLI that flag is a DRY RUN and would silently skip the actual benchmark.
 * Provider dispatch remains fail-closed until a separate, real tool+sandbox boundary is proven. */
function nativePlan(ctx) {
  const { lock } = ctx;
  const argv = [
    "bench", "--suite", path.join(ctx.root, "inputs", "suite.json"),
    "--executor", lock.executor,
    "--budget-tokens", String(lock.budget_tokens_per_arm_task),
    "--repetitions", String(lock.repeats),
    "--seed", String(lock.seed),
    "--workspace", "<private-temporary-workspace>", "--json", "--quiet"
  ];
  if (lock.executor === "provider") {
    argv.push("--bench-model", lock.model,
      "--price-table", path.join(ctx.root, "inputs", "prices.json"));
    // This does not grant host shell access, but alone is NOT an OS sandbox.
    argv.push("--allow-write");
  }
  return {
    experiment_id: lock.experiment_id,
    executor: lock.executor,
    model: lock.executor === "provider" ? lock.model : null,
    job_count: lock.matrix_jobs,
    planned_max_tokens: lock.planned_max_total_tokens,
    frozen_input_sha256: Object.fromEntries(Object.entries(lock.inputs).map(([k,v]) => [k,v.sha256])),
    native_argv: argv,
    runnable_in_this_driver: lock.executor === "synthetic",
    provider_block_reason: lock.executor === "provider" ?
      "Native provider Bench currently isolates the evaluator command, not every model-invoked tool; an external real execution sandbox + consent must be independently verified before automatic paid dispatch." : null
  };
}
function hashFile(file, max = 256 * 1024 * 1024) {
  const st = fs.lstatSync(file);
  must(st.isFile() && !st.isSymbolicLink() && st.size <= max, `${file}: unsafe or oversized executable`);
  const hasher = crypto.createHash("sha256");
  const handle = fs.openSync(file, "r");
  const chunk = Buffer.allocUnsafe(1024 * 1024);
  try {
    let offset = 0;
    while (offset < st.size) {
      const n = fs.readSync(handle, chunk, 0, Math.min(chunk.length, st.size - offset), offset);
      must(n > 0, "executable changed during hashing");
      hasher.update(chunk.subarray(0,n));
      offset += n;
    }
  } finally { fs.closeSync(handle); }
  return hasher.digest("hex");
}
function runLocked(flags) {
  requireOptions(flags, ["lock", "kroma", "out", "timeout-ms", "expect-lock-sha256"], ["lock", "kroma", "out"]);
  const ctx = loadLock(flags.lock);
  must(ctx.lock.executor === "synthetic",
    "provider auto-run is blocked: no attested tool sandbox and no explicit paid-run authorization; freeze/plan/evaluate remain available");
  const lockDigest = ctx.lock_sha256;
  if (flags["expect-lock-sha256"] !== undefined) {
    must(/^[0-9a-f]{64}$/.test(flags["expect-lock-sha256"]) && flags["expect-lock-sha256"] === lockDigest,
      "external lock digest does not match; refusing to execute");
  }
  const timeout = Number(flags["timeout-ms"] ?? 120000);
  integer(timeout, "timeout-ms", 1000);
  must(timeout <= 3600000, "timeout-ms cannot exceed one hour");
  const binary = path.resolve(flags.kroma);
  const st = fs.lstatSync(binary);
  must(st.isFile() && !st.isSymbolicLink(), "--kroma must be a regular executable, not a symlink");
  fs.accessSync(binary, fs.constants.X_OK);
  const binaryDigest = hashFile(binary);
  must(!fs.existsSync(path.resolve(flags.out)), "execution output must not already exist");
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-bench-v1-"));
  try {
    const workspace = path.join(scratch, "workspace");
    fs.mkdirSync(workspace, { mode: 0o700 });
    const plan = nativePlan(ctx);
    const argv = plan.native_argv.map(x => x === "<private-temporary-workspace>" ? workspace : x);
    const started = process.hrtime.bigint();
    const run = spawnSync(binary, argv, {
      cwd: scratch,
      encoding: "utf8", timeout, maxBuffer: MAX_JSON_BYTES,
      env: { PATH: process.env.PATH || "", HOME: scratch, TMPDIR: scratch },
      shell: false
    });
    const wallMs = Number((process.hrtime.bigint() - started) / 1000000n);
    must(!run.error && run.signal == null && run.status === 0,
      `Kroma synthetic execution failed (exit=${run.status ?? "unknown"}, signal=${run.signal ?? "none"}); raw diagnostics intentionally not exported`);
    must(hashFile(binary) === binaryDigest, "benchmark executable changed during execution");
    let report;
    try { report = JSON.parse(run.stdout); } catch (_) { fail("Kroma synthetic run produced no valid JSON report"); }
    const rawFile = path.join(scratch, "rust-report.json");
    fs.writeFileSync(rawFile, run.stdout, { mode: 0o600, flag: "wx" });
    const out = evaluate({ lock: flags.lock, report: rawFile, out: flags.out });
    atomicWrite(path.join(out.outDir, "execution-receipt.json"), Buffer.from(JSON.stringify({
      schema: SCHEMA,
      experiment_id: ctx.lock.experiment_id,
      executor: "synthetic",
      evidence_kind: "synthetic_plumbing_only",
      lock_sha256: lockDigest,
      executable_sha256: binaryDigest,
      native_report_sha256: sha(Buffer.from(run.stdout)),
      tool_exit: run.status,
      wall_time_ms: wallMs,
      executed_job_count: report.results.length,
      raw_report_persisted: false,
      externally_verified_provider_success: false
    }, null, 2) + "\n"));
    return out;
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
}
/** Previous smoke alias retained for reproducibility of Foundation v1 instructions/tests. */
function smoke(flags) {
  requireOptions(flags, ["lock", "kroma", "out"], ["lock", "kroma", "out"]);
  const ctx = loadLock(flags.lock);
  must(ctx.lock.executor === "synthetic", "smoke refuses provider execution: this command is offline only");
  return runLocked(flags);
}

function main() {
  try {
    const { verb, flags } = parseArgs(process.argv.slice(2));
    if (verb === "freeze") {
      const out = freeze(flags);
      console.log(`FROZEN ${out.lock.experiment_id}: ${out.lock.matrix_jobs} jobs, lock=${out.lockPath}`);
    } else if (verb === "evaluate") {
      const out = evaluate(flags);
      console.log(`EVALUATED ${out.summary.experiment_id}: ${out.summary.promotion}, ${out.outDir}`);
    } else if (verb === "smoke") {
      const out = smoke(flags);
      console.log(`SYNTHETIC SMOKE ${out.summary.experiment_id}: ${out.summary.promotion}, ${out.outDir}`);
    } else if (verb === "run") {
      const out = runLocked(flags);
      console.log(`FROZEN RUN ${out.summary.experiment_id}: ${out.summary.promotion}, ${out.outDir}`);
    } else if (verb === "plan") {
      requireOptions(flags, ["lock"], ["lock"]);
      const ctx = loadLock(flags.lock);
      console.log(JSON.stringify(nativePlan(ctx), null, 2));
    } else {
      fail("usage: bench-foundation.js freeze|plan|run|evaluate|smoke --<key> <value> ... (see docs/benchmark/FOUNDATION_V1.md)");
    }
  } catch (error) {
    console.error(`${error.name === "BenchFoundationError" ? "BENCH_PRECHECK_FAILED" : "BENCH_ERROR"}: ${error.message}`);
    process.exitCode = 2;
  }
}
if (require.main === module) main();
module.exports = {
  BenchFoundationError, freeze, loadLock, rustSuiteFingerprint, ensureTask,
  validateReport, summarize, renderReport, evaluate, validateManifest, smoke,
  nativePlan, runLocked, independentlyPricedCost,
  canonicalJson, verifyScorerAttestation
};
