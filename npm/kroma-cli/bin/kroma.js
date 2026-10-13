#!/usr/bin/env node
"use strict";

const { spawnSync } = require("node:child_process");
const https = require("node:https");
const path = require("node:path");
const fs = require("node:fs");

const pkgJson = require("../package.json");

const PLATFORM_MAP = {
  "linux:x64": {
    pkg: "kroma-cli-linux-x64",
    bin: "kroma"
  },
  "win32:x64": {
    pkg: "kroma-cli-win32-x64",
    bin: "kroma.exe"
  },
  "darwin:arm64": {
    pkg: "kroma-cli-darwin-arm64",
    bin: "kroma"
  },
  "darwin:x64": {
    pkg: "kroma-cli-darwin-x64",
    bin: "kroma"
  }
};

function fetchLatestVersion(registryUrl) {
  return new Promise((resolve, reject) => {
    const url = `${registryUrl.replace(/\/+$/, "")}/kroma-cli/latest`;
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
          const parsed = JSON.parse(raw);
          resolve(parsed.version || null);
        } catch (err) {
          reject(err);
        }
      });
    });
    req.on("error", reject);
    req.on("timeout", () => {
      req.destroy(new Error("timeout"));
    });
  });
}

async function handleSelfUpgrade(args) {
  const checkOnly = args.includes("--check") || args.includes("-c");
  const force = args.includes("--force") || args.includes("-f");
  const registry = process.env.KROMA_NPM_REGISTRY || "https://registry.npmjs.org";
  const current = pkgJson.version;

  console.log(`● upgrade  checking ${registry}/kroma-cli/latest (current: v${current})...`);
  try {
    const latest = await fetchLatestVersion(registry);
    if (!latest) {
      console.log(`● upgrade  kroma-cli is not published on ${registry} yet (local v${current}).`);
      process.exit(0);
    }
    if (latest === current && !force) {
      console.log(`● upgrade  ✓ kroma v${current} is already the latest version.`);
      process.exit(0);
    }
    console.log(`● upgrade  new version available: v${current} → v${latest}`);
    if (checkOnly) {
      console.log(`  Run 'kroma upgrade' or 'npm install -g kroma-cli@latest' to update.`);
      process.exit(0);
    }
  } catch (err) {
    if (checkOnly) {
      console.error(`● upgrade  ✗ failed to query registry: ${err.message}`);
      process.exit(1);
    }
    console.log(`● upgrade  registry check skipped (${err.message}); running npm install -g kroma-cli@latest...`);
  }

  const npmCmd = process.platform === "win32" ? "npm.cmd" : "npm";
  const res = spawnSync(npmCmd, ["install", "-g", "kroma-cli@latest", `--registry=${registry}`], {
    stdio: "inherit",
    shell: process.platform === "win32"
  });
  if (res.status !== 0) {
    process.exit(res.status ?? 1);
  }
  console.log(`● upgrade  ✓ kroma-cli upgraded successfully.`);
  process.exit(0);
}

function resolveBinaryPath() {
  // 1. Check bundled binary inside bin/vendor/ (standalone offline tgz mode)
  const localBinName = process.platform === "win32" ? "kroma.exe" : "kroma";
  const bundled = path.join(__dirname, "vendor", `${process.platform}-${process.arch}`, localBinName);
  if (fs.existsSync(bundled)) {
    return bundled;
  }

  // 2. Check platform-specific optionalDependency package
  const key = `${process.platform}:${process.arch}`;
  const entry = PLATFORM_MAP[key];
  if (!entry) {
    console.error(`[kroma] Unsupported platform/architecture: ${key}`);
    process.exit(1);
  }

  try {
    const pkgJsonPath = require.resolve(`${entry.pkg}/package.json`);
    const candidate = path.join(path.dirname(pkgJsonPath), "bin", entry.bin);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  } catch (_) {
    // Fall through to sibling node_modules lookup
  }

  const siblingCandidate = path.resolve(__dirname, "..", "..", entry.pkg, "bin", entry.bin);
  if (fs.existsSync(siblingCandidate)) {
    return siblingCandidate;
  }

  console.error(
    `[kroma] Native binary for ${key} (${entry.pkg}) was not found.\n` +
    `Make sure optionalDependencies were not disabled (--no-optional) during npm install.`
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
