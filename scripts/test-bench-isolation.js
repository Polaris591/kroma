#!/usr/bin/env node
"use strict";
/** BF-14A: signature-bound, complete per-job scorer+OS-isolation evidence tests.
 * All runtimes and outcomes are fictional; these tests never launch a real model or sandbox. */
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const crypto = require("node:crypto");
const {freeze, loadLock, evaluate, canonicalJson, BenchFoundationError} = require("./bench-foundation.js");
const fixtureDir = path.join(__dirname, "..", "experiments", "foundation-v1");
const sha = bytes => crypto.createHash("sha256").update(bytes).digest("hex");
const shaFile = file => sha(fs.readFileSync(file));
const hex = text => sha(Buffer.from(text));
function setup() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-bf14-a-"));
  for (const kind of ["manifest", "suite", "prices"]) {
    fs.copyFileSync(path.join(fixtureDir, `${kind}.json`), path.join(root, `${kind}.json`));
  }
  const manifest = JSON.parse(fs.readFileSync(path.join(root, "manifest.json")));
  const prices = JSON.parse(fs.readFileSync(path.join(root, "prices.json")));
  manifest.id = "BF14-FICTITIOUS-PROVIDER-TEST";
  manifest.sandbox = "test-only-external-os-isolation-declaration";
  manifest.holdout = manifest.tasks.slice(1).map(task => task.id);
  for (const arm of manifest.arms) {
    arm.model = "bf14-test-model";
    arm.price_table = "bf14-rates-test";
  }
  prices.models["bf14-test-model"] = prices.models["demo-only-model"];
  delete prices.models["demo-only-model"];
  prices.version = "bf14-rates-test";
  prices.source = "unit-test rate fixture, not a provider invoice";
  fs.writeFileSync(path.join(root,"manifest.json"),JSON.stringify(manifest));
  fs.writeFileSync(path.join(root,"prices.json"),JSON.stringify(prices));
  const pair = crypto.generateKeyPairSync("ed25519");
  const publicKeyPath = path.join(root,"scorer-public.pem");
  fs.writeFileSync(publicKeyPath, pair.publicKey.export({format:"pem",type:"spki"}));
  const lockPath = freeze({
    manifest:path.join(root,"manifest.json"),suite:path.join(root,"suite.json"),
    prices:path.join(root,"prices.json"),out:path.join(root,"frozen"),
    executor:"provider","budget-tokens":"10000",seed:"12345",
    "scorer-key":publicKeyPath
  }).lockPath;
  const ctx = loadLock(lockPath);
  const results = [];
  for (const task of ctx.tasks) for (const arm of ctx.lock.arms) for (let repetition=0;repetition<ctx.lock.repeats;repetition++) {
    const resolved = arm === "adaptive_tournament_3";
    results.push({
      task_id:task.id,strategy:arm,repetition,resolved,
      gate_status: resolved ? "approved" : "rejected",
      self_tokens:2,children_tokens:0,total_tokens:2,
      self_input_tokens:1,self_cached_input_tokens:0,self_output_tokens:1,
      children_input_tokens:0,children_cached_input_tokens:0,children_output_tokens:0,
      budget_tokens:10000,budget_overage:0,budget_exceeded:false,cap_ok:true,
      finished_normally:true,wall_time_ms:2,verify_time_ms:0,
      regressions:Number(task.regression_guard && !resolved),
      usage_basis:"provider_reported",reported_model:"bf14-test-model",
      estimated_cost_nano_usd:null,
      request_attempts_used:1,request_usage_complete:true,
      request_usage:[{turn:0,request_identity_sha256:hex(`${task.id}-${arm}-${repetition}`),
        reported_model:"bf14-test-model",usage_basis:"provider_reported",
        input_tokens:1,cached_input_tokens:0,cache_creation_input_tokens:0,output_tokens:1,
        estimated_cost_nano_usd:null}],child_usage_complete:true,child_request_usage:[]
    });
  }
  const report = {
    executor:"provider",suite:"fixture-only",budget_tokens:10000,
    metadata:{seed:ctx.lock.seed,repetitions:ctx.lock.repeats,randomized_order:true,
      suite_sha256:ctx.lock.suite_fingerprint_rust,model:ctx.lock.model,
      protocol:"chat-completions",price_table_version:ctx.lock.price_table_version,
      source_fingerprint:{source_sha256:"a".repeat(64),source_identity_incomplete:false}},
    results
  };
  const reportPath = path.join(root,"native-report.json");
  fs.writeFileSync(reportPath,JSON.stringify(report));
  return {root,ctx,lockPath,reportPath,report,pair,publicKeyPath,
    cleanup(){fs.rmSync(root,{recursive:true,force:true});}};
}
function scoped(fn) {const f=setup();try{return fn(f);}finally{f.cleanup();}}
function flags(f,attestation,proofFile,evidenceRoot) {
  return {lock:f.lockPath,report:f.reportPath,attestation,"scorer-key":f.publicKeyPath,
    "expect-lock-sha256":shaFile(f.lockPath),
    ...(proofFile?{"isolation-proof":proofFile}:{}),
    ...(evidenceRoot?{"evidence-root":evidenceRoot}:{}),
    out:path.join(f.root,"verified-output")};
}
const RAW_FILES = [
  ["sandbox_probe_sha256", "sandbox-probe.json"],
  ["agent_execution_log_sha256", "agent-execution.json"],
  ["scorer_execution_log_sha256", "scorer-execution.json"],
  ["workspace_snapshot_sha256", "workspace-snapshot.json"]
];
function artifactBytes(job, name) {
  // FICTITIOUS input: a digest-backed file fixture, not evidence of a real sandbox or scorer.
  return Buffer.from(JSON.stringify({schema:"bf14b.synthetic-artifact-fixture-only/1",
    run_id:job.run_id,task_id:job.task_id,strategy:job.strategy,
    repetition:job.repetition,artifact:name,synthetic:true})+"\n");
}
function materializeRawArtifacts(f,proof) {
  const root=path.join(f.root,"raw-artifacts");
  fs.mkdirSync(root,{mode:0o700});
  for (const job of proof.jobs) {
    const dir=path.join(root,job.run_id);
    fs.mkdirSync(dir,{mode:0o700});
    for (const [,name] of RAW_FILES) fs.writeFileSync(path.join(dir,name),artifactBytes(job,name),{mode:0o600});
  }
  return root;
}
function makeProof(f) {
  const jobs=f.report.results.map((result,index)=>{
    const job={
    task_id:result.task_id,strategy:result.strategy,repetition:result.repetition,
    run_id:`sandbox-run-${index}`,sandbox_backend:"bwrap",
    tool_execution_isolated:true,network_policy_enforced:true,
    host_secrets_inaccessible:true,workspace_writes_confined:true,
    timeout_and_cancellation_enforced:true,budget_enforced:true,
    oracle_unwritable_by_agent:true,scorer_unwritable_by_agent:true,
    oracle_resolved:result.resolved,oracle_gate_status:result.gate_status
    };
    for (const [field,name] of RAW_FILES) job[field]=sha(artifactBytes(job,name));
    return job;
  });
  return {
    schema:"kroma.benchmark.isolation-evidence/1",
    experiment_id:f.ctx.lock.experiment_id,lock_sha256:shaFile(f.lockPath),
    report_sha256:shaFile(f.reportPath),suite_sha256:f.ctx.lock.inputs.suite.sha256,
    source_sha256:f.report.metadata.source_fingerprint.source_sha256,
    oracle:f.ctx.lock.oracle,sandbox_declaration:f.ctx.manifest.sandbox,
    scorer_binary_sha256:"b".repeat(64),scorer_environment_sha256:hex("external-scorer-env"),
    sandbox_policy_sha256:hex("operator-pre-registered-policy"),jobs
  };
}
function writeSignedEvidence(f,proof, opts={}) {
  const proofFile=path.join(f.root,"external-isolation-evidence.json");
  if (proof !== null) fs.writeFileSync(proofFile,JSON.stringify(proof),{mode:0o600});
  const payload={experiment_id:f.ctx.lock.experiment_id,lock_sha256:shaFile(f.lockPath),
    report_sha256:shaFile(f.reportPath),suite_sha256:f.ctx.lock.inputs.suite.sha256,
    oracle:f.ctx.lock.oracle,source_sha256:f.report.metadata.source_fingerprint.source_sha256,
    scorer_binary_sha256:"b".repeat(64),
    isolation_proof_sha256:proof === null ? "c".repeat(64) : shaFile(proofFile),
    sandbox_passed:true,all_jobs_isolated:true,scorer_unwritable_by_agent:true};
  if (opts.payload) opts.payload(payload);
  const signature=crypto.sign(null,Buffer.from(canonicalJson(payload)),f.pair.privateKey);
  const attestationFile=path.join(f.root,"scorer-attestation.json");
  fs.writeFileSync(attestationFile,JSON.stringify({schema:"kroma.scorer-attestation/1",
    payload,signature_b64:signature.toString("base64")}),{mode:0o600});
  return {proofFile,attestationFile};
}
function rejects(f,needle){assert.throws(f,err=>err instanceof BenchFoundationError && err.message.includes(needle));}

test("BF14: legacy signed sandbox booleans alone cannot grant manual promotion review",()=>scoped(f=>{
  const bare=evaluate({lock:f.lockPath,report:f.reportPath,out:path.join(f.root,"bare")});
  assert.equal(bare.summary.promotion,"needs_isolation_attestation");
  const s=writeSignedEvidence(f,null);
  const evidence=evaluate(flags(f,s.attestationFile));
  assert.equal(evidence.summary.scorer_attestation.status,"verified_by_pre_registered_scorer_key");
  assert.equal(evidence.summary.scorer_attestation.isolation_evidence,null);
  assert.equal(evidence.summary.promotion,"needs_isolation_attestation");
}));
test("BF14B: signed job matrix with nonexistent raw files cannot enter manual review",()=>scoped(f=>{
  const s=writeSignedEvidence(f,makeProof(f));
  const result=evaluate(flags(f,s.attestationFile,s.proofFile));
  assert.equal(result.summary.promotion,"needs_isolation_attestation");
  assert.equal(result.summary.scorer_attestation.isolation_evidence.raw_artifacts_verified,false);
  assert.equal(result.summary.evidence_kind,"provider_scored_with_signed_matrix_raw_missing");
}));
test("BF14B: signed complete matrix + all actual referenced raw bytes permit ONLY manual review",()=>scoped(f=>{
  const proof=makeProof(f), s=writeSignedEvidence(f,proof);
  const root=materializeRawArtifacts(f,proof);
  const result=evaluate(flags(f,s.attestationFile,s.proofFile,root));
  assert.equal(result.summary.promotion,"eligible_for_manual_review");
  assert.equal(result.summary.scorer_attestation.isolation_evidence.job_count,24);
  assert.equal(result.summary.scorer_attestation.isolation_evidence.raw_artifact_count,96);
  assert.equal(result.summary.evidence_kind,"provider_scored_with_signed_matrix_and_raw_bytes");
  assert.equal(fs.existsSync(path.join(f.root,"verified-output","metrics.json")),true);
  assert.equal(JSON.stringify(result.summary).includes("PRIVATE DATA MUST NOT ENTER"),false);
}));
test("BF14: external evidence must be accompanied by signature and lock digest",()=>scoped(f=>{
  const proof=path.join(f.root,"external-isolation-evidence.json");
  fs.writeFileSync(proof,JSON.stringify(makeProof(f)));
  rejects(()=>evaluate({lock:f.lockPath,report:f.reportPath,out:path.join(f.root,"out"),
    "isolation-proof":proof}),"linked to a signed scorer attestation");
}));
test("BF14: changing the proof after signing fails SHA-256 integrity",()=>scoped(f=>{
  const proof=makeProof(f),s=writeSignedEvidence(f,proof);
  proof.jobs[0].oracle_resolved=!proof.jobs[0].oracle_resolved;
  fs.writeFileSync(s.proofFile,JSON.stringify(proof));
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"bytes differ");
}));
test("BF14: signing a wrong proof hash cannot substitute another proof",()=>scoped(f=>{
  const s=writeSignedEvidence(f,makeProof(f),{payload:p=>{p.isolation_proof_sha256="d".repeat(64);}});
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"bytes differ");
}));
test("BF14: absence of one sandbox job refuses even with fresh trusted signature",()=>scoped(f=>{
  const proof=makeProof(f);proof.jobs.pop();
  const s=writeSignedEvidence(f,proof);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"cover every planned");
}));
test("BF14: duplicate job identity is rejected",()=>scoped(f=>{
  const proof=makeProof(f);proof.jobs[1]={...proof.jobs[0],run_id:"unique-run-id"};
  const s=writeSignedEvidence(f,proof);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"repeated or unexpected job");
}));
test("BF14: reuse of sandbox run ID across two jobs is rejected",()=>scoped(f=>{
  const proof=makeProof(f);proof.jobs[1].run_id=proof.jobs[0].run_id;
  const s=writeSignedEvidence(f,proof);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"reused run ID");
}));
test("BF14: shell-only non-isolating host runner cannot claim a verified sandbox",()=>scoped(f=>{
  const proof=makeProof(f);proof.jobs[0].sandbox_backend="host-process";
  const s=writeSignedEvidence(f,proof);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"lacks an OS sandbox backend");
}));
test("BF14: tool isolation cannot be asserted false in a passing job",()=>scoped(f=>{
  const proof=makeProof(f);proof.jobs[0].tool_execution_isolated=false;
  const s=writeSignedEvidence(f,proof);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"did not verify tool_execution_isolated");
}));
test("BF14: a signer cannot substitute a different independent grading result",()=>scoped(f=>{
  const proof=makeProof(f);proof.jobs[0].oracle_resolved=!proof.jobs[0].oracle_resolved;
  const s=writeSignedEvidence(f,proof);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"independent scoring result disagrees");
}));
test("BF14: a scorer claiming different source is rejected",()=>scoped(f=>{
  const proof=makeProof(f);proof.source_sha256="0".repeat(64);
  const s=writeSignedEvidence(f,proof);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"does not match the frozen experiment");
}));
test("BF14: proof cannot change frozen sandbox declaration",()=>scoped(f=>{
  const proof=makeProof(f);proof.sandbox_declaration="other-sandbox";
  const s=writeSignedEvidence(f,proof);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"does not match the frozen experiment");
}));
test("BF14: must bind the scorer binary identity",()=>scoped(f=>{
  const proof=makeProof(f);proof.scorer_binary_sha256="0".repeat(64);
  const s=writeSignedEvidence(f,proof);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"does not match the frozen experiment");
}));
test("BF14: missing per-job probe digest rejected",()=>scoped(f=>{
  const proof=makeProof(f);delete proof.jobs[0].sandbox_probe_sha256;
  const s=writeSignedEvidence(f,proof);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"missing sandbox_probe_sha256");
}));
test("BF14: reject accidental prompt, output or secret in scoring certificate",()=>scoped(f=>{
  const proof=makeProof(f);proof.jobs[0].prompt="NEVER LEAK THIS";
  const s=writeSignedEvidence(f,proof);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"unsupported key prompt");
}));
test("BF14: reject symlinked isolation evidence even when hash is signed",()=>scoped(f=>{
  const s=writeSignedEvidence(f,makeProof(f));
  const symlink=path.join(f.root,"evidence-link.json");
  fs.symlinkSync(s.proofFile,symlink);
  rejects(()=>evaluate(flags(f,s.attestationFile,symlink)),"never a symlink");
}));
test("BF14: signatures cannot declare an absent scorer environment digest",()=>scoped(f=>{
  const proof=makeProof(f);delete proof.scorer_environment_sha256;
  const s=writeSignedEvidence(f,proof);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile)),"missing scorer_environment_sha256");
}));

// BF14B: possession and digest checks are deliberately separate from the Ed25519 assertion.
test("BF14B: raw evidence cannot be provided without an isolation proof",()=>scoped(f=>{
  rejects(()=>evaluate({lock:f.lockPath,report:f.reportPath,
    out:path.join(f.root,"out"),"evidence-root":f.root}),"require a signed isolation evidence matrix");
}));
test("BF14B: signed claims with missing artifact files refuse review",()=>scoped(f=>{
  const proof=makeProof(f),s=writeSignedEvidence(f,proof);
  const root=materializeRawArtifacts(f,proof);
  fs.unlinkSync(path.join(root,proof.jobs[0].run_id,"agent-execution.json"));
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile,root)),"missing or unexpected artifacts");
}));
test("BF14B: signed claims with mutated raw artifact refuse review",()=>scoped(f=>{
  const proof=makeProof(f),s=writeSignedEvidence(f,proof);
  const root=materializeRawArtifacts(f,proof);
  fs.appendFileSync(path.join(root,proof.jobs[0].run_id,"sandbox-probe.json"),"mutated");
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile,root)),"bytes differ from the signed digest");
}));
test("BF14B: signer cannot authorize an extra unreferenced artifact",()=>scoped(f=>{
  const proof=makeProof(f),s=writeSignedEvidence(f,proof);
  const root=materializeRawArtifacts(f,proof);
  fs.writeFileSync(path.join(root,proof.jobs[0].run_id,"untrusted-extra.log"),"secret");
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile,root)),"missing or unexpected artifacts");
}));
test("BF14B: reject extra run folders not bound to signed job matrix",()=>scoped(f=>{
  const proof=makeProof(f),s=writeSignedEvidence(f,proof);
  const root=materializeRawArtifacts(f,proof);
  fs.mkdirSync(path.join(root,"unscored-extra-run"));
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile,root)),"exactly the signed job directories");
}));
test("BF14B: reject symlinked raw evidence file",()=>scoped(f=>{
  const proof=makeProof(f),s=writeSignedEvidence(f,proof);
  const root=materializeRawArtifacts(f,proof);
  const original=path.join(root,proof.jobs[0].run_id,"sandbox-probe.json");
  const source=path.join(f.root,"relocated.json");
  fs.renameSync(original,source);fs.symlinkSync(source,original);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile,root)),"unsafe or oversized raw evidence file");
}));
test("BF14B: reject symlinked job directory",()=>scoped(f=>{
  const proof=makeProof(f),s=writeSignedEvidence(f,proof);
  const root=materializeRawArtifacts(f,proof);
  const old=path.join(root,proof.jobs[0].run_id);
  const moved=path.join(f.root,"moved-evidence-dir");
  fs.renameSync(old,moved);fs.symlinkSync(moved,old);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile,root)),"not a real directory");
}));
test("BF14B: reject symlinked raw evidence root",()=>scoped(f=>{
  const proof=makeProof(f),s=writeSignedEvidence(f,proof);
  const root=materializeRawArtifacts(f,proof),link=path.join(f.root,"raw-link");
  fs.symlinkSync(root,link);
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile,link)),"never a symlink");
}));
test("BF14B: reject oversized raw log before reading it",()=>scoped(f=>{
  const proof=makeProof(f),s=writeSignedEvidence(f,proof);
  const root=materializeRawArtifacts(f,proof);
  fs.writeFileSync(path.join(root,proof.jobs[0].run_id,"workspace-snapshot.json"),Buffer.alloc(1024*1024+1));
  rejects(()=>evaluate(flags(f,s.attestationFile,s.proofFile,root)),"unsafe or oversized raw evidence file");
}));
