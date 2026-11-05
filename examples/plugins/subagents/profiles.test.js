"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const {parseProfile, loadProfiles} = require("./profiles");

const valid = (name, body = "Inspect the code and report evidence.") => `---\nname: ${name}\ndescription: Profile ${name}\nmodel: auto\nshell: read-only\ntools: [shell]\nmaxTurns: 15\nmaxTokens: 100000\n---\n${body}\n`;

test("builtin profiles load with strict bounded fields", () => {
  const profiles = loadProfiles(os.tmpdir(), {user: path.join(os.tmpdir(), "kroma-test-no-user-profiles"),
    project: path.join(os.tmpdir(), "kroma-test-no-project-profiles")});
  for (const name of ["general", "explore", "planner", "reviewer", "verifier"]) {
    assert.ok(profiles.has(name), name);
    const p = profiles.get(name);
    assert.ok(p.prompt);
    assert.ok(p.maxTurns > 0);
  }
});

test("rejects unknown keys, duplicate keys, duplicate tools and invalid limits", () => {
  assert.throws(() => parseProfile(valid("reviewer").replace("model: auto", "model: impossible"), "bad"),
    /model or shell/);
  assert.throws(() => parseProfile(valid("reviewer").replace("model: auto", "thinking: turbo"), "bad"),
    /thinking tier/);
  assert.throws(() => parseProfile(valid("reviewer").replace("maxTurns: 15", "maxTurns: 0"), "bad"),
    /maxTurns/);
  assert.throws(() => parseProfile(valid("reviewer").replace("tools: [shell]", "tools: [shell, shell]"), "bad"),
    /duplicate/);
  assert.throws(() => parseProfile(valid("reviewer").replace("model: auto", "model: auto\nunsafe: yes"), "bad"),
    /unknown/);
  assert.throws(() => parseProfile(valid("reviewer").replace("model: auto", "model: auto\nmodel: heavy"), "bad"),
    /repeated/);
});

test("project scope overrides user and builtins, duplicates in one scope fail", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-profiles-"));
  try {
    const builtins = path.join(root, "builtins"), user = path.join(root, "user");
    const project = path.join(root, "project");
    for (const dir of [builtins, user, project]) fs.mkdirSync(dir);
    fs.writeFileSync(path.join(builtins, "general.md"), valid("general", "builtin"));
    fs.writeFileSync(path.join(user, "general.md"), valid("general", "user"));
    fs.writeFileSync(path.join(project, "general.md"), valid("general", "project"));
    assert.equal(loadProfiles(root, {builtins, user, project}).get("general").prompt, "project");
    fs.writeFileSync(path.join(project, "duplicate.md"), valid("general", "other"));
    assert.throws(() => loadProfiles(root, {builtins, user, project}), /duplicate profile/);
  } finally {
    fs.rmSync(root, {recursive: true, force: true});
  }
});
