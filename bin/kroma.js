#!/usr/bin/env node
"use strict";

const { spawnSync } = require("node:child_process");
const https = require("node:https");
const path = require("node:path");
const os = require("node:os");
const fs = require("node:fs");
const crypto = require("node:crypto");

const pkgJson = require("../package.json");
const MIRROR_TARBALL_URL = "https://cobblemon.asia/kroma.tgz";
const MIRROR_VERSION_URL = "https://cobblemon.asia/kroma-version.json";
// Hosts a tarball install target is ever allowed to point at. A mirror response is untrusted
// input: without this allow-list a compromised/MITM'd response could redirect the install to an
// attacker-controlled archive.
const ALLOWED_TARBALL_HOSTS = new Set(["cobblemon.asia"]);
const TARBALL_PATH_RE = /^\/[A-Za-z0-9._~/%+-]*$/;

// ---------------------------------------------------------------------------------------------
// Pure validators (exported so `scripts/test-kroma-js-upgrade.js` can exercise them directly).
// ---------------------------------------------------------------------------------------------

/**
 * Validate an https tarball URL before it is ever handed to a package manager.
 *
 * Rejects anything that is not `https:`, not on {@link ALLOWED_TARBALL_HOSTS}, carries credentials
 * or a non-default port, or contains whitespace/control characters. Together with the host and
 * path character allow-lists this guarantees the value can never be re-parsed as a second command
 * by `cmd.exe` (which is what `shell: true` would do on Windows).
 */
function isValidHttpsTarballUrl(raw) {
  if (typeof raw !== "string" || raw.length === 0 || raw.length > 2048) {
    return false;
  }
  if (/[\s\u0000-\u001f\u007f]/.test(raw)) {
    return false;
  }
  let url;
  try {
    url = new URL(raw);
  } catch (_) {
    return false;
  }
  if (url.protocol !== "https:") {
    return false;
  }
  if (url.username || url.password) {
    return false;
  }
  if (url.port) {
    return false;
  }
  if (!ALLOWED_TARBALL_HOSTS.has(url.hostname.toLowerCase())) {
    return false;
  }
  if (!TARBALL_PATH_RE.test(url.pathname)) {
    return false;
  }
  if (url.search || url.hash) {
    return false;
  }
  return true;
}

/**
 * A safe `npm install -g` target is either a validated mirror tarball URL or a plain npm package
 * spec (`name`, `@scope/name`, optionally `@version`/`@tag`). Everything else is refused.
 */
function isSafeInstallTarget(target) {
  if (typeof target !== "string" || target.length === 0 || target.length > 2048) {
    return false;
  }
  if (isValidHttpsTarballUrl(target)) {
    return true;
  }
  return /^(?:@[a-z0-9][a-z0-9._-]*\/)?[a-z0-9][a-z0-9._-]*(?:@[a-z0-9][a-z0-9._-]*)?$/i.test(
    target
  );
}

/** Parse `major.minor.patch` SemVer (an optional leading `v` is accepted); `null` otherwise. */
function parseSemver(raw) {
  if (typeof raw !== "string") {
    return null;
  }
  const match = /^v?(\d+)\.(\d+)\.(\d+)$/.exec(raw.trim());
  if (!match) {
    return null;
  }
  return [Number(match[1]), Number(match[2]), Number(match[3])];
}

/** Compare two SemVer strings: `-1`, `0`, `1`, or `null` when either side is not parseable. */
function compareSemver(a, b) {
  const left = parseSemver(a);
  const right = parseSemver(b);
  if (!left || !right) {
    return null;
  }
  for (let i = 0; i < 3; i++) {
    if (left[i] !== right[i]) {
      return left[i] < right[i] ? -1 : 1;
    }
  }
  return 0;
}

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

/** Pick the mirror install target, falling back to the pinned URL when metadata is unusable. */
function mirrorInstallTarget(mirrorMeta) {
  if (mirrorMeta && typeof mirrorMeta.tarball === "string" && isValidHttpsTarballUrl(mirrorMeta.tarball)) {
    return mirrorMeta.tarball;
  }
  return MIRROR_TARBALL_URL;
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
        installTarget = mirrorInstallTarget(mirrorMeta);
      }
    }
  } catch (_) {
    try {
      const mirrorMeta = await fetchJson(MIRROR_VERSION_URL);
      if (mirrorMeta && mirrorMeta.version) {
        latest = mirrorMeta.version;
        installTarget = mirrorInstallTarget(mirrorMeta);
      }
    } catch (_) {}
  }

  if (latest) {
    const cmp = compareSemver(latest, current);
    if (cmp === null) {
      // An unparseable version must never be guessed at: refusing is safer than a downgrade loop.
      console.error(
        `● upgrade  ✗ cannot compare local v${current} with remote v${latest}; refusing to install.`
      );
      process.exit(1);
    }
    if (cmp <= 0 && !force) {
      if (cmp === 0) {
        console.log(`● upgrade  ✓ kroma v${current} is already the latest version.`);
      } else {
        console.log(
          `● upgrade  ✓ local v${current} is newer than remote v${latest}; nothing to do.`
        );
      }
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

  if (!isSafeInstallTarget(installTarget)) {
    console.error(`● upgrade  ✗ refusing unsafe install target: ${installTarget}`);
    process.exit(1);
  }

  const npmCmd = process.platform === "win32" ? "npm.cmd" : "npm";
  const installArgs = ["install", "-g", installTarget];
  let res;
  if (process.platform === "win32") {
    // Avoid `shell: true`: it routes through `cmd.exe`, which re-parses the target and would turn a
    // tampered mirror response (`...tgz & calc.exe`) into arbitrary code execution. Spawn `cmd.exe`
    // directly with the already-validated target as a single argv entry instead.
    res = spawnSync(process.env.ComSpec || "cmd.exe", ["/d", "/c", npmCmd, ...installArgs], {
      stdio: "inherit",
      windowsHide: true
    });
  } else {
    res = spawnSync(npmCmd, installArgs, { stdio: "inherit" });
  }
  if (res.status !== 0) {
    process.exit(res.status ?? 1);
  }
  console.log(`● upgrade  ✓ kroma upgraded successfully.`);
  process.exit(0);
}

/** Best-effort synchronous read of the mirror's `sha256`, or `null` when it publishes none. */
function fetchMirrorSha256Sync() {
  const res = spawnSync("curl", ["-fsSL", "--retry", "2", MIRROR_VERSION_URL], {
    encoding: "utf8"
  });
  if (res.status !== 0 || !res.stdout) {
    return null;
  }
  try {
    const meta = JSON.parse(res.stdout);
    if (meta && typeof meta.sha256 === "string" && /^[a-f0-9]{64}$/i.test(meta.sha256)) {
      return meta.sha256.toLowerCase();
    }
  } catch (_) {}
  return null;
}

function sha256File(filePath) {
  return crypto.createHash("sha256").update(fs.readFileSync(filePath)).digest("hex");
}

function downloadBinaryIfNeeded(destPath, key) {
  if (fs.existsSync(destPath)) {
    return true;
  }
  console.log(`[kroma] Fetching native pre-built binary for ${key}...`);
  fs.mkdirSync(path.dirname(destPath), { recursive: true });
  if (!isValidHttpsTarballUrl(MIRROR_TARBALL_URL)) {
    console.error(`[kroma] refusing unsafe download URL: ${MIRROR_TARBALL_URL}`);
    return false;
  }
  // An exclusive temp directory: concurrent launches (or a pre-planted file) can no longer clobber
  // each other's download the way the old fixed `kroma-bundle.tgz` path allowed.
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-dl-"));
  const tmpTarball = path.join(tmpDir, "kroma-bundle.tgz");
  try {
    const tarRes = spawnSync("curl", ["-fSL", "--retry", "2", "-o", tmpTarball, MIRROR_TARBALL_URL], {
      stdio: "inherit"
    });
    if (tarRes.status !== 0) {
      return false;
    }
    // The published metadata carries no checksum today; when it does, verify it before extracting
    // so a tampered mirror response can never install an arbitrary binary.
    const expectedSha256 = fetchMirrorSha256Sync();
    if (expectedSha256 && sha256File(tmpTarball) !== expectedSha256) {
      console.error(`[kroma] checksum mismatch for ${MIRROR_TARBALL_URL}; refusing to extract.`);
      return false;
    }
    const subPath = `package/bin/vendor/${key}/${path.basename(destPath)}`;
    const untarRes = spawnSync("tar", ["-xzf", tmpTarball, "-C", path.dirname(destPath), "--strip-components=4", subPath], {
      stdio: "inherit"
    });
    if (untarRes.status === 0 && fs.existsSync(destPath)) {
      try { fs.chmodSync(destPath, 0o755); } catch (_) {}
      return true;
    }
    return false;
  } finally {
    try { fs.rmSync(tmpDir, { recursive: true, force: true }); } catch (_) {}
  }
}

function resolveBinaryPath() {
  const localBinName = process.platform === "win32" ? "kroma.exe" : "kroma";
  let platformKey = `${process.platform}-${process.arch}`;
  // Android Termux runs ARM64 Linux ELF binaries directly
  let bundled = path.join(__dirname, "vendor", platformKey, localBinName);
  if (!fs.existsSync(bundled) && process.platform === "android" && process.arch === "arm64") {
    const fallbackArm = path.join(__dirname, "vendor", "linux-arm64", localBinName);
    if (fs.existsSync(fallbackArm)) {
      return fallbackArm;
    }
  }
  if (fs.existsSync(bundled)) {
    return bundled;
  }
  if (downloadBinaryIfNeeded(bundled, platformKey)) {
    return bundled;
  }
  console.error(
    `[kroma] Native binary for ${process.platform}-${process.arch} was not found at ${bundled}.`
  );
  process.exit(1);
}

module.exports = {
  isValidHttpsTarballUrl,
  isSafeInstallTarget,
  parseSemver,
  compareSemver
};

if (require.main === module) {
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
}
