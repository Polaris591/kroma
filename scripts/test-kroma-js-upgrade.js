#!/usr/bin/env node
"use strict";

// Standalone regression test for the pure helpers in `bin/kroma.js`. The repo has no JS test
// runner, so run this directly: `node scripts/test-kroma-js-upgrade.js`.

const assert = require("node:assert");
const path = require("node:path");

const {
  isValidHttpsTarballUrl,
  isSafeInstallTarget,
  parseSemver,
  compareSemver
} = require(path.join(__dirname, "..", "bin", "kroma.js"));

let passed = 0;
function check(name, fn) {
  try {
    fn();
    passed += 1;
    console.log(`  ok  ${name}`);
  } catch (err) {
    console.error(`FAIL  ${name}\n      ${err.message}`);
    process.exitCode = 1;
  }
}

console.log("isValidHttpsTarballUrl");
check("accepts the pinned mirror tarball", () => {
  assert.strictEqual(isValidHttpsTarballUrl("https://cobblemon.asia/kroma.tgz"), true);
});
check("rejects a plain http URL", () => {
  assert.strictEqual(isValidHttpsTarballUrl("http://cobblemon.asia/kroma.tgz"), false);
});
check("rejects a `&` command chain", () => {
  assert.strictEqual(isValidHttpsTarballUrl("https://cobblemon.asia/kroma.tgz&calc.exe"), false);
});
check("rejects a `;` command separator", () => {
  assert.strictEqual(
    isValidHttpsTarballUrl("https://cobblemon.asia/kroma.tgz; rm -rf /"),
    false
  );
});
check("rejects a pipe into a shell", () => {
  assert.strictEqual(isValidHttpsTarballUrl("https://cobblemon.asia/kroma.tgz | sh"), false);
});
check("rejects embedded whitespace/newlines", () => {
  assert.strictEqual(
    isValidHttpsTarballUrl("https://cobblemon.asia/kroma.tgz\n& calc.exe"),
    false
  );
});
check("rejects a non-allow-listed host", () => {
  assert.strictEqual(isValidHttpsTarballUrl("https://evil.example.com/kroma.tgz"), false);
});
check("rejects embedded credentials", () => {
  assert.strictEqual(isValidHttpsTarballUrl("https://user:pass@cobblemon.asia/kroma.tgz"), false);
});
check("rejects a non-default port", () => {
  assert.strictEqual(isValidHttpsTarballUrl("https://cobblemon.asia:8443/kroma.tgz"), false);
});
check("rejects a query string", () => {
  assert.strictEqual(isValidHttpsTarballUrl("https://cobblemon.asia/kroma.tgz?x=1"), false);
});
check("rejects a non-https scheme", () => {
  assert.strictEqual(isValidHttpsTarballUrl("javascript:alert(1)"), false);
});
check("rejects empty and non-string input", () => {
  assert.strictEqual(isValidHttpsTarballUrl(""), false);
  assert.strictEqual(isValidHttpsTarballUrl(null), false);
  assert.strictEqual(isValidHttpsTarballUrl(42), false);
});

console.log("isSafeInstallTarget");
check("accepts a plain npm package spec", () => {
  assert.strictEqual(isSafeInstallTarget("kroma-cli@latest"), true);
});
check("accepts a validated tarball URL", () => {
  assert.strictEqual(isSafeInstallTarget("https://cobblemon.asia/kroma.tgz"), true);
});
check("rejects a package spec with a command chain", () => {
  assert.strictEqual(isSafeInstallTarget("kroma-cli@latest & calc.exe"), false);
});
check("rejects a non-allow-listed tarball host", () => {
  assert.strictEqual(isSafeInstallTarget("https://evil.example.com/kroma.tgz"), false);
});

console.log("parseSemver / compareSemver");
check("compareSemver detects an upgrade", () => {
  assert.strictEqual(compareSemver("1.5.1", "1.5.0"), 1);
});
check("compareSemver detects equality", () => {
  assert.strictEqual(compareSemver("1.5.0", "1.5.0"), 0);
});
check("compareSemver detects a downgrade (mirror older than local)", () => {
  assert.strictEqual(compareSemver("1.4.9", "1.5.0"), -1);
});
check("compareSemver accepts a leading v", () => {
  assert.strictEqual(compareSemver("v1.5.1", "1.5.0"), 1);
});
check("compareSemver compares numerically, not lexicographically", () => {
  assert.strictEqual(compareSemver("2.0.0", "10.0.0"), -1);
});
check("compareSemver returns null for a non-SemVer value", () => {
  assert.strictEqual(compareSemver("latest", "1.5.0"), null);
  assert.strictEqual(compareSemver("1.5", "1.5.0"), null);
  assert.strictEqual(parseSemver("1.5"), null);
});

if (process.exitCode) {
  console.error(`\nFAILED (${passed} passed)`);
} else {
  console.log(`\nall ${passed} checks passed`);
}
