"use strict";
const {test} = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs"), os = require("node:os"), path = require("node:path");
const {compare, version, parse, updateArgs, published, runUpdate} = require("../update.cjs");
const response = (data, status = 200) => async (url, options) => {
  assert.match(url, /^https:\/\/registry\.npmjs\.org\/kroma\/(latest|next)$/);
  assert.equal(options.redirect, "error");
  assert.ok(options.signal instanceof AbortSignal);
  return new Response(typeof data === "string" ? data : JSON.stringify(data), {status});
};
function fixture(global = true) {
  const temp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "kroma update test ")));
  const prefix = path.join(temp, "global & prefix");
  const root = global ? path.join(prefix, process.platform === "win32" ? "node_modules" : "lib/node_modules", "kroma") : path.join(temp, "project space", "node_modules", "kroma");
  fs.mkdirSync(root, {recursive: true});
  fs.writeFileSync(path.join(root, "package.json"), JSON.stringify({name: "kroma", version: "1.0.0"}));
  const calls = [];
  return {temp, root, prefix, calls, cli: path.join(temp, "npm-cli.js"), log() {},
    execute(file, args, options) {
      calls.push({file, args, options});
      if (args.includes("install")) {
        fs.writeFileSync(path.join(root, "package.json"), JSON.stringify({name: "kroma", version: "1.1.0"}));
        const {packageName, forHost} = require("../platforms.cjs"), name = packageName(forHost());
        const native = path.join(root, "node_modules", name); fs.mkdirSync(native, {recursive: true});
        fs.writeFileSync(path.join(native, "package.json"), JSON.stringify({name, version: "1.1.0"}));
      }
      return {status: 0, stdout: prefix};
    },
    fetchImpl: response({name: "kroma", version: "1.1.0"})};
}
async function withFixture(global, fn) { const f = fixture(global); try { await fn(f); } finally { fs.rmSync(f.temp, {recursive: true, force: true}); } }

test("semantic ordering handles prereleases, numbers and ignores build metadata", () => {
  const ordered = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0", "1.0.1", "1.2.0", "1.10.0", "2.0.0"];
  ordered.slice(1).forEach((v, i) => { assert.equal(compare(ordered[i], v), -1); assert.equal(compare(v, ordered[i]), 1); });
  assert.equal(compare("1.0.0+one", "1.0.0+two"), 0);
  for (const v of ["v1.0.0", "1.0", "01.2.3", "1.2.3-01", "1.2.3;evil", "1.2.3\n"]) assert.throws(() => version(v));
});
test("update parsing is strict and does not hijack task text", () => {
  assert.deepEqual(parse(["--check", "--channel=next"]), {check: true, channel: "next"});
  for (const args of [["--channel"], ["--channel", "evil"], ["--check", "--check"], ["--channel", "next", "--channel", "latest"], ["--bogus"]]) assert.throws(() => parse(args));
  assert.deepEqual(updateArgs(["--cwd", "space dir", "update", "--quiet", "--check"]), ["--check"]);
  for (const args of [["--print", "update"], ["--", "update"], ["explain", "update"], ["--language", "bad", "update"]]) assert.equal(updateArgs(args), null);
});
test("unpublished, malformed, wrong-package, oversized and prerelease-latest fail closed", async () => {
  for (const [body, status] of [[{}, 404], [{}, 500], ["bad", 200], [{name: "other", version: "1.0.0"}, 200], [{name: "kroma", version: "1.0.0-beta"}, 200], ["x".repeat(1024 * 1024 + 1), 200]]) {
    await assert.rejects(published("latest", response(body, status)));
  }
  assert.equal(await published("next", response({name: "kroma", version: "2.0.0-beta.1"})), "2.0.0-beta.1");
});
test("check and help never execute npm or modify installation", async () => withFixture(true, async f => {
  const before = fs.readFileSync(path.join(f.root, "package.json"), "utf8");
  await runUpdate(["--check"], f);
  await runUpdate(["--help"], {...f, fetchImpl: () => { throw Error("must not fetch"); }});
  assert.equal(f.calls.length, 0);
  assert.equal(fs.readFileSync(path.join(f.root, "package.json"), "utf8"), before);
}));
test("same/newer installed versions do not downgrade or invoke npm", async () => withFixture(true, async f => {
  for (const v of ["1.0.0", "0.9.0"]) await runUpdate([], {...f, fetchImpl: response({name: "kroma", version: v})});
  assert.equal(f.calls.length, 0);
}));
test("global update is pinned, shell-free, script-free and explicitly scoped", async () => withFixture(true, async f => {
  await runUpdate([], f);
  const install = f.calls[1];
  assert.equal(f.calls.length, 2);
  assert.equal(install.file, process.execPath);
  assert.ok(install.args.includes("--global"));
  assert.ok(install.args.includes("kroma@1.1.0"));
  assert.ok(install.args.includes("--ignore-scripts"));
  assert.ok(install.args.includes("--include=optional"));
  assert.equal(install.args[install.args.indexOf("--prefix") + 1], f.prefix);
  assert.equal(install.options.shell, false);
}));
test("local update uses owning project, never current cwd or global scope", async () => withFixture(false, async f => {
  const project = path.dirname(path.dirname(f.root));
  fs.writeFileSync(path.join(project, "package.json"), '{"devDependencies":{"kroma":"^1.0.0"}}');
  await runUpdate([], f);
  assert.equal(f.calls[1].args.includes("--global"), false);
  assert.equal(f.calls[1].args[f.calls[1].args.indexOf("--prefix") + 1], project);
}));
test("ambiguous and other-package-manager installations never invoke install", async () => withFixture(false, async f => {
  await assert.rejects(runUpdate([], f));
  const project = path.dirname(path.dirname(f.root));
  fs.writeFileSync(path.join(project, "package.json"), '{"dependencies":{"kroma":"1"},"packageManager":"pnpm@9"}');
  await assert.rejects(runUpdate([], f), /another package manager/);
  assert.ok(f.calls.every(c => !c.args.includes("install")));
}));
test("registry and npm failures surface without claiming an upgrade", async () => withFixture(true, async f => {
  await assert.rejects(runUpdate([], {...f, fetchImpl: async () => { throw Error("offline"); }}), /offline/);
  assert.equal(f.calls.length, 0);
  await assert.rejects(runUpdate([], {...f, execute(file, args) { return args.includes("install") ? {status: 1} : {status: 0, stdout: f.prefix}; }}), /npm upgrade failed/);
}));

test("npm exit zero is not reported as success without verified installed versions", async () => withFixture(true, async f => {
  await assert.rejects(runUpdate([], {...f, execute() { return {status: 0, stdout: f.prefix}; }}), /not verified/);
}));
