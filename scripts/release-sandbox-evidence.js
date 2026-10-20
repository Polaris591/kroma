#!/usr/bin/env node
"use strict";

/** A runner-produced, unsigned observation; it becomes authoritative only after owner signing. */
const fs = require("node:fs");
function fail(message) { throw new Error(message); }
function make({ commit, repository, runId, runAttempt, status, exitCode }) {
  if (!/^[0-9a-f]{40}$/.test(commit || "")) fail("invalid commit");
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository || "")) fail("invalid repository");
  if (!/^[1-9][0-9]{0,18}$/.test(String(runId || ""))) fail("invalid runId");
  if (!/^[1-9][0-9]{0,8}$/.test(String(runAttempt || ""))) fail("invalid runAttempt");
  if (!["PASS", "HOST_CAPABILITY_BLOCKED", "FAIL"].includes(status)) fail("invalid status");
  if (![0, 1, 2].includes(exitCode)) fail("invalid exitCode");
  if ((exitCode === 0 ? "PASS" : exitCode === 2 ? "HOST_CAPABILITY_BLOCKED" : "FAIL") !== status) fail("exit code and status disagree");
  return {
    schema: "kroma-sandbox-evidence/1", git_commit: commit, source_repository: repository,
    source_run_id: String(runId), source_run_attempt: Number(runAttempt),
    job: "sandbox isolation (real bwrap)", test_script: "scripts/run-sandbox-tests.sh",
    status, exit_code: exitCode
  };
}
if (require.main === module) {
  try {
    const [status, exitCode, out] = process.argv.slice(2);
    if (!out) fail("usage: release-sandbox-evidence.js <status> <exit code> <out>");
    const doc = make({ commit: process.env.GITHUB_SHA, repository: process.env.GITHUB_REPOSITORY,
      runId: process.env.GITHUB_RUN_ID, runAttempt: process.env.GITHUB_RUN_ATTEMPT,
      status, exitCode: Number(exitCode) });
    fs.writeFileSync(out, JSON.stringify(doc, null, 2) + "\n", { flag: "wx", mode: 0o600 });
    console.log(`sandbox evidence: ${status}`);
  } catch (err) {
    console.error(`sandbox evidence: ${err.message}`);
    process.exitCode = 1;
  }
}
module.exports = { make };
