#!/usr/bin/env node
"use strict";

const { spawnSync } = require("node:child_process");
const path = require("node:path");
const fs = require("node:fs");

const PLATFORM_MAP = {
  "linux:x64": {
    pkg: "@kroma-ai/cli-linux-x64",
    bin: "kroma"
  },
  "win32:x64": {
    pkg: "@kroma-ai/cli-win32-x64",
    bin: "kroma.exe"
  },
  "darwin:arm64": {
    pkg: "@kroma-ai/cli-darwin-arm64",
    bin: "kroma"
  },
  "darwin:x64": {
    pkg: "@kroma-ai/cli-darwin-x64",
    bin: "kroma"
  }
};

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

const binPath = resolveBinaryPath();
const result = spawnSync(binPath, process.argv.slice(2), {
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
