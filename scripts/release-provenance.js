#!/usr/bin/env node
"use strict";

/**
 * RC1-05 CLI: sign or verify a release-provenance document.
 *
 *   node scripts/release-provenance.js sign   --version 1.5.0 --git-commit <sha> \
 *        --artifact kroma-linux-x64=target/release/kroma [--artifact ...] \
 *        --key-id kroma-release-2026 --private-key-file /secure/key.pem --out provenance.json
 *   node scripts/release-provenance.js verify provenance.json --artifacts <dir>
 *
 * The private key is read from a file path only: never from argv content, env or stdin, so it cannot
 * leak through a process listing or a log line.
 */

const fs = require("node:fs");
const path = require("node:path");
const { buildDocument, signDocument, verifyDocument } = require("./provenance.js");
const { PRODUCTION_TRUSTED_KEYS } = require(path.join(__dirname, "..", "bin", "kroma.js"));

function parseArgs(argv) {
  const args = { artifacts: [] };
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i];
    const next = argv[i + 1];
    switch (key) {
      case "--version": args.version = next; i += 1; break;
      case "--git-commit": args.gitCommit = next; i += 1; break;
      case "--key-id": args.keyId = next; i += 1; break;
      case "--private-key-file": args.privateKeyFile = next; i += 1; break;
      case "--out": args.out = next; i += 1; break;
      case "--artifacts": args.artifactRoot = next; i += 1; break;
      case "--artifact": {
        const [name, file] = String(next || "").split("=");
        if (!name || !file) throw new Error("--artifact expects name=path");
        args.artifacts.push({ name, file });
        i += 1;
        break;
      }
      default:
        if (!key.startsWith("-")) args.document = key;
        break;
    }
  }
  return args;
}

function main(argv) {
  const command = argv[2];
  const args = parseArgs(argv.slice(3));
  try {
    if (command === "sign") {
      if (!args.privateKeyFile) throw new Error("sign requires --private-key-file");
      const stat = fs.statSync(args.privateKeyFile);
      if ((stat.mode & 0o077) !== 0) {
        throw new Error(`refusing a group/world-readable private key: ${args.privateKeyFile}`);
      }
      const privateKey = require("node:crypto").createPrivateKey(
        fs.readFileSync(args.privateKeyFile)
      );
      if (privateKey.asymmetricKeyType !== "ed25519") {
        throw new Error(`expected an Ed25519 key, found ${privateKey.asymmetricKeyType}`);
      }
      const doc = buildDocument({
        version: args.version,
        gitCommit: args.gitCommit,
        artifacts: args.artifacts
      });
      const signed = signDocument(doc, args.keyId, privateKey);
      const out = args.out || "release-provenance.json";
      const tmp = `${out}.tmp`;
      fs.writeFileSync(tmp, `${JSON.stringify(signed, null, 2)}\n`, { mode: 0o644 });
      fs.renameSync(tmp, out);
      console.log(
        `signed ${signed.artifacts.length} artifact(s) with ${signed.key_id} -> ${out}`
      );
      return 0;
    }
    if (command === "verify") {
      if (!args.document) throw new Error("verify requires a document path");
      const root = args.artifactRoot || path.dirname(args.document);
      const doc = verifyDocument(fs.readFileSync(args.document, "utf8"), {
        trustedKeys: PRODUCTION_TRUSTED_KEYS,
        artifactRoot: root
      });
      console.log(
        `verified ${doc.artifacts.length} artifact(s) against ${doc.key_id} (${doc.git_commit.slice(0, 12)})`
      );
      return 0;
    }
    console.error("usage: release-provenance.js <sign|verify> ...");
    return 2;
  } catch (err) {
    console.error(`error: ${err.message}`);
    return 1;
  }
}

process.exitCode = main(process.argv);
