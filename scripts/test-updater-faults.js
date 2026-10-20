#!/usr/bin/env node
"use strict";

// Regression cases for KRV4-04/06: run without network or privileged filesystems.
const assert = require('node:assert/strict');
const test = require('node:test');
const { Readable, Writable } = require('node:stream');
const { EventEmitter } = require('node:events');
const fs = require('node:fs');
const https = require('node:https');
const path = require('node:path');
const os = require('node:os');
const crypto = require('node:crypto');
const { downloadTarballWithIntegrity, isSafeInstallTarget } = require('../bin/kroma.js');

// Deliberately mocks HTTPS at the transport seam (not the trusted install-URL validator).
async function withResponse(chunks, headers, fn) {
  const original = https.get;
  https.get = (_url, _options, callback) => {
    const req = new EventEmitter();
    req.destroy = (error) => { if (error) queueMicrotask(() => req.emit('error', error)); };
    const res = Readable.from(chunks);
    res.statusCode = 200;
    res.headers = headers;
    process.nextTick(() => callback(res));
    return req;
  };
  try { return await fn(); } finally { https.get = original; }
}

const URL = 'https://cobblemon.asia/kroma.tgz';
const sha = (buf) => crypto.createHash('sha256').update(buf).digest('hex');

// Sequential so the temporary global HTTPS/fs mocks cannot overlap.
test('update transport failure and backpressure regressions', async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'kroma updater with spaces '));
  try {
    await t.test('local tarball with spaces is accepted without a shell', () => {
      const archive = path.join(root, 'a safe file name.tgz');
      fs.writeFileSync(archive, Buffer.from('fixture'));
      assert.equal(isSafeInstallTarget(archive), true);
      assert.equal(isSafeInstallTarget(archive + '\n'), false);
      assert.equal(isSafeInstallTarget('npm-cli; echo injected'), false);
    });

    await t.test('successful streaming download checks exact size, SHA-256 and payload', async () => {
      const body = Buffer.alloc(23_000, 0x42);
      const dest = path.join(root, 'normal.tgz');
      await withResponse([body.subarray(0, 2_000), body.subarray(2_000)],
        { 'content-length': String(body.length) }, async () => {
          const outcome = await downloadTarballWithIntegrity(URL, sha(body), body.length, dest);
          assert.equal(outcome.bytesReceived, body.length);
          assert.equal(outcome.sha256, sha(body));
        });
      assert.deepEqual(fs.readFileSync(dest), body);
    });

    await t.test('checksum mismatch rejects and removes partial target', async () => {
      const body = Buffer.alloc(30_000, 0x46);
      const dest = path.join(root, 'mismatch.tgz');
      await withResponse([body], {}, async () => {
        await assert.rejects(downloadTarballWithIntegrity(URL, '0'.repeat(64), body.length, dest), /Checksum mismatch/);
      });
      assert.equal(fs.existsSync(dest), false);
    });

    await t.test('file collision cannot overwrite or delete existing bytes', async () => {
      const body = Buffer.alloc(100_000, 0x46);
      const dest = path.join(root, 'do-not-overwrite.tgz');
      fs.writeFileSync(dest, 'IMPORTANT');
      await withResponse([body], {}, async () => {
        await assert.rejects(downloadTarballWithIntegrity(URL, sha(body), body.length, dest), /EEXIST/);
      });
      assert.equal(fs.readFileSync(dest, 'utf8'), 'IMPORTANT');
    });

    await t.test('ENOSPC from asynchronous writer rejects without an unhandled error', async () => {
      const realCreate = fs.createWriteStream;
      const body = Buffer.alloc(512_000, 0x48);
      const dest = path.join(root, 'enospc.tgz');
      fs.createWriteStream = function (...args) {
        const ws = realCreate.apply(this, args);
        ws.once('open', () => process.nextTick(() => ws.destroy(Object.assign(new Error('ENOSPC simulated'), { code: 'ENOSPC' }))));
        return ws;
      };
      try {
        await withResponse([body.subarray(0, 1000), body.subarray(1000)], {}, async () => {
          await assert.rejects(downloadTarballWithIntegrity(URL, sha(body), body.length, dest), /ENOSPC/);
        });
        assert.equal(fs.existsSync(dest), false);
      } finally { fs.createWriteStream = realCreate; }
    });

    await t.test('EACCES from writer rejects rather than crash', async () => {
      const realCreate = fs.createWriteStream;
      const body = Buffer.alloc(8192, 0x44);
      fs.createWriteStream = () => {
        const sink = new Writable({ write(_buf, _enc, cb) { cb(Object.assign(new Error('EACCES simulated'), { code: 'EACCES' })); } });
        return sink;
      };
      try {
        await withResponse([body], {}, async () => {
          await assert.rejects(downloadTarballWithIntegrity(URL, sha(body), body.length, path.join(root, 'access.tgz')), /EACCES/);
        });
      } finally { fs.createWriteStream = realCreate; }
    });

    await t.test('slow sink backpressures an 8MiB response instead of buffering whole body', async () => {
      const body = Buffer.alloc(8 * 1024 * 1024, 0x61);
      const chunks = Array.from({ length: body.length / 16384 }, (_, i) => body.subarray(i * 16384, (i + 1) * 16384));
      let maxQueued = 0;
      const realCreate = fs.createWriteStream;
      fs.createWriteStream = () => {
        const sink = new Writable({
          highWaterMark: 1024,
          write(_buf, _enc, cb) {
            maxQueued = Math.max(maxQueued, this.writableLength);
            setTimeout(cb, 1);
          }
        });
        return sink;
      };
      try {
        await withResponse(chunks, {}, async () => {
          await downloadTarballWithIntegrity(URL, sha(body), body.length, path.join(root, 'slow.tgz'));
        });
        assert.ok(maxQueued <= 32768, `buffered ${maxQueued} bytes in 1KiB HWM sink`);
      } finally { fs.createWriteStream = realCreate; }
    });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
