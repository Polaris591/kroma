#!/usr/bin/env node
"use strict";

/** End-to-end local release protocol tests with ephemeral signing keys (never production key). */
const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const { stage, expectedCandidate, PLATFORMS } = require("./release-candidate.js");
const { make } = require("./release-sandbox-evidence.js");
const { build, verify, verdict, readAttestation } = require("./release-attestation.js");
const { signCandidate } = require("./release-provenance.js");
const { validate, REQUIRED_JOBS } = require("./verify-release-run.js");

const SHA = "a".repeat(40), RUN = "88888222", REPO = "test-owner/kroma", ATTEMPT = 1, VERSION = "1.5.0";
const workspaces = [];
let checks = 0;
function check(title, task) {
  try { task(); checks++; console.log(`ok ${checks} - ${title}`); }
  catch (err) { console.error(`FAIL ${title}: ${err.stack}`); process.exitCode = 1; }
}
function fixture({ status = "PASS", include = true } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-release-test-")); workspaces.push(root);
  const downloads = path.join(root, "downloads"); fs.mkdirSync(downloads);
  for (const p of PLATFORMS) {
    const dir = path.join(downloads, p.action); fs.mkdirSync(dir);
    fs.writeFileSync(path.join(dir, p.input), `${p.action}-example-executable\n`);
  }
  const npm = path.join(downloads, "kroma-cli-tarball"); fs.mkdirSync(npm);
  fs.writeFileSync(path.join(npm, `kroma-cli-${VERSION}.tgz`), "fake npm tarball");
  const sandbox = path.join(downloads, "sandbox-evidence"); fs.mkdirSync(sandbox);
  const exitCode = status === "PASS" ? 0 : status === "HOST_CAPABILITY_BLOCKED" ? 2 : 1;
  fs.writeFileSync(path.join(sandbox, "sandbox-evidence.json"), JSON.stringify(make({
    commit: SHA, repository: REPO, runId: RUN, runAttempt: ATTEMPT, status, exitCode
  }), null, 2) + "\n");
  const bundle = path.join(root, "bundle"); fs.mkdirSync(bundle);
  if (include) {
    stage(downloads, path.join(bundle, "candidate"), VERSION);
    build({ root: path.join(bundle, "candidate"), out: path.join(bundle, "release-attestation.json"),
      commit: SHA, repo: REPO, runId: RUN, runAttempt: ATTEMPT, version: VERSION, sandbox: status });
  }
  return {root, downloads, bundle, candidate: path.join(bundle, "candidate"), attestation: path.join(bundle, "release-attestation.json")};
}
function signedFixture(status = "PASS") {
  const f = fixture({status});
  const { privateKey, publicKey } = crypto.generateKeyPairSync("ed25519");
  const raw = publicKey.export({ type: "spki", format: "der" }).subarray(-32).toString("hex");
  const doc = path.join(f.root, "owner-signed-provenance.json");
  if (status === "PASS") signCandidate({candidateRoot:f.bundle,version:VERSION,keyId:"test-key",privateKey,out:doc});
  const out = path.join(f.root, "verdict.json");
  const run = () => verdict({document:f.attestation,provenance:doc,root:f.bundle,
    trustedKeys:{"test-key":raw},expectedRepo:REPO,expectedRunId:RUN,
    expectedRunAttempt:ATTEMPT,expectedCommit:SHA,out});
  return {...f, doc, out, run, privateKey, trustedKeys:{"test-key":raw}};
}
function refused(fn, fragment) {
  assert.throws(fn, (err) => !fragment || String(err.message).includes(fragment), `expected rejection containing ${fragment}`);
}

check("candidate stages all six unique binary names, npm tarball and sandbox evidence", () => {
  const f=fixture(); assert.deepEqual(fs.readdirSync(f.candidate).sort(),expectedCandidate(VERSION));
  assert.deepEqual(readAttestation(f.attestation).artifacts.map(x=>x.name), expectedCandidate(VERSION));
});
check("candidate download missing one platform fails closed",()=> {
  const f=fixture({include:false});fs.rmSync(path.join(f.downloads,"darwin-x64"),{recursive:true});
  refused(()=>stage(f.downloads,path.join(f.bundle,"candidate"),VERSION),"inventory mismatch");
});
check("candidate rejects two platform artifacts with unexpected collision names",()=> {
  const f=fixture({include:false});
  fs.writeFileSync(path.join(f.downloads,"linux-arm64","kroma-linux-x64"),"collision");
  refused(()=>stage(f.downloads,path.join(f.bundle,"candidate"),VERSION),"inventory mismatch");
});
check("candidate rejects symlink binary",()=> {
  const f=fixture({include:false});
  fs.rmSync(path.join(f.downloads,"linux-x64","kroma"));
  fs.symlinkSync("/etc/passwd",path.join(f.downloads,"linux-x64","kroma"));
  refused(()=>stage(f.downloads,path.join(f.bundle,"candidate"),VERSION),"not a nonempty regular file");
});
check("attestation verifies the actual candidate directory",()=> {
  const f=fixture();assert.equal(verify({document:f.attestation,root:f.candidate}).git_commit,SHA);
});
check("attestation rejects byte changes",()=> {
  const f=fixture();fs.writeFileSync(path.join(f.candidate,"kroma-linux-x64"),"LINUX-x64-example-executable\n");
  refused(()=>verify({document:f.attestation,root:f.candidate}),"bytes changed");
});
check("attestation rejects extra artifacts",()=> {
  const f=fixture();fs.writeFileSync(path.join(f.candidate,"unexpected"),"x");
  refused(()=>verify({document:f.attestation,root:f.candidate}),"inventory mismatch");
});
check("attestation rejects missing artifacts",()=> {
  const f=fixture();fs.rmSync(path.join(f.candidate,"kroma-linux-x64"));
  refused(()=>verify({document:f.attestation,root:f.candidate}),"inventory mismatch");
});
check("sandbox FAIL or HOST_CAPABILITY_BLOCKED cannot be signed for release",()=> {
  for (const status of ["FAIL","HOST_CAPABILITY_BLOCKED"]) {
    const f=fixture({status});
    const {privateKey}=crypto.generateKeyPairSync("ed25519");
    refused(()=>signCandidate({candidateRoot:f.bundle,version:VERSION,keyId:"test",privateKey,
      out:path.join(f.root,"signed.json")}),"cannot sign sandbox status");
  }
});
check("offline owner signing succeeds, exact bundle verifies with ephemeral trusted key",()=> {
  const f=signedFixture();assert.equal(f.run(),0);assert.equal(JSON.parse(fs.readFileSync(f.out)).state,"ARTIFACT_SET_VERIFIED");
});
check("P0 regression: editing only unsigned sandbox status does not pass verdict",()=> {
  const f=signedFixture();const a=JSON.parse(fs.readFileSync(f.attestation));a.sandbox_status="HOST_CAPABILITY_BLOCKED";
  fs.writeFileSync(f.attestation,JSON.stringify(a,null,2)+"\n");
  refused(()=>f.run(),"sandbox status does not match");
});
check("P0 original direction: a blocked signed candidate cannot be made PASS by editing unsigned JSON",()=> {
  const f=fixture({status:"HOST_CAPABILITY_BLOCKED"});
  const {privateKey,publicKey}=crypto.generateKeyPairSync("ed25519");
  const {buildDocument,signDocument}=require("./provenance.js");
  const a=readAttestation(f.attestation);
  const items=[...a.artifacts.map(x=>({name:`candidate/${x.name}`,file:path.join(f.candidate,x.name)})),
    {name:"release-attestation.json",file:f.attestation}];
  const unsigned=buildDocument({version:VERSION,gitCommit:SHA,artifacts:items,
    quality:{source_repository:REPO,source_run_id:RUN,source_run_attempt:ATTEMPT,sandbox_status:"HOST_CAPABILITY_BLOCKED"}});
  const signed=signDocument(unsigned,"test-key",privateKey);
  const doc=path.join(f.root,"manual-signed.json"),out=path.join(f.root,"verdict.json");
  fs.writeFileSync(doc,JSON.stringify(signed,null,2)+"\n");
  const trusted={"test-key":publicKey.export({format:"der",type:"spki"}).subarray(-32).toString("hex")};
  const args={document:f.attestation,provenance:doc,root:f.bundle,trustedKeys:trusted,
    expectedRepo:REPO,expectedRunId:RUN,expectedRunAttempt:ATTEMPT,expectedCommit:SHA,out};
  assert.equal(verdict(args),1);
  a.sandbox_status="PASS";fs.writeFileSync(f.attestation,JSON.stringify(a,null,2)+"\n");
  refused(()=>verdict(args),"sandbox status does not match");
});
check("P0 regression: editing BOTH attestation and evidence to PASS invalidates signature",()=> {
  const f=signedFixture();
  const a=JSON.parse(fs.readFileSync(f.attestation));a.sandbox_status="HOST_CAPABILITY_BLOCKED";
  const e=JSON.parse(fs.readFileSync(path.join(f.candidate,"sandbox-evidence.json")));
  e.status="HOST_CAPABILITY_BLOCKED";e.exit_code=2;
  fs.writeFileSync(path.join(f.candidate,"sandbox-evidence.json"),JSON.stringify(e,null,2)+"\n");
  // To pass the preliminary evidence check, edit the attestation's evidence digest too.
  const buf=fs.readFileSync(path.join(f.candidate,"sandbox-evidence.json"));
  const rec=a.artifacts.find(x=>x.name==="sandbox-evidence.json");
  rec.size=buf.length;rec.sha256=crypto.createHash("sha256").update(buf).digest("hex");
  fs.writeFileSync(f.attestation,JSON.stringify(a,null,2)+"\n");
  assert.equal(f.run(),1);
  assert.match(fs.readFileSync(f.out,"utf8"),/invalid signature or artifact/);
});
check("a correctly re-signed but inconsistent version or quality claim is refused",()=> {
  const {signDocument}=require("./provenance.js");
  for (const kind of ["version","quality"]) {
    const f=signedFixture();const doc=JSON.parse(fs.readFileSync(f.doc));
    if(kind==="version") doc.version="9.9.9";
    else doc.quality.sandbox_status="HOST_CAPABILITY_BLOCKED";
    const signed=signDocument({...doc,signature:null},"test-key",f.privateKey);
    fs.writeFileSync(f.doc,JSON.stringify(signed,null,2)+"\n");
    assert.equal(f.run(),1);
  }
});
check("tampered signed provenance is rejected",()=> {
  const f=signedFixture();const doc=JSON.parse(fs.readFileSync(f.doc));
  doc.quality.sandbox_status="HOST_CAPABILITY_BLOCKED";
  fs.writeFileSync(f.doc,JSON.stringify(doc,null,2)+"\n");
  assert.equal(f.run(),1);
});
check("signed candidate rejects changed binary or tarball",()=> {
  const f=signedFixture();fs.writeFileSync(path.join(f.candidate,`kroma-cli-${VERSION}.tgz`),"fake nAm tarball");
  refused(()=>f.run(),"bytes changed");
});
check("wrong GitHub repo, run, attempt, or source commit blocks verdict",()=> {
  for (const key of ["expectedRepo","expectedRunId","expectedRunAttempt","expectedCommit"]) {
    const f=signedFixture();const options={document:f.attestation,provenance:f.doc,root:f.bundle,
      trustedKeys:f.trustedKeys,expectedRepo:REPO,expectedRunId:RUN,expectedRunAttempt:ATTEMPT,expectedCommit:SHA,out:f.out};
    options[key] = {expectedRepo:"wrong/kroma",expectedRunId:"1234",expectedRunAttempt:3,expectedCommit:"b".repeat(40)}[key];
    assert.equal(verdict(options),1,key);
  }
});
check("verdict refuses provenance that does not sign the attestation JSON",()=> {
  const f=signedFixture();
  const original=JSON.parse(fs.readFileSync(f.doc));
  original.artifacts=original.artifacts.filter(x=>x.name!=="release-attestation.json");
  const {signDocument}=require("./provenance.js");
  const altered=signDocument({...original,signature:null},"test-key",f.privateKey);
  fs.writeFileSync(f.doc,JSON.stringify(altered,null,2)+"\n");
  assert.equal(f.run(),1);
});
check("verdict fails if bundle layout is flattened or extra file appears",()=> {
  for (const type of ["flatten","extra"]) {
    const f=signedFixture();
    if(type==="flatten")fs.renameSync(path.join(f.candidate,"kroma-linux-x64"),path.join(f.bundle,"kroma-linux-x64"));
    else fs.writeFileSync(path.join(f.bundle,"evil.txt"),"x");
    if(type==="flatten")refused(()=>f.run(),"inventory mismatch");
    else assert.equal(f.run(),1);
  }
});
check("owner's sign-candidate CLI works with key file only; offline public JSON is portable",()=> {
  const f=fixture();const {privateKey,publicKey}=crypto.generateKeyPairSync("ed25519");
  const key=path.join(f.root,"owner-private.pem"),signed=path.join(f.root,"signed.json");
  fs.writeFileSync(key,privateKey.export({format:"pem",type:"pkcs8"}),{mode:0o600});
  const cmd=spawnSync(process.execPath,[path.join(__dirname,"release-provenance.js"),"sign-candidate",
    "--candidate-root",f.bundle,"--version",VERSION,"--key-id","ephemeral","--private-key-file",key,"--out",signed],{encoding:"utf8"});
  assert.equal(cmd.status,0,cmd.stderr);
  const decoded=path.join(f.root,"transported.json");
  fs.writeFileSync(decoded,Buffer.from(fs.readFileSync(signed).toString("base64"),"base64"));
  const trusted={ephemeral:publicKey.export({format:"der",type:"spki"}).subarray(-32).toString("hex")};
  assert.equal(verdict({document:f.attestation,provenance:decoded,root:f.bundle,trustedKeys:trusted,
    expectedRepo:REPO,expectedRunId:RUN,expectedRunAttempt:ATTEMPT,expectedCommit:SHA,
    out:path.join(f.root,"verdict.json")}),0);
});
check("attestation rejects unknown source commit, schema and noncanonical JSON",()=> {
  const f=fixture(); const a=JSON.parse(fs.readFileSync(f.attestation));
  a.git_commit="bad";fs.writeFileSync(f.attestation,JSON.stringify(a,null,2)+"\n");
  refused(()=>readAttestation(f.attestation),"git_commit");
  a.git_commit=SHA;a.schema="kroma-release-attestation/1";
  fs.writeFileSync(f.attestation,JSON.stringify(a,null,2)+"\n");
  refused(()=>readAttestation(f.attestation),"schema");
  a.schema="kroma-release-attestation/2";
  fs.writeFileSync(f.attestation,JSON.stringify(a));
  refused(()=>readAttestation(f.attestation),"canonical");
});
check("GitHub source-run metadata requires successful expected jobs and one candidate artifact",()=> {
  const jobs=REQUIRED_JOBS.map((name,i)=>({name, conclusion:"success",id:i+1,run_attempt:ATTEMPT}));
  const data={id:Number(RUN),path:".github/workflows/release.yml",status:"completed",conclusion:"success",
    head_sha:SHA,run_attempt:ATTEMPT,event:"workflow_dispatch",repository:{full_name:REPO},head_repository:{full_name:REPO}};
  const response={total_count:jobs.length,jobs};
  const artifacts={total_count:1,artifacts:[{name:"release-candidate",expired:false,size_in_bytes:1000}]};
  assert.equal(validate(data,response,artifacts,{runId:RUN,repository:REPO}).commit,SHA);
  refused(()=>validate({...data,head_sha:"short"},response,artifacts,{runId:RUN,repository:REPO}),"invalid candidate run commit");
});
check("source-run validation rejects wrong repository, workflow, failed sandbox and missing candidate",()=> {
  const jobs=REQUIRED_JOBS.map((name,i)=>({name,conclusion:"success",id:i+1,run_attempt:ATTEMPT}));
  const data={id:Number(RUN),path:".github/workflows/release.yml",status:"completed",conclusion:"success",
    head_sha:SHA,run_attempt:ATTEMPT,event:"workflow_dispatch",repository:{full_name:REPO},head_repository:{full_name:REPO}};
  const response={total_count:jobs.length,jobs};
  const artifact={total_count:1,artifacts:[{name:"release-candidate",expired:false,size_in_bytes:1000}]};
  const opts={runId:RUN,repository:REPO};
  refused(()=>validate({...data,path:".github/workflows/evil.yml"},response,artifact,opts),"different workflow");
  refused(()=>validate({...data,repository:{full_name:"evil/repo"}},response,artifact,opts),"different repository");
  const failed={total_count:jobs.length,jobs:jobs.map(j=>j.name==="sandbox isolation (real bwrap)"?{...j,conclusion:"failure"}:j)};
  refused(()=>validate(data,failed,artifact,opts),"sandbox isolation");
  refused(()=>validate(data,response,{total_count:0,artifacts:[]},opts),"missing, duplicated");
});
check("workflow inputs are not interpolated directly in shell command text",()=> {
  const yaml=fs.readFileSync(path.join(__dirname,"..",".github/workflows/release.yml"),"utf8");
  assert.ok(!/gh run download "?\$\{\{ inputs\./.test(yaml));
  assert.ok(!/--provenance "?\$\{\{ inputs\./.test(yaml));
  assert.ok(/permissions:\s*\n\s+contents: read\s*\n\s+actions: read/.test(yaml));
  // A capability-blocked sandbox job must FAIL at GitHub's authenticated job boundary too, not
  // merely rely on a later owner-signed claim that someone could mistakenly approve offline.
  assert.ok(yaml.includes('if [ "$status" != PASS ]; then exit 1; fi'));
});
for (const workspace of workspaces) fs.rmSync(workspace,{recursive:true,force:true});
if (!process.exitCode) console.log(`all ${checks} release attestation checks passed`);
