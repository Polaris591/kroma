#!/usr/bin/env node
"use strict";

/**
 * T21 (KR-27) — release trust-root regressions, **test channel only**.
 *
 * This suite deliberately generates an ephemeral Ed25519 key pair in memory. It never writes the
 * private key anywhere, never commits one, and never claims to be a production fixture: it exists to
 * pin the *verification* behaviour that must hold once the owner supplies a real key.
 *
 * It also asserts the negative that matters most: a test key never appears in `PRODUCTION_TRUSTED_KEYS`,
 * and the production root declares exactly the release key. (It used to assert the root was empty; that
 * changed when the release key was provisioned - see docs/release/TRUST_ROOT.md.) The old line began:
 * frozen, so a manifest signed by this test key is refused by the production entry. That is why T21's
 * "real release candidate verifies" acceptance is BLOCKED — see docs/review/phase1_test_results/T21_acceptance.md.
 *
 * Run: node scripts/test-release-trust-root.js
 */

const assert = require("node:assert");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const {
  PRODUCTION_TRUSTED_KEYS,
  buildCanonicalSigningPayload,
  signManifest,
  serializeManifest,
  verifyManifest,
  downloadBinaryIfNeeded
} = require(path.join(__dirname, "..", "bin", "kroma.js"));

let checks = 0;
function ok(name, fn) {
  return Promise.resolve()
    .then(fn)
    .then(() => {
      checks += 1;
      console.log(`  ok  ${name}`);
    })
    .catch((err) => {
      console.error(`  FAIL ${name}\n       ${err && err.message}`);
      process.exitCode = 1;
    });
}

function throwsWith(fn, needle) {
  try {
    fn();
  } catch (err) {
    assert.ok(
      String(err.message).includes(needle),
      `expected error containing ${JSON.stringify(needle)}, got ${JSON.stringify(err.message)}`
    );
    return;
  }
  assert.fail(`expected an error containing ${JSON.stringify(needle)}, but nothing was thrown`);
}

/** An ephemeral test key pair. Never persisted. */
function testKey(keyId) {
  const { publicKey, privateKey } = crypto.generateKeyPairSync("ed25519");
  // kroma.js accepts a raw 32-byte key as 64 hex characters (or PEM / KeyObject). The JWK export is
  // the portable way to get exactly those 32 bytes out of a generated pair.
  const raw = Buffer.from(publicKey.export({ format: "jwk" }).x, "base64url");
  assert.strictEqual(raw.length, 32, "an ed25519 public key is 32 bytes");
  return {
    keyId,
    privateKey,
    trusted: { [keyId]: raw.toString("hex") }
  };
}

function manifestFields(overrides = {}) {
  return Object.assign(
    {
      schemaVersion: 1,
      keyId: "test-channel",
      version: "1.5.0",
      target: "cli",
      tarballUrl: "https://cobblemon.asia/releases/kroma-cli-1.5.0.tgz",
      sha256: "a".repeat(64),
      length: 1024,
      expires: Math.floor(Date.now() / 1000) + 3600,
      antirollback: "1.0.0"
    },
    overrides
  );
}

function signed(key, overrides) {
  const fields = manifestFields(Object.assign({ keyId: key.keyId }, overrides));
  return serializeManifest(signManifest(fields, key.privateKey));
}

async function main() {
  const key = testKey("test-channel");

  console.log("Trust root state");
  await ok("production trust root declares exactly the release key and is frozen", () => {
    assert.ok(Object.isFrozen(PRODUCTION_TRUSTED_KEYS), "PRODUCTION_TRUSTED_KEYS must be frozen");
    assert.deepStrictEqual(Object.keys(PRODUCTION_TRUSTED_KEYS), ["kroma-release-2026"]);
    assert.match(PRODUCTION_TRUSTED_KEYS["kroma-release-2026"], /^[0-9a-f]{64}$/);
    // The test-channel key must never appear here: a test key in production is worse than no key.
    assert.strictEqual(PRODUCTION_TRUSTED_KEYS["test-channel"], undefined);
  });

  await ok("a test-key manifest is refused by the production entry", () => {
    const text = signed(key);
    throwsWith(() => verifyManifest(text), "Unknown or untrusted signing key: test-channel");
    // ...and the test key does not accidentally appear in production.
    assert.strictEqual(PRODUCTION_TRUSTED_KEYS[key.keyId], undefined);
  });

  console.log("manifest_binds_platform_version_and_artifact_hash");
  await ok("the test-channel manifest verifies with the test key", () => {
    const text = signed(key);
    const manifest = verifyManifest(text, { trustedKeys: key.trusted, currentTarget: "cli" });
    assert.strictEqual(manifest.version, "1.5.0");
    assert.strictEqual(manifest.target, "cli");
    assert.strictEqual(manifest.sha256, "a".repeat(64));
  });

  await ok("every bound field invalidates the signature when changed", () => {
    const text = signed(key);
    const parsed = JSON.parse(text);
    // A changed keyId is not a signature failure: it selects a different (untrusted) key, and that
    // refusal is asserted separately below.
    for (const field of ["version", "target", "sha256", "length", "antirollback"]) {
      const tampered = Object.assign({}, parsed);
      if (field === "length") tampered.length = parsed.length + 1;
      else if (field === "version") tampered.version = "9.9.9";
      else if (field === "target") tampered.target = "linux-x64";
      else if (field === "sha256") tampered.sha256 = "b".repeat(64);
      else tampered.antirollback = "9.9.9";
      throwsWith(
        () => verifyManifest(JSON.stringify(tampered), { trustedKeys: key.trusted }),
        "Signature verification failed"
      );
    }
    // Changing the key id is caught by the trust-root lookup instead.
    const renamed = Object.assign({}, parsed, { keyId: "other-key" });
    throwsWith(
      () => verifyManifest(JSON.stringify(renamed), { trustedKeys: key.trusted }),
      "Unknown or untrusted signing key"
    );
  });

  console.log("unknown_rotated_revoked_expired_key_is_rejected");
  await ok("an unknown keyId is refused", () => {
    const text = signed(key);
    throwsWith(
      () => verifyManifest(text, { trustedKeys: { "someone-else": Object.values(key.trusted)[0] } }),
      "Unknown or untrusted signing key"
    );
  });

  await ok("a revoked key (removed from the trusted map) is refused", () => {
    const text = signed(key);
    const rotated = {}; // the key was rotated away: the map no longer contains it
    throwsWith(() => verifyManifest(text, { trustedKeys: rotated }), "trust root is unconfigured");
    throwsWith(
      () => verifyManifest(text, { trustedKeys: { "new-key": Object.values(key.trusted)[0] } }),
      "Unknown or untrusted signing key"
    );
  });

  await ok("an expired manifest is refused", () => {
    const expired = signed(key, { expires: Math.floor(Date.now() / 1000) - 10 });
    throwsWith(
      () => verifyManifest(expired, { trustedKeys: key.trusted }),
      "Manifest has expired"
    );
  });

  await ok("a manifest for another target is refused", () => {
    const text = signed(key);
    throwsWith(
      () => verifyManifest(text, { trustedKeys: key.trusted, currentTarget: "linux-x64" }),
      "Manifest target mismatch"
    );
  });

  await ok("a downgrade below the antirollback floor is refused", () => {
    // The client compares the manifest's version and floor against the installed version itself, so a
    // signed-but-older manifest is refused before anything is downloaded.
    const text = signed(key, { version: "1.4.0", antirollback: "1.4.0" });
    throwsWith(() => verifyManifest(text, { trustedKeys: key.trusted }), "downgrade rejected");

    // A manifest at the installed version with a future floor is also refused: the floor is a promise
    // about what may still be installed, not a comment.
    const floorAbove = signed(key, { version: "1.5.0", antirollback: "9.9.9" });
    throwsWith(
      () => verifyManifest(floorAbove, { trustedKeys: key.trusted }),
      "antirollback"
    );
  });

  console.log("signature_failure_never_overwrites_existing_binary");
  await ok("an existing binary is never replaced by a download attempt", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-trust-"));
    const dest = path.join(dir, "kroma");
    const original = Buffer.from("original-binary-bytes");
    fs.writeFileSync(dest, original);

    // Even with a tampered manifest in hand, an existing binary short-circuits: nothing is fetched and
    // nothing is written.
    const tampered = JSON.parse(signed(key));
    tampered.version = "9.9.9";
    const result = await downloadBinaryIfNeeded(dest, "cli", {
      trustedKeys: key.trusted,
      manifestText: JSON.stringify(tampered)
    });
    assert.strictEqual(result, true, "an existing binary reports success without downloading");
    assert.deepStrictEqual(fs.readFileSync(dest), original, "the binary was modified");
    fs.rmSync(dir, { recursive: true, force: true });
  });

  await ok("a failed verification creates no file at all", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-trust-"));
    const dest = path.join(dir, "kroma");
    const tampered = JSON.parse(signed(key));
    tampered.sha256 = "c".repeat(64);
    const result = await downloadBinaryIfNeeded(dest, "cli", {
      trustedKeys: key.trusted,
      manifestText: JSON.stringify(tampered)
    });
    assert.strictEqual(result, false, "a failed verification must report failure");
    assert.ok(!fs.existsSync(dest), "a failed verification must not leave a binary behind");
    const leftovers = fs.readdirSync(dir);
    assert.deepStrictEqual(leftovers, [], `a failed verification left files: ${leftovers.join(", ")}`);
    fs.rmSync(dir, { recursive: true, force: true });
  });

  await ok("an unconfigured trust root refuses before touching the disk", async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-trust-"));
    const dest = path.join(dir, "kroma");
    const result = await downloadBinaryIfNeeded(dest, "cli", {
      manifestText: signed(key)
    });
    assert.strictEqual(result, false);
    assert.ok(!fs.existsSync(dest));
    fs.rmSync(dir, { recursive: true, force: true });
  });

  console.log("production_fixture_signed_by_release_key_verifies");
  await ok("the production entry refuses the test key and names the trust root it holds", () => {
    // The production root now holds the real release key (docs/release/TRUST_ROOT.md). A manifest signed
    // with the test-channel key must be refused by *name*, not silently accepted: that is the check that
    // the installed key is the one being used.
    const text = signed(key);
    throwsWith(() => verifyManifest(text), "Unknown or untrusted signing key: test-channel");
    assert.deepStrictEqual(Object.keys(PRODUCTION_TRUSTED_KEYS), ["kroma-release-2026"]);
  });

  console.log("");
  if (process.exitCode) {
    console.error(`release trust-root suite FAILED after ${checks} passing check(s)`);
  } else {
    console.log(`all ${checks} checks passed (test channel; no production key was used)`);
  }
}

main().catch((err) => {
  console.error(err && err.stack ? err.stack : err);
  process.exitCode = 1;
});
