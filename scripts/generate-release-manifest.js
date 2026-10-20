#!/usr/bin/env node
"use strict";

/**
 * Offline / CI Release Manifest Generation Tool for Kroma (B14 / F13).
 *
 * Uses the exact canonical payload generation and signing functions from `bin/kroma.js`
 * to guarantee zero drift between generation and client verification.
 *
 * Security Requirements:
 * - Private key is strictly loaded from a protected file path (--private-key-file);
 *   never passed via argv interpolation, environment variables, or logged to stdout/stderr.
 * - Tarball hash and length are computed via bounded streaming (never buffer large archives into memory).
 * - All schema fields (target, version, URL, key-id, expires) are strictly validated.
 * - Atomic output write: checks that destination is not a symlink, writes to temporary sibling,
 *   and atomically renames with 0o644 permissions.
 */

const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");

const {
  ALLOWED_TARGETS,
  MAX_TARBALL_BYTES,
  isValidHttpsTarballUrl,
  parseSemver,
  buildCanonicalSigningPayload,
  serializeManifest
} = require(path.join(__dirname, "..", "bin", "kroma.js"));

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg.startsWith("--")) {
      const key = arg.slice(2);
      const next = argv[i + 1];
      if (next && !next.startsWith("--")) {
        args[key] = next;
        i++;
      } else {
        args[key] = true;
      }
    }
  }
  return args;
}

function streamHashAndLength(filePath, maxBytes = MAX_TARBALL_BYTES) {
  return new Promise((resolve, reject) => {
    if (!fs.existsSync(filePath)) {
      return reject(new Error(`Tarball file does not exist: ${filePath}`));
    }
    const stat = fs.lstatSync(filePath);
    if (stat.isSymbolicLink() || !stat.isFile()) {
      return reject(new Error(`Tarball is not a regular file: ${filePath}`));
    }
    if (stat.size <= 0 || stat.size > maxBytes) {
      return reject(new Error(`Tarball size ${stat.size} exceeds allowed bounds (1..${maxBytes})`));
    }

    const hash = crypto.createHash("sha256");
    let totalBytes = 0;
    const stream = fs.createReadStream(filePath);

    stream.on("data", (chunk) => {
      totalBytes += chunk.length;
      if (totalBytes > maxBytes) {
        stream.destroy();
        return reject(new Error(`Tarball exceeded maximum allowed bytes: ${maxBytes}`));
      }
      hash.update(chunk);
    });

    stream.on("end", () => {
      if (totalBytes !== stat.size) {
        return reject(new Error(`Streamed byte count (${totalBytes}) does not match stat size (${stat.size})`));
      }
      resolve({
        sha256: hash.digest("hex").toLowerCase(),
        length: totalBytes
      });
    });

    stream.on("error", reject);
  });
}

function loadPrivateKey(keyFilePath) {
  if (typeof keyFilePath !== "string" || !keyFilePath) {
    throw new Error("Missing required --private-key-file argument");
  }
  if (!fs.existsSync(keyFilePath)) {
    throw new Error(`Private key file not found: ${keyFilePath}`);
  }
  const stat = fs.lstatSync(keyFilePath);
  if (stat.isSymbolicLink() || !stat.isFile()) {
    throw new Error(`Private key file must be a regular file: ${keyFilePath}`);
  }
  const keyContent = fs.readFileSync(keyFilePath);
  let privateKeyObj;
  try {
    privateKeyObj = crypto.createPrivateKey(keyContent);
  } catch (err) {
    throw new Error(`Failed to parse Ed25519 private key: ${err.message}`);
  }
  if (privateKeyObj.asymmetricKeyType !== "ed25519") {
    throw new Error(`Key must be an Ed25519 private key, found: ${privateKeyObj.asymmetricKeyType}`);
  }
  return privateKeyObj;
}

async function generateManifest(options) {
  const {
    target,
    version,
    tarball,
    url,
    keyId,
    privateKeyFile,
    antirollback,
    ttlDays,
    out
  } = options;

  if (!target || !ALLOWED_TARGETS.has(target)) {
    throw new Error(`Invalid or unsupported --target: ${target} (allowed: ${Array.from(ALLOWED_TARGETS).join(", ")})`);
  }

  if (!version || !parseSemver(version)) {
    throw new Error(`Invalid SemVer --version: ${version}`);
  }

  if (!url || !isValidHttpsTarballUrl(url)) {
    throw new Error(`Invalid --url: must be HTTPS on allow-listed host`);
  }

  if (!keyId || !/^[A-Za-z0-9_-]{1,64}$/.test(keyId)) {
    throw new Error(`Invalid --key-id: must be 1-64 alphanumeric/hyphen/underscore characters`);
  }

  const antirollbackVer = antirollback || version;
  if (!parseSemver(antirollbackVer)) {
    throw new Error(`Invalid SemVer --antirollback: ${antirollbackVer}`);
  }

  const days = ttlDays !== undefined ? parseInt(ttlDays, 10) : 30;
  if (!Number.isSafeInteger(days) || days <= 0 || days > 365) {
    throw new Error(`Invalid --ttl-days: must be safe integer between 1 and 365`);
  }

  const currentTime = Math.floor(Date.now() / 1000);
  const expires = currentTime + days * 86400;

  // Stream tarball hash and length safely
  const { sha256, length } = await streamHashAndLength(tarball);

  // Load private key without logging or exposing content
  const privateKey = loadPrivateKey(privateKeyFile);

  const fields = {
    schemaVersion: 1,
    keyId,
    version,
    target,
    tarballUrl: url,
    sha256,
    length,
    expires,
    antirollback: antirollbackVer
  };

  const payload = buildCanonicalSigningPayload(fields);
  const signature = crypto.sign(null, payload, privateKey);
  const signatureBase64 = signature.toString("base64");

  const manifest = {
    ...fields,
    signature: signatureBase64
  };

  const jsonStr = serializeManifest(manifest);

  if (out) {
    const outResolved = path.resolve(out);
    if (fs.existsSync(outResolved)) {
      const outStat = fs.lstatSync(outResolved);
      if (outStat.isSymbolicLink()) {
        throw new Error(`Output file path is a symbolic link: ${outResolved}`);
      }
    }
    const outDir = path.dirname(outResolved);
    fs.mkdirSync(outDir, { recursive: true });

    // Atomic write to sibling temp file
    const tmpOut = `${outResolved}.tmp.${process.pid}.${Date.now()}`;
    try {
      fs.writeFileSync(tmpOut, jsonStr, { encoding: "utf8", mode: 0o644 });
      fs.renameSync(tmpOut, outResolved);
    } catch (err) {
      try { fs.unlinkSync(tmpOut); } catch (_) {}
      throw err;
    }
    console.log(`[manifest-gen] ✓ Signed release manifest generated: ${outResolved}`);
  }

  return manifest;
}

module.exports = {
  generateManifest,
  streamHashAndLength,
  loadPrivateKey
};

if (require.main === module) {
  const args = parseArgs(process.argv.slice(2));
  if (args.help || !args.target || !args.version || !args.tarball || !args.url || !args["key-id"] || !args["private-key-file"]) {
    console.log("Usage: node scripts/generate-release-manifest.js \\");
    console.log("  --target <target> \\");
    console.log("  --version <version> \\");
    console.log("  --tarball <path-to-archive.tgz> \\");
    console.log("  --url <https://tarball-url> \\");
    console.log("  --key-id <key-id> \\");
    console.log("  --private-key-file <path-to-private-key.pem> \\");
    console.log("  [--antirollback <version>] \\");
    console.log("  [--ttl-days <days>] \\");
    console.log("  [--out <output-path.json>]");
    process.exit(args.help ? 0 : 1);
  }

  generateManifest({
    target: args.target,
    version: args.version,
    tarball: args.tarball,
    url: args.url,
    keyId: args["key-id"],
    privateKeyFile: args["private-key-file"],
    antirollback: args.antirollback,
    ttlDays: args["ttl-days"],
    out: args.out
  }).then((manifest) => {
    if (!args.out) {
      console.log(serializeManifest(manifest));
    }
  }).catch((err) => {
    console.error(`[manifest-gen] ✗ Error: ${err.message}`);
    process.exit(1);
  });
}
