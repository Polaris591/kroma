#!/usr/bin/env node
"use strict";

/** BF10-12: offline-only integration and adversarial contract tests. No real provider calls. */
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const crypto = require("node:crypto");
const {
  freeze, loadLock, evaluate, nativePlan, runLocked, canonicalJson,
  independentlyPricedCost, BenchFoundationError
} = require("./bench-foundation.js");

const fixtures = path.join(__dirname, "..", "experiments", "foundation-v1");
function fixture(provider = false, scorer = false) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-bf12-"));
  for (const name of ["suite", "manifest", "prices"]) {
    fs.copyFileSync(path.join(fixtures, `${name}.json`), path.join(root, `${name}.json`));
  }
  if (provider) {
    const m = JSON.parse(fs.readFileSync(path.join(root, "manifest.json")));
    const p = JSON.parse(fs.readFileSync(path.join(root, "prices.json")));
    m.id = "PHASE2-PROVIDER-FIXTURE-NO-API";
    m.sandbox = "externally-verified-sandbox-test-only";
    for (const arm of m.arms) {
      arm.model = "fixture-model";
      arm.price_table = "test-20261018";
    }
    p.models["fixture-model"] = p.models["demo-only-model"];
    p.models["fixture-model-b"] = {
      uncached_input_nano_usd: 200,
      cached_input_nano_usd: 20,
      cache_creation_input_nano_usd: 250,
      output_nano_usd: 800
    };
    delete p.models["demo-only-model"];
    p.version = "test-20261018";
    p.source = "test fixture, not a commercial price sheet";
    fs.writeFileSync(path.join(root, "manifest.json"), JSON.stringify(m));
    fs.writeFileSync(path.join(root, "prices.json"), JSON.stringify(p));
  }
  let privateKey;
  let publicKeyFile;
  if (scorer) {
    const kp = crypto.generateKeyPairSync("ed25519");
    privateKey = kp.privateKey;
    publicKeyFile = path.join(root, "external-scorer.pub.pem");
    fs.writeFileSync(publicKeyFile, kp.publicKey.export({ type: "spki", format: "pem" }));
  }
  const opts = {
    manifest: path.join(root, "manifest.json"),
    suite: path.join(root, "suite.json"),
    prices: path.join(root, "prices.json"),
    out: path.join(root, "frozen"),
    executor: provider ? "provider" : "synthetic",
    "budget-tokens": "10000", seed: "12345",
    ...(scorer ? {"scorer-key": publicKeyFile } : {})
  };
  const { lockPath } = freeze(opts);
  const ctx = loadLock(lockPath);
  return {
    root, ctx, lockPath, privateKey, publicKeyFile,
    cleanup() { fs.rmSync(root, { recursive: true, force: true }); }
  };
}
function scoped(provider, scorer, fn) {
  const f = fixture(provider, scorer);
  try { return fn(f); }
  finally { f.cleanup(); }
}
function refuses(f, needle) {
  assert.throws(f, err => err instanceof BenchFoundationError && err.message.includes(needle),
    `expected BenchFoundationError containing '${needle}'`);
}
function nativeReport(f) {
  const lock = f.ctx.lock;
  const results = [];
  for (const task of f.ctx.tasks) for (const arm of lock.arms) for (let repetition = 0; repetition < lock.repeats; repetition++) {
    const child = arm === "single_agent" ? 0 : 750;
    const resolved = arm !== "single_agent" || task.id.startsWith("fixable");
    const id = crypto.createHash("sha256").update(`${task.id}/${arm}/${repetition}`).digest("hex");
    const request = {
      turn: 0, request_identity_sha256: id, reported_model: lock.model,
      usage_basis: "provider_reported", input_tokens: 1, cached_input_tokens: 0,
      cache_creation_input_tokens: 0, output_tokens: 1, estimated_cost_nano_usd: 500
    };
    results.push({
      task_id: task.id, strategy: arm, repetition, resolved,
      gate_status: resolved ? "approved" : "rejected",
      self_tokens: 2, children_tokens: child, total_tokens: 2 + child,
      self_input_tokens: 1, self_cached_input_tokens: 0, self_output_tokens: 1,
      children_input_tokens: child, children_cached_input_tokens: 0, children_output_tokens: 0,
      budget_tokens: 10000, budget_overage: 0, budget_exceeded: false,
      cap_ok: true, finished_normally: true, wall_time_ms: 10, verify_time_ms: 0,
      regressions: Number(task.regression_guard && !resolved), usage_basis: "provider_reported",
      reported_model: f.ctx.lock.model, estimated_cost_nano_usd: child ? null : 500,
      request_usage: [request], request_attempts_used: 1, request_usage_complete: true,
      final_text: "PRIVATE DATA MUST NOT ENTER THE REDACTED OUTPUT",
      tool_trace: ["secret tool trace"]
    });
  }
  return {
    executor: lock.executor, suite: "unit-test", budget_tokens: 10000,
    metadata: { seed: lock.seed, repetitions: lock.repeats, randomized_order: true,
      suite_sha256: lock.suite_fingerprint_rust, model: lock.executor === "provider" ? lock.model : null,
      protocol: lock.executor === "provider" ? "chat-completions" : null,
      price_table_version: lock.executor === "provider" ? lock.price_table_version : null,
      source_fingerprint: {source_sha256: "a".repeat(64), source_identity_incomplete:false}
    },
    results
  };
}
function reportFile(f, report) {
  const reportFile = path.join(f.root, "native-report.json");
  fs.writeFileSync(reportFile, JSON.stringify(report), { mode: 0o600 });
  return reportFile;
}
function attestationFile(f, reportFile, mutate = p => p, signer = f.privateKey) {
  const report = JSON.parse(fs.readFileSync(reportFile));
  const digest = file => crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
  const payload = mutate({
    experiment_id: f.ctx.lock.experiment_id,
    lock_sha256: digest(f.lockPath),
    report_sha256: digest(reportFile),
    suite_sha256: f.ctx.lock.inputs.suite.sha256,
    oracle: f.ctx.lock.oracle,
    source_sha256: report.metadata.source_fingerprint.source_sha256,
    scorer_binary_sha256: "b".repeat(64),
    isolation_proof_sha256: "c".repeat(64),
    sandbox_passed: true, all_jobs_isolated: true, scorer_unwritable_by_agent: true
  });
  const signature_b64 = crypto.sign(null, Buffer.from(canonicalJson(payload)), signer).toString("base64");
  const file = path.join(f.root, "scorer-attestation.json");
  fs.writeFileSync(file, JSON.stringify({ schema:"kroma.scorer-attestation/1",payload,signature_b64 }));
  return file;
}
function signingFlags(f, report, attestation) {
  return { lock: f.lockPath, report, attestation, "scorer-key": f.publicKeyFile,
    "expect-lock-sha256": crypto.createHash("sha256").update(fs.readFileSync(f.lockPath)).digest("hex"),
    out: path.join(f.root, "evidence") };
}
function mockExecutable(f, report, status = 0) {
  const file = path.join(f.root, "fake-kroma.js");
  fs.writeFileSync(file, `#!/usr/bin/env node\n'use strict';\n` +
    `if(process.argv[2]!=='bench'||!process.argv.includes('--json')||process.argv.includes('--experiment'))process.exit(7);\n` +
    `process.stdout.write(${JSON.stringify(JSON.stringify(report))});process.exit(${status});\n`);
  fs.chmodSync(file, 0o700);
  return file;
}

test("BF10: frozen manifest produces executable native argv, never --experiment dry-run", () => scoped(false,false,f => {
  const plan = nativePlan(f.ctx);
  assert.equal(plan.job_count, 24);
  assert.equal(plan.planned_max_tokens, 240000);
  assert.deepEqual(plan.native_argv.slice(0,3), ["bench","--suite",path.join(f.ctx.root,"inputs","suite.json")]);
  assert.equal(plan.native_argv.includes("--experiment"),false);
  assert.equal(plan.runnable_in_this_driver,true);
}));

test("BF10: one frozen synthetic command executes and leaves redacted receipt, never raw report", () => scoped(false,false,f => {
  const native = nativeReport(f);
  // Synthetic fixtures don't provide priced provider claims.
  for (const r of native.results) {
    r.usage_basis = "unknown"; r.estimated_cost_nano_usd = null;
    r.request_usage = []; r.request_attempts_used = 0; r.request_usage_complete = false;
    r.reported_model = null;
  }
  const bin = mockExecutable(f, native);
  const flags = {lock:f.lockPath,kroma:bin,out:path.join(f.root,"execution")};
  const { summary } = runLocked(flags);
  assert.equal(summary.promotion,"not_applicable_synthetic");
  const receipt = JSON.parse(fs.readFileSync(path.join(flags.out,"execution-receipt.json")));
  assert.equal(receipt.executed_job_count,24);
  assert.equal(receipt.raw_report_persisted,false);
  assert.ok(receipt.lock_sha256.length===64 && receipt.executable_sha256.length===64);
  assert.equal(fs.readdirSync(flags.out).includes("rust-report.json"),false);
  assert.doesNotMatch(fs.readFileSync(path.join(flags.out,"results.redacted.json"),"utf8"), /PRIVATE DATA/);
}));

test("BF10: wrong external lock digest prevents even starting the executable", () => scoped(false,false,f => {
  const bin = mockExecutable(f, nativeReport(f));
  const output = path.join(f.root,"execution");
  refuses(() => runLocked({lock:f.lockPath,kroma:bin,out:output,"expect-lock-sha256":"0".repeat(64)}),"external lock digest");
  assert.equal(fs.existsSync(output),false);
}));

test("BF10: the provider native plan pins model and prices; automatic host dispatch is forbidden", () => scoped(true,false,f => {
  const plan=nativePlan(f.ctx);
  assert.equal(plan.runnable_in_this_driver,false);
  assert.equal(plan.native_argv[plan.native_argv.indexOf("--bench-model")+1],"fixture-model");
  assert.equal(plan.native_argv[plan.native_argv.indexOf("--price-table")+1],path.join(f.ctx.root,"inputs","prices.json"));
  const bin=mockExecutable(f,nativeReport(f));
  const out=path.join(f.root,"provider-never-executed");
  refuses(() => runLocked({lock:f.lockPath,kroma:bin,out}),"provider auto-run is blocked");
  assert.equal(fs.existsSync(out),false);
}));

test("BF11: per-request cache write is priced independently using frozen rates",()=>scoped(true,false,f=>{
  const base = nativeReport(f).results[0];
  base.children_tokens=base.children_input_tokens=0;
  base.self_tokens=1100; base.total_tokens=1100;
  base.self_input_tokens=1000; base.self_cached_input_tokens=600; base.self_output_tokens=100;
  Object.assign(base.request_usage[0], {input_tokens:1000,cached_input_tokens:600,cache_creation_input_tokens:300,
    output_tokens:100,estimated_cost_nano_usd:125000});
  base.estimated_cost_nano_usd=125000;
  assert.equal(independentlyPricedCost(base,{price_table_version:f.ctx.lock.price_table_version},f.ctx),125000);
  base.request_usage[0].estimated_cost_nano_usd=115000;
  refuses(()=>independentlyPricedCost(base,{price_table_version:f.ctx.lock.price_table_version},f.ctx),"request 0 price estimate");
}));

test("BF11: mixed-model parent requests are priced at their own reported model",()=>scoped(true,false,f=>{
  const base=nativeReport(f).results[0];
  const first=base.request_usage[0];
  first.input_tokens=500;first.output_tokens=50;first.estimated_cost_nano_usd=70000; // 500*100 + 50*400
  const second={...first, turn:1, request_identity_sha256:"f".repeat(64), reported_model:"fixture-model-b",
    input_tokens:500, output_tokens:50, estimated_cost_nano_usd:140000};
  base.request_usage=[first,second]; base.request_attempts_used=2;
  base.self_input_tokens=1000;base.self_output_tokens=100;base.self_tokens=1100;base.total_tokens=1100;
  base.estimated_cost_nano_usd=210000;
  assert.equal(independentlyPricedCost(base,{price_table_version:f.ctx.lock.price_table_version},f.ctx),210000);
}));

test("BF11: cache split omission, unknown retry or children cannot produce false cheap price",()=>scoped(true,false,f=>{
  const base=nativeReport(f).results[0];
  const metadata={price_table_version:f.ctx.lock.price_table_version};
  base.request_usage[0].cache_creation_input_tokens=null;
  assert.equal(independentlyPricedCost(base,metadata,f.ctx),null);
  base.request_usage[0].cache_creation_input_tokens=0;
  base.request_attempts_used=2;
  assert.equal(independentlyPricedCost(base,metadata,f.ctx),null);
  base.request_attempts_used=1;
  base.children_tokens=1;
  assert.equal(independentlyPricedCost(base,metadata,f.ctx),null);
}));

test("BF12: frozen independent public key authenticates exact report+suite+lock evidence",()=>scoped(true,true,f=>{
  const file=reportFile(f,nativeReport(f));
  const proof=attestationFile(f,file);
  const {summary}=evaluate(signingFlags(f,file,proof));
  assert.equal(summary.scorer_attestation.status,"verified_by_pre_registered_scorer_key");
  assert.equal(summary.evidence_kind,"provider_scored_with_frozen_external_attestation");
  assert.notEqual(summary.promotion,"promote");
}));

test("BF12: changes to a model's raw output invalidate its external score signature",()=>scoped(true,true,f=>{
  const native=nativeReport(f);
  const report=reportFile(f,native);
  const proof=attestationFile(f,report);
  native.results[0].final_text="the report was changed after scoring";
  fs.writeFileSync(report,JSON.stringify(native));
  refuses(()=>evaluate(signingFlags(f,report,proof)),"not bound to this exact run");
}));

test("BF12: invalid sandbox statement rejected even if it is correctly signed",()=>scoped(true,true,f=>{
  const report=reportFile(f,nativeReport(f));
  const proof=attestationFile(f,report,p=>({...p,sandbox_passed:false}));
  refuses(()=>evaluate(signingFlags(f,report,proof)),"did not attest to passing isolation");
}));

test("BF12: wrong scorer signature rejected; cannot bypass with attacker key",()=>scoped(true,true,f=>{
  const report=reportFile(f,nativeReport(f));
  const foreign=crypto.generateKeyPairSync("ed25519").privateKey;
  const proof=attestationFile(f,report,p=>p,foreign);
  refuses(()=>evaluate(signingFlags(f,report,proof)),"scorer signature invalid");
}));

test("BF12: requires external lock digest independent of the report",()=>scoped(true,true,f=>{
  const report=reportFile(f,nativeReport(f));
  const proof=attestationFile(f,report);
  refuses(()=>evaluate({...signingFlags(f,report,proof),"expect-lock-sha256":"0".repeat(64)}),"externally pinned lock SHA");
}));

// BF13: deterministic multi-model, multi-candidate ledger fixtures. These do NOT run providers.
function completeTournamentFixture(f) {
  const result = nativeReport(f).results.find(r => r.strategy === "adaptive_tournament_3");
  const hashes = ["b".repeat(64), "c".repeat(64)];
  const child = (subagent_id, model, input, output, hash, estimate) => ({
    subagent_id, request_attempts_used: 1, request_usage_complete: true,
    input_tokens: input, cached_input_tokens: 0, output_tokens: output,
    requests: [{
      turn: 0, request_identity_sha256: hash, reported_model: model,
      usage_basis: "provider_reported", input_tokens: input,
      cached_input_tokens: 0, cache_creation_input_tokens: 0,
      output_tokens: output, estimated_cost_nano_usd: estimate
    }]
  });
  result.child_request_usage = [
    child("tournament-c1", "fixture-model", 2, 1, hashes[0], 600),
    child("tournament-c2", "fixture-model-b", 4, 2, hashes[1], 2400)
  ];
  result.child_usage_complete = true;
  result.children_input_tokens = 6;
  result.children_cached_input_tokens = 0;
  result.children_output_tokens = 3;
  result.children_tokens = 9;
  result.total_tokens = 11;
  result.estimated_cost_nano_usd = 3500; // parent 500 + both child/candidate costs
  return result;
}

test("BF13: losing Tournament candidates are all charged using actual reported model rates", () => scoped(true,false,f => {
  const result = completeTournamentFixture(f);
  assert.equal(independentlyPricedCost(result, {price_table_version:f.ctx.lock.price_table_version}, f.ctx), 3500);
  // Identical wire digests across different child agents are scoped separately, not deduplicated.
  result.child_request_usage[1].requests[0].request_identity_sha256 = "b".repeat(64);
  assert.equal(independentlyPricedCost(result, {price_table_version:f.ctx.lock.price_table_version}, f.ctx), 3500);
}));

test("BF13: retransmitted child attempt without a completed usage event remains unknown", () => scoped(true,false,f => {
  const result = completeTournamentFixture(f);
  result.child_request_usage[0].request_attempts_used = 2;
  assert.equal(independentlyPricedCost(result, {price_table_version:f.ctx.lock.price_table_version}, f.ctx), null);
  result.child_request_usage[0].request_attempts_used = 1;
  result.child_request_usage[0].request_usage_complete = false;
  assert.equal(independentlyPricedCost(result, {price_table_version:f.ctx.lock.price_table_version}, f.ctx), null);
}));

test("BF13: interrupted child with zero reported tokens cannot look free", () => scoped(true,false,f => {
  const result = completeTournamentFixture(f);
  result.child_request_usage = [];
  result.children_input_tokens=0; result.children_output_tokens=0; result.children_tokens=0;
  result.child_usage_complete = false;
  assert.equal(independentlyPricedCost(result, {price_table_version:f.ctx.lock.price_table_version}, f.ctx), null);
  // A historical Tournament JSON with no child evidence is not upgraded by inference.
  delete result.child_usage_complete;
  assert.equal(independentlyPricedCost(result, {price_table_version:f.ctx.lock.price_table_version}, f.ctx), null);
}));

test("BF13: unknown child model, missing cache write, or unlisted rates prevent pricing", () => scoped(true,false,f => {
  const result = completeTournamentFixture(f);
  const metadata = {price_table_version:f.ctx.lock.price_table_version};
  result.child_request_usage[1].requests[0].reported_model = "not-in-frozen-rates";
  assert.equal(independentlyPricedCost(result,metadata,f.ctx),null);
  result.child_request_usage[1].requests[0].reported_model = "fixture-model-b";
  result.child_request_usage[0].requests[0].cache_creation_input_tokens=null;
  assert.equal(independentlyPricedCost(result,metadata,f.ctx),null);
}));

test("BF13: child summary tampering and duplicated child envelopes cannot inflate a price", () => scoped(true,false,f => {
  const result = completeTournamentFixture(f);
  const metadata = {price_table_version:f.ctx.lock.price_table_version};
  result.child_request_usage[0].input_tokens++;
  refuses(()=>independentlyPricedCost(result,metadata,f.ctx),"does not reconcile to its summary");
  result.child_request_usage[0].input_tokens--;
  result.children_input_tokens++;
  refuses(()=>independentlyPricedCost(result,metadata,f.ctx),"does not reconcile to the run tree");
  result.children_input_tokens--;
  result.child_request_usage[1].subagent_id="tournament-c1";
  assert.equal(independentlyPricedCost(result,metadata,f.ctx),null);
}));

test("BF13: changed child rate or claim is rejected against the locked operator table", () => scoped(true,false,f => {
  const result = completeTournamentFixture(f);
  const metadata = {price_table_version:f.ctx.lock.price_table_version};
  result.child_request_usage[1].requests[0].estimated_cost_nano_usd=1400;
  refuses(()=>independentlyPricedCost(result,metadata,f.ctx),"price estimate does not match frozen rate table");
  result.child_request_usage[1].requests[0].estimated_cost_nano_usd=2400;
  result.estimated_cost_nano_usd=2500;
  refuses(()=>independentlyPricedCost(result,metadata,f.ctx),"price estimate does not match frozen operator rate table");
}));
