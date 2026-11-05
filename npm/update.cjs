"use strict";
const fs = require("node:fs");
const path = require("node:path");
const {spawnSync} = require("node:child_process");
const REGISTRY = "https://registry.npmjs.org";
const HELP = "Usage: kroma update [--check] [--channel latest|next]\nNo startup checks. Close other Kroma sessions before upgrading. Uses npm, never sudo.";

function version(value) {
  const match = typeof value === "string" && /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/.exec(value);
  if (!match || value.length > 256) throw new Error("Invalid release version");
  const core = match.slice(1, 4).map(BigInt), pre = match[4]?.split(".");
  if (pre?.some(s => /^\d+$/.test(s) && s.length > 1 && s[0] === "0")) throw new Error("Invalid prerelease version");
  return {core, pre};
}
function compare(a, b) {
  a = version(a); b = version(b);
  const cmp = (x, y) => x < y ? -1 : x > y ? 1 : 0;
  for (let i = 0; i < 3; i++) { const n = cmp(a.core[i], b.core[i]); if (n) return n; }
  if (!a.pre || !b.pre) return a.pre ? -1 : b.pre ? 1 : 0;
  for (let i = 0; i < Math.max(a.pre.length, b.pre.length); i++) {
    if (a.pre[i] === undefined) return -1;
    if (b.pre[i] === undefined) return 1;
    const x = a.pre[i], y = b.pre[i], xn = /^\d+$/.test(x), yn = /^\d+$/.test(y);
    const n = xn && yn ? cmp(BigInt(x), BigInt(y)) : xn !== yn ? (xn ? -1 : 1) : cmp(x, y);
    if (n) return n;
  }
  return 0;
}

// Only recognize the update command, never prompt text or flags after `--`.
function updateArgs(args) {
  const flags = new Set(["--quiet", "-q", "--verbose", "-v", "--no-color", "--no-mcp"]);
  const values = new Set(["--cwd", "--config-file", "--language", "--mcp"]);
  const kept = [];
  for (let i = 0; i < args.length; i++) {
    const arg = args[i], key = arg.split("=", 1)[0];
    if (arg === "--") { kept.push(...args.slice(i)); break; }
    if (flags.has(arg)) continue;
    if (values.has(key)) {
      const value = arg.includes("=") ? arg.slice(key.length + 1) : args[++i];
      if (!value || value.startsWith("-")) return null;
      if (key === "--language" && !["en", "zh"].includes(value)) return null;
      continue;
    }
    kept.push(arg);
    if (kept[0] !== "update") return null;
  }
  return kept[0] === "update" ? kept.slice(1) : null;
}
function parse(args) {
  let check = false, channel;
  for (let i = 0; i < args.length; i++) {
    const arg = args[i];
    if (arg === "--help" || arg === "-h") return {help: true};
    if (arg === "--check" && !check) { check = true; continue; }
    if (channel === undefined && (arg === "--channel" || arg.startsWith("--channel="))) {
      channel = arg === "--channel" ? args[++i] : arg.slice(10);
      if (!["latest", "next"].includes(channel)) throw new Error(HELP);
      continue;
    }
    throw new Error(HELP);
  }
  return {check, channel: channel || "latest"};
}

async function published(channel, fetchImpl = fetch) {
  const response = await fetchImpl(`${REGISTRY}/kroma/${channel}`, {signal: AbortSignal.timeout(10000), redirect: "error", headers: {accept: "application/json"}});
  if (response.status === 404) throw new Error(`No published kroma version on npm channel '${channel}'. Nothing was changed.`);
  if (response.status !== 200) throw new Error(`npm version check returned ${response.status}. Nothing was changed.`);
  const reader = response.body.getReader();
  const parts = []; let size = 0;
  try {
    while (true) {
      const {done, value} = await reader.read(); if (done) break;
      size += value.length;
      if (size > 1024 * 1024) { await reader.cancel(); throw new Error("npm metadata exceeds the size limit"); }
      parts.push(Buffer.from(value));
    }
  } finally { reader.releaseLock(); }
  const data = JSON.parse(Buffer.concat(parts).toString("utf8"));
  const parsed = version(data.version);
  if (data.name !== "kroma" || (channel === "latest" && parsed.pre)) throw new Error("Unexpected npm release metadata");
  return data.version;
}

function npmCli() {
  const candidates = [process.env.npm_execpath, path.join(path.dirname(process.execPath), "node_modules/npm/bin/npm-cli.js")];
  for (const dir of (process.env.PATH || "").split(path.delimiter)) {
    candidates.push(path.join(dir, "npm"), path.join(dir, "node_modules/npm/bin/npm-cli.js"));
  }
  for (const file of candidates.filter(Boolean)) {
    try { const real = fs.realpathSync(file); if (path.basename(real) === "npm-cli.js") return real; } catch {}
  }
  throw new Error("Cannot locate npm-cli.js. Use your original package manager to upgrade Kroma.");
}
const canonical = file => { try { return fs.realpathSync(file); } catch { return path.resolve(file); } };
function installTarget(root, cli, execute = spawnSync) {
  root = canonical(root);
  const result = execute(process.execPath, [cli, "prefix", "--global"], {encoding: "utf8", shell: false});
  if (result.error || result.status !== 0) throw new Error("Cannot determine npm's global prefix; no installation was changed.");
  const prefix = result.stdout.trim();
  if (!path.isAbsolute(prefix)) throw new Error("Invalid npm global prefix");
  const globalRoot = path.join(prefix, process.platform === "win32" ? "node_modules" : "lib/node_modules", "kroma");
  if (root === canonical(globalRoot)) return {prefix, global: true};
  const project = path.dirname(path.dirname(root));
  if (path.basename(path.dirname(root)) !== "node_modules" || path.basename(root) !== "kroma") {
    throw new Error("Linked or non-npm installation; use the original package manager to upgrade.");
  }
  let manifest;
  try { manifest = JSON.parse(fs.readFileSync(path.join(project, "package.json"), "utf8")); }
  catch { throw new Error("Ambiguous local installation; use npm install kroma@latest in its project."); }
  if ((manifest.packageManager && !manifest.packageManager.startsWith("npm@")) ||
      ["yarn.lock", "pnpm-lock.yaml", "bun.lock", "bun.lockb"].some(file => fs.existsSync(path.join(project, file)))) {
    throw new Error("This project uses another package manager; upgrade Kroma with that manager.");
  }
  if (![manifest.dependencies, manifest.devDependencies, manifest.optionalDependencies].some(deps => typeof deps?.kroma === "string")) {
    throw new Error("Kroma is not a direct project dependency; update it with the owning package manager.");
  }
  return {prefix: project, global: false};
}

async function runUpdate(args, {root = __dirname, fetchImpl = fetch, execute = spawnSync, cli, log = console.log} = {}) {
  let options;
  try { options = parse(args); } catch (error) { error.exitCode = 2; throw error; }
  if (options.help) { log(HELP); return; }
  const manifest = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
  if (manifest.name !== "kroma") throw new Error("Not a packaged Kroma installation");
  version(manifest.version);
  const latest = await published(options.channel, fetchImpl);
  log(`Installed: ${manifest.version}; npm ${options.channel}: ${latest}`);
  if (compare(latest, manifest.version) <= 0) { log("No newer version on this channel. Nothing was changed."); return; }
  if (options.check) { log("An update is available. Run kroma update to install it."); return; }
  cli ||= npmCli();
  const target = installTarget(root, cli, execute);
  log(`Updating ${target.global ? "global" : "project-local"} Kroma at ${target.prefix}. Local updates may modify package.json and its lockfile.`);
  log("Close other Kroma sessions first. No sessions are killed and no sudo is used.");
  const install = [cli, "install", ...(target.global ? ["--global"] : []), "--prefix", target.prefix,
    `kroma@${latest}`, `--registry=${REGISTRY}`, "--include=optional", "--ignore-scripts", "--no-audit", "--no-fund"];
  const result = execute(process.execPath, install, {stdio: "inherit", shell: false});
  if (result.error || result.status !== 0) {
    throw new Error("npm upgrade failed. Close running Kroma sessions (especially on Windows), inspect npm's output and reinstall with the original package manager if needed.");
  }
  try {
    const installed = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
    const {forHost, packageName} = require("./platforms.cjs");
    const name = packageName(forHost());
    const native = JSON.parse(fs.readFileSync(require.resolve(name + "/package.json", {paths: [root]}), "utf8"));
    if (installed.name !== "kroma" || installed.version !== latest || native.name !== name || native.version !== latest) throw new Error("version mismatch");
  } catch {
    throw new Error("npm finished but the expected main/native packages were not verified. Reinstall with the original package manager; the installation may be incomplete.");
  }
  log(`Kroma ${latest} installed. Restart Kroma to use the new version.`);
}
module.exports = {compare, version, parse, updateArgs, published, installTarget, runUpdate};
