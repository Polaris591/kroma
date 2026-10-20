#!/usr/bin/env node
"use strict";

/** BF14B: test the offline kernel sandbox SUPERVISOR contract; never run real Provider. */
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const { freeze } = require("./bench-foundation");
const {
  SandboxGateError, buildBwrapArgs, checkBwrap, kernelPreflight,
  isolatedSynthetic, parseCli, PINNED_MOUNT_POLICY, EXPECTED_PROBE_FLAGS, PROBE_PYTHON
} = require("./bench-sandbox-supervisor");

function scoped(fn) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(),"kroma-bf14b-tests-"));
  try { return fn(root); } finally { fs.rmSync(root,{recursive:true,force:true}); }
}
const fixture = path.join(__dirname,"..","experiments","foundation-v1");
function frozen(root, executor="synthetic") {
  const out = path.join(root,"lock");
  return freeze({
    manifest:path.join(fixture,"manifest.json"), suite:path.join(fixture,"suite.json"),
    prices:path.join(fixture,"prices.json"), out, executor,
    "budget-tokens":"10000", seed:"12345"
  }).lockPath;
}
function rejects(fn,needle) {
  assert.throws(fn,e=>e instanceof SandboxGateError && e.message.includes(needle),needle);
}
function safeArgs(root, more = []) {
  const workspace=path.join(root,"workspace");
  fs.mkdirSync(workspace,{recursive:true});
  const program=path.join(root,"probe.py");
  fs.writeFileSync(program,"print('test')\n");
  return buildBwrapArgs({workspace,guestProgram:"/usr/bin/python3",
    guestArgs:["/runner/probe.py"],readonly:[{host:program,guest:"/runner/probe.py"},...more]});
}
function hasPair(args,one,two) {
  return args.some((item,index)=>item===one&&args[index+1]===two);
}
function roBindSource(args,dest) {
  for (let i=0;i+2<args.length;i++) if (args[i]==="--ro-bind"&&args[i+2]===dest) return args[i+1];
  return undefined;
}
// Host capability is measured, never assumed: only a real bwrap namespace smoke test separates
// "this host cannot sandbox" from "the sandbox leaked". Anything else must fail the suite.
function hostCanSandbox() {
  if (!fs.existsSync("/usr/bin/bwrap")) return false;
  const r=spawnSync("/usr/bin/bwrap",["--unshare-all","--ro-bind","/","/","--","/bin/true"],
    {encoding:"utf8",timeout:15000});
  return !r.error && r.status===0;
}
const HOST_CAN_SANDBOX=hostCanSandbox();
test("BF14B: sandbox policy locks all namespaces and forbids fallback",()=>scoped(root=>{
  const args=safeArgs(root);
  for(const item of ["--die-with-parent","--new-session","--unshare-all","--clearenv","--cap-drop"])
    assert.ok(args.includes(item),item);
  assert.ok(hasPair(args,"--cap-drop","ALL"));
  assert.ok(hasPair(args,"--ro-bind","/usr"));
  assert.ok(hasPair(args,"--chdir","/workspace"));
  assert.equal(args[args.indexOf("--")+1],"/usr/bin/python3");
  assert.ok(!args.includes("--share-net"));
  assert.ok(!args.includes("/root"));
  assert.ok(!args.includes("/home"));
  assert.match(PINNED_MOUNT_POLICY,/no-credentials/);
}));
test("BF14B: binds exactly one private workspace and no host root",()=>scoped(root=>{
  const args=safeArgs(root);
  assert.equal(args.filter(x=>x==="--bind").length,1);
  assert.ok(args.includes("--tmpfs"));
  assert.ok(args.includes("/workspace"));
  assert.ok(!hasPair(args,"--ro-bind","/"));
  assert.ok(!hasPair(args,"--bind","/"));
}));
test("BF14B: guest env drops user tokens and inherits no host env",()=>scoped(root=>{
  const args=safeArgs(root);
  const setenv=args.filter(x=>x==="--setenv").length;
  assert.equal(setenv,5);
  assert.ok(hasPair(args,"--setenv","HOME"));
  assert.equal(args.includes("API_KEY"),false);
  assert.equal(args.includes("KROMA_API_KEY"),false);
}));
test("BF14B: guest executable must be absolute",()=>scoped(root=>{
  const workspace=path.join(root,"workspace");fs.mkdirSync(workspace);
  rejects(()=>buildBwrapArgs({workspace,guestProgram:"sh",guestArgs:[]}),"guest executable");
}));
test("BF14B: argv must be an array without NUL",()=>scoped(root=>{
  const workspace=path.join(root,"workspace");fs.mkdirSync(workspace);
  rejects(()=>buildBwrapArgs({workspace,guestProgram:"/bin/sh",guestArgs:["bad\0arg"]}),"argv invalid");
}));
test("BF14B: rejects outside or writable host bind targets",()=>scoped(root=>{
  rejects(()=>safeArgs(root,[{host:root,guest:"/etc/secrets"}]),"only fixed read-only");
}));
test("BF14B: refuses a symlinked host mount source",()=>scoped(root=>{
  const target=path.join(root,"target");fs.writeFileSync(target,"x");
  const link=path.join(root,"link");fs.symlinkSync(target,link);
  rejects(()=>safeArgs(root,[{host:link,guest:"/runner/link"}]),"unsafe host mount");
}));
test("BF14B: rejects duplicate read-only guest mount",()=>scoped(root=>{
  const file=path.join(root,"file");fs.writeFileSync(file,"x");
  rejects(()=>safeArgs(root,[{host:file,guest:"/runner/probe.py"}]),"reused guest mount");
}));
test("BF14B: frozen inputs mount read-only and stay separate from workspace",()=>scoped(root=>{
  const file=path.join(root,"input");fs.mkdirSync(file);
  const args=safeArgs(root,[{host:file,guest:"/frozen"}]);
  assert.equal(roBindSource(args,"/frozen"),file);
  assert.equal(args.filter(x=>x==="--bind").length,1);
}));
test("BF14B: readonly mount parent is one sealed tmpfs, not a writable dir",()=>scoped(root=>{
  const second=path.join(root,"another");fs.writeFileSync(second,"x");
  const args=safeArgs(root,[{host:second,guest:"/runner/second"}]);
  // A plain --dir leaves a writable directory on the root tmpfs, so the guest could create files
  // beside the read-only mounts (measured: outside_write_denied=false). The parent must be a
  // single tmpfs mount point, sealed read-only after all of its children are mounted.
  assert.equal(args.filter((x,i)=>x==="--tmpfs"&&args[i+1]==="/runner").length,1);
  assert.equal(args.filter((x,i)=>x==="--dir"&&args[i+1]==="/runner").length,0);
  assert.equal(args.filter((x,i)=>x==="--remount-ro"&&args[i+1]==="/runner").length,1);
  const seal=args.lastIndexOf("/runner");
  assert.equal(args[seal-1],"--remount-ro");
  const lastChild=args.reduce((n,x,i)=>
    x==="--ro-bind"&&typeof args[i+2]==="string"&&args[i+2].startsWith("/runner/")?i:n,-1);
  assert.ok(lastChild>=0&&lastChild<seal-1,"every /runner child mount precedes the seal");
  assert.equal(roBindSource(args,"/runner/second"),second);
}));
test("BF14B: fixed trusted bwrap path cannot be overridden",()=>{
  rejects(()=>checkBwrap("/bin/true"),"no custom runner override");
});
test("BF14B: refuse provider even if a kernel sandbox exists",()=>scoped(root=>{
  // Re-freeze a normal frozen manifest as provider solely to check the guard ordering.
  const manifest=JSON.parse(fs.readFileSync(path.join(fixture,"manifest.json")));
  const prices=JSON.parse(fs.readFileSync(path.join(fixture,"prices.json")));
  manifest.id="BF14B-PROVIDER-REFUSAL";
  manifest.sandbox="bwrap-declared-only";
  for (const arm of manifest.arms) { arm.model="fixture-model-a"; arm.price_table="fixture-price-table"; }
  prices.version="fixture-price-table";
  prices.source="test fixture only";
  prices.models={"fixture-model-a":prices.models["demo-only-model"]};
  const manifestPath=path.join(root,"manifest.json"), pricesPath=path.join(root,"prices.json");
  fs.copyFileSync(path.join(fixture,"suite.json"),path.join(root,"suite.json"));
  fs.writeFileSync(manifestPath,JSON.stringify(manifest));
  fs.writeFileSync(pricesPath,JSON.stringify(prices));
  const lock=freeze({manifest:manifestPath,suite:path.join(root,"suite.json"),prices:pricesPath,
    out:path.join(root,"lock"),executor:"provider","budget-tokens":"10000",seed:"12345"}).lockPath;
  rejects(()=>isolatedSynthetic({lock,kroma:"/bin/true",out:path.join(root,"output")}),
    "BF14B_PROVIDER_BLOCKED");
  assert.equal(fs.existsSync(path.join(root,"output")),false);
}));
test("BF14B: reject invalid time budgets before launching anything",()=>scoped(root=>{
  const lock=frozen(root);
  rejects(()=>isolatedSynthetic({lock,kroma:"/bin/true",out:path.join(root,"out"),timeoutMs:0}),"timeout-ms");
  rejects(()=>isolatedSynthetic({lock,kroma:"/bin/true",out:path.join(root,"out"),timeoutMs:3600001}),"timeout-ms");
}));
test("BF14B: refuse mismatched frozen lock before accessing sandbox",()=>scoped(root=>{
  const lock=frozen(root);
  rejects(()=>isolatedSynthetic({lock,kroma:"/bin/true",out:path.join(root,"out"),
    expectedLockSha256:"0".repeat(64)}),"lock digest mismatch");
}));
test("BF14B: reject malformed freeze input before sandbox",()=>scoped(root=>{
  const lock=frozen(root);
  fs.appendFileSync(path.join(root,"lock","inputs","suite.json")," ");
  assert.throws(()=>isolatedSynthetic({lock,kroma:"/bin/true",out:path.join(root,"out")}),/changed since freeze/);
}));
test("BF14B: strict CLI accepts only supported operations",()=>{
  assert.deepEqual(parseCli(["probe","--out","/tmp/a"]),{verb:"probe",flags:{out:"/tmp/a"}});
  rejects(()=>parseCli(["probe","--out","/tmp/a","--executor","provider"]),"unknown or duplicate");
  rejects(()=>parseCli(["probe","--out","/tmp/a","--out","/tmp/b"]),"unknown or duplicate");
  rejects(()=>parseCli(["shell","--out","/tmp/a"]),"usage");
});
test("BF14B: fixed kernel probe includes nested subprocess and namespace tests",()=>{
  assert.ok(EXPECTED_PROBE_FLAGS.includes("nested_process_confined"));
  assert.ok(EXPECTED_PROBE_FLAGS.includes("network_namespace_distinct"));
  assert.ok(EXPECTED_PROBE_FLAGS.includes("host_canary_hidden"));
  assert.ok(EXPECTED_PROBE_FLAGS.includes("trusted_code_write_denied"));
});
test("BF14B: actual kernel preflight never lies about missing bwrap",{skip:fs.existsSync("/usr/bin/bwrap")},()=>scoped(root=>{
  rejects(()=>kernelPreflight({out:path.join(root,"out")}),"BF14B_ENV_BLOCKED");
  assert.equal(fs.existsSync(path.join(root,"out")),false);
}));
test("BF14B: CLI missing bwrap returns blocked instead of host execution",{skip:fs.existsSync("/usr/bin/bwrap")},()=>scoped(root=>{
  const child=spawnSync(process.execPath,[path.join(__dirname,"bench-sandbox-supervisor.js"),"probe",
    "--out",path.join(root,"out")],{encoding:"utf8"});
  assert.equal(child.status,2);
  assert.match(child.stderr,/BF14B_BLOCKED: BF14B_ENV_BLOCKED/);
  assert.equal(fs.existsSync(path.join(root,"out")),false);
}));
test("BF14B: real kernel probe passes every assertion when the host can sandbox",
  {skip:HOST_CAN_SANDBOX?false:"host cannot create bwrap namespaces (capability probe failed)"},()=>scoped(root=>{
  // No catch: a host that CAN sandbox but leaks must fail the suite. The previous version caught
  // "kernel probe failed <flag>" and passed, which is how a writable /runner stayed green.
  const result=kernelPreflight({out:path.join(root,"out")});
  assert.deepEqual(Object.keys(result.result.probe.checks).sort(),[...EXPECTED_PROBE_FLAGS].sort());
  for (const key of EXPECTED_PROBE_FLAGS)
    assert.equal(result.result.probe.checks[key],true,`kernel probe failed ${key}`);
  assert.equal(result.result.policy.all_assertions_passed,true);
  assert.equal(result.result.policy.measured,"fixed_guest_probe_in_kernel_namespaces");
}));

test("BF14B: fixed probe rejects its own execution on the unconfined host",()=>scoped(root=>{
  const canary=path.join(root,"host-private-canary"); fs.writeFileSync(canary,"host secret");
  const ns=Object.fromEntries(["mnt","pid","net","user"].map(n=>[n,fs.readlinkSync(`/proc/self/ns/${n}`)]));
  const out=spawnSync("/usr/bin/python3",["-c",PROBE_PYTHON,JSON.stringify(ns),canary],
    {encoding:"utf8",timeout:5000,maxBuffer:65536});
  assert.equal(out.status,0,out.stderr);
  const result=JSON.parse(out.stdout);
  assert.equal(result.checks.mount_namespace_distinct,false);
  assert.equal(result.checks.network_namespace_distinct,false);
  assert.equal(result.checks.host_canary_hidden,false);
  assert.equal(result.checks.user_namespace_distinct,false);
}));
