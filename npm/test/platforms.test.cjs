"use strict";

const {test} = require("node:test");
const assert = require("node:assert/strict");
const {platforms, forHost, packageName, executable} = require("../platforms.cjs");
const {cargoVersion, parseArgs, stage} = require("../stage.cjs");

test("every declared target has a unique platform package and executable", () => {
  assert.equal(new Set(platforms.map(packageName)).size, 5);
  const targets = platforms.flatMap(item => item.targets);
  assert.equal(new Set(targets).size, targets.length);
  for (const platform of platforms) {
    assert.equal(forHost(platform.os, platform.cpu), platform);
    assert.equal(executable(platform), platform.os === "win32" ? "kroma.exe" : "kroma");
  }
});

test("unsupported hosts fail rather than selecting a wrong binary", () => {
  for (const [os, cpu] of [["win32", "arm64"], ["linux", "ia32"], ["freebsd", "x64"]]) {
    assert.throws(() => forHost(os, cpu), /Unsupported platform/);
  }
});

test("staging takes its version from Cargo and strictly parses arguments", () => {
  assert.match(cargoVersion(), /^\d+\.\d+\.\d+/);
  assert.deepEqual(parseArgs(["--target", "triple", "--binary", "a b", "--out", "output"]),
    {target: "triple", binary: "a b", out: "output"});
  for (const args of [[], ["--target"], ["--wat", "x"], ["--binary", "--out"],
    ["--target", "x", "--target", "y", "--binary", "b", "--out", "o"]]) {
    assert.throws(() => parseArgs(args));
  }
});

test("staging rejects unsupported targets, cross-host binaries and version mismatches", () => {
  assert.throws(() => stage({target: "not-a-target"}), /Unsupported Rust target/);
  const foreign = platforms.find(item => item.os !== process.platform);
  assert.throws(() => stage({target: foreign.targets[0]}), /Stage on the target platform/);
  assert.throws(() => stage({target: forHost().targets[0], binary: process.execPath, out: "unused"}),
    /Binary must run and report kroma/);
});
