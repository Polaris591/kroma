#!/usr/bin/env node
"use strict";

const { spawnSync } = require("node:child_process");
const https = require("node:https");
const path = require("node:path");
const fs = require("node:fs");

const pkgJson = require("../package.json");
const MIRROR_TARBALL_URL = "https://cobblemon.asia/kroma.tgz";
const MIRROR_VERSION_URL = "https://cobblemon.asia/kroma-version.json";

function fetchJson(url) {
  return new Promise((resolve, reject) => {
    const req = https.get(url, { timeout: 8000 }, (res) => {
      if (res.statusCode === 404) {
        return resolve(null);
      }
      if (res.statusCode < 200 || res.statusCode >= 300) {
        return reject(new Error(`HTTP ${res.statusCode}`));
      }
      let raw = "";
      res.on("data", (chunk) => (raw += chunk));
      res.on("end", () => {
        try {
          resolve(JSON.parse(raw));
        } catch (err) {
          reject(err);
        }
      });
    });
    req.on("error", reject);
    req.on("timeout", () => req.destroy(new Error("timeout")));
  });
}

async function handleSelfUpgrade(args) {
  const checkOnly = args.includes("--check") || args.includes("-c");
  const force = args.includes("--force") || args.includes("-f");
  const registry = process.env.KROMA_NPM_REGISTRY || "https://registry.npmjs.org";
  const current = pkgJson.version;

  console.log(`● upgrade  checking latest Kroma release (current: v${current})...`);
  let installTarget = "kroma-cli@latest";
  let latest = null;

  try {
    const npmMeta = await fetchJson(`${registry.replace(/\/+$/, "")}/kroma-cli/latest`);
    if (npmMeta && npmMeta.version) {
      latest = npmMeta.version;
      installTarget = "kroma-cli@latest";
    } else {
      const mirrorMeta = await fetchJson(MIRROR_VERSION_URL);
      if (mirrorMeta && mirrorMeta.version) {
        latest = mirrorMeta.version;
        installTarget = mirrorMeta.tarball || MIRROR_TARBALL_URL;
      }
    }
  } catch (_) {
    try {
      const mirrorMeta = await fetchJson(MIRROR_VERSION_URL);
      if (mirrorMeta && mirrorMeta.version) {
        latest = mirrorMeta.version;
        installTarget = mirrorMeta.tarball || MIRROR_TARBALL_URL;
      }
    } catch (_) {}
  }

  if (latest) {
    if (latest === current && !force) {
      console.log(`● upgrade  ✓ kroma v${current} is already the latest version.`);
      process.exit(0);
    }
    console.log(`● upgrade  new version available: v${current} → v${latest}`);
    if (checkOnly) {
      console.log(`  Run 'kroma upgrade' or 'npm install -g ${installTarget}' to update.`);
      process.exit(0);
    }
  } else if (checkOnly) {
    console.log(`● upgrade  unable to reach release server (local v${current}).`);
    process.exit(1);
  } else {
    installTarget = MIRROR_TARBALL_URL;
  }

  const npmCmd = process.platform === "win32" ? "npm.cmd" : "npm";
  const res = spawnSync(npmCmd, ["install", "-g", installTarget], {
    stdio: "inherit",
    shell: process.platform === "win32"
  });
  if (res.status !== 0) {
    process.exit(res.status ?? 1);
  }
  console.log(`● upgrade  ✓ kroma upgraded successfully.`);
  process.exit(0);
}


function downloadBinaryIfNeeded(destPath, key) {
  if (fs.existsSync(destPath)) {
    return true;
  }
  console.log(`[kroma] Fetching native pre-built binary for ${key}...`);
  fs.mkdirSync(path.dirname(destPath), { recursive: true });
  const tmpTarball = path.join(path.dirname(destPath), "kroma-bundle.tgz");
  const tarRes = spawnSync("curl", ["-fSL", "--retry", "2", "-o", tmpTarball, MIRROR_TARBALL_URL], { stdio: "inherit" });
  if (tarRes.status !== 0) {
    return false;
  }
  const subPath = `package/bin/vendor/${key}/${path.basename(destPath)}`;
  const untarRes = spawnSync("tar", ["-xzf", tmpTarball, "-C", path.dirname(destPath), "--strip-components=4", subPath], { stdio: "inherit" });
  try { fs.unlinkSync(tmpTarball); } catch (_) {}
  if (untarRes.status === 0 && fs.existsSync(destPath)) {
    try { fs.chmodSync(destPath, 0o755); } catch (_) {}
    return true;
  }
  return false;
}

function resolveBinaryPath() {
  const localBinName = process.platform === "win32" ? "kroma.exe" : "kroma";
  const bundled = path.join(__dirname, "vendor", `${process.platform}-${process.arch}`, localBinName);
  if (fs.existsSync(bundled)) {
    return bundled;
  }
  const key = `${process.platform}-${process.arch}`;
  if (downloadBinaryIfNeeded(bundled, key)) {
    return bundled;
  }
  console.error(
    `[kroma] Native binary for ${process.platform}-${process.arch} was not found at ${bundled}.`
  );
  process.exit(1);
}

const userArgs = process.argv.slice(2);
if (userArgs[0] === "upgrade" || userArgs[0] === "self-update") {
  handleSelfUpgrade(userArgs.slice(1));
} else {
  const binPath = resolveBinaryPath();
  const result = spawnSync(binPath, userArgs, {
    stdio: "inherit",
    windowsHide: false
  });
  if (result.error) {
    console.error(`[kroma] Failed to launch native binary (${binPath}): ${result.error.message}`);
    process.exit(1);
  }
  if (result.signal) {
    process.kill(process.pid, result.signal);
  }
  process.exit(result.status ?? 0);
}
