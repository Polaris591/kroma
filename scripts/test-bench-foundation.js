#!/usr/bin/env node
"use strict";

/** Offline foundation contract tests: no provider calls, no spawned shell, no secrets. */
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { freeze, loadLock, evaluate, BenchFoundationError, rustSuiteFingerprint } = require("./bench-foundation.js");

const repo = path.join(__dirname, "..");
const inputs = path.join(repo, "experiments", "foundation-v1");
function clone() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-bf-v1-test-"));
  for (const file of ["manifest.json", "suite.json", "prices.json"]) {
    fs.copyFileSync(path.join(inputs, file), path.join(root, file));
  }
  return {
    root,
    get(name) { return JSON.parse(fs.readFileSync(path.join(root, `${name}.json`), "utf8")); },
    put(name, data) { fs.writeFileSync(path.join(root, `${name}.json`), JSON.stringify(data, null, 2)); },
    flags(extra = {}) { return {
      manifest: path.join(root, "manifest.json"), suite: path.join(root, "suite.json"),
      prices: path.join(root, "prices.json"), out: path.join(root, "lock"),
      executor: "synthetic", "budget-tokens": "10000", seed: "12345", ...extra
    }; },
    freeze(extra = {}) { return freeze(this.flags(extra)); },
    cleanup() { fs.rmSync(root, { recursive: true, force: true }); }
  };
}
function scoped(fn) {
  const f = clone();
  try { return fn(f); }
  finally { f.cleanup(); }
}
function providerFixture(f) {
  const manifest = f.get("manifest");
  const prices = f.get("prices");
  manifest.id = "BF01-PROVIDER-IMPORT-TEST-ONLY";
  manifest.sandbox = "bwrap-declared-not-attested";
  for (const arm of manifest.arms) {
    arm.model = "fixture-model-a";
    arm.price_table = "operator-fixture-price-table";
  }
  prices.version = "operator-fixture-price-table";
  prices.source = "unit test rate table, not an invoice";
  prices.models = { "fixture-model-a": prices.models["demo-only-model"] };
  f.put("manifest", manifest);
  f.put("prices", prices);
}

function refuses(callback, needle) {
  assert.throws(callback, error => error instanceof BenchFoundationError && error.message.includes(needle),
    `expected a fail-closed refusal containing ${needle}`);
}
function exampleReport(ctx, overrides = {}) {
  const { lock, tasks } = ctx;
  const results = [];
  for (let rep = 0; rep < lock.repeats; rep++) for (const task of tasks) for (const arm of lock.arms) {
    const resolved = task.id.startsWith("fixable") || arm !== "single_agent";
    const self_tokens = 900;
    const children_tokens = arm === "single_agent" ? 0 : 750;
    results.push({
      task_id: task.id, strategy: arm, repetition: rep,
      resolved, gate_status: resolved ? "approved" : "rejected",
      self_tokens, children_tokens, total_tokens: self_tokens + children_tokens,
      budget_tokens: lock.budget_tokens_per_arm_task,
      budget_overage: 0, budget_exceeded: false, cap_ok: true, finished_normally: true,
      wall_time_ms: 10, verify_time_ms: 0,
      regressions: Number(task.regression_guard && !resolved),
      usage_basis: "unknown", estimated_cost_nano_usd: null,
      final_text: "PRIVATE_TOKEN_SHOULD_NEVER_BE_PERSISTED",
      tool_trace: ["Sensitive private repo path"]
    });
  }
  return {
    suite: "suite", executor: overrides.executor ?? lock.executor,
    budget_tokens: lock.budget_tokens_per_arm_task,
    metadata: { seed: lock.seed, repetitions: lock.repeats, randomized_order: true,
      suite_sha256: lock.suite_fingerprint_rust, model: overrides.model ?? null,
      protocol: overrides.protocol ?? null, source_fingerprint: overrides.source_fingerprint ?? null,
      price_table_version: overrides.price_table_version ?? null },
    results,
    arms: lock.arms.map(strategy => ({ strategy, tasks: results.filter(r=>r.strategy === strategy).length,
      resolved: results.filter(r=>r.strategy === strategy && r.resolved).length }))
  };
}
function writeReport(f, report) {
  const file = path.join(f.root, "native-report.json");
  fs.writeFileSync(file, JSON.stringify(report));
  return file;
}
function evalFixture(f, modify = report => report) {
  const { lockPath } = f.freeze();
  const ctx = loadLock(lockPath);
  const report = modify(exampleReport(ctx));
  const file = writeReport(f, report);
  return evaluate({ lock: lockPath, report: file, out: path.join(f.root, "analysis") });
}

test("freeze: writes immutable input copies, hex IDs, deterministic 24-job token cap", () => scoped(f => {
  const output = f.freeze();
  const ctx = loadLock(output.lockPath);
  assert.equal(ctx.lock.matrix_jobs, 24);
  assert.equal(ctx.lock.planned_max_total_tokens, 240000);
  assert.deepEqual(ctx.lock.holdout, ["fixable-smoke-five", "stall-smoke-six"]);
  assert.match(ctx.lock.suite_fingerprint_rust, /^[0-9a-f]{16}$/);
  assert.equal(ctx.lock.suite_fingerprint_rust, rustSuiteFingerprint(ctx.tasks));
  assert.equal(ctx.lock.executor, "synthetic");
}));

test("freeze refuses existing target rather than modifying existing evidence", () => scoped(f => {
  f.freeze();
  refuses(() => f.freeze(), "must not already exist");
}));

test("freeze rejects a misleading strategy that the Rust CLI cannot execute", () => scoped(f => {
  const m = f.get("manifest"); m.arms[1].id = "candidate-on-demand-tools"; f.put("manifest", m);
  refuses(() => f.freeze(), "Rust CLI currently executes only");
}));

test("freeze rejects absent/duplicated/unknown holdout", () => scoped(f => {
  let m = f.get("manifest"); delete m.holdout; f.put("manifest", m);
  refuses(() => f.freeze(), "explicit nonempty holdout");
  m = f.get("manifest"); m.holdout = ["not-a-task"]; f.put("manifest", m);
  refuses(() => f.freeze(), "unknown task");
  m.holdout = ["stall-smoke-six", "stall-smoke-six"]; f.put("manifest", m);
  refuses(() => f.freeze(), "duplicate holdout");
}));

test("freeze refuses invalid initial task fixture paths", () => scoped(f => {
  const suite = f.get("suite"); suite[0].files = { "../../secrets": "x" }; f.put("suite", suite);
  refuses(() => f.freeze(), "unsafe initial file");
}));

test("freeze rejects cross-platform path hazards in expected files", () => scoped(f => {
  const suite = f.get("suite"); suite[0].expect_contains = [["C:/Windows/system32", "x"]]; f.put("suite", suite);
  refuses(() => f.freeze(), "unsafe/empty expect_contains");
}));

test("freeze rejects a vacuous oracle that passes before any run", () => scoped(f => {
  const suite = f.get("suite"); suite[0].expect_contains = [["src/one.txt", "TODO"]]; f.put("suite", suite);
  refuses(() => f.freeze(), "already satisfies all static post-conditions");
}));

test("freeze rejects empty post-condition needles", () => scoped(f => {
  const suite = f.get("suite"); suite[0].expect_contains = [["src/one.txt", ""]]; f.put("suite", suite);
  refuses(() => f.freeze(), "unsafe/empty expect_contains");
}));

test("freeze rejects manifest/suite ID mismatches", () => scoped(f => {
  const m = f.get("manifest"); m.tasks[0].id = "other"; f.put("manifest", m);
  refuses(() => f.freeze(), "suite task id");
}));

test("freeze rejects an incorrect external suite source instead of silently using another", () => scoped(f => {
  const m = f.get("manifest"); m.tasks[0].source = "different.json#0"; f.put("manifest", m);
  refuses(() => f.freeze(), "must resolve to the frozen suite");
}));

test("freeze rejects declared token budgets that cannot cover the matrix", () => scoped(f => {
  refuses(() => f.freeze({ "budget-tokens": "10001" }), "tokens insufficient");
}));

test("freeze requires matching model identity for a same-model A/B", () => scoped(f => {
  const m = f.get("manifest"); m.arms[1].model = "other-model"; f.put("manifest", m);
  refuses(() => f.freeze(), "missing price rates");
}));

test("freeze requires a real oracle contract, not a prose label", () => scoped(f => {
  const m = f.get("manifest"); m.oracle = "just grade it"; f.put("manifest", m);
  refuses(() => f.freeze(), "manifest.oracle");
}));

test("lock fails closed when frozen suite bytes change", () => scoped(f => {
  const output = f.freeze();
  fs.appendFileSync(path.join(f.root, "lock", "inputs", "suite.json"), "\n");
  refuses(() => loadLock(output.lockPath), "frozen suite changed");
}));

test("lock refuses modified derived metadata without replacing hashed inputs", () => scoped(f => {
  const output = f.freeze();
  const lock = JSON.parse(fs.readFileSync(output.lockPath));
  lock.model = "different-model";
  fs.writeFileSync(output.lockPath, JSON.stringify(lock));
  refuses(() => loadLock(output.lockPath), "lock metadata does not match");
}));

test("evaluation produces redacted evidence and explicitly refuses synthetic promotion", () => scoped(f => {
  const { summary, outDir } = evalFixture(f);
  assert.equal(summary.promotion, "not_applicable_synthetic");
  assert.equal(summary.paired.holdout.clusters, 2);
  assert.equal(summary.arms[0].successes, 6);
  assert.equal(summary.arms[1].successes, 12);
  assert.equal(summary.arms[0].cost_per_success_nano_usd, null);
  const files = fs.readdirSync(outDir).sort();
  assert.deepEqual(files, ["metrics.json", "report.md", "results.redacted.json"]);
  for (const file of files) {
    assert.ok(!fs.readFileSync(path.join(outDir,file), "utf8").includes("PRIVATE_TOKEN_SHOULD_NEVER_BE_PERSISTED"));
  }
  assert.match(fs.readFileSync(path.join(outDir,"report.md"),"utf8"), /synthetic_plumbing_only/);
}));

test("evaluation fails closed on altered Rust suite fingerprint", () => scoped(f => {
  refuses(() => evalFixture(f, r => { r.metadata.suite_sha256 = "0".repeat(16); return r; }), "suite/oracle fingerprint");
}));

test("evaluation fails closed on incorrect randomized order or seed", () => scoped(f => {
  refuses(() => evalFixture(f, r => { r.metadata.seed = 1; return r; }), "seed/repetitions/order");
}));

test("evaluation refuses missing and duplicate results", () => scoped(f => {
  refuses(() => evalFixture(f, r => { r.results.pop(); return r; }), "expected 24 individual results");
  // second run needs a distinct output lock directory
  const g = clone();
  try {
    refuses(() => evalFixture(g, r => { r.results[1] = {...r.results[0]}; return r; }), "duplicate result");
  } finally { g.cleanup(); }
}));

test("evaluation refuses forged result gate status", () => scoped(f => {
  refuses(() => evalFixture(f, r => { r.results[0].gate_status = "rejected"; return r; }), "gate_status mismatches");
}));

test("evaluation refuses inconsistent provider/token accounting", () => scoped(f => {
  refuses(() => evalFixture(f, r => { r.results[0].total_tokens += 1; return r; }), "token arithmetic mismatch");
}));

test("evaluation refuses hidden budget overage", () => scoped(f => {
  refuses(() => evalFixture(f, r => { r.results[0].budget_overage = 1; return r; }), "overage mismatch");
}));

test("evaluation refuses fabricated regression counter", () => scoped(f => {
  refuses(() => evalFixture(f, r => { r.results[0].regressions = 9; return r; }), "regression-guard counter mismatch");
}));

test("evaluation treats an untrusted priced number as unknown, never a free run", () => scoped(f => {
  const { summary } = evalFixture(f, r => { for (const v of r.results) v.estimated_cost_nano_usd = 123; return r; });
  assert.ok(summary.arms.every(a => a.cost_estimate_nano_usd === null));
  assert.ok(summary.arms.every(a => a.priced_runs === 0));
}));

test("evaluation refuses a fabricated arm aggregate", () => scoped(f => {
  refuses(() => evalFixture(f, r => { r.arms[0].resolved = 99; return r; }), "reported aggregate inconsistent");
}));

test("task-level CI treats repeated observations as correlated", () => scoped(f => {
  const { summary } = evalFixture(f);
  assert.equal(summary.paired.holdout.clusters, 2);
  assert.equal(summary.paired.holdout.paired_observations, 4);
  assert.deepEqual(summary.paired.holdout.ci_95pct, [0, 1]);
}));

test("freeze provider mode is evidence import only; cannot turn 2 holdout tasks into a win", () => scoped(f => {
  providerFixture(f);
  const { lockPath } = f.freeze({ executor: "provider" });
  const ctx = loadLock(lockPath);
  const r = exampleReport(ctx, { executor: "provider", model: ctx.lock.model,
    protocol: "chat-completions", source_fingerprint: { source_sha256: "1".repeat(64), source_identity_incomplete: false } });
  for (const v of r.results) {
    v.usage_basis = "provider_reported";
    v.reported_model = ctx.lock.model;
    v.self_input_tokens = 1;
    v.self_cached_input_tokens = 0;
    v.self_output_tokens = 1;
    v.children_input_tokens = 0;
    v.children_cached_input_tokens = 0;
    v.children_output_tokens = 0;
    v.estimated_cost_nano_usd = 500;
    v.self_tokens = 2;
    v.total_tokens = 2 + v.children_tokens;
    if (v.strategy === "single_agent") {
      v.request_attempts_used = 1;
      v.request_usage_complete = true;
      v.request_usage = [{
        turn: 0,
        request_identity_sha256: require("node:crypto").createHash("sha256")
          .update(`${v.task_id}/${v.strategy}/${v.repetition}`).digest("hex"),
        reported_model: ctx.lock.model, usage_basis: "provider_reported",
        input_tokens: 1, cached_input_tokens: 0, cache_creation_input_tokens: 0,
        output_tokens: 1, estimated_cost_nano_usd: 500
      }];
    }
  }
  r.metadata.price_table_version = ctx.lock.price_table_version;
  const out = evaluate({ lock: lockPath, report: writeReport(f, r), out: path.join(f.root, "analysis") });
  assert.equal(out.summary.promotion, "inconclusive");
  assert.equal(out.summary.arms[0].cost_per_success_nano_usd, 1000);
  assert.equal(out.summary.arms[1].cost_per_success_nano_usd, null); // child identities not available
  assert.equal(out.summary.evidence_kind, "provider_descriptive_not_a_release_gate");
}));

test("provider metadata model must match frozen model", () => scoped(f => {
  providerFixture(f);
  const { lockPath } = f.freeze({ executor: "provider" });
  const ctx = loadLock(lockPath);
  const report = exampleReport(ctx, { executor: "provider", model: "wrong-model", protocol: "chat-completions" });
  refuses(() => evaluate({ lock: lockPath, report: writeReport(f, report), out: path.join(f.root, "analysis") }), "requested provider model differs");
}));

test("evidence output refuses overwrite on second analysis run", () => scoped(f => {
  const { lockPath } = f.freeze();
  const ctx = loadLock(lockPath);
  const file = writeReport(f, exampleReport(ctx));
  const params = { lock: lockPath, report: file, out: path.join(f.root, "analysis") };
  evaluate(params);
  refuses(() => evaluate(params), "must not already exist");
}));

test("CLI help and offline commands fail without network or provider", () => scoped(f => {
  const { spawnSync } = require("node:child_process");
  const run = spawnSync(process.execPath, [path.join(__dirname, "bench-foundation.js"), "freeze",
    "--manifest", path.join(f.root,"manifest.json"), "--suite", path.join(f.root,"suite.json"),
    "--prices", path.join(f.root,"prices.json"), "--out", path.join(f.root,"lock"),
    "--budget-tokens", "10000"], { encoding: "utf8", timeout: 3000, env: { PATH: process.env.PATH } });
  assert.equal(run.status, 0, run.stderr);
  assert.match(run.stdout, /FROZEN/);
}));

test("CLI smoke runs a real executable entry point without saving raw model/tool text", () => scoped(f => {
  const { smoke } = require("./bench-foundation.js");
  const { lockPath } = f.freeze();
  const ctx = loadLock(lockPath);
  const sample = exampleReport(ctx);
  const binary = path.join(f.root, "fake-kroma-executable.js");
  fs.writeFileSync(binary, `#!/usr/bin/env node\n'use strict';\n` +
    `const args=process.argv.slice(2);\n` +
    `if(args[0]!=='bench' || args[args.indexOf('--executor')+1]!=='synthetic'||!args.includes('--json')) process.exit(4);\n` +
    `process.stdout.write(${JSON.stringify(JSON.stringify(sample))});\n`);
  fs.chmodSync(binary, 0o700);
  const { summary } = smoke({ lock: lockPath, kroma: binary, out: path.join(f.root, "smoke-evidence") });
  assert.equal(summary.promotion, "not_applicable_synthetic");
  assert.equal(summary.arms[0].runs, 12);
}));

test("smoke never launches a model even if given a provider-mode experiment", () => scoped(f => {
  const { smoke } = require("./bench-foundation.js");
  providerFixture(f);
  const { lockPath } = f.freeze({ executor: "provider" });
  const binary = path.join(f.root, "fake-executable.js");
  fs.writeFileSync(binary, "#!/usr/bin/env node\nthrow new Error('should not run');\n");
  fs.chmodSync(binary, 0o700);
  refuses(() => smoke({ lock: lockPath, kroma: binary, out: path.join(f.root,"results") }), "smoke refuses provider execution");
  assert.equal(fs.existsSync(path.join(f.root,"results")), false);
}));

test("provider-side partial cost is unknown when the price estimate conflicts with rates", () => scoped(f => {
  providerFixture(f);
  const { lockPath } = f.freeze({ executor: "provider" });
  const ctx = loadLock(lockPath);
  const report = exampleReport(ctx, { executor: "provider", model: ctx.lock.model, protocol: "chat-completions" });
  report.metadata.price_table_version = ctx.lock.price_table_version;
  for (const result of report.results) {
    result.reported_model = ctx.lock.model;
    result.usage_basis = "provider_reported";
    result.self_input_tokens = 1;
    result.self_cached_input_tokens = 0;
    result.self_output_tokens = 1;
    result.children_input_tokens = result.children_cached_input_tokens = result.children_output_tokens = 0;
    result.estimated_cost_nano_usd = 200;
    result.self_tokens = 2;
    result.total_tokens = 2 + result.children_tokens;
    result.request_attempts_used = 1;
    result.request_usage_complete = true;
    result.request_usage = [{
      turn: 0,
      request_identity_sha256: require("node:crypto").createHash("sha256")
        .update(`${result.task_id}/${result.strategy}/${result.repetition}`).digest("hex"),
      reported_model: ctx.lock.model, usage_basis: "provider_reported",
      input_tokens: 1, cached_input_tokens: 0, cache_creation_input_tokens: 0,
      output_tokens: 1, estimated_cost_nano_usd: 200
    }];
  }
  refuses(() => evaluate({ lock: lockPath, report: writeReport(f, report), out: path.join(f.root,"analysis") }),
    "request 0 price estimate does not match frozen rate table");
}));

test("tampering with reported cap_ok is rejected even when token numbers look plausible", () => scoped(f => {
  refuses(() => evalFixture(f, r => { r.results[0].cap_ok = false; return r; }), "cap_ok disagrees");
}));

test("preflight rejects unsafe Windows device names", () => scoped(f => {
  const suite = f.get("suite"); suite[0].files = { "src/CON.txt": "x" }; f.put("suite", suite);
  refuses(() => f.freeze(), "unsafe initial file");
}));


test("provider freeze refuses demonstration prices and model identities", () => scoped(f => {
  refuses(() => f.freeze({ executor: "provider" }), "synthetic demonstration fixtures");
}));
