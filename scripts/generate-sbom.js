#!/usr/bin/env node
"use strict";

/**
 * T22 (KR-28): SBOM / dependency manifest for a release.
 *
 * Generated from `Cargo.lock` rather than `cargo metadata`, on purpose: the lockfile *is* the
 * resolved dependency graph (name, version, source, checksum), it is already in the repository, and
 * it needs no registry access — so the SBOM can be recomputed from a checkout with no network and
 * compared byte for byte. `cargo metadata` would additionally need every crate downloaded.
 *
 * Determinism: components are sorted by `(name, version)`, no timestamps, no absolute paths. The
 * output carries the sha256 of the `Cargo.lock` it was generated from and the sha256 of the SBOM
 * itself, so a release record can bind the three together.
 *
 * Usage: node scripts/generate-sbom.js [--out dist/sbom.json]
 */

const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

const ROOT = path.join(__dirname, "..");

/** Minimal `[[package]]` reader: Cargo.lock is a TOML subset with flat string keys. */
function parseLockfile(text) {
  const packages = [];
  let current = null;
  for (const rawLine of text.split("\n")) {
    const line = rawLine.trim();
    if (line === "[[package]]") {
      if (current) packages.push(current);
      current = {};
      continue;
    }
    if (!current) continue;
    const match = /^([A-Za-z0-9_]+)\s*=\s*"([^"]*)"$/.exec(line);
    if (match) {
      current[match[1]] = match[2];
      continue;
    }
    if (line.startsWith("[") && line.endsWith("]")) {
      // A nested table (e.g. [package.dependencies]) ends the flat fields we care about.
      continue;
    }
  }
  if (current) packages.push(current);
  return packages;
}

function sha256(text) {
  return crypto.createHash("sha256").update(text).digest("hex");
}

function buildSbom({ lockText, packageJsonText, toolchain }) {
  const packages = parseLockfile(lockText);
  if (packages.length === 0) {
    throw new Error("Cargo.lock contains no [[package]] entries");
  }
  const components = packages
    .map((pkg) => {
      if (!pkg.name || !pkg.version) {
        throw new Error(`a Cargo.lock package has no name or version: ${JSON.stringify(pkg)}`);
      }
      const component = {
        name: pkg.name,
        version: pkg.version,
        kind: pkg.source ? "registry-or-git" : "workspace"
      };
      if (pkg.source) component.source = pkg.source;
      // Registry crates must carry a checksum: without it the component is not pinned.
      if (pkg.checksum) component.checksum = pkg.checksum;
      else if (pkg.source && /^registry\+/.test(pkg.source)) {
        throw new Error(`${pkg.name} ${pkg.version} is a registry crate with no checksum`);
      }
      return component;
    })
    .sort((a, b) => (a.name === b.name ? a.version.localeCompare(b.version) : a.name.localeCompare(b.name)));

  const packageJson = JSON.parse(packageJsonText);
  const sbom = {
    schemaVersion: 1,
    // The product identity, not the toolchain identity: they are separate facts.
    product: { name: packageJson.name, version: packageJson.version },
    toolchain: { channel: toolchain },
    lockfileSha256: sha256(lockText),
    componentCount: components.length,
    components
  };
  sbom.sbomSha256 = sha256(JSON.stringify(sbom));
  return sbom;
}

function main(argv) {
  const outIndex = argv.indexOf("--out");
  const outPath = outIndex > 0 ? argv[outIndex + 1] : path.join(ROOT, "dist", "sbom.json");
  const lockText = fs.readFileSync(path.join(ROOT, "Cargo.lock"), "utf8");
  const packageJsonText = fs.readFileSync(path.join(ROOT, "package.json"), "utf8");
  const toolchainMatch = /channel\s*=\s*"([^"]+)"/.exec(
    fs.readFileSync(path.join(ROOT, "rust-toolchain.toml"), "utf8")
  );
  const sbom = buildSbom({
    lockText,
    packageJsonText,
    toolchain: toolchainMatch ? toolchainMatch[1] : "unknown"
  });
  fs.mkdirSync(path.dirname(outPath), { recursive: true });
  fs.writeFileSync(outPath, `${JSON.stringify(sbom, null, 2)}\n`);
  console.log(
    `sbom: ${sbom.componentCount} components, lockfile ${sbom.lockfileSha256.slice(0, 16)}, sbom ${sbom.sbomSha256.slice(0, 16)} -> ${outPath}`
  );
  return 0;
}

module.exports = { parseLockfile, buildSbom, sha256 };

if (require.main === module) {
  try {
    process.exitCode = main(process.argv);
  } catch (err) {
    console.error(`error: ${err.message}`);
    process.exitCode = 1;
  }
}
