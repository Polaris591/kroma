#!/usr/bin/env node
"use strict";

const fs = require("node:fs");
const path = require("node:path");
const {spawn} = require("node:child_process");
const {forHost, packageName, executable} = require("./platforms.cjs");

function resolveBinary() {
  const platform = forHost();
  if (platform.os === "linux" && !process.report.getReport().header.glibcVersionRuntime) {
    throw new Error("This npm build requires glibc Linux; Alpine/musl is not supported. Use a source build instead.");
  }
  const name = packageName(platform);
  const version = require("./package.json").version;
  let manifest;
  try {
    manifest = require.resolve(`${name}/package.json`);
  } catch {
    throw new Error(`Missing ${name}@${version}. Reinstall kroma with optional dependencies enabled (do not use --omit=optional).`);
  }
  const installed = JSON.parse(fs.readFileSync(manifest, "utf8"));
  if (installed.name !== name || installed.version !== version) {
    throw new Error(`Expected ${name}@${version}, found ${installed.name}@${installed.version}. Reinstall kroma to repair the installation.`);
  }
  const binary = path.join(path.dirname(manifest), "bin", executable(platform));
  fs.accessSync(binary, fs.constants.X_OK);
  return binary;
}

async function main() {
  const {updateArgs, runUpdate} = require("./update.cjs");
  const args = updateArgs(process.argv.slice(2));
  if (args !== null) {
    try { await runUpdate(args); } catch (error) { console.error(`Kroma: ${error.message}`); process.exitCode = error.exitCode || 1; }
    return;
  }
  let binary;
  try {
    binary = resolveBinary();
  } catch (error) {
    console.error(`Kroma: ${error.message}`);
    process.exitCode = 1;
    return;
  }
  const nativeDir = path.dirname(path.dirname(binary));
  const childEnv = {...process.env};
  const bundled = path.join(nativeDir, "share", "kroma", "plugins");
  if (fs.existsSync(bundled) && !childEnv.KROMA_OFFICIAL_PLUGIN_DIR) {
    childEnv.KROMA_OFFICIAL_PLUGIN_DIR = bundled;
  }
  // The separately compiled local SQLite memory worker is a platform binary,
  // not an npm install script or a runtime download. It is found on PATH when
  // the official project-memory plugin is explicitly enabled by the user.
  childEnv.PATH = path.join(nativeDir,"bin") + path.delimiter + (childEnv.PATH ?? "");
  const child = spawn(binary, process.argv.slice(2), {stdio: "inherit", shell: false, env:childEnv});
  const handlers = new Map();
  for (const signal of ["SIGINT", "SIGTERM"]) {
    const handler = () => {
      // Windows delivers console Ctrl+C to both processes. kill("SIGINT") would
      // forcibly terminate Rust instead of letting its cancel handler run.
      if (process.platform !== "win32" || signal !== "SIGINT") child.kill(signal);
    };
    handlers.set(signal, handler);
    process.on(signal, handler);
  }
  function cleanup() {
    for (const [signal, handler] of handlers) process.removeListener(signal, handler);
  }
  child.once("error", error => {
    cleanup();
    console.error(`Kroma: could not start ${binary}: ${error.message}`);
    process.exitCode = 1;
  });
  child.once("exit", (code, signal) => {
    cleanup();
    if (signal && process.platform !== "win32") {
      process.kill(process.pid, signal);
    } else {
      process.exitCode = code ?? (signal === "SIGINT" ? 130 : 1);
    }
  });
}

main();
