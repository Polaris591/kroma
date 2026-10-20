#!/usr/bin/env node
"use strict";

/**
 * Canonical release candidate layout. The seven build artifacts and the sandbox result arrive in
 * separate GitHub Actions artifact directories, never merged by basename. Normalize only after
 * checking the complete exact set. Paths inside candidate/ are independent of upload-artifact's
 * common-ancestor rules and stable across downloads.
 */
const fs = require("node:fs");
const path = require("node:path");

const PLATFORMS = Object.freeze([
  { action: "linux-x64", input: "kroma", output: "kroma-linux-x64" },
  { action: "win32-x64", input: "kroma.exe", output: "kroma-win32-x64.exe" },
  { action: "darwin-arm64", input: "kroma", output: "kroma-darwin-arm64" },
  { action: "darwin-x64", input: "kroma", output: "kroma-darwin-x64" },
  { action: "android-arm64", input: "kroma", output: "kroma-android-arm64" },
  { action: "linux-arm64", input: "kroma", output: "kroma-linux-arm64" }
]);
const SANDBOX_ACTION = "sandbox-evidence";
const NPM_ACTION = "kroma-cli-tarball";

function fail(text) { throw new Error(text); }
function assertVersion(version) {
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version || "")) fail(`invalid release version: ${version}`);
  return version;
}
function tarball(version) { return `kroma-cli-${assertVersion(version)}.tgz`; }
function expectedCandidate(version) {
  return [...PLATFORMS.map((x) => x.output), tarball(version), "sandbox-evidence.json"].sort();
}
function entries(dir) {
  if (!fs.existsSync(dir) || !fs.lstatSync(dir).isDirectory()) fail(`not a directory: ${dir}`);
  return fs.readdirSync(dir).sort();
}
function exact(actual, expected, label) {
  const a = [...actual].sort(), b = [...expected].sort();
  if (JSON.stringify(a) !== JSON.stringify(b)) {
    fail(`${label} inventory mismatch: expected [${b.join(", ")}], got [${a.join(", ")}]`);
  }
}
function regularFile(file) {
  const st = fs.lstatSync(file); // never follow symlinks, including directory components
  if (!st.isFile() || st.size <= 0) fail(`not a nonempty regular file: ${file}`);
  return st;
}
function checkCandidate(root, version) {
  exact(entries(root), expectedCandidate(version), "candidate");
  for (const name of expectedCandidate(version)) regularFile(path.join(root, name));
  return expectedCandidate(version);
}
function stage(downloads, destination, version) {
  const sourceDirs = [...PLATFORMS.map((p) => p.action), NPM_ACTION, SANDBOX_ACTION];
  exact(entries(downloads), sourceDirs, "GitHub artifact");
  if (fs.existsSync(destination)) fail(`candidate destination already exists: ${destination}`);
  const parent = path.dirname(destination);
  fs.mkdirSync(parent, { recursive: true });
  const temp = fs.mkdtempSync(path.join(parent, ".candidate-stage-"));
  const items = [...PLATFORMS.map((p) => [p.action, p.input, p.output]),
    [NPM_ACTION, tarball(version), tarball(version)],
    [SANDBOX_ACTION, "sandbox-evidence.json", "sandbox-evidence.json"]];
  try {
    for (const [directory, name, outName] of items) {
      const sourceDir = path.join(downloads, directory);
      if (!fs.lstatSync(sourceDir).isDirectory()) fail(`not a real artifact directory: ${sourceDir}`);
      exact(entries(sourceDir), [name], `${directory} artifact`);
      const source = path.join(sourceDir, name);
      const st = regularFile(source);
      fs.copyFileSync(source, path.join(temp, outName), fs.constants.COPYFILE_EXCL);
      fs.chmodSync(path.join(temp, outName), st.mode & 0o777);
    }
    checkCandidate(temp, version);
    fs.renameSync(temp, destination);
  } catch (err) {
    fs.rmSync(temp, { recursive: true, force: true });
    throw err;
  }
  return destination;
}
function args(argv) {
  const values = {};
  for (let i = 0; i < argv.length; i++) {
    const name = argv[i];
    if (!["--downloads", "--out", "--root", "--version"].includes(name) || i + 1 >= argv.length) fail(`unknown or incomplete option: ${name}`);
    values[name.slice(2)] = argv[++i];
  }
  return values;
}
if (require.main === module) {
  try {
    const cmd = process.argv[2], a = args(process.argv.slice(3));
    if (cmd === "stage") {
      if (!a.downloads || !a.out || !a.version) fail("stage requires --downloads --out --version");
      stage(a.downloads, a.out, a.version);
    } else if (cmd === "check") {
      if (!a.root || !a.version) fail("check requires --root --version");
      checkCandidate(a.root, a.version);
    } else fail("usage: release-candidate.js <stage|check> ...");
    console.log(`release candidate ${cmd}: PASS`);
  } catch (err) {
    console.error(`release candidate ${process.argv[2]}: ${err.message}`);
    process.exitCode = 1;
  }
}
module.exports = { PLATFORMS, expectedCandidate, stage, checkCandidate, exact, assertVersion };
