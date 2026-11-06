"use strict";

// Profiles are data, not executable JavaScript. Keep the format small enough
// to audit without a YAML runtime or a second agent orchestration framework.
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const FIELDS = new Set(["name", "description", "model", "thinking", "shell", "tools",
  "disallowedTools", "maxTurns", "maxTokens"]);
const MODEL = new Set(["auto", "inherit", "light", "medium", "heavy"]);
const SHELL = new Set(["inherit", "read-only", "test", "deny"]);
const NAME = /^[a-z0-9][a-z0-9_-]{0,63}$/;

function error(message) {
  const err = new Error(message);
  err.code = "PROFILE_INVALID";
  return err;
}

function parseList(value, file, field) {
  if (!value.startsWith("[") || !value.endsWith("]")) {
    throw error(`${file}: ${field} must be an inline array`);
  }
  const inner = value.slice(1, -1).trim();
  if (!inner) return [];
  const items = inner.split(",").map(value => value.trim().replace(/^['"]|['"]$/g, ""));
  if (items.some(item => !/^[A-Za-z0-9_-]+$/.test(item)) || new Set(items).size !== items.length) {
    throw error(`${file}: invalid or duplicate ${field} entries`);
  }
  return items;
}

function parseProfile(source, file) {
  if (Buffer.byteLength(source, "utf8") > 40 * 1024) throw error(`${file}: profile too large`);
  const match = /^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]*)$/.exec(source);
  if (!match) throw error(`${file}: expected Markdown YAML frontmatter`);
  const fields = {};
  for (const line of match[1].split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const pair = /^([A-Za-z][A-Za-z0-9]*):\s*(.*?)\s*$/.exec(line);
    if (!pair || !FIELDS.has(pair[1]) || Object.hasOwn(fields, pair[1])) {
      throw error(`${file}: unknown, malformed or repeated profile field`);
    }
    fields[pair[1]] = pair[2].replace(/\s+#.*$/, "");
  }
  if (!NAME.test(fields.name ?? "")) throw error(`${file}: invalid profile name`);
  if (!fields.description || fields.description.length > 240) {
    throw error(`${file}: description must contain 1..240 characters`);
  }
  const model = fields.model ?? "inherit";
  const shell = fields.shell ?? "inherit";
  if (!MODEL.has(model) || !SHELL.has(shell)) throw error(`${file}: invalid model or shell policy`);
  const prompt = match[2].trim();
  if (!prompt || Buffer.byteLength(prompt, "utf8") > 32 * 1024) {
    throw error(`${file}: profile prompt must contain 1..32768 bytes`);
  }
  const number = (field, fallback, min, max) => {
    const value = fields[field];
    if (value === undefined) return fallback;
    if (!/^\d+$/.test(value) || Number(value) < min || Number(value) > max) {
      throw error(`${file}: ${field} must be in ${min}..${max}`);
    }
    return Number(value);
  };
  return {
    name: fields.name, description: fields.description, model, shell,
    thinking: fields.thinking || null,
    tools: fields.tools === undefined ? null : parseList(fields.tools, file, "tools"),
    disallowedTools: fields.disallowedTools === undefined ? [] :
      parseList(fields.disallowedTools, file, "disallowedTools"),
    maxTurns: number("maxTurns", 40, 1, 100),
    maxTokens: number("maxTokens", 0, 0, 1_000_000), prompt,
  };
}

function loadProfiles(workdir, options = {}) {
  const sources = [
    options.builtins ?? path.join(__dirname, "profiles"),
    options.user ?? path.join(os.homedir(), ".kroma", "agents"),
    options.project ?? path.join(workdir, ".kroma", "agents"),
  ];
  const loaded = new Map();
  for (const folder of sources) {
    if (!fs.existsSync(folder)) continue;
    const names = new Set();
    for (const filename of fs.readdirSync(folder).filter(name => name.endsWith(".md")).sort()) {
      const file = path.join(folder, filename);
      if (!fs.statSync(file).isFile()) throw error(`${file}: expected a regular file`);
      const profile = parseProfile(fs.readFileSync(file, "utf8"), file);
      if (names.has(profile.name)) throw error(`${folder}: duplicate profile ${profile.name}`);
      names.add(profile.name);
      loaded.set(profile.name, profile);
    }
  }
  return loaded;
}

module.exports = {parseProfile, loadProfiles};
