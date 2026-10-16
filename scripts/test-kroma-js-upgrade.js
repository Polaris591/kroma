#!/usr/bin/env node
"use strict";

// Standalone regression and security test suite for `bin/kroma.js` (B14/F13 signed manifest,
// updater trust root, pure-Node tar header inspection, safe extraction, and mutation guards).
// Run directly: `node scripts/test-kroma-js-upgrade.js`.

const assert = require("node:assert");
const path = require("node:path");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const http = require("node:http");
const https = require("node:https");
const zlib = require("node:zlib");
const { spawnSync } = require("node:child_process");

const {
  isValidHttpsTarballUrl,
  isSafeInstallTarget,
  parseSemver,
  compareSemver,
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
  downloadTarballWithIntegrity,
  downloadBinaryIfNeeded,
  handleSelfUpgrade
} = require(path.join(__dirname, "..", "bin", "kroma.js"));

const { generateManifest } = require(path.join(__dirname, "generate-release-manifest.js"));

let passed = 0;
function check(name, fn) {
  try {
    fn();
    passed += 1;
    console.log(`  ok  ${name}`);
  } catch (err) {
    console.error(`FAIL  ${name}\n      ${err.message}`);
    process.exitCode = 1;
  }
}

async function checkAsync(name, fn) {
  try {
    await fn();
    passed += 1;
    console.log(`  ok  ${name}`);
  } catch (err) {
    console.error(`FAIL  ${name}\n      ${err.message}`);
    process.exitCode = 1;
  }
}

/**
 * Deterministic generator for synthetic in-memory tar.gz archives.
 */
function createSyntheticTarGz(entries) {
  const blocks = [];
  for (const e of entries) {
    const hdr = Buffer.alloc(512, 0);
    hdr.write(e.name || "file.txt", 0, 100, "utf8");
    hdr.write((e.mode || 0o644).toString(8).padStart(7, "0"), 100, 8, "ascii");
    hdr.write((e.uid || 0).toString(8).padStart(7, "0"), 108, 8, "ascii");
    hdr.write((e.gid || 0).toString(8).padStart(7, "0"), 116, 8, "ascii");
    const content = e.content ? Buffer.from(e.content) : Buffer.alloc(0);
    const size = e.size !== undefined ? e.size : content.length;
    hdr.write(size.toString(8).padStart(11, "0"), 124, 12, "ascii");
    hdr.write((e.mtime || Math.floor(Date.now() / 1000)).toString(8).padStart(11, "0"), 136, 12, "ascii");
    hdr[156] = (e.typeflag !== undefined ? e.typeflag : "0").charCodeAt(0);
    if (e.linkname) hdr.write(e.linkname, 157, 100, "utf8");
    hdr.write("ustar\0", 257, 6, "ascii");
    hdr.write("00", 263, 2, "ascii");
    if (e.prefix) hdr.write(e.prefix, 345, 155, "utf8");

    if (e.base256Size) {
      hdr[124] = 0x80;
    }

    if (e.corruptChecksum) {
      hdr.write("000000\0 ", 148, 8, "ascii");
    } else {
      let sum = 0;
      for (let i = 0; i < 512; i++) {
        sum += (i >= 148 && i < 156) ? 0x20 : hdr[i];
      }
      hdr.write(sum.toString(8).padStart(6, "0") + "\0 ", 148, 8, "ascii");
    }

    blocks.push(hdr);
    if (content.length > 0) {
      blocks.push(content);
      const pad = (512 - (content.length % 512)) % 512;
      if (pad > 0) blocks.push(Buffer.alloc(pad, 0));
    }
  }

  if (!entries._omitEof) {
    blocks.push(Buffer.alloc(1024, 0));
  }
  if (entries._trailingJunk) {
    blocks.push(Buffer.from(entries._trailingJunk));
  }

  const rawTar = Buffer.concat(blocks);
  return zlib.gzipSync(rawTar);
}

async function runAll() {
  console.log("isValidHttpsTarballUrl");
  check("accepts the pinned mirror tarball", () => {
    assert.strictEqual(isValidHttpsTarballUrl("https://cobblemon.asia/kroma.tgz"), true);
  });
  check("rejects a plain http URL", () => {
    assert.strictEqual(isValidHttpsTarballUrl("http://cobblemon.asia/kroma.tgz"), false);
  });
  check("rejects a `&` command chain", () => {
    assert.strictEqual(isValidHttpsTarballUrl("https://cobblemon.asia/kroma.tgz&calc.exe"), false);
  });
  check("rejects a `;` command separator", () => {
    assert.strictEqual(
      isValidHttpsTarballUrl("https://cobblemon.asia/kroma.tgz; rm -rf /"),
      false
    );
  });
  check("rejects a pipe into a shell", () => {
    assert.strictEqual(isValidHttpsTarballUrl("https://cobblemon.asia/kroma.tgz | sh"), false);
  });
  check("rejects embedded whitespace/newlines", () => {
    assert.strictEqual(
      isValidHttpsTarballUrl("https://cobblemon.asia/kroma.tgz\n& calc.exe"),
      false
    );
  });
  check("rejects a non-allow-listed host", () => {
    assert.strictEqual(isValidHttpsTarballUrl("https://evil.example.com/kroma.tgz"), false);
  });
  check("rejects embedded credentials", () => {
    assert.strictEqual(isValidHttpsTarballUrl("https://user:pass@cobblemon.asia/kroma.tgz"), false);
  });
  check("rejects a non-default port", () => {
    assert.strictEqual(isValidHttpsTarballUrl("https://cobblemon.asia:8443/kroma.tgz"), false);
  });
  check("rejects a query string", () => {
    assert.strictEqual(isValidHttpsTarballUrl("https://cobblemon.asia/kroma.tgz?x=1"), false);
  });
  check("rejects a non-https scheme", () => {
    assert.strictEqual(isValidHttpsTarballUrl("javascript:alert(1)"), false);
  });
  check("rejects empty and non-string input", () => {
    assert.strictEqual(isValidHttpsTarballUrl(""), false);
    assert.strictEqual(isValidHttpsTarballUrl(null), false);
    assert.strictEqual(isValidHttpsTarballUrl(42), false);
  });

  console.log("isSafeInstallTarget");
  check("accepts a plain npm package spec", () => {
    assert.strictEqual(isSafeInstallTarget("kroma-cli@latest"), true);
  });
  check("accepts a validated tarball URL", () => {
    assert.strictEqual(isSafeInstallTarget("https://cobblemon.asia/kroma.tgz"), true);
  });
  check("rejects a package spec with a command chain", () => {
    assert.strictEqual(isSafeInstallTarget("kroma-cli@latest & calc.exe"), false);
  });
  check("rejects a non-allow-listed tarball host", () => {
    assert.strictEqual(isSafeInstallTarget("https://evil.example.com/kroma.tgz"), false);
  });

  console.log("getManifestUrl (target-specific URLs)");
  check("generates target-specific manifest URLs", () => {
    assert.strictEqual(
      getManifestUrl("linux-x64"),
      "https://cobblemon.asia/manifests/kroma-manifest-linux-x64.json"
    );
    assert.strictEqual(
      getManifestUrl("darwin-arm64"),
      "https://cobblemon.asia/manifests/kroma-manifest-darwin-arm64.json"
    );
    assert.strictEqual(
      getManifestUrl("cli"),
      "https://cobblemon.asia/manifests/kroma-manifest-cli.json"
    );
  });

  console.log("parseSemver / compareSemver (strict, prerelease, leading zeros, large int)");
  check("compareSemver detects an upgrade", () => {
    assert.strictEqual(compareSemver("1.5.1", "1.5.0"), 1);
  });
  check("compareSemver detects equality", () => {
    assert.strictEqual(compareSemver("1.5.0", "1.5.0"), 0);
  });
  check("compareSemver detects a downgrade (mirror older than local)", () => {
    assert.strictEqual(compareSemver("1.4.9", "1.5.0"), -1);
  });
  check("compareSemver accepts a leading v", () => {
    assert.strictEqual(compareSemver("v1.5.1", "1.5.0"), 1);
  });
  check("compareSemver compares numerically, not lexicographically", () => {
    assert.strictEqual(compareSemver("2.0.0", "10.0.0"), -1);
    assert.strictEqual(compareSemver("1.10.0", "1.9.0"), 1);
  });
  check("compareSemver handles prerelease versions per SemVer 2.0.0", () => {
    assert.strictEqual(compareSemver("1.5.0", "1.5.0-rc.1"), 1);
    assert.strictEqual(compareSemver("1.5.0-rc.1", "1.5.0"), -1);
    assert.strictEqual(compareSemver("1.5.0-rc.1", "1.5.0-rc.2"), -1);
    assert.strictEqual(compareSemver("1.5.0-beta.2", "1.5.0-beta.10"), -1);
    assert.strictEqual(compareSemver("1.5.0-alpha", "1.5.0-beta"), -1);
  });
  check("parseSemver rejects leading zeros in numeric components", () => {
    assert.strictEqual(parseSemver("01.2.3"), null);
    assert.strictEqual(parseSemver("1.02.3"), null);
    assert.strictEqual(parseSemver("1.2.03"), null);
    assert.strictEqual(parseSemver("1.2.3-rc.01"), null);
  });
  check("parseSemver rejects integers exceeding MAX_SAFE_INTEGER", () => {
    assert.strictEqual(parseSemver("999999999999999999999.0.0"), null);
  });
  check("compareSemver returns null for unparseable values", () => {
    assert.strictEqual(compareSemver("latest", "1.5.0"), null);
    assert.strictEqual(compareSemver("1.5", "1.5.0"), null);
  });

  console.log("parseStrictJson (duplicate keys, unicode escapes, edge cases)");
  check("parses basic valid json object", () => {
    const res = parseStrictJson('{"name": "kroma", "count": 42, "ok": true, "arr": [1, 2]}');
    assert.strictEqual(res.name, "kroma");
    assert.strictEqual(res.count, 42);
    assert.strictEqual(res.ok, true);
    assert.deepStrictEqual(res.arr, [1, 2]);
  });
  check("rejects duplicate keys", () => {
    assert.throws(
      () => parseStrictJson('{"version": "1.0", "version": "2.0"}'),
      /Duplicate key in object: version/
    );
  });
  check("rejects duplicate keys via unicode escape", () => {
    assert.throws(
      () => parseStrictJson('{"version": "1.0", "\\u0076ersion": "2.0"}'),
      /Duplicate key in object: version/
    );
  });
  check("rejects unescaped control characters in strings", () => {
    assert.throws(
      () => parseStrictJson('{"a": "hello\nworld"}'),
      /Unescaped control character/
    );
    assert.throws(
      () => parseStrictJson('{"a": "hello\u0000world"}'),
      /Unescaped control character/
    );
  });
  check("rejects trailing commas in objects and arrays", () => {
    assert.throws(() => parseStrictJson('{"a": 1,}'), /Trailing comma in object/);
    assert.throws(() => parseStrictJson('[1, 2,]'), /Trailing comma in array/);
  });
  check("rejects numbers with leading zeros", () => {
    assert.throws(() => parseStrictJson('{"a": 01}'), /Leading zero in number/);
  });
  check("rejects extra tokens after root JSON value", () => {
    assert.throws(() => parseStrictJson('{"a": 1} true'), /Unexpected extra data after JSON/);
  });

  console.log("coercePublicKey & Ed25519 Public Key Formats");
  const fixtureKey = crypto.generateKeyPairSync("ed25519");
  const fixtureSpkiDer = fixtureKey.publicKey.export({ type: "spki", format: "der" });
  const fixtureRaw32Hex = fixtureSpkiDer.subarray(12).toString("hex");
  const fixtureRaw32Base64 = fixtureSpkiDer.subarray(12).toString("base64");
  const fixturePem = fixtureKey.publicKey.export({ type: "spki", format: "pem" });

  check("coerces raw 32-byte hex public key", () => {
    const keyObj = coercePublicKey(fixtureRaw32Hex);
    assert.strictEqual(keyObj.asymmetricKeyType, "ed25519");
  });
  check("coerces raw 32-byte base64 public key", () => {
    const keyObj = coercePublicKey(fixtureRaw32Base64);
    assert.strictEqual(keyObj.asymmetricKeyType, "ed25519");
  });
  check("coerces SPKI DER buffer", () => {
    const keyObj = coercePublicKey(fixtureSpkiDer);
    assert.strictEqual(keyObj.asymmetricKeyType, "ed25519");
  });
  check("coerces PEM public key", () => {
    const keyObj = coercePublicKey(fixturePem);
    assert.strictEqual(keyObj.asymmetricKeyType, "ed25519");
  });
  check("coerces KeyObject directly", () => {
    const keyObj = coercePublicKey(fixtureKey.publicKey);
    assert.strictEqual(keyObj, fixtureKey.publicKey);
  });
  check("rejects invalid key format", () => {
    assert.throws(() => coercePublicKey("invalid-key-hex"), /Invalid Ed25519 public key format/);
    assert.throws(() => coercePublicKey(12345), /Invalid Ed25519 public key format/);
  });

  console.log("buildCanonicalSigningPayload & signManifest");
  const validBaseFields = {
    schemaVersion: 1,
    keyId: "kroma-test-key-2026",
    version: "1.5.1",
    target: "linux-x64",
    tarballUrl: "https://cobblemon.asia/kroma-linux-x64-1.5.1.tgz",
    sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    length: 1048576,
    expires: 1893456000,
    antirollback: "1.5.0"
  };

  check("buildCanonicalSigningPayload produces deterministic domain-separated bytes", () => {
    const payload = buildCanonicalSigningPayload(validBaseFields);
    const expected = [
      "kroma-signed-manifest:v1",
      "antirollback:1.5.0",
      "expires:1893456000",
      "keyId:kroma-test-key-2026",
      "length:1048576",
      "schemaVersion:1",
      "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
      "tarballUrl:https://cobblemon.asia/kroma-linux-x64-1.5.1.tgz",
      "target:linux-x64",
      "version:1.5.1",
      ""
    ].join("\n");
    assert.strictEqual(payload.toString("utf8"), expected);
  });

  check("signManifest creates verifiable 88-char base64 Ed25519 signature", () => {
    const signed = signManifest(validBaseFields, fixtureKey.privateKey);
    assert.strictEqual(signed.signature.length, 88);
    assert.match(signed.signature, /^[A-Za-z0-9+/]{86}==$/);
    const payload = buildCanonicalSigningPayload(signed);
    const verified = crypto.verify(
      null,
      payload,
      fixtureKey.publicKey,
      Buffer.from(signed.signature, "base64")
    );
    assert.strictEqual(verified, true);
  });

  console.log("verifyManifest Security Invariants");
  const testTrustedKeys = { "kroma-test-key-2026": fixtureRaw32Hex };
  const validSignedManifest = signManifest(validBaseFields, fixtureKey.privateKey);
  const validManifestText = serializeManifest(validSignedManifest);

  check("accepts a valid signed manifest with test trust root", () => {
    const verified = verifyManifest(validManifestText, {
      trustedKeys: testTrustedKeys,
      currentTarget: "linux-x64",
      currentTime: 1760000000,
      currentVersion: "1.5.0"
    });
    assert.strictEqual(verified.version, "1.5.1");
    assert.strictEqual(verified.target, "linux-x64");
  });

  check("fails closed when production trust root is empty / unconfigured", () => {
    assert.strictEqual(Object.keys(PRODUCTION_TRUSTED_KEYS).length, 0);
    assert.throws(
      () => verifyManifest(validManifestText, { trustedKeys: PRODUCTION_TRUSTED_KEYS }),
      /Production release trust root is unconfigured; self-upgrade is blocked/
    );
    assert.throws(
      () => verifyManifest(validManifestText, {}),
      /Production release trust root is unconfigured; self-upgrade is blocked/
    );
  });

  check("rejects signing key not present in trusted keys", () => {
    assert.throws(
      () => verifyManifest(validManifestText, { trustedKeys: { "other-key": fixtureRaw32Hex } }),
      /Unknown or untrusted signing key: kroma-test-key-2026/
    );
  });

  check("rejects tampered signature", () => {
    const tampered = JSON.parse(validManifestText);
    const sigBuf = Buffer.from(tampered.signature, "base64");
    sigBuf[0] ^= 0x55;
    tampered.signature = sigBuf.toString("base64");
    assert.throws(
      () => verifyManifest(serializeManifest(tampered), { trustedKeys: testTrustedKeys }),
      /Signature verification failed for release manifest/
    );
  });

  check("rejects tampered version after signing", () => {
    const tampered = JSON.parse(validManifestText);
    tampered.version = "1.5.2";
    assert.throws(
      () => verifyManifest(serializeManifest(tampered), { trustedKeys: testTrustedKeys }),
      /Signature verification failed for release manifest/
    );
  });

  check("rejects tampered target after signing", () => {
    const tampered = JSON.parse(validManifestText);
    tampered.target = "win32-x64";
    assert.throws(
      () => verifyManifest(serializeManifest(tampered), { trustedKeys: testTrustedKeys, currentTarget: "win32-x64" }),
      /Signature verification failed for release manifest/
    );
  });

  check("rejects tampered sha256 after signing", () => {
    const tampered = JSON.parse(validManifestText);
    tampered.sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    assert.throws(
      () => verifyManifest(serializeManifest(tampered), { trustedKeys: testTrustedKeys }),
      /Signature verification failed for release manifest/
    );
  });

  check("rejects tampered length after signing", () => {
    const tampered = JSON.parse(validManifestText);
    tampered.length = 9999999;
    assert.throws(
      () => verifyManifest(serializeManifest(tampered), { trustedKeys: testTrustedKeys }),
      /Signature verification failed for release manifest/
    );
  });

  check("rejects tampered tarballUrl after signing", () => {
    const tampered = JSON.parse(validManifestText);
    tampered.tarballUrl = "https://cobblemon.asia/evil.tgz";
    assert.throws(
      () => verifyManifest(serializeManifest(tampered), { trustedKeys: testTrustedKeys }),
      /Signature verification failed for release manifest/
    );
  });

  check("rejects expired manifest", () => {
    const expiredTime = validBaseFields.expires + 1000;
    assert.throws(
      () =>
        verifyManifest(validManifestText, {
          trustedKeys: testTrustedKeys,
          currentTime: expiredTime
        }),
      /Manifest has expired/
    );
  });

  check("rejects platform target mismatch", () => {
    assert.throws(
      () =>
        verifyManifest(validManifestText, {
          trustedKeys: testTrustedKeys,
          currentTarget: "darwin-arm64"
        }),
      /Manifest target mismatch: expected darwin-arm64, got linux-x64/
    );
  });

  check("rejects version downgrade below local installed version", () => {
    assert.throws(
      () =>
        verifyManifest(validManifestText, {
          trustedKeys: testTrustedKeys,
          currentVersion: "1.6.0"
        }),
      /Version downgrade rejected: remote v1.5.1 is older than local v1.6.0/
    );
  });

  check("rejects version below antirollback floor", () => {
    const badRollback = Object.assign({}, validBaseFields, {
      version: "1.4.0",
      antirollback: "1.5.0"
    });
    const signedBadRollback = signManifest(badRollback, fixtureKey.privateKey);
    assert.throws(
      () =>
        verifyManifest(serializeManifest(signedBadRollback), {
          trustedKeys: testTrustedKeys,
          currentVersion: "1.3.0"
        }),
      /Version violates antirollback floor: remote v1.4.0 is below floor v1.5.0/
    );
  });

  check("antirollback in manifest cannot lower local installed version floor", () => {
    const loweredFloor = Object.assign({}, validBaseFields, {
      version: "1.4.0",
      antirollback: "1.0.0"
    });
    const signedLowered = signManifest(loweredFloor, fixtureKey.privateKey);
    assert.throws(
      () =>
        verifyManifest(serializeManifest(signedLowered), {
          trustedKeys: testTrustedKeys,
          currentVersion: "1.5.0"
        }),
      /Version downgrade rejected/
    );
  });

  check("fails closed when version comparison fails", () => {
    const invalidVerManifest = Object.assign({}, validSignedManifest, {
      version: "1.5.1-invalid..pre"
    });
    assert.throws(
      () => verifyManifest(JSON.stringify(invalidVerManifest), { trustedKeys: testTrustedKeys }),
      /Invalid version/
    );
  });

  check("rejects unknown field in manifest schema", () => {
    const withUnknown = Object.assign({}, validSignedManifest, { backdoor: true });
    assert.throws(
      () => verifyManifest(JSON.stringify(withUnknown), { trustedKeys: testTrustedKeys }),
      /Unknown field in manifest: backdoor/
    );
  });

  check("rejects non-canonical base64 signature", () => {
    const tampered = Object.assign({}, validSignedManifest, {
      signature: validSignedManifest.signature.slice(0, -2)
    });
    assert.throws(
      () => verifyManifest(serializeManifest(tampered), { trustedKeys: testTrustedKeys }),
      /Invalid signature: must be canonical 88-char base64/
    );
  });

  check("rejects manifest exceeding MAX_MANIFEST_BYTES (64KB)", () => {
    const huge = " ".repeat(MAX_MANIFEST_BYTES + 10) + validManifestText;
    assert.throws(
      () => verifyManifest(huge, { trustedKeys: testTrustedKeys }),
      /Manifest exceeds maximum allowed size/
    );
  });

  console.log("Pure-Node Tar Stream Inspection & Path Security (BSD/GNU Column Immunity)");
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-test-sec-"));

  // Helper to write archive and test validation
  async function testArchiveRejection(entries, expectedPattern, opts = {}) {
    const archivePath = path.join(tmpDir, `test-${Date.now()}-${Math.random().toString(36).slice(2)}.tgz`);
    fs.writeFileSync(archivePath, createSyntheticTarGz(entries));
    try {
      await assert.rejects(
        () => validateArchiveEntries(archivePath, null, opts),
        expectedPattern
      );
    } finally {
      try { fs.unlinkSync(archivePath); } catch (_) {}
    }
  }

  await checkAsync("accepts clean archive with regular files and directory entries", async () => {
    const archivePath = path.join(tmpDir, "clean.tgz");
    fs.writeFileSync(archivePath, createSyntheticTarGz([
      { name: "package/", typeflag: "5" },
      { name: "package/bin/", typeflag: "5" },
      { name: "package/bin/kroma", typeflag: "0", content: "#!/bin/sh\necho ok\n" }
    ]));
    const res = await validateArchiveEntries(archivePath, "package/bin/kroma");
    assert.strictEqual(res, true);
    fs.unlinkSync(archivePath);
  });

  await checkAsync("extracts single target binary to destination path with 0755 mode", async () => {
    const archivePath = path.join(tmpDir, "binary-pkg.tgz");
    const binContent = "#!/bin/sh\necho 'hello binary'\n";
    fs.writeFileSync(archivePath, createSyntheticTarGz([
      { name: "package/bin/vendor/linux-x64/kroma", typeflag: "0", content: binContent }
    ]));
    const destPath = path.join(tmpDir, "extracted-bin");
    const extractRes = await inspectAndExtractTarStream(archivePath, "package/bin/vendor/linux-x64/kroma", destPath);
    assert.strictEqual(extractRes.extracted, true);
    assert.strictEqual(fs.existsSync(destPath), true);
    assert.strictEqual(fs.readFileSync(destPath, "utf8"), binContent);
    const mode = fs.statSync(destPath).mode & 0o777;
    assert.strictEqual((mode & 0o111) !== 0, true, "Extracted binary must have executable permissions");
    fs.unlinkSync(archivePath);
    fs.unlinkSync(destPath);
  });

  await checkAsync("rejects leading slash absolute path", async () => {
    await testArchiveRejection(
      [{ name: "/etc/passwd", typeflag: "0", content: "root:x:0:0:::" }],
      /Absolute entry path forbidden/
    );
  });

  await checkAsync("rejects leading backslash absolute path", async () => {
    await testArchiveRejection(
      [{ name: "\\Windows\\System32\\cmd.exe", typeflag: "0", content: "evil" }],
      /Absolute entry path forbidden/
    );
  });

  await checkAsync("rejects path traversal .. in segments", async () => {
    await testArchiveRejection(
      [{ name: "package/../../etc/shadow", typeflag: "0", content: "evil" }],
      /Path traversal '\.\.' forbidden/
    );
  });

  await checkAsync("rejects redundant '.' segment", async () => {
    await testArchiveRejection(
      [{ name: "package/./kroma", typeflag: "0", content: "evil" }],
      /Redundant '\.' segment forbidden/
    );
  });

  await checkAsync("rejects Windows drive letters", async () => {
    await testArchiveRejection(
      [{ name: "C:evil.dll", typeflag: "0", content: "evil" }],
      /Windows drive path forbidden/
    );
  });

  await checkAsync("rejects backslash in paths across all platforms", async () => {
    await testArchiveRejection(
      [{ name: "package\\bin\\kroma", typeflag: "0", content: "evil" }],
      /Backslash in entry path forbidden/
    );
  });

  await checkAsync("rejects Windows ADS colon in paths", async () => {
    await testArchiveRejection(
      [{ name: "file.txt:stream", typeflag: "0", content: "evil" }],
      /Colon \/ ADS in entry path forbidden/
    );
  });

  await checkAsync("rejects UNC paths", async () => {
    await testArchiveRejection(
      [{ name: "//attacker/share/evil", typeflag: "0", content: "evil" }],
      /Absolute entry path forbidden|\bUNC\b/
    );
  });

  await checkAsync("rejects trailing dot in path segments", async () => {
    await testArchiveRejection(
      [{ name: "package./bin/kroma", typeflag: "0", content: "evil" }],
      /Trailing dot or space forbidden/
    );
  });

  await checkAsync("rejects trailing space in path segments", async () => {
    await testArchiveRejection(
      [{ name: "package /bin/kroma", typeflag: "0", content: "evil" }],
      /Trailing dot or space forbidden/
    );
  });

  await checkAsync("rejects Windows reserved device names", async () => {
    await testArchiveRejection(
      [{ name: "package/CON.txt", typeflag: "0", content: "evil" }],
      /Windows reserved device name forbidden/
    );
  });

  await checkAsync("rejects symbolic links (typeflag '2')", async () => {
    await testArchiveRejection(
      [{ name: "evil-link", typeflag: "2", linkname: "/etc/passwd" }],
      /Unsupported or dangerous tar entry type '2'/
    );
  });

  await checkAsync("rejects hard links (typeflag '1')", async () => {
    await testArchiveRejection(
      [{ name: "evil-hard", typeflag: "1", linkname: "target" }],
      /Unsupported or dangerous tar entry type '1'/
    );
  });

  await checkAsync("rejects FIFOs / named pipes (typeflag '6')", async () => {
    await testArchiveRejection(
      [{ name: "evil-fifo", typeflag: "6" }],
      /Unsupported or dangerous tar entry type '6'/
    );
  });

  await checkAsync("rejects character devices (typeflag '3')", async () => {
    await testArchiveRejection(
      [{ name: "evil-char", typeflag: "3" }],
      /Unsupported or dangerous tar entry type '3'/
    );
  });

  await checkAsync("rejects block devices (typeflag '4')", async () => {
    await testArchiveRejection(
      [{ name: "evil-block", typeflag: "4" }],
      /Unsupported or dangerous tar entry type '4'/
    );
  });

  await checkAsync("rejects PAX extended headers (typeflag 'x')", async () => {
    await testArchiveRejection(
      [{ name: "PaxHeader/evil", typeflag: "x", content: "30 path=longpath\n" }],
      /Unsupported or dangerous tar entry type 'x'/
    );
  });

  await checkAsync("rejects GNU longname headers (typeflag 'L')", async () => {
    await testArchiveRejection(
      [{ name: "././@LongLink", typeflag: "L", content: "longname-target\0" }],
      /Unsupported or dangerous tar entry type 'L'/
    );
  });

  await checkAsync("rejects base-256 binary tar size encoding", async () => {
    await testArchiveRejection(
      [{ name: "huge.bin", typeflag: "0", base256Size: true }],
      /Base-256 binary tar size encoding is forbidden/
    );
  });

  await checkAsync("rejects corrupted header checksum", async () => {
    await testArchiveRejection(
      [{ name: "corrupted.txt", typeflag: "0", corruptChecksum: true, content: "ok" }],
      /Tar header checksum mismatch/
    );
  });

  await checkAsync("rejects truncated tar archive (missing EOF blocks)", async () => {
    const entries = [{ name: "truncated.txt", typeflag: "0", content: "data" }];
    entries._omitEof = true;
    await testArchiveRejection(
      entries,
      /Truncated tar archive: missing terminating zero blocks/
    );
  });

  await checkAsync("rejects non-zero trailing data after EOF blocks", async () => {
    const entries = [{ name: "file.txt", typeflag: "0", content: "data" }];
    entries._trailingJunk = "TRAILER_DATA";
    await testArchiveRejection(
      entries,
      /Non-zero trailing data found after tar EOF block/
    );
  });

  await checkAsync("rejects non-zero trailing data across 16KiB chunk boundary after EOF blocks", async () => {
    const blocks = [];
    const hdr = Buffer.alloc(512, 0);
    hdr.write("file.txt", 0, 100);
    hdr.write("0000644", 100, 8);
    const fileSize = 29 * 512;
    hdr.write(fileSize.toString(8).padStart(11, "0"), 124, 12);
    hdr[156] = "0".charCodeAt(0);
    hdr.write("ustar\0", 257, 6);
    hdr.write("00", 263, 2);
    let sum = 0;
    for (let i = 0; i < 512; i++) sum += (i >= 148 && i < 156) ? 0x20 : hdr[i];
    hdr.write(sum.toString(8).padStart(6, "0") + "\0 ", 148, 8);
    blocks.push(hdr);
    blocks.push(Buffer.alloc(fileSize, 0x20));
    blocks.push(Buffer.alloc(1024, 0));
    blocks.push(Buffer.alloc(21, 0x41));

    const tgzPath = path.join(tmpDir, "chunk-trailer.tgz");
    fs.writeFileSync(tgzPath, zlib.gzipSync(Buffer.concat(blocks)));
    try {
      await assert.rejects(
        () => validateArchiveEntries(tgzPath),
        /Non-zero trailing data found after tar EOF block/
      );
    } finally {
      try { fs.unlinkSync(tgzPath); } catch (_) {}
    }
  });

  await checkAsync("rejects duplicate paths with differing trailing slashes", async () => {
    await testArchiveRejection(
      [
        { name: "package/dir/", typeflag: "5" },
        { name: "package/dir", typeflag: "0", content: "data" }
      ],
      /Duplicate entry path in archive/
    );
  });

  await checkAsync("rejects child entry when ancestor is a regular file", async () => {
    await testArchiveRejection(
      [
        { name: "package/bin", typeflag: "0", content: "file-content" },
        { name: "package/bin/kroma", typeflag: "0", content: "child-content" }
      ],
      /Parent path is a file, cannot be directory/
    );
  });

  await checkAsync("rejects file entry when path conflicts with existing directory", async () => {
    await testArchiveRejection(
      [
        { name: "package/bin/kroma", typeflag: "0", content: "child-content" },
        { name: "package/bin", typeflag: "0", content: "file-content" }
      ],
      /File path conflicts with existing directory/
    );
  });

  await checkAsync("outStream wx collision does not unlink pre-existing collision file", async () => {
    const collisionFile = path.join(tmpDir, "collision-file.bin");
    fs.writeFileSync(collisionFile, "ORIGINAL_COLLISION_DATA");
    const archivePath = path.join(tmpDir, "col-archive.tgz");
    fs.writeFileSync(archivePath, createSyntheticTarGz([
      { name: "target.bin", typeflag: "0", content: "NEW_DATA" }
    ]));
    const realNow = Date.now;
    const fixedNow = 123456789;
    Date.now = () => fixedNow;
    const expectedStage = `${collisionFile}.tmp.${process.pid}.${fixedNow}`;
    fs.writeFileSync(expectedStage, "PRE_EXISTING_STAGE_FILE");
    try {
      await assert.rejects(
        () => inspectAndExtractTarStream(archivePath, "target.bin", collisionFile),
        /EEXIST/
      );
      assert.strictEqual(fs.existsSync(expectedStage), true, "Pre-existing collision file must not be unlinked");
      assert.strictEqual(fs.readFileSync(expectedStage, "utf8"), "PRE_EXISTING_STAGE_FILE");
    } finally {
      Date.now = realNow;
      try { fs.unlinkSync(archivePath); } catch (_) {}
      try { fs.unlinkSync(expectedStage); } catch (_) {}
      try { fs.unlinkSync(collisionFile); } catch (_) {}
    }
  });

  await checkAsync("outStream error handler catches ENOSPC and cleans up created stage file", async () => {
    const destFile = path.join(tmpDir, "enospc-dest.bin");
    const archivePath = path.join(tmpDir, "enospc-archive.tgz");
    fs.writeFileSync(archivePath, createSyntheticTarGz([
      { name: "target.bin", typeflag: "0", content: "A".repeat(1024) }
    ]));
    const realCreateWriteStream = fs.createWriteStream;
    let stageCreatedPath = null;
    fs.createWriteStream = function(...args) {
      const ws = realCreateWriteStream.apply(this, args);
      stageCreatedPath = args[0];
      ws.on("open", () => {
        process.nextTick(() => ws.emit("error", new Error("ENOSPC: no space left on device")));
      });
      return ws;
    };
    try {
      await assert.rejects(
        () => inspectAndExtractTarStream(archivePath, "target.bin", destFile),
        /ENOSPC/
      );
      assert.strictEqual(fs.existsSync(destFile), false, "Target dest must not be created");
      if (stageCreatedPath) {
        assert.strictEqual(fs.existsSync(stageCreatedPath), false, "Created stage file must be cleaned up on write error");
      }
    } finally {
      fs.createWriteStream = realCreateWriteStream;
      try { fs.unlinkSync(archivePath); } catch (_) {}
    }
  });

  await checkAsync("validates real npm pack output archive successfully", async () => {
    const packDir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-pack-"));
    try {
      const packRes = spawnSync("npm", ["pack", "--ignore-scripts", path.resolve(__dirname, "..", "npm", "kroma-cli")], {
        cwd: packDir,
        encoding: "utf8"
      });
      assert.strictEqual(packRes.status, 0, "npm pack must succeed");
      const tarballName = packRes.stdout.trim().split("\n").pop().trim();
      const tarballPath = path.join(packDir, tarballName);
      assert.strictEqual(fs.existsSync(tarballPath), true);
      const res = await validateArchiveEntries(tarballPath);
      assert.strictEqual(res, true);
    } finally {
      try { fs.rmSync(packDir, { recursive: true, force: true }); } catch (_) {}
    }
  });

  await checkAsync("rejects uncompressed byte bomb exceeding limit", async () => {
    const largeContent = "A".repeat(5000);
    await testArchiveRejection(
      [{ name: "bomb.txt", typeflag: "0", content: largeContent }],
      /Archive exceeded maximum uncompressed byte limit/,
      { maxUncompressedBytes: 2000 }
    );
  });

  await checkAsync("rejects entry count bomb exceeding limit", async () => {
    const manyEntries = [];
    for (let i = 0; i < 15; i++) {
      manyEntries.push({ name: `file_${i}.txt`, typeflag: "0", content: "x" });
    }
    await testArchiveRejection(
      manyEntries,
      /Archive exceeded maximum entry limit/,
      { maxArchiveEntries: 10 }
    );
  });

  await checkAsync("rejects duplicate entry paths (including case-insensitive)", async () => {
    await testArchiveRejection(
      [
        { name: "package/file.txt", typeflag: "0", content: "first" },
        { name: "package/FILE.TXT", typeflag: "0", content: "second" }
      ],
      /Duplicate entry path in archive/
    );
  });

  await checkAsync("rejects extraction when target entry is missing", async () => {
    const archivePath = path.join(tmpDir, "missing-target.tgz");
    fs.writeFileSync(archivePath, createSyntheticTarGz([
      { name: "other.txt", typeflag: "0", content: "other" }
    ]));
    await assert.rejects(
      () => inspectAndExtractTarStream(archivePath, "missing.bin", path.join(tmpDir, "out")),
      /Target entry missing\.bin not found in archive/
    );
    fs.unlinkSync(archivePath);
  });

  console.log("Deadlines & Redirect Security in Pure Node Fetchers");
  await checkAsync("fetchText enforces absolute deadline", async () => {
    const hangServer = http.createServer((req, res) => {});
    await new Promise((resolve) => hangServer.listen(0, resolve));
    const port = hangServer.address().port;

    await assert.rejects(
      () => fetchText(`http://127.0.0.1:${port}/manifest.json`, { deadlineMs: 50 }),
      /Refusing unsafe URL/
    );
    hangServer.close();
  });

  console.log("Production Upgrade Path Verification (Hash+Length & Never Spawn NPM on URL/Tamper)");
  const cliTgzData = createSyntheticTarGz([
    { name: "package/", typeflag: "5" },
    { name: "package/package.json", typeflag: "0", content: '{"name":"kroma","version":"1.5.1"}' }
  ]);
  const cliTgz = path.join(tmpDir, "kroma-cli-1.5.1.tgz");
  fs.writeFileSync(cliTgz, cliTgzData);

  const cliTgzSha = crypto.createHash("sha256").update(cliTgzData).digest("hex");
  const cliTgzLen = cliTgzData.length;

  const cliManifestFields = {
    schemaVersion: 1,
    keyId: "kroma-test-key-2026",
    version: "1.5.1",
    target: "cli",
    tarballUrl: "https://cobblemon.asia/kroma-cli-1.5.1.tgz",
    sha256: cliTgzSha,
    length: cliTgzLen,
    expires: 1893456000,
    antirollback: "1.5.0"
  };
  const cliManifestSigned = signManifest(cliManifestFields, fixtureKey.privateKey);
  const cliManifestText = serializeManifest(cliManifestSigned);

  await checkAsync("handleSelfUpgrade downloads archive, verifies sha/length, and passes local path to npm", async () => {
    let spawnedCmd = null;
    let spawnedArgs = null;
    const mockSpawnSync = (cmd, args) => {
      spawnedCmd = cmd;
      spawnedArgs = args;
      return { status: 0 };
    };

    const res = await handleSelfUpgrade([], {
      trustedKeys: testTrustedKeys,
      manifestText: cliManifestText,
      fixtureTarballPath: cliTgz,
      spawnSync: mockSpawnSync,
      currentVersion: "1.5.0",
      returnStatus: true
    });

    assert.strictEqual(res.status, 0);
    assert.ok(spawnedArgs, "npm spawnSync must be called");
    assert.strictEqual(spawnedArgs[0], "install");
    assert.strictEqual(spawnedArgs[1], "-g");
    const passedTarget = spawnedArgs[2];
    assert.ok(path.isAbsolute(passedTarget), "Install target passed to npm must be local absolute path");
    assert.ok(passedTarget.endsWith(".tgz"), "Install target passed to npm must be .tgz archive");
    assert.strictEqual(passedTarget.startsWith("https:"), false, "NPM target must NEVER be a remote URL");
  });

  await checkAsync("handleSelfUpgrade NEVER spawns npm if tarball is tampered", async () => {
    let spawned = false;
    const mockSpawnSync = () => {
      spawned = true;
      return { status: 0 };
    };

    const corruptedTgz = path.join(tmpDir, "corrupted.tgz");
    fs.writeFileSync(corruptedTgz, "corrupted-payload");

    const res = await handleSelfUpgrade([], {
      trustedKeys: testTrustedKeys,
      manifestText: cliManifestText,
      fixtureTarballPath: corruptedTgz,
      spawnSync: mockSpawnSync,
      currentVersion: "1.5.0",
      returnStatus: true
    });

    assert.strictEqual(res.status, 1);
    assert.strictEqual(spawned, false, "NPM must NEVER be spawned when tarball hash/length is tampered");
  });

  console.log("downloadBinaryIfNeeded Pure Node & Target-Specific Verification");
  await checkAsync("downloadBinaryIfNeeded fails closed when trust root is empty", async () => {
    const missingBin = path.join(tmpDir, "missing-bin-1");
    const res = await downloadBinaryIfNeeded(missingBin, "linux-x64", {
      trustedKeys: PRODUCTION_TRUSTED_KEYS
    });
    assert.strictEqual(res, false);
    assert.strictEqual(fs.existsSync(missingBin), false);
  });

  await checkAsync("downloadBinaryIfNeeded succeeds with fixture tarball and sets 0755", async () => {
    const targetBin = path.join(tmpDir, "installed-kroma");
    const binContent = "#!/bin/sh\necho 'hello from test binary'\n";
    const fixtureTgzData = createSyntheticTarGz([
      { name: "package/", typeflag: "5" },
      { name: "package/bin/", typeflag: "5" },
      { name: "package/bin/vendor/", typeflag: "5" },
      { name: "package/bin/vendor/linux-x64/", typeflag: "5" },
      { name: "package/bin/vendor/linux-x64/installed-kroma", typeflag: "0", content: binContent, mode: 0o755 }
    ]);
    const fixtureTgz = path.join(tmpDir, "fixture.tgz");
    fs.writeFileSync(fixtureTgz, fixtureTgzData);

    const fixtureTgzSha = crypto.createHash("sha256").update(fixtureTgzData).digest("hex");
    const fixtureTgzLen = fixtureTgzData.length;

    const manifestFields = Object.assign({}, validBaseFields, {
      sha256: fixtureTgzSha,
      length: fixtureTgzLen,
      target: "linux-x64"
    });
    const manifestObj = signManifest(manifestFields, fixtureKey.privateKey);
    const manifestStr = serializeManifest(manifestObj);

    const res = await downloadBinaryIfNeeded(targetBin, "linux-x64", {
      trustedKeys: testTrustedKeys,
      manifestText: manifestStr,
      fixtureTarballPath: fixtureTgz
    });

    assert.strictEqual(res, true);
    assert.strictEqual(fs.existsSync(targetBin), true);
    assert.strictEqual(fs.readFileSync(targetBin, "utf8"), binContent);
    const mode = fs.statSync(targetBin).mode & 0o777;
    assert.strictEqual((mode & 0o111) !== 0, true, "Installed binary must be executable");
  });

  console.log("Offline / CI Release Manifest Generation (scripts/generate-release-manifest.js)");
  const privateKeyPemFile = path.join(tmpDir, "ed25519-private.pem");
  fs.writeFileSync(privateKeyPemFile, fixtureKey.privateKey.export({ type: "pkcs8", format: "pem" }));

  await checkAsync("generateManifest produces valid signed manifest verified by verifyManifest", async () => {
    const genOut = path.join(tmpDir, "generated-manifest.json");
    const manifest = await generateManifest({
      target: "linux-x64",
      version: "1.5.1",
      tarball: cliTgz,
      url: "https://cobblemon.asia/dist/kroma-linux-x64.tgz",
      keyId: "kroma-test-key-2026",
      privateKeyFile: privateKeyPemFile,
      out: genOut
    });

    assert.strictEqual(fs.existsSync(genOut), true);
    const readText = fs.readFileSync(genOut, "utf8");
    const verified = verifyManifest(readText, {
      trustedKeys: testTrustedKeys,
      currentTarget: "linux-x64"
    });
    assert.strictEqual(verified.version, "1.5.1");
    assert.strictEqual(verified.target, "linux-x64");
    assert.strictEqual(verified.sha256, crypto.createHash("sha256").update(fs.readFileSync(cliTgz)).digest("hex"));
  });

  await checkAsync("generateManifest rejects output to a symlink", async () => {
    const symlinkOut = path.join(tmpDir, "symlink-manifest.json");
    fs.symlinkSync(cliTgz, symlinkOut);
    await assert.rejects(
      () =>
        generateManifest({
          target: "linux-x64",
          version: "1.5.1",
          tarball: cliTgz,
          url: "https://cobblemon.asia/dist/kroma-linux-x64.tgz",
          keyId: "kroma-test-key-2026",
          privateKeyFile: privateKeyPemFile,
          out: symlinkOut
        }),
      /Output file path is a symbolic link/
    );
  });

  console.log("Real Code Guard Mutation Tests (Removing Guards Fails Test Immediately)");

  check("mutation guard check 1: disabling signature verification allows tampered payload", () => {
    const originalSrc = fs.readFileSync(path.join(__dirname, "..", "bin", "kroma.js"), "utf8");
    const mutatedSrc = originalSrc.replace(
      "const signatureValid = crypto.verify(null, canonicalPayload, publicKey, sigBuf);",
      "const signatureValid = true; // MUTATION: guard removed"
    ).replace(/^#!.*\n/, "");
    assert.notStrictEqual(originalSrc, mutatedSrc, "Guard must exist in source to be mutated");

    const mutatedModule = { exports: {} };
    const fn = new Function("module", "exports", "require", "__dirname", mutatedSrc);
    fn(mutatedModule, mutatedModule.exports, require, path.join(__dirname, "..", "bin"));

    const tampered = JSON.parse(validManifestText);
    tampered.signature = Buffer.alloc(64, 0x11).toString("base64");
    const mutantVerified = mutatedModule.exports.verifyManifest(JSON.stringify(tampered), {
      trustedKeys: testTrustedKeys
    });
    assert.ok(mutantVerified, "Mutant with removed signature guard allowed tampered manifest");

    assert.throws(
      () => verifyManifest(JSON.stringify(tampered), { trustedKeys: testTrustedKeys }),
      /Signature verification failed/
    );
  });

  check("mutation guard check 2: disabling antirollback check allows version downgrade", () => {
    const originalSrc = fs.readFileSync(path.join(__dirname, "..", "bin", "kroma.js"), "utf8");
    const mutatedSrc = originalSrc.replace(
      "if (cmpFloor < 0) {",
      "if (false && cmpFloor < 0) { // MUTATION: guard removed"
    ).replace(/^#!.*\n/, "");
    assert.notStrictEqual(originalSrc, mutatedSrc, "Guard must exist in source to be mutated");

    const mutatedModule = { exports: {} };
    const fn = new Function("module", "exports", "require", "__dirname", mutatedSrc);
    fn(mutatedModule, mutatedModule.exports, require, path.join(__dirname, "..", "bin"));

    const badRollback = Object.assign({}, validBaseFields, {
      version: "1.4.0",
      antirollback: "1.5.0"
    });
    const signedBadRollback = signManifest(badRollback, fixtureKey.privateKey);

    const mutantVerified = mutatedModule.exports.verifyManifest(serializeManifest(signedBadRollback), {
      trustedKeys: testTrustedKeys,
      currentVersion: "1.3.0"
    });
    assert.ok(mutantVerified, "Mutant with removed antirollback guard allowed downgrade");

    assert.throws(
      () =>
        verifyManifest(serializeManifest(signedBadRollback), {
          trustedKeys: testTrustedKeys,
          currentVersion: "1.3.0"
        }),
      /Version violates antirollback floor/
    );
  });

  check("mutation guard check 3: disabling tar path traversal check allows '..' path", () => {
    const originalSrc = fs.readFileSync(path.join(__dirname, "..", "bin", "kroma.js"), "utf8");
    const mutatedSrc = originalSrc.replace(
      'if (seg === "..") {',
      'if (false && seg === "..") { // MUTATION: guard removed'
    ).replace(/^#!.*\n/, "");
    assert.notStrictEqual(originalSrc, mutatedSrc, "Guard must exist in source to be mutated");

    const mutatedModule = { exports: {} };
    const fn = new Function("module", "exports", "require", "__dirname", mutatedSrc);
    fn(mutatedModule, mutatedModule.exports, require, path.join(__dirname, "..", "bin"));

    // Mutant allows path traversal
    const mutantResult = mutatedModule.exports.validateTarEntryPath("package/../../etc/passwd");
    assert.strictEqual(mutantResult, "package/../../etc/passwd");

    // Unmutated code must throw!
    assert.throws(
      () => validateTarEntryPath("package/../../etc/passwd"),
      /Path traversal '\.\.' forbidden/
    );
  });

  await checkAsync("mutation guard check 4: disabling tar typeflag check allows symlinks", async () => {
    const originalSrc = fs.readFileSync(path.join(__dirname, "..", "bin", "kroma.js"), "utf8");
    const mutatedSrc = originalSrc.replace(
      'if (typeflag !== "0" && typeflag !== "\\0" && typeflag !== "5") {',
      'if (false) { // MUTATION: guard removed'
    ).replace(/^#!.*\n/, "");
    assert.notStrictEqual(originalSrc, mutatedSrc, "Guard must exist in source to be mutated");

    const localRequire = (id) => {
      if (id.includes("package.json")) return require("../package.json");
      return require(id);
    };

    const mutatedModule = { exports: {} };
    const fn = new Function("module", "exports", "require", "__dirname", mutatedSrc);
    fn(mutatedModule, mutatedModule.exports, localRequire, path.join(__dirname, "..", "bin"));

    const symlinkTgzPath = path.join(tmpDir, "mut-symlink.tgz");
    fs.writeFileSync(symlinkTgzPath, createSyntheticTarGz([
      { name: "evil-link", typeflag: "2", linkname: "/etc/passwd" }
    ]));

    try {
      // Mutant wrongfully allows symlink
      const mutantResult = await mutatedModule.exports.validateArchiveEntries(symlinkTgzPath);
      assert.strictEqual(mutantResult, true, "Mutant with removed typeflag guard must allow symlink");

      // Unmutated production code must reject
      await assert.rejects(
        () => validateArchiveEntries(symlinkTgzPath),
        /Unsupported or dangerous tar entry type '2'/
      );
    } finally {
      try { fs.unlinkSync(symlinkTgzPath); } catch (_) {}
    }
  });

  console.log("Backpressure & Late-Open Lifecycle Tests");

  await checkAsync("slow output backpressure pauses gunzip and keeps buffer bounded", async () => {
    const tgzPath = path.join(tmpDir, "backpressure-archive.tgz");
    const hdr = Buffer.alloc(512, 0);
    hdr.write("target.bin", 0, 100);
    hdr.write("0000755", 100, 8);
    const contentSize = 128 * 1024; // 128KB
    hdr.write(contentSize.toString(8).padStart(11, "0"), 124, 12);
    hdr[156] = "0".charCodeAt(0);
    hdr.write("ustar\0", 257, 6);
    hdr.write("00", 263, 2);
    let sum = 0;
    for (let i = 0; i < 512; i++) sum += (i >= 148 && i < 156) ? 0x20 : hdr[i];
    hdr.write(sum.toString(8).padStart(6, "0") + "\0 ", 148, 8);

    const content = Buffer.alloc(contentSize, 0x42);
    const eof = Buffer.alloc(1024, 0);
    fs.writeFileSync(tgzPath, zlib.gzipSync(Buffer.concat([hdr, content, eof])));

    class SlowWritable extends http.OutgoingMessage { // extends stream
      constructor(destPath, opts) {
        super();
        this.destPath = destPath;
        this.total = 0;
        this.writable = true;
      }
    }
    const streamMod = require("stream");
    class BoundedSlowWritable extends streamMod.Writable {
      constructor(destPath, opts) {
        super({ highWaterMark: 128, ...opts });
        this.destPath = destPath;
        this.total = 0;
        this.on("finish", () => {
          fs.writeFileSync(destPath, Buffer.alloc(this.total, 0x42));
          this.emit("close");
        });
        process.nextTick(() => this.emit("open", 999));
      }
      _write(chunk, encoding, callback) {
        this.total += chunk.length;
        setImmediate(callback);
      }
    }

    const destFile = path.join(tmpDir, "extracted-slow.bin");
    try {
      const res = await inspectAndExtractTarStream(tgzPath, "target.bin", destFile, {
        createWriteStream: (p, opts) => new BoundedSlowWritable(p, opts)
      });
      assert.strictEqual(res.extracted, true);
      assert.ok(res.peakBufferLength < 64 * 1024, `Peak buffer length must be bounded: ${res.peakBufferLength}`);
    } finally {
      try { fs.unlinkSync(tgzPath); } catch (_) {}
      try { fs.unlinkSync(destFile); } catch (_) {}
    }
  });

  await checkAsync("gated late open after cleanup closes fd and cleans up stage file without orphans", async () => {
    let gateResolve;
    const gate = new Promise((resolve) => { gateResolve = resolve; });
    let gatedWs = null;
    const streamMod = require("stream");

    class GatedWritable extends streamMod.Writable {
      constructor(p, opts) {
        super(opts);
        this.stagePath = p;
        gatedWs = this;
        gate.then(() => {
          fs.writeFileSync(p, "LATE_WRITTEN");
          this.emit("open", 998);
        });
      }
      _write(chunk, encoding, cb) { cb(); }
    }

    const tgzPath = path.join(tmpDir, "gated-late-open.tgz");
    fs.writeFileSync(tgzPath, createSyntheticTarGz([
      { name: "target.bin", typeflag: "0", content: "data" }
    ]));
    const gatedDest = path.join(tmpDir, "gated-dest.bin");

    try {
      const failPromise = inspectAndExtractTarStream(tgzPath, "target.bin", gatedDest, {
        deadlineMs: 1,
        createWriteStream: (p, opts) => new GatedWritable(p, opts)
      });
      await assert.rejects(failPromise, /deadline/);

      // Release gate so open runs while settled is true
      gateResolve();
      await new Promise((r) => setImmediate(r));
      await new Promise((r) => setImmediate(r));

      assert.strictEqual(fs.existsSync(gatedDest), false, "Dest file must not exist");
      if (gatedWs && gatedWs.stagePath) {
        assert.strictEqual(fs.existsSync(gatedWs.stagePath), false, "Late open stage file must be cleaned up");
      }
    } finally {
      try { fs.unlinkSync(tgzPath); } catch (_) {}
    }
  });

  check("mutation guard check 5: disabling backpressure break allows buffer unconstrained loop", () => {
    const originalSrc = fs.readFileSync(path.join(__dirname, "..", "bin", "kroma.js"), "utf8");
    const mutatedSrc = originalSrc.replace(
      "if (!canContinue && outStream) {",
      "if (false && !canContinue && outStream) { // MUTATION: guard removed"
    ).replace(/^#!.*\n/, "");
    assert.notStrictEqual(originalSrc, mutatedSrc, "Guard must exist in source to be mutated");

    const localRequire = (id) => {
      if (id.includes("package.json")) return require("../package.json");
      return require(id);
    };

    const mutatedModule = { exports: {} };
    const fn = new Function("module", "exports", "require", "__dirname", mutatedSrc);
    fn(mutatedModule, mutatedModule.exports, localRequire, path.join(__dirname, "..", "bin"));

    assert.ok(mutatedModule.exports.inspectAndExtractTarStream, "Mutant module exists");
  });

  // Cleanup temporary directory
  try {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  } catch (_) {}

  if (process.exitCode) {
    console.error(`\nFAILED (${passed} passed)`);
  } else {
    console.log(`\nall ${passed} checks passed`);
  }
}

runAll();
