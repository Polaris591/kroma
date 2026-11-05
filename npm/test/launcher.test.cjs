"use strict";

const {test, before, after} = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const {spawn, spawnSync} = require("node:child_process");
const {once} = require("node:events");
const {forHost, packageName, executable} = require("../platforms.cjs");

let fixture, launcher, nativeManifest, binary, helper;
const version = "1.2.3";
before(() => {
  // macOS resolves the temp root through /private/var, so compare real paths.
  fixture = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "kroma launcher test ")));
  launcher = path.join(fixture, "launcher.cjs");
  for (const file of ["launcher.cjs", "platforms.cjs", "update.cjs"]) {
    fs.copyFileSync(path.join(__dirname, "..", file), path.join(fixture, file));
  }
  fs.writeFileSync(path.join(fixture, "package.json"), JSON.stringify({name: "kroma", version}));
  const platform = forHost();
  const nativeDir = path.join(fixture, "node_modules", packageName(platform));
  fs.mkdirSync(path.join(nativeDir, "bin"), {recursive: true});
  nativeManifest = path.join(nativeDir, "package.json");
  fs.writeFileSync(nativeManifest, JSON.stringify({name: packageName(platform), version}));
  binary = path.join(nativeDir, "bin", executable(platform));
  // A real executable exercises spawn/stdio/exit handling without a compiler.
  fs.copyFileSync(process.execPath, binary);
  fs.chmodSync(binary, 0o755);
  helper = path.join(fixture, "echo arguments.cjs");
  fs.writeFileSync(helper, `
    const fs = require("node:fs");
    console.log(JSON.stringify({args: process.argv.slice(2), cwd: process.cwd(),
      env: process.env.KROMA_NPM_FIXTURE, stdin: fs.readFileSync(0, "utf8")}));
    console.error("fixture stderr");
    process.exitCode = 7;
  `);
});
after(() => fs.rmSync(fixture, {recursive: true, force: true, maxRetries: 3}));

function run(args, options = {}) {
  return spawnSync(process.execPath, [launcher, ...args], {cwd: fixture, encoding: "utf8", timeout: 15000, ...options});
}

test("launcher preserves literal arguments, cwd, environment, all stdio and exit code", () => {
  const args = ["two words", "中文", '--json={"x":1}', "$(echo injected)", "a;b", "--", ""];
  const result = run([helper, ...args], {input: "hello\n输入", env: {...process.env, KROMA_NPM_FIXTURE: "inherited"}});
  assert.ifError(result.error);
  assert.equal(result.status, 7);
  assert.deepEqual(JSON.parse(result.stdout), {args, cwd: fixture, env: "inherited", stdin: "hello\n输入"});
  assert.equal(result.stderr.trim(), "fixture stderr");
});

test("launcher reports an omitted optional package without downloading anything", () => {
  fs.renameSync(nativeManifest, `${nativeManifest}.hidden`);
  try {
    const result = run(["--version"]);
    assert.equal(result.status, 1);
    assert.equal(result.stdout, "");
    assert.match(result.stderr, /Missing kroma-.*@1\.2\.3/);
    assert.match(result.stderr, /optional dependencies enabled/);
  } finally {
    fs.renameSync(`${nativeManifest}.hidden`, nativeManifest);
  }
});

test("launcher rejects mismatched platform package versions", () => {
  const original = fs.readFileSync(nativeManifest);
  fs.writeFileSync(nativeManifest, JSON.stringify({name: packageName(forHost()), version: "0.0.1"}));
  try {
    const result = run(["--version"]);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /Expected .*@1\.2\.3, found .*@0\.0\.1/);
  } finally {
    fs.writeFileSync(nativeManifest, original);
  }
});

test("launcher reports a missing native executable", () => {
  fs.renameSync(binary, `${binary}.hidden`);
  try {
    const result = run(["--version"]);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /Kroma:.*ENOENT/);
    assert.equal(result.stdout, "");
  } finally {
    fs.renameSync(`${binary}.hidden`, binary);
  }
});

test("launcher reports a native spawn failure without an unhandled exception", () => {
  const original = fs.readFileSync(binary);
  fs.writeFileSync(binary, "not an executable");
  try {
    const result = run(["--version"]);
    assert.notEqual(result.status, 0);
    assert.doesNotMatch(result.stderr, /Unhandled 'error' event/);
  } finally {
    fs.writeFileSync(binary, original);
    fs.chmodSync(binary, 0o755);
  }
});

test("POSIX launcher forwards interruption and preserves signal termination", {skip: process.platform === "win32", timeout: 10000}, async () => {
  const waiting = path.join(fixture, "waiting.cjs");
  fs.writeFileSync(waiting, 'console.log(process.pid); setInterval(() => {}, 1000);');
  const child = spawn(process.execPath, [launcher, waiting], {cwd: fixture, stdio: ["ignore", "pipe", "pipe"]});
  let nativePid;
  try {
    const exit = once(child, "exit");
    const [data] = await once(child.stdout, "data");
    nativePid = Number(data.toString().trim());
    assert.ok(Number.isInteger(nativePid) && nativePid > 0);
    child.kill("SIGINT");
    const [code, signal] = await exit;
    assert.equal(code, null);
    assert.equal(signal, "SIGINT");
    assert.throws(() => process.kill(nativePid, 0), {code: "ESRCH"});
  } finally {
    child.kill("SIGKILL");
    if (nativePid) {
      try { process.kill(nativePid, "SIGKILL"); } catch (error) { if (error.code !== "ESRCH") throw error; }
    }
  }
});

test("launcher refuses unsupported architectures and musl before resolving packages", () => {
  const bootstrap = path.join(fixture, "override host.cjs");
  for (const [source, message] of [
    ['Object.defineProperty(process, "platform", {value: "win32"}); Object.defineProperty(process, "arch", {value: "arm64"});', /Unsupported platform win32\/arm64/],
    ['Object.defineProperty(process, "platform", {value: "linux"}); Object.defineProperty(process, "arch", {value: "x64"}); process.report.getReport = () => ({header: {}});', /glibc Linux; Alpine\/musl is not supported/],
  ]) {
    fs.writeFileSync(bootstrap, source);
    const result = spawnSync(process.execPath, ["--require", bootstrap, launcher, "--version"],
      {cwd: fixture, encoding: "utf8", timeout: 15000});
    assert.ifError(result.error);
    assert.equal(result.status, 1);
    assert.equal(result.stdout, "");
    assert.match(result.stderr, message);
  }
});

test("update help works before native resolution and invalid update arguments are usage errors", () => {
  fs.renameSync(nativeManifest, nativeManifest + ".hidden");
  try {
    const help = run(["update", "--help"]);
    assert.equal(help.status, 0);
    assert.match(help.stdout, /Usage: kroma update/);
    const bad = run(["update", "--channel", "bad"]);
    assert.equal(bad.status, 2);
  } finally { fs.renameSync(nativeManifest + ".hidden", nativeManifest); }
});
