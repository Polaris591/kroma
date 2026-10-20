#!/usr/bin/env node
"use strict";
/**
 * Independently validates the GitHub API's identity for the FIRST pass. Do not trust an attestation
 * or a workflow_dispatch text input to tell the verifier which commit/runner made it.
 */
const fs = require("node:fs");
function fail(message) { throw new Error(message); }
const REQUIRED_JOBS = [
  "quality gate (fmt, clippy, rust tests, node, gates)",
  "sandbox isolation (real bwrap)",
  "linux-x64", "win32-x64", "darwin-arm64 + darwin-x64", "android-arm64", "linux-arm64 (musl)",
  "pack npm tarball", "attest the candidate artifact set"
];
function validate(metadata, jobsResponse, artifactsResponse, expected) {
  if (!/^[1-9][0-9]{0,18}$/.test(String(expected.runId))) fail("invalid candidate_run id");
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(expected.repository || "")) fail("invalid repository context");
  if (String(metadata.id) !== String(expected.runId)) fail("source run id mismatch");
  if (metadata.repository?.full_name !== expected.repository ||
      metadata.head_repository?.full_name !== expected.repository) fail("candidate run is from a different repository");
  if (metadata.path !== ".github/workflows/release.yml") fail("candidate came from a different workflow");
  if (metadata.status !== "completed" || metadata.conclusion !== "success") fail("candidate run is not successful and complete");
  if (!/^[0-9a-f]{40}$/.test(metadata.head_sha || "")) fail("invalid candidate run commit");
  if (!Number.isSafeInteger(metadata.run_attempt) || metadata.run_attempt < 1) fail("invalid run attempt");
  if (!["push", "workflow_dispatch"].includes(metadata.event)) fail("candidate run event is not a release event");
  if (!Array.isArray(jobsResponse.jobs) || jobsResponse.total_count > jobsResponse.jobs.length) fail("source job list incomplete");
  for (const name of REQUIRED_JOBS) {
    const matches = jobsResponse.jobs.filter((job) => job.name === name);
    if (matches.length !== 1 || matches[0].conclusion !== "success") fail(`source release job did not succeed exactly once: ${name}`);
    if (Number.isInteger(matches[0].run_attempt) && matches[0].run_attempt !== metadata.run_attempt) {
      fail(`source job from another run attempt: ${name}`);
    }
  }
  if (!Array.isArray(artifactsResponse.artifacts) || artifactsResponse.total_count > artifactsResponse.artifacts.length) {
    fail("candidate artifact list incomplete");
  }
  const candidates = artifactsResponse.artifacts.filter((a) => a.name === "release-candidate");
  if (candidates.length !== 1 || candidates[0].expired || !Number.isSafeInteger(candidates[0].size_in_bytes) ||
      candidates[0].size_in_bytes < 1) fail("missing, duplicated, empty or expired release-candidate artifact");
  return { commit: metadata.head_sha, repo: expected.repository, runId: String(metadata.id), attempt: String(metadata.run_attempt) };
}
function main(argv) {
  try {
    if (argv.length !== 6) fail("usage: verify-release-run.js <run.json> <jobs.json> <artifacts.json> <expected-run> <repo> <GITHUB_OUTPUT>");
    const [run, jobs, artifacts, id, repo, output] = argv;
    const result = validate(JSON.parse(fs.readFileSync(run)), JSON.parse(fs.readFileSync(jobs)), JSON.parse(fs.readFileSync(artifacts)), {runId:id,repository:repo});
    fs.appendFileSync(output, Object.entries(result).map(([k,v]) => `${k}=${v}\n`).join(""));
    console.log(`validated source run ${result.runId} / attempt ${result.attempt} / ${result.commit}`);
    return 0;
  } catch (err) { console.error(`release run verification: ${err.message}`); return 1; }
}
if (require.main === module) process.exitCode = main(process.argv.slice(2));
module.exports = { validate, REQUIRED_JOBS };
