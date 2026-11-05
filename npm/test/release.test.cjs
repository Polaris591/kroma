"use strict";
const {test} = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs"), os = require("node:os"), path = require("node:path");
const {execFileSync} = require("node:child_process");
const {platforms, packageName, executable} = require("../platforms.cjs");
const {collect, validate, publish} = require("../release.cjs");
function fixture() {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), "kroma release test "));
  const input = path.join(temp, "input"), source = path.join(temp, "source");
  fs.mkdirSync(input);
  const make = (name, platform) => {
    const dir = path.join(source, name, "package"); fs.mkdirSync(dir, {recursive: true});
    const manifest = {name, version: "1.2.3", license: "MIT"};
    if (platform) {
      manifest.os = [platform.os]; manifest.cpu = [platform.cpu];
      if (platform.os === "linux") manifest.libc = ["glibc"];
      fs.mkdirSync(path.join(dir, "bin"));
      fs.writeFileSync(path.join(dir, "bin", executable(platform)), "fixture only");
      fs.chmodSync(path.join(dir, "bin", executable(platform)), 0o755);
    } else {
      manifest.optionalDependencies = Object.fromEntries(platforms.map(p => [packageName(p), "1.2.3"]));
      for (const f of ["launcher.cjs", "platforms.cjs", "update.cjs"]) fs.writeFileSync(path.join(dir, f), "fixture only");
    }
    fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify(manifest));
    fs.writeFileSync(path.join(dir, "README.md"), "fixture"); fs.writeFileSync(path.join(dir, "LICENSE"), "MIT");
    const file = path.join(input, `${name}-1.2.3.tgz`);
    execFileSync("tar", ["-czf", path.relative(path.dirname(dir), file).replaceAll("\\", "/"), "package"], {cwd: path.dirname(dir)});
    // The fixture includes Unix packages even when its creator runs on Windows.
    const zlib = require("node:zlib"), bytes = zlib.gunzipSync(fs.readFileSync(file));
    for (let offset = 0; offset + 512 <= bytes.length; ) {
      const header = bytes.subarray(offset, offset + 512);
      const name = header.subarray(0, 100).toString().split("\0")[0];
      const size = parseInt(header.subarray(124, 136).toString().replace(/\0/g, "").trim(), 8) || 0;
      if (name.startsWith("package/bin/") && !name.endsWith("/")) {
        header.write("0000755\0", 100, 8, "ascii"); header.fill(32, 148, 156);
        const sum = [...header].reduce((a, b) => a + b, 0);
        header.write(sum.toString(8).padStart(6, "0") + "\0 ", 148, 8, "ascii");
      }
      offset += 512 + Math.ceil(size / 512) * 512;
    }
    fs.writeFileSync(file, zlib.gzipSync(bytes));
    return file;
  };
  for (const platform of platforms) make(packageName(platform), platform);
  make("kroma");
  return {temp, input};
}
function use(fn) { const f = fixture(); try { fn(f); } finally { fs.rmSync(f.temp, {recursive: true, force: true}); } }

test("collector requires all platforms, exact version and fresh output", () => use(({temp, input}) => {
  const out = path.join(temp, "output");
  const result = collect(input, out, "v1.2.3", "next");
  assert.equal(result.packages.length, 6);
  assert.equal(result.packages.at(-1).name, "kroma");
  assert.equal(fs.readFileSync(path.join(out, "SHA256SUMS"), "utf8").trim().split("\n").length, 6);
  assert.throws(() => collect(input, out, "v1.2.3", "next"), /fresh/);
  assert.throws(() => validate(input, "v2.0.0", "next"), /version/);
  fs.unlinkSync(path.join(input, `${packageName(platforms[0])}-1.2.3.tgz`));
  assert.throws(() => validate(input, "v1.2.3", "next"), /all five/);
}));
test("identical duplicate main tarballs are deduplicated, native duplicates rejected", () => use(({input}) => {
  const other = path.join(input, "other"); fs.mkdirSync(other);
  fs.copyFileSync(path.join(input, "kroma-1.2.3.tgz"), path.join(other, "kroma-1.2.3.tgz"));
  assert.equal(validate(input, "v1.2.3", "next").packages.length, 6);
  const native = `${packageName(platforms[0])}-1.2.3.tgz`;
  fs.copyFileSync(path.join(input, native), path.join(other, native));
  assert.throws(() => validate(input, "v1.2.3", "next"), /Duplicate/);
}));
test("publishing requires confirmation, publishes main last and stops on failure", () => use(({temp, input}) => {
  const out = path.join(temp, "output"); collect(input, out, "v1.2.3", "next");
  const previous = process.env.KROMA_RELEASE_CONFIRMED;
  try {
    delete process.env.KROMA_RELEASE_CONFIRMED;
    assert.throws(() => publish(out, () => { throw Error("must not execute"); }), /confirmation/);
    process.env.KROMA_RELEASE_CONFIRMED = "1";
    const calls = [];
    publish(out, (cmd, args, options) => { calls.push(args); assert.equal(cmd, "npm"); assert.equal(options.shell, false); return {status: 0}; });
    assert.equal(calls.length, 6); assert.ok(calls.at(-1)[1].endsWith("kroma-1.2.3.tgz"));
    assert.ok(calls.every(args => args.includes("--provenance")));
    let count = 0;
    assert.throws(() => publish(out, () => ({status: ++count === 2 ? 1 : 0})), /stopped/);
    assert.equal(count, 2);
    const manifest = path.join(out, "release-manifest.json"), data = JSON.parse(fs.readFileSync(manifest));
    data.packages[0].sha256 = "bad"; fs.writeFileSync(manifest, JSON.stringify(data));
    assert.throws(() => publish(out, () => { throw Error("must not execute"); }), /checksum/);
  } finally { if (previous === undefined) delete process.env.KROMA_RELEASE_CONFIRMED; else process.env.KROMA_RELEASE_CONFIRMED = previous; }
}));
