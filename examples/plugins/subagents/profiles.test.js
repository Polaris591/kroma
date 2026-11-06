"use strict";
const {test} = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const {parseProfile, loadProfiles} = require("./profiles");

function profile(name, description = "A useful profile") {
  return `---\nname: ${name}\ndescription: ${description}\nmodel: light\n` +
    `shell: deny\ntools: [shell]\nmaxTurns: 20\nmaxTokens: 500\n---\nRead code only.\n`;
}

test("built-in profiles load without user setup and expose valid limits", () => {
  const found = loadProfiles(process.cwd(), {user: "/path/that/does/not/exist", project: "/path/that/does/not/exist"});
  assert.deepEqual([...found.keys()].sort(), ["explore", "general", "planner", "reviewer", "verifier"]);
  assert.equal(found.get("reviewer").shell, "allow");
  assert.equal(found.get("verifier").maxTurns, 30);
  assert.equal(found.get("general").tools, null);
});

test("profile layers use project over user over bundled, without merging policies", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-profile-scope-"));
  try {
    for (const folder of ["builtin", "user", "project"]) fs.mkdirSync(path.join(dir, folder));
    fs.writeFileSync(path.join(dir, "builtin", "custom.md"), profile("custom", "builtin profile"));
    fs.writeFileSync(path.join(dir, "user", "custom.md"), profile("custom", "user profile"));
    fs.writeFileSync(path.join(dir, "project", "custom.md"), profile("custom", "project profile"));
    const profiles = loadProfiles(dir, {
      builtins: path.join(dir, "builtin"), user: path.join(dir, "user"), project: path.join(dir, "project"),
    });
    assert.equal(profiles.get("custom").description, "project profile");
    assert.equal(profiles.get("custom").shell, "deny");
    assert.equal(profiles.size, 1);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});

test("invalid profiles and duplicate names cannot silently widen child permissions", () => {
  for (const malformed of [
    profile("test").replace("maxTurns: 20", "maxTurns: 0"),
    profile("test").replace("model: light", "model: light\nthinking: turbo"),
    profile("test").replace("shell: deny", "shell: unrestricted"),
    profile("test").replace("tools: [shell]", "tools: [shell, shell]"),
    profile("test").replace("maxTokens: 500", "hooks: everything"),
    profile("test").replace("name: test", "name: ../test"),
    profile("test").replace("Read code only.", ""),
    "---\nname: test\ndescription: valid\nname: duplicate\n---\ntext",
  ]) assert.throws(() => parseProfile(malformed, "broken.md"));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-profile-dupe-"));
  try {
    fs.writeFileSync(path.join(dir, "one.md"), profile("same"));
    fs.writeFileSync(path.join(dir, "two.md"), profile("same"));
    assert.throws(() => loadProfiles(dir, {builtins: dir, user: "/nonexistent", project: "/nonexistent"}),
      /duplicate profile/);
  } finally {
    fs.rmSync(dir, {recursive: true, force: true});
  }
});
