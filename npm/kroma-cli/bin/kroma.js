#!/usr/bin/env node
"use strict";

const { spawnSync } = require("node:child_process");
const https = require("node:https");
const path = require("node:path");
const os = require("node:os");
const fs = require("node:fs");
const crypto = require("node:crypto");
const zlib = require("node:zlib");

const pkgJson = require("../package.json");

// ---------------------------------------------------------------------------------------------
// Constants and Trust Root Configuration
// ---------------------------------------------------------------------------------------------

/**
 * Production release trust root: a map of { [keyId: string]: string (Ed25519 public key) }.
 *
 * Configured with the release signing key declared in `docs/release/TRUST_ROOT.md`. When a manifest is
 * signed with any other key (including the test-channel key), verification refuses it by name. When the
 * map is empty, all online self-upgrade and on-demand binary downloads are strictly blocked
 * (fail-closed).
 *
 * Native execution of already-installed binaries and `kroma --version` are completely unaffected.
 */
const PRODUCTION_TRUSTED_KEYS = Object.freeze({
  "kroma-release-2026": "2cabb3d0cb06b03ffd478f37e07217e09286a8fa3b6a512c0b4288adc78114bc",
});

const MIRROR_MANIFEST_URL = "https://cobblemon.asia/kroma-manifest.json";
const MIRROR_TARBALL_URL = "https://cobblemon.asia/kroma.tgz";
const MIRROR_VERSION_URL = "https://cobblemon.asia/kroma-version.json";

// Hosts a tarball install target is ever allowed to point at. A mirror response is untrusted
// input: without this allow-list a compromised/MITM'd response could redirect the install to an
// attacker-controlled archive.
const ALLOWED_TARBALL_HOSTS = new Set(["cobblemon.asia"]);
const TARBALL_PATH_RE = /^\/[A-Za-z0-9._~/%+-]*$/;

// Canonical platform target identifiers supported by the release harness.
const ALLOWED_TARGETS = new Set([
  "cli",
  "linux-x64",
  "linux-arm64",
  "linux-arm",
  "darwin-x64",
  "darwin-arm64",
  "win32-x64",
  "win32-arm64",
  "android-arm64"
]);

const MAX_MANIFEST_BYTES = 65536; // 64 KB
const MAX_TARBALL_BYTES = 200 * 1024 * 1024; // 200 MB
const MAX_ARCHIVE_ENTRIES = 10000;
const MAX_UNCOMPRESSED_BYTES = 500 * 1024 * 1024; // 500 MB

const ED25519_SPKI_PREFIX = Buffer.from("302a300506032b6570032100", "hex");

const WINDOWS_RESERVED_NAMES = new Set([
  "con", "prn", "aux", "nul",
  "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
  "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9"
]);

/**
 * Derive target-specific release manifest URL.
 */
function getManifestUrl(target) {
  if (target && ALLOWED_TARGETS.has(target)) {
    return `https://cobblemon.asia/manifests/kroma-manifest-${target}.json`;
  }
  return MIRROR_MANIFEST_URL;
}

// ---------------------------------------------------------------------------------------------
// Pure validators (exported so JS test suites can exercise them directly).
// ---------------------------------------------------------------------------------------------

/**
 * Validate an https tarball URL before it is ever handed to a package manager or downloader.
 *
 * Rejects anything that is not `https:`, not on {@link ALLOWED_TARBALL_HOSTS}, carries credentials
 * or a non-default port, or contains whitespace/control characters.
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
 * A safe `npm install -g` target is either a validated mirror tarball URL, a validated local archive file,
 * or a plain npm package spec (`name`, `@scope/name`, optionally `@version`/`@tag`). Everything else is refused.
 */
function isSafeInstallTarget(target) {
  if (typeof target !== "string" || target.length === 0 || target.length > 2048) {
    return false;
  }
  if (isValidHttpsTarballUrl(target)) {
    return true;
  }
  if (path.isAbsolute(target) && (target.endsWith(".tgz") || target.endsWith(".tar.gz"))) {
    if (!/[\s\u0000-\u001f\u007f]/.test(target) && fs.existsSync(target) && fs.statSync(target).isFile()) {
      return true;
    }
  }
  return /^(?:@[a-z0-9][a-z0-9._-]*\/)?[a-z0-9][a-z0-9._-]*(?:@[a-z0-9][a-z0-9._-]*)?$/i.test(
    target
  );
}

/**
 * Parse strict SemVer 2.0.0 (optional leading `v` accepted, optional prerelease identifiers).
 * Rejects leading zeros in numeric components and numbers exceeding Number.MAX_SAFE_INTEGER.
 * Returns structured object or `null` if invalid.
 */
function parseSemver(raw) {
  if (typeof raw !== "string") {
    return null;
  }
  const str = raw.trim().replace(/^v/, "");
  const match = /^([0-9A-Za-z.-]+)(?:\+([0-9A-Za-z.-]+))?$/.exec(str);
  if (!match) {
    return null;
  }
  const coreAndPre = match[1];
  const preIndex = coreAndPre.indexOf("-");
  const core = preIndex === -1 ? coreAndPre : coreAndPre.slice(0, preIndex);
  const prerelease = preIndex === -1 ? null : coreAndPre.slice(preIndex + 1);

  const parts = core.split(".");
  if (parts.length !== 3) {
    return null;
  }
  const nums = [];
  for (const part of parts) {
    if (!/^(0|[1-9]\d*)$/.test(part)) {
      return null;
    }
    const n = Number(part);
    if (!Number.isSafeInteger(n)) {
      return null;
    }
    nums.push(n);
  }

  let preParts = null;
  if (prerelease !== null) {
    if (!prerelease) {
      return null;
    }
    const idents = prerelease.split(".");
    preParts = [];
    for (const id of idents) {
      if (!id || !/^[0-9A-Za-z-]+$/.test(id)) {
        return null;
      }
      if (/^\d+$/.test(id)) {
        if (!/^(0|[1-9]\d*)$/.test(id)) {
          return null;
        }
        const num = Number(id);
        if (!Number.isSafeInteger(num)) {
          return null;
        }
        preParts.push({ isNum: true, val: num });
      } else {
        preParts.push({ isNum: false, val: id });
      }
    }
  }

  return { major: nums[0], minor: nums[1], patch: nums[2], prerelease: preParts };
}

/**
 * Compare two SemVer 2.0.0 strings: `-1`, `0`, `1`, or `null` when either side is not parseable.
 * Honors prerelease precedence rules (release > prerelease, numeric < string identifiers).
 */
function compareSemver(a, b) {
  const left = parseSemver(a);
  const right = parseSemver(b);
  if (!left || !right) {
    return null;
  }

  if (left.major !== right.major) return left.major < right.major ? -1 : 1;
  if (left.minor !== right.minor) return left.minor < right.minor ? -1 : 1;
  if (left.patch !== right.patch) return left.patch < right.patch ? -1 : 1;

  if (left.prerelease === null && right.prerelease === null) return 0;
  if (left.prerelease === null && right.prerelease !== null) return 1;
  if (left.prerelease !== null && right.prerelease === null) return -1;

  const minLen = Math.min(left.prerelease.length, right.prerelease.length);
  for (let i = 0; i < minLen; i++) {
    const p1 = left.prerelease[i];
    const p2 = right.prerelease[i];
    if (p1.isNum && p2.isNum) {
      if (p1.val !== p2.val) return p1.val < p2.val ? -1 : 1;
    } else if (!p1.isNum && !p2.isNum) {
      if (p1.val !== p2.val) return p1.val < p2.val ? -1 : 1;
    } else {
      return p1.isNum ? -1 : 1;
    }
  }
  if (left.prerelease.length !== right.prerelease.length) {
    return left.prerelease.length < right.prerelease.length ? -1 : 1;
  }
  return 0;
}

// ---------------------------------------------------------------------------------------------
// Strict JSON Parser (Guards against duplicate keys, Unicode escape aliasing, ambiguous numbers)
// ---------------------------------------------------------------------------------------------

/**
 * Strict recursive-descent JSON parser for bounded text input.
 */
function parseStrictJson(text) {
  if (typeof text !== "string") {
    throw new Error("Expected string input for strict JSON parsing");
  }
  let pos = 0;
  const len = text.length;

  function skipWhitespace() {
    while (pos < len) {
      const ch = text.charCodeAt(pos);
      if (ch === 0x20 || ch === 0x09 || ch === 0x0a || ch === 0x0d) {
        pos++;
      } else {
        break;
      }
    }
  }

  function parseString() {
    if (text.charCodeAt(pos) !== 0x22) {
      throw new Error(`Expected '\"' at position ${pos}`);
    }
    pos++;
    let str = "";
    while (pos < len) {
      const ch = text.charCodeAt(pos);
      if (ch === 0x22) {
        pos++;
        return str;
      }
      if (ch < 0x20) {
        throw new Error(`Unescaped control character in string at position ${pos}`);
      }
      if (ch === 0x5c) {
        pos++;
        if (pos >= len) {
          throw new Error("Unterminated escape sequence in string");
        }
        const esc = text.charAt(pos);
        pos++;
        if (esc === "\"" || esc === "\\" || esc === "/") {
          str += esc;
        } else if (esc === "b") {
          str += "\b";
        } else if (esc === "f") {
          str += "\f";
        } else if (esc === "n") {
          str += "\n";
        } else if (esc === "r") {
          str += "\r";
        } else if (esc === "t") {
          str += "\t";
        } else if (esc === "u") {
          if (pos + 4 > len) {
            throw new Error("Incomplete unicode escape in string");
          }
          const hex = text.slice(pos, pos + 4);
          if (!/^[0-9a-fA-F]{4}$/.test(hex)) {
            throw new Error(`Invalid unicode escape: \\u${hex}`);
          }
          pos += 4;
          str += String.fromCharCode(parseInt(hex, 16));
        } else {
          throw new Error(`Invalid escape character: \\${esc}`);
        }
      } else {
        str += text.charAt(pos);
        pos++;
      }
    }
    throw new Error("Unterminated string literal");
  }

  function parseNumber() {
    const start = pos;
    if (text.charAt(pos) === "-") {
      pos++;
    }
    if (pos >= len) {
      throw new Error("Invalid empty number");
    }
    const firstDigit = text.charAt(pos);
    if (firstDigit < "0" || firstDigit > "9") {
      throw new Error(`Invalid number digit at position ${pos}`);
    }
    if (firstDigit === "0") {
      pos++;
      if (pos < len && text.charAt(pos) >= "0" && text.charAt(pos) <= "9") {
        throw new Error("Leading zero in number");
      }
    } else {
      while (pos < len && text.charAt(pos) >= "0" && text.charAt(pos) <= "9") {
        pos++;
      }
    }
    if (pos < len && text.charAt(pos) === ".") {
      pos++;
      if (pos >= len || text.charAt(pos) < "0" || text.charAt(pos) > "9") {
        throw new Error("Invalid fraction in number");
      }
      while (pos < len && text.charAt(pos) >= "0" && text.charAt(pos) <= "9") {
        pos++;
      }
    }
    if (pos < len && (text.charAt(pos) === "e" || text.charAt(pos) === "E")) {
      pos++;
      if (pos < len && (text.charAt(pos) === "+" || text.charAt(pos) === "-")) {
        pos++;
      }
      if (pos >= len || text.charAt(pos) < "0" || text.charAt(pos) > "9") {
        throw new Error("Invalid exponent in number");
      }
      while (pos < len && text.charAt(pos) >= "0" && text.charAt(pos) <= "9") {
        pos++;
      }
    }
    const numStr = text.slice(start, pos);
    const num = Number(numStr);
    if (!Number.isFinite(num)) {
      throw new Error(`Number is not finite: ${numStr}`);
    }
    return num;
  }

  function parseValue() {
    skipWhitespace();
    if (pos >= len) {
      throw new Error("Unexpected end of input");
    }
    const ch = text.charAt(pos);
    if (ch === "{") return parseObject();
    if (ch === "[") return parseArray();
    if (ch === "\"") return parseString();
    if (ch === "t" && text.startsWith("true", pos)) { pos += 4; return true; }
    if (ch === "f" && text.startsWith("false", pos)) { pos += 5; return false; }
    if (ch === "n" && text.startsWith("null", pos)) { pos += 4; return null; }
    if (ch === "-" || (ch >= "0" && ch <= "9")) return parseNumber();
    throw new Error(`Unexpected character '${ch}' at position ${pos}`);
  }

  function parseObject() {
    pos++;
    const obj = Object.create(null);
    const seen = new Set();
    skipWhitespace();
    if (pos < len && text.charAt(pos) === "}") {
      pos++;
      return obj;
    }
    while (pos < len) {
      skipWhitespace();
      if (text.charAt(pos) !== "\"") {
        throw new Error(`Expected string property key at position ${pos}`);
      }
      const key = parseString();
      if (seen.has(key)) {
        throw new Error(`Duplicate key in object: ${key}`);
      }
      seen.add(key);
      skipWhitespace();
      if (pos >= len || text.charAt(pos) !== ":") {
        throw new Error(`Expected ':' after key '${key}' at position ${pos}`);
      }
      pos++;
      const val = parseValue();
      obj[key] = val;
      skipWhitespace();
      if (pos >= len) {
        throw new Error("Unterminated object");
      }
      if (text.charAt(pos) === "}") {
        pos++;
        return obj;
      }
      if (text.charAt(pos) === ",") {
        pos++;
        skipWhitespace();
        if (text.charAt(pos) === "}") {
          throw new Error("Trailing comma in object");
        }
      } else {
        throw new Error(`Expected ',' or '}' at position ${pos}`);
      }
    }
    throw new Error("Unterminated object");
  }

  function parseArray() {
    pos++;
    const arr = [];
    skipWhitespace();
    if (pos < len && text.charAt(pos) === "]") {
      pos++;
      return arr;
    }
    while (pos < len) {
      arr.push(parseValue());
      skipWhitespace();
      if (pos >= len) {
        throw new Error("Unterminated array");
      }
      if (text.charAt(pos) === "]") {
        pos++;
        return arr;
      }
      if (text.charAt(pos) === ",") {
        pos++;
        skipWhitespace();
        if (text.charAt(pos) === "]") {
          throw new Error("Trailing comma in array");
        }
      } else {
        throw new Error(`Expected ',' or ']' at position ${pos}`);
      }
    }
    throw new Error("Unterminated array");
  }

  skipWhitespace();
  const result = parseValue();
  skipWhitespace();
  if (pos !== len) {
    throw new Error(`Unexpected extra data after JSON at position ${pos}`);
  }
  return result;
}

// ---------------------------------------------------------------------------------------------
// Ed25519 Canonical Signed Manifest & Cryptographic Verification
// ---------------------------------------------------------------------------------------------

function buildCanonicalSigningPayload(fields) {
  return Buffer.from(
    [
      "kroma-signed-manifest:v1",
      `antirollback:${fields.antirollback}`,
      `expires:${fields.expires}`,
      `keyId:${fields.keyId}`,
      `length:${fields.length}`,
      `schemaVersion:${fields.schemaVersion}`,
      `sha256:${fields.sha256}`,
      `tarballUrl:${fields.tarballUrl}`,
      `target:${fields.target}`,
      `version:${fields.version}`,
      ""
    ].join("\n"),
    "utf8"
  );
}

function coercePublicKey(key) {
  if (key instanceof crypto.KeyObject) {
    if (key.type !== "public" || key.asymmetricKeyType !== "ed25519") {
      throw new Error("KeyObject must be an ed25519 public key");
    }
    return key;
  }
  if (typeof key === "string") {
    const trimmed = key.trim();
    if (trimmed.startsWith("-----BEGIN")) {
      const parsed = crypto.createPublicKey(trimmed);
      if (parsed.asymmetricKeyType !== "ed25519") {
        throw new Error("PEM key is not an ed25519 key");
      }
      return parsed;
    }
    if (/^[0-9a-f]{64}$/i.test(trimmed)) {
      const der = Buffer.concat([ED25519_SPKI_PREFIX, Buffer.from(trimmed, "hex")]);
      return crypto.createPublicKey({ key: der, format: "der", type: "spki" });
    }
    if (/^[A-Za-z0-9+/]{43}=$|^[A-Za-z0-9+/]{44}$/.test(trimmed)) {
      const raw = Buffer.from(trimmed, "base64");
      if (raw.length === 32) {
        const der = Buffer.concat([ED25519_SPKI_PREFIX, raw]);
        return crypto.createPublicKey({ key: der, format: "der", type: "spki" });
      }
    }
  }
  if (Buffer.isBuffer(key)) {
    if (key.length === 32) {
      const der = Buffer.concat([ED25519_SPKI_PREFIX, key]);
      return crypto.createPublicKey({ key: der, format: "der", type: "spki" });
    }
    if (key.length === 44 && key.subarray(0, 12).equals(ED25519_SPKI_PREFIX)) {
      return crypto.createPublicKey({ key, format: "der", type: "spki" });
    }
  }
  throw new Error("Invalid Ed25519 public key format");
}

function signManifest(fields, privateKey) {
  const payload = buildCanonicalSigningPayload(fields);
  const signature = crypto.sign(null, payload, privateKey);
  return {
    schemaVersion: fields.schemaVersion,
    keyId: fields.keyId,
    version: fields.version,
    target: fields.target,
    tarballUrl: fields.tarballUrl,
    sha256: fields.sha256,
    length: fields.length,
    expires: fields.expires,
    antirollback: fields.antirollback,
    signature: signature.toString("base64")
  };
}

function serializeManifest(manifest) {
  return JSON.stringify(
    manifest,
    [
      "schemaVersion",
      "keyId",
      "version",
      "target",
      "tarballUrl",
      "sha256",
      "length",
      "expires",
      "antirollback",
      "signature"
    ],
    2
  );
}

function verifyManifest(rawManifestText, options = {}) {
  if (typeof rawManifestText !== "string") {
    throw new Error("Manifest must be a string");
  }
  const maxBytes = options.maxManifestBytes || MAX_MANIFEST_BYTES;
  if (Buffer.byteLength(rawManifestText, "utf8") > maxBytes) {
    throw new Error(`Manifest exceeds maximum allowed size of ${maxBytes} bytes`);
  }

  const rawObj = parseStrictJson(rawManifestText);
  if (!rawObj || typeof rawObj !== "object" || Array.isArray(rawObj)) {
    throw new Error("Manifest root must be a JSON object");
  }

  const allowedKeys = new Set([
    "schemaVersion",
    "keyId",
    "version",
    "target",
    "tarballUrl",
    "sha256",
    "length",
    "expires",
    "antirollback",
    "signature"
  ]);

  for (const k of Object.keys(rawObj)) {
    if (!allowedKeys.has(k)) {
      throw new Error(`Unknown field in manifest: ${k}`);
    }
  }

  for (const required of allowedKeys) {
    if (rawObj[required] === undefined) {
      throw new Error(`Missing required manifest field: ${required}`);
    }
  }

  if (rawObj.schemaVersion !== 1) {
    throw new Error(`Unsupported manifest schemaVersion: ${rawObj.schemaVersion}`);
  }

  if (typeof rawObj.keyId !== "string" || !/^[A-Za-z0-9_-]{1,64}$/.test(rawObj.keyId)) {
    throw new Error("Invalid keyId: must be 1-64 alphanumeric/hyphen/underscore characters");
  }

  if (typeof rawObj.version !== "string" || !parseSemver(rawObj.version)) {
    throw new Error("Invalid version: must be valid SemVer major.minor.patch");
  }

  if (typeof rawObj.target !== "string" || !ALLOWED_TARGETS.has(rawObj.target)) {
    throw new Error(`Invalid or unsupported target: ${rawObj.target}`);
  }

  if (typeof rawObj.tarballUrl !== "string" || !isValidHttpsTarballUrl(rawObj.tarballUrl)) {
    throw new Error(`Invalid tarballUrl: ${rawObj.tarballUrl}`);
  }

  if (typeof rawObj.sha256 !== "string" || !/^[0-9a-f]{64}$/.test(rawObj.sha256)) {
    throw new Error("Invalid sha256: must be 64 lowercase hex characters");
  }

  if (
    typeof rawObj.length !== "number" ||
    !Number.isSafeInteger(rawObj.length) ||
    rawObj.length <= 0 ||
    rawObj.length > MAX_TARBALL_BYTES
  ) {
    throw new Error(`Invalid length: must be safe positive integer <= ${MAX_TARBALL_BYTES}`);
  }

  if (
    typeof rawObj.expires !== "number" ||
    !Number.isSafeInteger(rawObj.expires) ||
    rawObj.expires <= 0
  ) {
    throw new Error("Invalid expires: must be safe positive integer unix timestamp in seconds");
  }

  if (typeof rawObj.antirollback !== "string" || !parseSemver(rawObj.antirollback)) {
    throw new Error("Invalid antirollback: must be valid SemVer major.minor.patch");
  }

  if (
    typeof rawObj.signature !== "string" ||
    !/^[A-Za-z0-9+/]{86}==$/.test(rawObj.signature)
  ) {
    throw new Error("Invalid signature: must be canonical 88-char base64 Ed25519 signature");
  }

  const sigBuf = Buffer.from(rawObj.signature, "base64");
  if (sigBuf.length !== 64 || sigBuf.toString("base64") !== rawObj.signature) {
    throw new Error("Invalid signature: non-canonical base64 encoding");
  }

  // Trust root check
  const trustedKeys = options.trustedKeys !== undefined ? options.trustedKeys : PRODUCTION_TRUSTED_KEYS;
  if (!trustedKeys || typeof trustedKeys !== "object" || Object.keys(trustedKeys).length === 0) {
    throw new Error("Production release trust root is unconfigured; self-upgrade is blocked");
  }

  const rawKey = trustedKeys[rawObj.keyId];
  if (!rawKey) {
    throw new Error(`Unknown or untrusted signing key: ${rawObj.keyId}`);
  }
  const publicKey = coercePublicKey(rawKey);

  // Cryptographic Ed25519 verification over canonical payload
  const canonicalPayload = buildCanonicalSigningPayload(rawObj);
  const signatureValid = crypto.verify(null, canonicalPayload, publicKey, sigBuf);
  if (!signatureValid) {
    throw new Error("Signature verification failed for release manifest");
  }

  // Target match check
  if (options.currentTarget !== undefined) {
    if (rawObj.target !== options.currentTarget) {
      throw new Error(`Manifest target mismatch: expected ${options.currentTarget}, got ${rawObj.target}`);
    }
  }

  // Expiration check
  const currentTime = options.currentTime !== undefined ? options.currentTime : Math.floor(Date.now() / 1000);
  if (currentTime > rawObj.expires) {
    throw new Error(`Manifest has expired (current: ${currentTime}, expires: ${rawObj.expires})`);
  }

  // Antirollback & version downgrade check (Fail-Closed)
  const currentVersion = options.currentVersion !== undefined ? options.currentVersion : pkgJson.version;
  if (currentVersion) {
    const cmpLocal = compareSemver(rawObj.version, currentVersion);
    if (cmpLocal === null) {
      throw new Error(
        `Version comparison failed: cannot compare remote v${rawObj.version} with local v${currentVersion}`
      );
    }
    if (cmpLocal < 0) {
      throw new Error(
        `Version downgrade rejected: remote v${rawObj.version} is older than local v${currentVersion}`
      );
    }
  }

  const cmpFloor = compareSemver(rawObj.version, rawObj.antirollback);
  if (cmpFloor === null) {
    throw new Error(
      `Version comparison failed: cannot compare remote v${rawObj.version} with antirollback floor v${rawObj.antirollback}`
    );
  }
  if (cmpFloor < 0) {
    throw new Error(
      `Version violates antirollback floor: remote v${rawObj.version} is below floor v${rawObj.antirollback}`
    );
  }

  return {
    schemaVersion: rawObj.schemaVersion,
    keyId: rawObj.keyId,
    version: rawObj.version,
    target: rawObj.target,
    tarballUrl: rawObj.tarballUrl,
    sha256: rawObj.sha256,
    length: rawObj.length,
    expires: rawObj.expires,
    antirollback: rawObj.antirollback,
    signature: rawObj.signature
  };
}

// ---------------------------------------------------------------------------------------------
// Pure-Node Streaming Tar Parser, Security Validator & Extractor
// ---------------------------------------------------------------------------------------------

/**
 * Validate path strings extracted from tar headers against traversals, backslashes,
 * Windows drive letters, colons/ADS, trailing dots/spaces, control characters, and device names.
 */
function validateTarEntryPath(rawPath) {
  if (typeof rawPath !== "string" || rawPath.length === 0) {
    throw new Error("Empty tar entry path");
  }
  if (/[\x00-\x1f\x7f]/.test(rawPath)) {
    throw new Error(`Control characters in entry path forbidden: ${rawPath}`);
  }
  if (rawPath.startsWith("/") || rawPath.startsWith("\\")) {
    throw new Error(`Absolute entry path forbidden: ${rawPath}`);
  }
  if (/^[a-zA-Z]:/.test(rawPath)) {
    throw new Error(`Windows drive path forbidden: ${rawPath}`);
  }
  if (rawPath.includes(":")) {
    throw new Error(`Colon / ADS in entry path forbidden: ${rawPath}`);
  }
  if (rawPath.includes("\\")) {
    throw new Error(`Backslash in entry path forbidden: ${rawPath}`);
  }
  if (rawPath.startsWith("//")) {
    throw new Error(`UNC path forbidden: ${rawPath}`);
  }

  const segments = rawPath.split("/");
  for (let i = 0; i < segments.length; i++) {
    const seg = segments[i];
    // Empty segment allowed only at the very end to indicate a directory (e.g. "dir/")
    if (seg === "") {
      if (i === segments.length - 1) continue;
      throw new Error(`Empty path segment in entry path: ${rawPath}`);
    }
    if (seg === "..") {
      throw new Error(`Path traversal '..' forbidden: ${rawPath}`);
    }
    if (seg === ".") {
      throw new Error(`Redundant '.' segment forbidden: ${rawPath}`);
    }
    if (seg !== "." && seg !== ".." && (seg.endsWith(".") || seg.endsWith(" "))) {
      throw new Error(`Trailing dot or space forbidden: ${rawPath}`);
    }
    const baseSeg = seg.split(".")[0].toLowerCase();
    if (WINDOWS_RESERVED_NAMES.has(baseSeg)) {
      throw new Error(`Windows reserved device name forbidden: ${rawPath}`);
    }
  }
  return rawPath;
}

/**
 * Pure-Node streaming inspection and extraction for gzipped tar archives.
 *
 * Guarantees:
 * - Bounded uncompressed bytes limit (options.maxUncompressedBytes || MAX_UNCOMPRESSED_BYTES)
 * - Bounded total entry count limit (options.maxArchiveEntries || MAX_ARCHIVE_ENTRIES)
 * - Rejects any entry type that is not regular file ('0'/'\0') or directory ('5')
 *   (symlinks, hard links, devices, FIFOs, PAX headers, GNU longnames are rejected fail-closed)
 * - Verifies checksum on every 512-byte header block
 * - Rejects base-256 size representations
 * - Validates paths against traversals, backslashes, colons, drives, devices, duplicate paths
 * - Two terminating zero blocks required with zero trailing non-zero garbage
 * - Absolute deadline timer (options.deadlineMs) covering the entire stream
 * - If extractDestPath is provided: extracts single targetSubPath with exclusive file creation ('wx'),
 *   sets mode 0o755, and atomically renames on completion.
 */
function inspectAndExtractTarStream(archivePath, targetSubPath, extractDestPath, options = {}) {
  const maxEntries = options.maxArchiveEntries || MAX_ARCHIVE_ENTRIES;
  const maxBytes = options.maxUncompressedBytes || MAX_UNCOMPRESSED_BYTES;
  const deadlineMs = options.deadlineMs || 30000;

  return new Promise((resolve, reject) => {
    if (!fs.existsSync(archivePath)) {
      return reject(new Error(`Archive file does not exist: ${archivePath}`));
    }
    const stat = fs.lstatSync(archivePath);
    if (stat.isSymbolicLink() || !stat.isFile()) {
      return reject(new Error(`Archive is not a regular file: ${archivePath}`));
    }

    let settled = false;
    let fileStream = null;
    let gunzipStream = null;
    let outStream = null;
    let outStreamPromise = null;
    let stagePath = null;
    let stageCreated = false;
    // Resolves when the staging write stream has opened (or failed to). Finalising before the open has
    // happened is a scheduling event, not evidence that the artifact is missing.
    let stageOpenPromise = null;
    let deadlineTimer = null;
    let activeDrainStream = null;
    let activeDrainListener = null;
    let isDraining = false;
    let gunzipEnded = false;
    let peakBufferLength = 0;

    // The first outcome wins. A late ENOENT from the finalize path must never replace the deadline /
    // abort error that actually caused the run to stop.
    let settledError = null;

    function cleanup(err) {
      if (settled) return;
      settled = true;
      settledError = err || null;
      if (deadlineTimer) clearTimeout(deadlineTimer);
      if (activeDrainStream && activeDrainListener) {
        try {
          activeDrainStream.removeListener("drain", activeDrainListener);
        } catch (_) {}
        activeDrainStream = null;
        activeDrainListener = null;
      }
      if (fileStream) try { fileStream.destroy(); } catch (_) {}
      if (gunzipStream) try { gunzipStream.destroy(); } catch (_) {}
      if (outStream) try { outStream.destroy(); } catch (_) {}
      if (stageCreated && stagePath && fs.existsSync(stagePath)) {
        try { fs.unlinkSync(stagePath); } catch (_) {}
      }
      reject(err);
    }

    function succeed(result) {
      if (settled) return;
      settled = true;
      if (deadlineTimer) clearTimeout(deadlineTimer);
      resolve(result);
    }

    deadlineTimer = setTimeout(() => {
      cleanup(new Error(`Archive processing exceeded deadline of ${deadlineMs}ms`));
    }, deadlineMs);

    if (extractDestPath) {
      const destDir = path.dirname(extractDestPath);
      fs.mkdirSync(destDir, { recursive: true });
      stagePath = `${extractDestPath}.tmp.${process.pid}.${Date.now()}`;
    }

    try {
      fileStream = fs.createReadStream(archivePath);
      gunzipStream = zlib.createGunzip();
    } catch (err) {
      return cleanup(err);
    }

    let buffer = Buffer.alloc(0);
    let totalUncompressed = 0;
    let entryCount = 0;
    let zeroBlockCount = 0;
    let currentEntry = null;
    let currentBytesRemaining = 0;
    let padRemaining = 0;
    const seenNormalized = new Set();
    const seenFiles = new Set();
    const seenDirs = new Set();
    let targetFound = false;

    gunzipStream.on("data", (chunk) => {
      if (settled) return;
      totalUncompressed += chunk.length;
      if (totalUncompressed > maxBytes) {
        return cleanup(new Error(`Archive exceeded maximum uncompressed byte limit of ${maxBytes}`));
      }

      if (zeroBlockCount >= 2) {
        if (!chunk.every((b) => b === 0)) {
          return cleanup(new Error("Non-zero trailing data found after tar EOF block"));
        }
        return;
      }

      buffer = Buffer.concat([buffer, chunk]);
      if (buffer.length > peakBufferLength) {
        peakBufferLength = buffer.length;
      }
      pump();
    });

    function pump() {
      if (settled || isDraining) return;

      try {
        while (!isDraining && (buffer.length >= 512 || (currentEntry && buffer.length > 0))) {
          if (padRemaining > 0) {
            if (buffer.length < padRemaining) {
              padRemaining -= buffer.length;
              buffer = Buffer.alloc(0);
              break;
            } else {
              buffer = buffer.subarray(padRemaining);
              padRemaining = 0;
              currentEntry = null;
            }
          }

          if (currentEntry) {
            const toTake = Math.min(buffer.length, currentBytesRemaining);
            const dataSlice = buffer.subarray(0, toTake);
            buffer = buffer.subarray(toTake);
            currentBytesRemaining -= toTake;

            let canContinue = true;
            if (outStream && currentEntry.isTarget) {
              canContinue = outStream.write(dataSlice);
            }

            if (currentBytesRemaining === 0) {
              if (outStream && currentEntry.isTarget) {
                const s = outStream;
                outStreamPromise = new Promise((res, rej) => {
                  s.on("finish", () => {
                    s.on("close", res);
                    if (s.closed) res();
                  });
                  s.on("error", rej);
                });
                outStreamPromise.catch(() => {});
                s.end();
                outStream = null;
              }
              const pad = (512 - (currentEntry.size % 512)) % 512;
              if (pad > 0) {
                padRemaining = pad;
              } else {
                currentEntry = null;
              }
            }

            if (!canContinue && outStream) {
              isDraining = true;
              if (!settled && gunzipStream) {
                try { gunzipStream.pause(); } catch (_) {}
              }
              const s = outStream;
              const onDrain = () => {
                s.removeListener("drain", onDrain);
                s.removeListener("error", onDrainError);
                if (activeDrainStream === s) {
                  activeDrainStream = null;
                  activeDrainListener = null;
                }
                if (settled) return;
                isDraining = false;
                pump();
                if (!isDraining && !settled && gunzipStream) {
                  try { gunzipStream.resume(); } catch (_) {}
                }
              };
              const onDrainError = () => {
                s.removeListener("drain", onDrain);
                s.removeListener("error", onDrainError);
                if (activeDrainStream === s) {
                  activeDrainStream = null;
                  activeDrainListener = null;
                }
              };
              activeDrainStream = s;
              activeDrainListener = onDrain;
              s.once("drain", onDrain);
              s.once("error", onDrainError);
              break;
            }
            continue;
          }

          if (buffer.length < 512) break;

          const header = buffer.subarray(0, 512);
          buffer = buffer.subarray(512);

          const isZero = header.every((b) => b === 0);
          if (isZero) {
            zeroBlockCount++;
            if (zeroBlockCount >= 2) {
              if (!buffer.every((b) => b === 0)) {
                return cleanup(new Error("Non-zero trailing data found after tar EOF block"));
              }
              break;
            }
            continue;
          }

          if (zeroBlockCount > 0) {
            return cleanup(new Error("Non-zero block encountered after zero block in tar archive"));
          }

          entryCount++;
          if (entryCount > maxEntries) {
            return cleanup(new Error(`Archive exceeded maximum entry limit of ${maxEntries}`));
          }

          // Verify header checksum
          const chkField = header.subarray(148, 156).toString("ascii").trim().replace(/\0.*$/, "");
          const expectedChk = parseInt(chkField, 8);
          if (Number.isNaN(expectedChk)) {
            return cleanup(new Error("Invalid tar header checksum field"));
          }
          let sum = 0;
          for (let i = 0; i < 512; i++) {
            sum += (i >= 148 && i < 156) ? 0x20 : header[i];
          }
          if (sum !== expectedChk) {
            return cleanup(new Error(`Tar header checksum mismatch: expected ${expectedChk}, got ${sum}`));
          }

          // Verify typeflag (only '0'/'\0' file and '5' dir are permitted)
          const typeflag = String.fromCharCode(header[156]);
          if (typeflag !== "0" && typeflag !== "\0" && typeflag !== "5") {
            return cleanup(new Error(`Unsupported or dangerous tar entry type '${typeflag}'`));
          }

          // Verify size
          if (header[124] & 0x80) {
            return cleanup(new Error("Base-256 binary tar size encoding is forbidden"));
          }
          const sizeStr = header.subarray(124, 136).toString("ascii").trim().replace(/\0.*$/, "");
          const size = sizeStr ? parseInt(sizeStr, 8) : 0;
          if (!Number.isSafeInteger(size) || size < 0 || size > maxBytes) {
            return cleanup(new Error(`Invalid tar entry size: ${sizeStr}`));
          }

          // Path extraction
          let rawName = header.subarray(0, 100).toString("utf8");
          const nameNul = rawName.indexOf("\0");
          if (nameNul !== -1) rawName = rawName.slice(0, nameNul);

          let rawPrefix = header.subarray(345, 500).toString("utf8");
          const prefixNul = rawPrefix.indexOf("\0");
          if (prefixNul !== -1) rawPrefix = rawPrefix.slice(0, prefixNul);

          const entryPath = rawPrefix ? `${rawPrefix}/${rawName}` : rawName;
          validateTarEntryPath(entryPath);

          const isDir = typeflag === "5" || entryPath.endsWith("/");
          const normPath = entryPath.replace(/\/+$/, "").toLowerCase();
          if (!normPath) {
            return cleanup(new Error(`Empty normalized entry path: ${entryPath}`));
          }

          if (seenNormalized.has(normPath)) {
            return cleanup(new Error(`Duplicate entry path in archive: ${entryPath}`));
          }
          seenNormalized.add(normPath);

          const pathParts = normPath.split("/");
          for (let i = 1; i < pathParts.length; i++) {
            const ancestor = pathParts.slice(0, i).join("/");
            if (seenFiles.has(ancestor)) {
              return cleanup(new Error(`Parent path is a file, cannot be directory: ${ancestor}`));
            }
          }

          if (isDir) {
            if (seenFiles.has(normPath)) {
              return cleanup(new Error(`Directory conflicts with existing file: ${entryPath}`));
            }
            seenDirs.add(normPath);
          } else {
            if (seenDirs.has(normPath)) {
              return cleanup(new Error(`File path conflicts with existing directory: ${entryPath}`));
            }
            seenFiles.add(normPath);
          }

          for (let i = 1; i < pathParts.length; i++) {
            seenDirs.add(pathParts.slice(0, i).join("/"));
          }

          const isTarget = targetSubPath ? (entryPath === targetSubPath || entryPath === `./${targetSubPath}`) : false;
          if (isTarget) {
            if (typeflag !== "0" && typeflag !== "\0") {
              return cleanup(new Error(`Target entry in archive is not a regular file: ${entryPath}`));
            }
            targetFound = true;
            if (extractDestPath) {
              const createWs = options.createWriteStream || fs.createWriteStream;
              const ws = createWs(stagePath, { flags: "wx", mode: 0o755 });
              outStream = ws;
              ws.on("error", cleanup);
              stageOpenPromise = new Promise((resolveOpen) => {
                ws.once("open", resolveOpen);
                ws.once("error", resolveOpen);
                ws.once("close", resolveOpen);
              });
              ws.on("open", () => {
                stageCreated = true;
                if (settled) {
                  try {
                    ws.destroy();
                  } catch (_) {}
                  ws.on("close", () => {
                    try {
                      if (stagePath && fs.existsSync(stagePath)) fs.unlinkSync(stagePath);
                    } catch (_) {}
                  });
                  try {
                    if (stagePath && fs.existsSync(stagePath)) fs.unlinkSync(stagePath);
                  } catch (_) {}
                }
              });
            }
          }

          currentEntry = {
            path: entryPath,
            size,
            type: typeflag === "5" ? "directory" : "file",
            isTarget
          };
          currentBytesRemaining = size;

          if (size === 0) {
            currentEntry = null;
          }
        }

        if (gunzipEnded && !isDraining && !settled) {
          finishArchive();
        }
      } catch (err) {
        cleanup(err);
      }
    }

    async function finishArchive() {
      if (settled) return;
      try {
        if (buffer.length > 0 && !buffer.every((b) => b === 0)) {
          return cleanup(new Error("Non-zero trailing data found after tar EOF block"));
        }
        if (outStreamPromise) {
          await outStreamPromise;
        }
        // The deadline may have fired while we waited for the write stream: if so, the stage file is
        // already gone and this function must stop here instead of reporting a misleading ENOENT.
        if (settled) return;
        if (currentEntry) {
          return cleanup(new Error("Truncated tar archive: unexpected EOF while reading file data"));
        }
        if (zeroBlockCount < 2) {
          return cleanup(new Error("Truncated tar archive: missing terminating zero blocks"));
        }
        if (targetSubPath && !targetFound) {
          return cleanup(new Error(`Target entry ${targetSubPath} not found in archive`));
        }

        if (stagePath && extractDestPath) {
          // A stage file that was never created (the stream never opened) or that cleanup already
          // removed is not "a truncated archive": report the outcome that actually caused it.
          // The write stream may not have opened yet; wait for it (the deadline timer still wins if it
          // fires first) instead of declaring the artifact missing.
          if (!stageCreated && stageOpenPromise) {
            await stageOpenPromise;
            if (settled) return;
          }
          if (!stageCreated || !fs.existsSync(stagePath)) {
            // The staging file never appeared (the write stream never opened, or an abort removed it).
            // That is the abort path, not a bad artifact: leave the outcome to the deadline timer, which
            // is always armed and will report the real reason. Settling here would replace it with a
            // misleading "missing artifact" error - the exact defect this fix removes.
            return;
          }
          const actualStat = fs.lstatSync(stagePath);
          if (actualStat.isSymbolicLink() || !actualStat.isFile()) {
            return cleanup(new Error("Extracted artifact is not a regular file"));
          }
          try {
            fs.chmodSync(stagePath, 0o755);
          } catch (_) {}
          fs.renameSync(stagePath, extractDestPath);
          stageCreated = false;
          stagePath = null;
        }

        succeed({
          entryCount,
          totalUncompressed,
          extracted: targetFound,
          peakBufferLength
        });
      } catch (err) {
        cleanup(settledError || err);
      }
    }

    gunzipStream.on("end", () => {
      gunzipEnded = true;
      if (!isDraining) {
        finishArchive();
      }
    });

    fileStream.on("error", cleanup);
    gunzipStream.on("error", cleanup);
    fileStream.pipe(gunzipStream);
  });
}

/**
 * Async inspection of tar archive entries. Returns true if valid, throws on violation.
 */
async function validateArchiveEntries(archivePath, expectedSubPath, options = {}) {
  await inspectAndExtractTarStream(archivePath, expectedSubPath, null, options);
  return true;
}

// ---------------------------------------------------------------------------------------------
// Network Fetchers and Download Integrity
// ---------------------------------------------------------------------------------------------

function fetchText(url, options = {}) {
  const maxBytes = options.maxBytes || MAX_MANIFEST_BYTES;
  const timeoutMs = options.timeout || 10000;
  const deadlineMs = options.deadlineMs || 15000;

  return new Promise((resolve, reject) => {
    let redirects = 0;
    const maxRedirects = 3;
    let activeReq = null;
    let settled = false;

    function cleanupAndReject(err) {
      if (settled) return;
      settled = true;
      clearTimeout(deadlineTimer);
      if (activeReq) {
        try { activeReq.destroy(); } catch (_) {}
      }
      reject(err);
    }

    function cleanupAndResolve(val) {
      if (settled) return;
      settled = true;
      clearTimeout(deadlineTimer);
      resolve(val);
    }

    const deadlineTimer = setTimeout(() => {
      cleanupAndReject(new Error(`Absolute deadline exceeded (${deadlineMs}ms) for ${url}`));
    }, deadlineMs);

    function get(targetUrl) {
      if (!isValidHttpsTarballUrl(targetUrl)) {
        return cleanupAndReject(new Error(`Refusing unsafe URL: ${targetUrl}`));
      }
      const req = https.get(
        targetUrl,
        {
          timeout: timeoutMs,
          headers: { "User-Agent": `kroma-updater/${pkgJson.version}` }
        },
        (res) => {
          if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
            redirects++;
            if (redirects > maxRedirects) {
              res.resume();
              return cleanupAndReject(new Error("Too many redirects"));
            }
            let next;
            try {
              next = new URL(res.headers.location, targetUrl).href;
            } catch (e) {
              res.resume();
              return cleanupAndReject(new Error(`Invalid redirect Location: ${res.headers.location}`));
            }
            if (!isValidHttpsTarballUrl(next)) {
              res.resume();
              return cleanupAndReject(new Error(`Redirect to unsafe URL rejected: ${next}`));
            }
            res.resume();
            return get(next);
          }
          if (res.statusCode === 404) {
            res.resume();
            return cleanupAndResolve(null);
          }
          if (res.statusCode < 200 || res.statusCode >= 300) {
            res.resume();
            return cleanupAndReject(new Error(`HTTP ${res.statusCode}`));
          }
          let raw = "";
          let bytes = 0;
          res.on("data", (chunk) => {
            bytes += chunk.length;
            if (bytes > maxBytes) {
              req.destroy();
              return cleanupAndReject(new Error(`Response exceeded maximum size of ${maxBytes} bytes`));
            }
            raw += chunk;
          });
          res.on("end", () => {
            cleanupAndResolve(raw);
          });
          res.on("error", cleanupAndReject);
        }
      );
      activeReq = req;
      req.on("error", cleanupAndReject);
      req.on("timeout", () => {
        req.destroy(new Error(`Socket timeout (${timeoutMs}ms)`));
      });
    }

    get(url);
  });
}

function fetchJson(url, options = {}) {
  return fetchText(url, options).then((text) => {
    if (text === null) return null;
    return JSON.parse(text);
  });
}

/**
 * Stream an HTTPS download with strict redirect checking, absolute deadline, byte length caps, and SHA-256 verification.
 */
function downloadTarballWithIntegrity(tarballUrl, expectedSha256, expectedLength, destFile, options = {}) {
  if (options.downloadFetcher) {
    return options.downloadFetcher(tarballUrl, expectedSha256, expectedLength, destFile);
  }

  return new Promise((resolve, reject) => {
    let currentUrl = tarballUrl;
    let redirects = 0;
    const maxRedirects = 3;
    const timeoutMs = options.timeout || 30000;
    const deadlineMs = options.deadlineMs || 60000;
    let activeReq = null;
    let outStream = null;
    let settled = false;

    function cleanup(err) {
      if (settled) return;
      settled = true;
      clearTimeout(deadlineTimer);
      if (activeReq) {
        try { activeReq.destroy(); } catch (_) {}
      }
      if (outStream) {
        try { outStream.destroy(); } catch (_) {}
      }
      try { fs.unlinkSync(destFile); } catch (_) {}
      reject(err);
    }

    function succeed(val) {
      if (settled) return;
      settled = true;
      clearTimeout(deadlineTimer);
      resolve(val);
    }

    const deadlineTimer = setTimeout(() => {
      cleanup(new Error(`Download absolute deadline exceeded (${deadlineMs}ms) for ${tarballUrl}`));
    }, deadlineMs);

    function doRequest(targetUrl) {
      if (!isValidHttpsTarballUrl(targetUrl)) {
        return cleanup(new Error(`Refusing unsafe download URL: ${targetUrl}`));
      }

      let parsed;
      try {
        parsed = new URL(targetUrl);
      } catch (err) {
        return cleanup(err);
      }

      if (parsed.protocol !== "https:") {
        return cleanup(new Error("Only HTTPS downloads are permitted"));
      }

      const req = https.get(
        targetUrl,
        {
          timeout: timeoutMs,
          headers: { "User-Agent": `kroma-updater/${pkgJson.version}` }
        },
        (res) => {
          if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
            redirects++;
            if (redirects > maxRedirects) {
              res.resume();
              return cleanup(new Error("Too many redirects during download"));
            }
            let nextUrl;
            try {
              nextUrl = new URL(res.headers.location, targetUrl).href;
            } catch (err) {
              res.resume();
              return cleanup(new Error(`Invalid redirect Location: ${res.headers.location}`));
            }
            if (!isValidHttpsTarballUrl(nextUrl)) {
              res.resume();
              return cleanup(new Error(`Redirect to unsafe URL rejected: ${nextUrl}`));
            }
            res.resume();
            return doRequest(nextUrl);
          }

          if (res.statusCode !== 200) {
            res.resume();
            return cleanup(new Error(`Download failed with HTTP ${res.statusCode}`));
          }

          const cl = res.headers["content-length"];
          if (cl !== undefined) {
            const clNum = parseInt(cl, 10);
            if (!Number.isNaN(clNum) && clNum !== expectedLength) {
              res.resume();
              return cleanup(
                new Error(`Content-Length mismatch: expected ${expectedLength}, header reported ${clNum}`)
              );
            }
          }

          const hash = crypto.createHash("sha256");
          let bytesReceived = 0;
          outStream = fs.createWriteStream(destFile);

          res.on("data", (chunk) => {
            bytesReceived += chunk.length;
            if (bytesReceived > expectedLength) {
              req.destroy();
              return cleanup(new Error(`Download exceeded expected size of ${expectedLength} bytes`));
            }
            hash.update(chunk);
            outStream.write(chunk);
          });

          res.on("end", () => {
            outStream.end(() => {
              if (bytesReceived !== expectedLength) {
                return cleanup(
                  new Error(`Download size mismatch: expected ${expectedLength} bytes, received ${bytesReceived}`)
                );
              }
              const digest = hash.digest("hex").toLowerCase();
              if (digest !== expectedSha256.toLowerCase()) {
                return cleanup(
                  new Error(`Checksum mismatch: expected sha256 ${expectedSha256}, calculated ${digest}`)
                );
              }
              succeed({ bytesReceived, sha256: digest });
            });
          });

          res.on("error", cleanup);
        }
      );
      activeReq = req;
      req.on("error", cleanup);
      req.on("timeout", () => {
        req.destroy(new Error(`Download socket timeout (${timeoutMs}ms)`));
      });
    }

    doRequest(currentUrl);
  });
}

function sha256File(filePath) {
  return crypto.createHash("sha256").update(fs.readFileSync(filePath)).digest("hex");
}

// ---------------------------------------------------------------------------------------------
// Upgrade and Binary Resolver
// ---------------------------------------------------------------------------------------------

async function handleSelfUpgrade(args, options = {}) {
  const checkOnly = args.includes("--check") || args.includes("-c");
  const force = args.includes("--force") || args.includes("-f");
  const current = options.currentVersion || pkgJson.version;

  const trustedKeys = options.trustedKeys !== undefined ? options.trustedKeys : PRODUCTION_TRUSTED_KEYS;
  if (!trustedKeys || typeof trustedKeys !== "object" || Object.keys(trustedKeys).length === 0) {
    if (checkOnly) {
      console.log(`● upgrade  production release trust root is unconfigured (release blocked, local v${current}).`);
      if (options.returnStatus) return { status: 1 };
      process.exit(1);
    }
    console.error(`● upgrade  ✗ production release trust root is unconfigured; self-upgrade is blocked.`);
    console.error(`           Bootstrap upgrades require an operator-signed release or trusted manual installation.`);
    if (options.returnStatus) return { status: 1 };
    process.exit(1);
  }

  console.log(`● upgrade  checking latest signed Kroma release (current: v${current})...`);
  const manifestUrl = options.manifestUrl || getManifestUrl("cli");
  let manifestText = options.manifestText;
  if (!manifestText) {
    try {
      manifestText = await fetchText(manifestUrl, options);
    } catch (err) {
      console.error(`● upgrade  ✗ unable to fetch release manifest: ${err.message}`);
      if (options.returnStatus) return { status: 1, error: err };
      process.exit(1);
    }
  }

  if (!manifestText) {
    console.error(`● upgrade  ✗ release manifest not found at ${manifestUrl}`);
    if (options.returnStatus) return { status: 1 };
    process.exit(1);
  }

  let manifest;
  try {
    manifest = verifyManifest(manifestText, {
      trustedKeys,
      currentTarget: "cli",
      currentVersion: current,
      ...options
    });
  } catch (err) {
    console.error(`● upgrade  ✗ release manifest verification failed: ${err.message}`);
    if (options.returnStatus) return { status: 1, error: err };
    process.exit(1);
  }

  const latest = manifest.version;
  const cmp = compareSemver(latest, current);
  if (cmp === null) {
    console.error(`● upgrade  ✗ cannot compare local v${current} with remote v${latest}; refusing to install.`);
    if (options.returnStatus) return { status: 1 };
    process.exit(1);
  }
  if (cmp <= 0 && !force) {
    if (cmp === 0) {
      console.log(`● upgrade  ✓ kroma v${current} is already the latest version.`);
    } else {
      console.log(`● upgrade  ✓ local v${current} is newer than remote v${latest}; nothing to do.`);
    }
    if (options.returnStatus) return { status: 0 };
    process.exit(0);
  }

  console.log(`● upgrade  new verified version available: v${current} → v${latest}`);
  if (checkOnly) {
    console.log(`  Run 'kroma upgrade' to update.`);
    if (options.returnStatus) return { status: 0 };
    process.exit(0);
  }

  // DOWNLOAD AND VERIFY TARBALL BEFORE EVER SPAWNING NPM
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-upgrade-"));
  const tmpArchive = path.join(tmpDir, `kroma-${manifest.version}.tgz`);
  try {
    console.log(`● upgrade  downloading verified release archive...`);
    if (options.fixtureTarballPath) {
      fs.copyFileSync(options.fixtureTarballPath, tmpArchive);
      const actualSha = sha256File(tmpArchive);
      const actualLen = fs.statSync(tmpArchive).size;
      if (actualSha !== manifest.sha256 || actualLen !== manifest.length) {
        throw new Error(`Fixture tarball checksum or length mismatch`);
      }
    } else {
      await downloadTarballWithIntegrity(
        manifest.tarballUrl,
        manifest.sha256,
        manifest.length,
        tmpArchive,
        options
      );
    }

    // Pre-install archive structure verification (pure-Node streaming inspection)
    await validateArchiveEntries(tmpArchive, null, options);

    if (!isSafeInstallTarget(tmpArchive)) {
      console.error(`● upgrade  ✗ refusing unsafe install target: ${tmpArchive}`);
      if (options.returnStatus) return { status: 1 };
      process.exit(1);
    }

    console.log(`● upgrade  installing verified local archive via npm...`);
    const npmCmd = process.platform === "win32" ? "npm.cmd" : "npm";
    const installArgs = ["install", "-g", tmpArchive];
    const spawner = options.spawnSync || spawnSync;
    let res;
    if (process.platform === "win32") {
      res = spawner(process.env.ComSpec || "cmd.exe", ["/d", "/c", npmCmd, ...installArgs], {
        stdio: options.stdio || "inherit",
        windowsHide: true,
        timeout: 60000
      });
    } else {
      res = spawner(npmCmd, installArgs, { stdio: options.stdio || "inherit", timeout: 60000 });
    }
    if (res.status !== 0) {
      if (options.returnStatus) return { status: res.status ?? 1 };
      process.exit(res.status ?? 1);
    }
    console.log(`● upgrade  ✓ kroma upgraded successfully.`);
    if (options.returnStatus) return { status: 0, archive: tmpArchive };
    process.exit(0);
  } catch (err) {
    console.error(`● upgrade  ✗ upgrade failed: ${err.message}`);
    if (options.returnStatus) return { status: 1, error: err };
    process.exit(1);
  } finally {
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch (_) {}
  }
}

async function downloadBinaryIfNeeded(destPath, key, options = {}) {
  if (fs.existsSync(destPath)) {
    return true;
  }

  const trustedKeys = options.trustedKeys !== undefined ? options.trustedKeys : PRODUCTION_TRUSTED_KEYS;
  if (!trustedKeys || typeof trustedKeys !== "object" || Object.keys(trustedKeys).length === 0) {
    console.error(
      `[kroma] production release trust root is unconfigured; on-demand binary download for ${key} is blocked.`
    );
    return false;
  }

  console.log(`[kroma] Fetching verified native pre-built binary for ${key}...`);
  fs.mkdirSync(path.dirname(destPath), { recursive: true });

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-dl-"));
  const tmpTarball = path.join(tmpDir, "kroma-bundle.tgz");

  try {
    let manifestText = options.manifestText;
    if (!manifestText) {
      const manifestUrl = options.manifestUrl || getManifestUrl(key);
      manifestText = await fetchText(manifestUrl, options);
      if (!manifestText) {
        console.error(`[kroma] failed to retrieve signed release manifest from ${manifestUrl}`);
        return false;
      }
    }

    const manifest = verifyManifest(manifestText, {
      trustedKeys,
      currentTarget: key,
      ...options
    });

    if (options.fixtureTarballPath) {
      fs.copyFileSync(options.fixtureTarballPath, tmpTarball);
      const actualSha = sha256File(tmpTarball);
      const actualLen = fs.statSync(tmpTarball).size;
      if (actualSha !== manifest.sha256 || actualLen !== manifest.length) {
        console.error(`[kroma] fixture tarball checksum/length mismatch; refusing to extract.`);
        return false;
      }
    } else {
      await downloadTarballWithIntegrity(
        manifest.tarballUrl,
        manifest.sha256,
        manifest.length,
        tmpTarball,
        options
      );
    }

    const subPath = `package/bin/vendor/${key}/${path.basename(destPath)}`;

    // Pure-Node streaming inspection and single binary extraction to exclusive 0755 temp file + atomic rename
    const extractRes = await inspectAndExtractTarStream(tmpTarball, subPath, destPath, options);
    if (!extractRes.extracted || !fs.existsSync(destPath)) {
      console.error(`[kroma] extraction failed: target binary not found in archive.`);
      return false;
    }

    try {
      fs.chmodSync(destPath, 0o755);
    } catch (_) {}

    return true;
  } catch (err) {
    console.error(`[kroma] verification or extraction failed: ${err.message}`);
    return false;
  } finally {
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch (_) {}
  }
}

async function resolveBinaryPath() {
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
  if (await downloadBinaryIfNeeded(bundled, platformKey)) {
    return bundled;
  }
  console.error(
    `[kroma] Native binary for ${process.platform}-${process.arch} was not found at ${bundled}.`
  );
  process.exit(1);
}

module.exports = {
  // Existing exports
  isValidHttpsTarballUrl,
  isSafeInstallTarget,
  parseSemver,
  compareSemver,
  // B14/F13 exports
  PRODUCTION_TRUSTED_KEYS,
  ALLOWED_TARBALL_HOSTS,
  ALLOWED_TARGETS,
  MAX_MANIFEST_BYTES,
  MAX_TARBALL_BYTES,
  MAX_ARCHIVE_ENTRIES,
  MAX_UNCOMPRESSED_BYTES,
  getManifestUrl,
  parseStrictJson,
  buildCanonicalSigningPayload,
  coercePublicKey,
  signManifest,
  serializeManifest,
  verifyManifest,
  validateTarEntryPath,
  inspectAndExtractTarStream,
  validateArchiveEntries,
  fetchText,
  fetchJson,
  downloadTarballWithIntegrity,
  downloadBinaryIfNeeded,
  handleSelfUpgrade
};

if (require.main === module) {
  const userArgs = process.argv.slice(2);
  if (userArgs[0] === "upgrade" || userArgs[0] === "self-update") {
    handleSelfUpgrade(userArgs.slice(1)).catch((err) => {
      console.error(`[kroma] ${err.message}`);
      process.exit(1);
    });
  } else {
    (async () => {
      const binPath = await resolveBinaryPath();
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
    })().catch((err) => {
      console.error(`[kroma] ${err.message}`);
      process.exit(1);
    });
  }
}
