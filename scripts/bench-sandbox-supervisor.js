#!/usr/bin/env node
"use strict";

/** BF-14B/1: offline-only whole-process bubblewrap parent supervisor.
 *
 * This deliberately does not start real Provider work. The Rust Bench CLI presently receives
 * held-out oracles in the suite, and Provider credentials are not brokered outside its tool tree.
 * A process sandbox cannot solve either problem by itself. Neither this preflight nor its receipt
 * is a BF-14A job-level signed scorer certificate.
 *
 * TRUST: this file, bwrap and the parent process run OUTSIDE the guest. The guest has only a
 * writable private workspace, read-only frozen inputs (synthetic only) and read-only executable.
 * The probe script is mounted read-only, so model-supplied commands cannot redefine its checks.
 */

const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");
const crypto = require("node:crypto");
const { spawnSync } = require("node:child_process");
const { loadLock, nativePlan, evaluate } = require("./bench-foundation");

const SCHEMA = "kroma.benchmark.bf14b-supervisor/1";
const PROBE_SCHEMA = "kroma.benchmark.bf14b-kernel-probe/1";
const PROBE_LIMIT = 64 * 1024;
const MAX_REPORT = 16 * 1024 * 1024;
const MAX_RUN_MS = 3600000;
const BWRAP_PATH = "/usr/bin/bwrap";
const PINNED_MOUNT_POLICY = "read-only-system-allowlist;private-workspace;private-tmpfs;unshare-all;no-network;no-credentials;no-host-execution-fallback";
const EXPECTED_PROBE_FLAGS = [
  "mount_namespace_distinct", "pid_namespace_distinct", "network_namespace_distinct",
  "user_namespace_distinct", "no_default_route", "host_canary_hidden",
  "host_secrets_hidden", "outside_write_denied", "input_write_denied",
  "trusted_code_write_denied", "nested_process_confined", "oracle_absent",
  "scorer_absent", "workspace_writable"
];

class SandboxGateError extends Error {
  constructor(message) { super(message); this.name = "SandboxGateError"; }
}
const must = (condition, message) => { if (!condition) throw new SandboxGateError(message); };
const sha = bytes => crypto.createHash("sha256").update(bytes).digest("hex");
const shaFile = file => sha(fs.readFileSync(file));
function regular(file, what, limit = 256 * 1024 * 1024) {
  const s = fs.lstatSync(file);
  must(s.isFile() && !s.isSymbolicLink() && s.size <= limit, `${what} must be a bounded regular file`);
  return file;
}
function checkBwrap(binary = BWRAP_PATH) {
  // No arbitrary executable override: accepting a fake "bwrap" would silently run unisolated.
  must(binary === BWRAP_PATH, "BF14B only accepts the system /usr/bin/bwrap; no custom runner override");
  must(fs.existsSync(binary), "BF14B_ENV_BLOCKED: /usr/bin/bwrap not installed; never fall back to host");
  regular(binary, "bubblewrap binary");
  fs.accessSync(binary, fs.constants.X_OK);
  const st = fs.statSync(binary);
  must(st.uid === 0 && (st.mode & 0o022) === 0,
    "bubblewrap must be root-owned and not group/world writable");
  return { path: binary, sha256: shaFile(binary) };
}
function appendRuntimeMounts(argv) {
  // Mirror the Rust sandbox whitelist, not the caller's home/root or its host environment.
  must(fs.statSync("/usr").isDirectory(), "missing /usr toolchain");
  argv.push("--ro-bind", "/usr", "/usr");
  for (const dest of ["/bin", "/sbin", "/lib", "/lib64"]) {
    try {
      const stat = fs.lstatSync(dest);
      if (stat.isSymbolicLink()) {
        const target = fs.readlinkSync(dest);
        must(!target.startsWith("/") && !target.includes(".."), `unexpected runtime link ${dest}`);
        argv.push("--symlink", target, dest);
      } else if (stat.isDirectory()) {
        argv.push("--ro-bind", dest, dest);
      } else {
        throw new SandboxGateError(`unexpected system mount type ${dest}`);
      }
    } catch (err) { if (err.code !== "ENOENT") throw err; }
  }
  argv.push("--dir", "/etc", "--ro-bind-try", "/etc/ld.so.cache", "/etc/ld.so.cache");
}
/** Pure argv builder; tests can inspect it but cannot use it to claim OS isolation. */
function buildBwrapArgs({ workspace, guestProgram, guestArgs = [], readonly = [] }) {
  must(path.isAbsolute(workspace) && fs.statSync(workspace).isDirectory(), "workspace must exist");
  must(typeof guestProgram === "string" && guestProgram.startsWith("/"), "guest executable must be absolute");
  must(Array.isArray(guestArgs) && guestArgs.every(x => typeof x === "string" && !x.includes("\0")),
    "sandbox guest argv invalid");
  const argv = ["--die-with-parent", "--new-session", "--unshare-all", "--cap-drop", "ALL",
    "--clearenv", "--setenv", "HOME", "/workspace", "--setenv", "PATH", "/usr/bin:/bin",
    "--setenv", "TMPDIR", "/tmp", "--setenv", "LANG", "C", "--setenv", "LC_ALL", "C"];
  appendRuntimeMounts(argv);
  argv.push("--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--dir", "/run",
    "--bind", workspace, "/workspace");
  const targets = new Set(["/workspace"]);
  const createdDirs = new Set();
  for (const entry of readonly) {
    must(entry && typeof entry === "object", "invalid readonly mount");
    must(path.isAbsolute(entry.host) && /^\/(?:runner|frozen)(?:\/[a-zA-Z0-9_.-]+)?$/.test(entry.guest),
      "only fixed read-only /runner and /frozen mounts are permitted");
    must(!targets.has(entry.guest), "reused guest mount destination");
    targets.add(entry.guest);
    const st = fs.lstatSync(entry.host);
    must(!st.isSymbolicLink() && (st.isFile() || st.isDirectory()), "unsafe host mount source");
    const parent = path.posix.dirname(entry.guest);
    if (parent !== "/" && !createdDirs.has(parent)) {
      // A plain --dir would leave a writable directory on the root tmpfs, so the guest could
      // create files beside the read-only mounts (measured: outside_write_denied=false).
      // An empty tmpfs makes the parent a mount point that --remount-ro can seal afterwards.
      argv.push("--tmpfs", parent);
      createdDirs.add(parent);
    }
    argv.push("--ro-bind", entry.host, entry.guest);
  }
  // Seal each created parent after its children are mounted. --remount-ro is not recursive, so
  // the read-only child binds stay readable and executable.
  for (const parent of createdDirs) argv.push("--remount-ro", parent);
  argv.push("--chdir", "/workspace", "--", guestProgram, ...guestArgs);
  return argv;
}
function boundedBwrap(binary, argv, timeout, maxBuffer, cwd) {
  const r = spawnSync(binary.path, argv, {
    cwd, shell: false, encoding: "utf8", timeout, maxBuffer,
    input: "", env: { PATH: "/usr/bin:/bin", HOME: "/nonexistent", TMPDIR: "/tmp", LC_ALL: "C" }
  });
  // stdout of a model is never attached to an error message; stderr can contain tool output.
  must(!r.error && r.signal === null && r.status === 0,
    `BF14B_ENV_BLOCKED: bubblewrap guest failed (exit=${r.status ?? "none"}, signal=${r.signal ?? "none"}); no unsandboxed fallback`);
  must(typeof r.stdout === "string" && Buffer.byteLength(r.stdout) <= maxBuffer,
    "sandbox output missing or oversized");
  return r.stdout;
}

// Everything below runs as *trusted, fixed* probe source from a readonly mount, never from a
// model-writable workspace. The nested subprocess checks the actual kernel sandbox inheritance.
const PROBE_PYTHON = String.raw`import json, os, subprocess, sys
host_ns = json.loads(sys.argv[1]); host_canary = sys.argv[2]

def ns(kind):
    try: return os.readlink('/proc/self/ns/' + kind)
    except OSError: return ''

def cannot_write(location):
    try:
        with open(location, 'w') as f: f.write('BAD')
        return False
    except OSError: return True

try:
    with open('/workspace/bf14b-write-probe', 'w') as f: f.write('sandboxed')
    workspace_writable = True
except OSError: workspace_writable = False

try:
    with open('/proc/net/route') as f:
        routes = f.read().splitlines()[1:]
    no_default = not any(line.split()[1] == '00000000' for line in routes if len(line.split()) >= 2)
except (OSError, IndexError): no_default = False
nested_source = "import os,sys;sys.exit(0 if (not os.path.exists(sys.argv[1]) and not os.path.exists('/oracle') and not os.path.exists('/scorer')) else 1)"
try:
    nested_ok = subprocess.run(['/usr/bin/python3', '-c', nested_source, host_canary],
      timeout=3, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0
except Exception: nested_ok = False

flags = {
  'mount_namespace_distinct': bool(ns('mnt') and ns('mnt') != host_ns['mnt']),
  'pid_namespace_distinct': bool(ns('pid') and ns('pid') != host_ns['pid']),
  'network_namespace_distinct': bool(ns('net') and ns('net') != host_ns['net']),
  'user_namespace_distinct': bool(ns('user') and ns('user') != host_ns['user']),
  'no_default_route': no_default,
  'host_canary_hidden': not os.path.exists(host_canary),
  'host_secrets_hidden': not os.path.exists('/etc/shadow') and not os.path.exists('/root/.ssh'),
  'outside_write_denied': cannot_write('/runner/bf14b-forbidden-write'),
  'input_write_denied': cannot_write('/frozen/bf14b-forbidden-write'),
  'trusted_code_write_denied': cannot_write('/runner/bf14b-fixed-probe.py'),
  'nested_process_confined': nested_ok,
  'oracle_absent': not os.path.exists('/oracle'),
  'scorer_absent': not os.path.exists('/scorer'),
  'workspace_writable': workspace_writable
}
print(json.dumps({'schema': 'kroma.benchmark.bf14b-kernel-probe/1', 'checks': flags}, sort_keys=True))
`;
function tempScratch(callback) {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-bf14b-"));
  try {
    fs.chmodSync(scratch, 0o700);
    return callback(scratch);
  } finally { fs.rmSync(scratch, { recursive: true, force: true }); }
}
function setupScratch(scratch) {
  const workspace = path.join(scratch, "workspace");
  const trusted = path.join(scratch, "trusted");
  fs.mkdirSync(workspace, { mode: 0o700 });
  fs.mkdirSync(trusted, { mode: 0o700 });
  const probePath = path.join(trusted, "bf14b-fixed-probe.py");
  fs.writeFileSync(probePath, PROBE_PYTHON, { mode: 0o400, flag: "wx" });
  const canaryPath = path.join(trusted, "bf14b-host-canary");
  fs.writeFileSync(canaryPath, crypto.randomBytes(32), { mode: 0o400, flag: "wx" });
  return { workspace, trusted, probePath, canaryPath };
}
function probeInScratch(binary, scratch, directories, extraReadonly = []) {
  const namespaces = {};
  for (const name of ["mnt", "pid", "net", "user"]) namespaces[name] = fs.readlinkSync(`/proc/self/ns/${name}`);
  const argv = buildBwrapArgs({
    workspace: directories.workspace,
    guestProgram: "/usr/bin/python3",
    guestArgs: ["/runner/bf14b-fixed-probe.py", JSON.stringify(namespaces), directories.canaryPath],
    readonly: [{ host: directories.probePath, guest: "/runner/bf14b-fixed-probe.py" }, ...extraReadonly]
  });
  const stdout = boundedBwrap(binary, argv, 15000, PROBE_LIMIT, scratch);
  let parsed;
  try { parsed = JSON.parse(stdout); } catch { throw new SandboxGateError("invalid kernel probe JSON"); }
  must(parsed && parsed.schema === PROBE_SCHEMA && parsed.checks &&
    Object.keys(parsed.checks).length === EXPECTED_PROBE_FLAGS.length,
  "kernel probe schema mismatch");
  for (const key of EXPECTED_PROBE_FLAGS) {
    must(parsed.checks[key] === true, `kernel probe failed ${key}; no host fallback`);
  }
  const plan = {
    schema: SCHEMA, backend: "bwrap", bwrap_sha256: binary.sha256,
    probe_script_sha256: sha(Buffer.from(PROBE_PYTHON)), policy: PINNED_MOUNT_POLICY,
    sandbox_argv_sha256: sha(Buffer.from(JSON.stringify(argv))),
    all_assertions_passed: true, measured: "fixed_guest_probe_in_kernel_namespaces"
  };
  return { probe: parsed, policy: plan };
}
function saveOutput(out, files) {
  const dest = path.resolve(out);
  must(!fs.existsSync(dest), "output directory must not exist (immutable receipt)");
  fs.mkdirSync(dest, { recursive: false, mode: 0o700 });
  for (const [name, value] of Object.entries(files)) {
    must(/^[a-z][a-z0-9._-]+$/.test(name), "unsafe output name");
    fs.writeFileSync(path.join(dest, name), JSON.stringify(value, null, 2) + "\n", { flag: "wx", mode: 0o600 });
  }
  return dest;
}
function kernelPreflight({out}) {
  must(typeof out === "string", "--out is required");
  const binary = checkBwrap();
  const result = tempScratch(scratch => probeInScratch(binary, scratch, setupScratch(scratch)));
  const dir = saveOutput(out, {
    "sandbox-probe.json": result.probe,
    "supervisor-receipt.json": {
      ...result.policy,
      evidence_kind: "offline_kernel_preflight_only",
      bf14a_job_isolation_proof: false,
      provider_execution_authorized: false,
      scorer_independence_verified: false
    }
  });
  return { dir, result };
}
function checkExecutable(binaryFile) {
  must(typeof binaryFile === "string", "--kroma is required");
  const resolved = path.resolve(binaryFile);
  regular(resolved, "kroma executable");
  fs.accessSync(resolved, fs.constants.X_OK);
  return { resolved, sha256: shaFile(resolved) };
}
function isolatedSynthetic({lock, kroma, out, timeoutMs = 120000, expectedLockSha256}) {
  must(typeof lock === "string" && typeof out === "string", "--lock and --out required");
  must(Number.isSafeInteger(timeoutMs) && timeoutMs >= 1000 && timeoutMs <= MAX_RUN_MS,
    "timeout-ms outside safe bounds");
  const ctx = loadLock(lock);
  // Do not allow a "provider" arm even in a bwrap sandbox: the old Rust bench loads hidden
  // oracles and Provider credentials in the same process as untrusted model-invoked tools.
  must(ctx.lock.executor === "synthetic", "BF14B_PROVIDER_BLOCKED: no credential broker, isolated oracle, or per-job capability attestations");
  if (expectedLockSha256 !== undefined) {
    must(/^[0-9a-f]{64}$/.test(expectedLockSha256) && ctx.lock_sha256 === expectedLockSha256,
      "frozen lock digest mismatch");
  }
  const binary = checkBwrap();
  const program = checkExecutable(kroma);
  must(!fs.existsSync(path.resolve(out)), "output already exists");
  return tempScratch(scratch => {
    const d = setupScratch(scratch);
    const inputDir = path.join(scratch, "frozen-inputs");
    fs.mkdirSync(inputDir, { mode: 0o700 });
    for (const [source, name] of [["suite", "suite.json"], ["prices", "prices.json"]]) {
      const frozen = ctx.lock.inputs[source];
      must(frozen && /^[0-9a-f]{64}$/.test(frozen.sha256), `missing frozen ${source} hash`);
      const original = path.join(ctx.root, "inputs", name);
      regular(original, `${source} frozen input`, MAX_REPORT);
      must(shaFile(original) === frozen.sha256, "frozen input mutated before run");
      const copied = path.join(inputDir, name);
      fs.copyFileSync(original, copied, fs.constants.COPYFILE_EXCL);
      fs.chmodSync(copied, 0o400);
      must(shaFile(copied) === frozen.sha256, "copied frozen input mismatch");
    }
    const runner = path.join(d.trusted, "kroma");
    fs.copyFileSync(program.resolved, runner, fs.constants.COPYFILE_EXCL);
    fs.chmodSync(runner, 0o500);
    must(shaFile(runner) === program.sha256, "executable changed during copy");
    // Probe the SAME readonly runner and suite mounts that the ensuing execution receives.
    // The fixed probe is an additional trusted mount; it cannot see a scorer or host credentials.
    const probe = probeInScratch(binary, scratch, d, [
      {host:runner,guest:"/runner/kroma"},{host:inputDir,guest:"/frozen"}
    ]);
    const plan = nativePlan(ctx);
    must(plan.executor === "synthetic" && plan.runnable_in_this_driver === true,
      "native plan not available for safe synthetic execution");
    const args = [...plan.native_argv.slice(1)];
    const replaceArg = (key, value) => {
      const i = args.indexOf(key);
      must(i !== -1 && i + 1 < args.length, `expected ${key} from native plan`);
      args[i+1] = value;
    };
    replaceArg("--suite", "/frozen/suite.json");
    replaceArg("--workspace", "/workspace");
    must(args.indexOf("--executor") >= 0 && args[args.indexOf("--executor")+1] === "synthetic",
      "provider execution cannot be dispatched");
    const guestArgs = buildBwrapArgs({
      workspace:d.workspace, guestProgram:"/runner/kroma", guestArgs:args,
      readonly:[{host:runner,guest:"/runner/kroma"},{host:inputDir,guest:"/frozen"}]
    });
    const started = process.hrtime.bigint();
    const raw = boundedBwrap(binary, guestArgs, timeoutMs, MAX_REPORT, scratch);
    const wallTimeMs = Number((process.hrtime.bigint()-started)/1000000n);
    must(shaFile(runner) === program.sha256, "executable mutated after sandbox run");
    must(shaFile(path.join(inputDir,"suite.json")) === ctx.lock.inputs.suite.sha256,
      "frozen suite mutated during sandbox run");
    let report;
    try { report = JSON.parse(raw); } catch { throw new SandboxGateError("sandboxed synthetic bench returned invalid report"); }
    const reportFile = path.join(scratch,"native-report.json");
    fs.writeFileSync(reportFile, raw, { flag:"wx", mode:0o600 });
    const result = evaluate({lock, report:reportFile, out});
    const receipt = {
      schema:SCHEMA, experiment_id:ctx.lock.experiment_id, executor:"synthetic",
      evidence_kind:"synthetic_os_sandbox_plumbing_only",
      lock_sha256:ctx.lock_sha256, executable_sha256:program.sha256,
      kernel_probe_sha256:sha(Buffer.from(JSON.stringify(probe.probe))),
      sandbox_policy_sha256:sha(Buffer.from(JSON.stringify(probe.policy))),
      isolation_backend:"bwrap", full_runner_wrapped_in_bwrap:true,
      measured_kernel_probe_passed:true,
      sandboxed_runner_argv_sha256:sha(Buffer.from(JSON.stringify(guestArgs))),
      report_sha256:sha(Buffer.from(raw)), observed_jobs:report.results.length,
      wall_time_ms:wallTimeMs, provider_execution_authorized:false,
      independent_oracle_executed:false, independent_scorer_attestation:false,
      bf14a_job_isolation_proof:false
    };
    // Evaluate wrote a new, private output dir; never write the sensitive raw stdout there.
    fs.writeFileSync(path.join(result.outDir,"sandbox-probe.json"),
      JSON.stringify(probe.probe,null,2)+"\n", { flag:"wx", mode:0o600 });
    fs.writeFileSync(path.join(result.outDir,"sandbox-receipt.json"),
      JSON.stringify(receipt,null,2)+"\n", { flag:"wx", mode:0o600 });
    return { outDir:result.outDir, receipt };
  });
}
function parseCli(argv) {
  const verb = argv.shift();
  must(verb === "probe" || verb === "synthetic", "usage: bench-sandbox-supervisor.js probe|synthetic --out ... [--lock ... --kroma ...]");
  const allowed = verb === "probe" ? ["out"] : ["out","lock","kroma","timeout-ms","expect-lock-sha256"];
  const flags = {};
  while (argv.length) {
    const name = argv.shift();
    must(/^--[a-z][a-z0-9-]*$/.test(name) && allowed.includes(name.slice(2)) && !(name.slice(2) in flags),
      `unknown or duplicate option ${name}`);
    must(argv.length > 0, `missing value for ${name}`);
    flags[name.slice(2)] = argv.shift();
  }
  return {verb, flags};
}
function main() {
  try {
    const {verb,flags} = parseCli(process.argv.slice(2));
    if (verb === "probe") {
      const result = kernelPreflight({out:flags.out});
      console.log(`BF14B_KERNEL_PREFLIGHT_PASS ${result.dir}`);
    } else {
      const result = isolatedSynthetic({lock:flags.lock,kroma:flags.kroma,out:flags.out,
        timeoutMs: flags["timeout-ms"] === undefined ? 120000 : Number(flags["timeout-ms"]),
        expectedLockSha256:flags["expect-lock-sha256"]});
      console.log(`BF14B_SYNTHETIC_ISOLATED_PASS ${result.outDir}`);
    }
  } catch(err) {
    console.error(`BF14B_BLOCKED: ${err instanceof SandboxGateError ? err.message : "runtime prerequisite/OS sandbox execution failed; no host fallback"}`);
    process.exitCode = 2;
  }
}
if (require.main === module) main();
module.exports = {SandboxGateError, SCHEMA, PROBE_SCHEMA, EXPECTED_PROBE_FLAGS, PINNED_MOUNT_POLICY,
  buildBwrapArgs, checkBwrap, kernelPreflight, isolatedSynthetic, parseCli, PROBE_PYTHON};
