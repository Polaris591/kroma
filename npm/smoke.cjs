"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const {spawnSync} = require("node:child_process");
const {Worker} = require("node:worker_threads");
const {once} = require("node:events");
const {forHost, packageName, executable} = require("./platforms.cjs");

function run(file, args, options = {}) {
  const result = spawnSync(file, args, {encoding: "utf8", timeout: 120000, ...options});
  if (result.error) throw result.error;
  assert.equal(result.status, 0, `${file} ${args.join(" ")}\n${result.stdout}\n${result.stderr}`);
  return result.stdout;
}

async function smoke(staged) {
  const npmCli = process.env.npm_execpath;
  if (!npmCli || !fs.existsSync(npmCli)) throw new Error("Run via: npm --prefix npm run smoke -- <staged directory>");
  const platform = forHost();
  const mainDir = path.join(staged, "kroma");
  const nativeDir = path.join(staged, packageName(platform));
  const readManifest = dir => JSON.parse(fs.readFileSync(path.join(dir, "package.json"), "utf8"));
  const main = readManifest(mainDir);
  const tarballs = path.join(staged, "tarballs");
  fs.mkdirSync(tarballs, {recursive: true});
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), "kroma npm smoke "));
  const config = path.join(temp, "empty.npmrc");
  fs.writeFileSync(config, "");
  const env = {...process.env, npm_config_cache: path.join(temp, "npm-cache"), npm_config_update_notifier: "false",
    npm_config_userconfig: config, npm_config_globalconfig: path.join(temp, "global.npmrc")};
  const npm = (args, cwd = temp) => run(process.execPath, [npmCli, ...args], {cwd, env});
  let registry;
  try {
    const nativeBundle = path.join(nativeDir,"share","kroma","plugins");
    for (const [id,entrypoint] of [["web","main.js"],["subagents","main.js"],
      ["project-memory","plugin.toml"]]) {
      assert.ok(fs.existsSync(path.join(nativeBundle,id,"plugin.toml")),
        `Native package must bundle disabled official plugin: ${id}`);
      assert.ok(fs.existsSync(path.join(nativeBundle,id,entrypoint)),
        `Native package is missing official plugin entrypoint: ${id}`);
    }
    const memoryExe=platform.os === "win32" ? "kroma-memory.exe" : "kroma-memory";
    assert.ok(fs.existsSync(path.join(nativeDir,"bin",memoryExe)),
      "Official project-memory must ship as a native binary with no Rust installation required");
    const packageFiles=(dir,prefix="")=>fs.readdirSync(dir,{withFileTypes:true})
      .flatMap(item=>{
        const relative=prefix ? `${prefix}/${item.name}` : item.name;
        return item.isDirectory() ? packageFiles(path.join(dir,item.name),relative) : [relative];
      });
    const expectedNative=["package.json","README.md","LICENSE",`bin/${executable(platform)}`,
      `bin/${memoryExe}`,...packageFiles(nativeBundle,"share/kroma/plugins")];
    const packages = [];
    for (const [dir, expected] of [
      [nativeDir, expectedNative],
      [mainDir, ["package.json", "README.md", "LICENSE", "launcher.cjs", "platforms.cjs", "update.cjs"]],
    ]) {
      const [packed] = JSON.parse(npm(["pack", "--json", "--ignore-scripts", "--pack-destination", tarballs], dir));
      assert.deepEqual(packed.files.map(file => file.path).sort(), expected.sort(), "Unexpected files in npm tarball");
      if (dir === nativeDir && platform.os !== "win32") {
        const binary = packed.files.find(file => file.path === `bin/${executable(platform)}`);
        assert.equal(binary.mode & 0o111, 0o111, "Native tarball lost executable permissions");
      }
      packages.push({...packed, tarball: path.join(tarballs, packed.filename)});
    }
    registry = new Worker(path.join(__dirname, "test", "registry.cjs"), {
      workerData: {packages: [
        {manifest: readManifest(nativeDir), tarball: packages[0].tarball, integrity: packages[0].integrity},
        {manifest: main, tarball: packages[1].tarball, integrity: packages[1].integrity},
      ]},
    });
    const [port] = await once(registry, "message", {signal: AbortSignal.timeout(15000)});
    const options = ["--ignore-scripts", "--no-audit", "--no-fund", `--registry=http://127.0.0.1:${port}`, "--fetch-retries=0"];
    async function upgrade(root, globalPrefix) {
      // Simulate stale package metadata, then repair through a real npm install.
      const nativeRoot = path.dirname(require.resolve(`${packageName(platform)}/package.json`, {paths: [root]}));
      for (const dir of [root, nativeRoot]) {
        const file = path.join(dir, "package.json"), data = JSON.parse(fs.readFileSync(file, "utf8"));
        data.version = "0.0.0"; fs.writeFileSync(file, JSON.stringify(data));
      }
      fs.rmSync(path.join(path.dirname(root), ".package-lock.json"), {force: true});
      await require("./update.cjs").runUpdate([], {
        root, cli: npmCli, log() {},
        fetchImpl: (url, opts) => { assert.equal(url, "https://registry.npmjs.org/kroma/latest"); return fetch(`http://127.0.0.1:${port}/kroma/latest`, opts); },
        execute(file, args, opts) {
          const actual = args.map(arg => arg.startsWith("--registry=") ? `--registry=http://127.0.0.1:${port}` : arg);
          return spawnSync(file, actual, {...opts, env: {...env, ...(globalPrefix ? {npm_config_prefix: globalPrefix} : {})}});
        },
      });
      assert.equal(readManifest(root).version, main.version);
      assert.equal(readManifest(path.dirname(require.resolve(`${packageName(platform)}/package.json`, {paths: [root]}))).version, main.version);
    }
    const mainTarball = packages[1].tarball;
    const local = path.join(temp, "local install");
    fs.mkdirSync(local);
    fs.writeFileSync(path.join(local, "package.json"), '{"name":"kroma-install-smoke","private":true}\n');
    npm(["install", ...options, "--package-lock=false", mainTarball], local);
    const installed = path.join(local, "node_modules", "kroma", "launcher.cjs");
    assert.equal(readManifest(path.dirname(installed)).version, main.version);
    const nativeVersion = run(path.join(nativeDir, "bin", executable(platform)), ["--version"]);
    assert.ok(nativeVersion.startsWith(`kroma ${main.version} (`), nativeVersion);
    assert.equal(run(process.execPath, [installed, "--version"], {cwd: temp, env}), nativeVersion);
    const official = run(process.execPath,[installed,"plugins","list"],{cwd:temp,env});
    for(const id of ["web","subagents","project-memory"]) {
      assert.match(official,new RegExp(id),`Bundled plugin ${id} should appear in discovery`);
    }
    assert.match(run(process.execPath, [installed, "--help"], {cwd: temp, env}), /Usage:/);
    const badFlag = spawnSync(process.execPath, [installed, "--kroma-smoke-invalid-option"], {cwd: temp, env, encoding: "utf8", timeout: 15000});
    assert.ifError(badFlag.error);
    assert.equal(badFlag.status, 2, `${badFlag.stdout}\n${badFlag.stderr}`);
    await upgrade(path.dirname(installed));
    assert.equal(npm(["exec", "--offline", "--yes=false", "--", "kroma", "--version"], local), nativeVersion);
    const prefix = path.join(temp, "global install");
    const installArgs = ["install", "--global", "--prefix", prefix, ...options, mainTarball];
    npm(installArgs);
    const globalVersion = () => {
      if (process.platform === "win32") {
        const shim = path.join(prefix, "kroma.cmd");
        return run(process.env.ComSpec || "cmd.exe", ["/d", "/s", "/c", '""' + shim + '" --version"'],
          {cwd: temp, env, windowsVerbatimArguments: true});
      }
      return run(path.join(prefix, "bin", "kroma"), ["--version"], {cwd: temp, env});
    };
    assert.equal(globalVersion(), nativeVersion);
    npm(installArgs);
    assert.equal(globalVersion(), nativeVersion, "Reinstall changed CLI behavior");
    await upgrade(path.join(prefix, process.platform === "win32" ? "node_modules/kroma" : "lib/node_modules/kroma"), prefix);
    assert.equal(globalVersion(), nativeVersion, "Global update damaged the command");
    npm(["uninstall", "--global", "--prefix", prefix, ...options, "kroma"]);
    const shim = path.join(prefix, process.platform === "win32" ? "kroma.cmd" : "bin/kroma");
    assert.equal(fs.existsSync(shim), false, "Uninstall left the bin shim behind");
    const modules = path.join(prefix, process.platform === "win32" ? "node_modules" : "lib/node_modules");
    assert.equal(fs.existsSync(path.join(modules, packageName(platform))), false, "Uninstall left the optional native package behind");
    const omitted = path.join(temp, "without optional dependencies");
    npm(["install", "--prefix", omitted, ...options, "--omit=optional", "--package-lock=false", mainTarball]);
    const missing = spawnSync(process.execPath, [path.join(omitted, "node_modules", "kroma", "launcher.cjs"), "--version"],
      {cwd: omitted, env, encoding: "utf8", timeout: 15000});
    assert.ifError(missing.error);
    assert.equal(missing.status, 1);
    assert.equal(missing.stdout, "");
    assert.match(missing.stderr, /optional dependencies enabled/);
    console.log(`Local-registry npm install, local/global update repair, bin shims, reinstall, uninstall and --omit=optional passed: ${platform.id}, kroma ${main.version}`);
    console.log(`Tarballs: ${tarballs}`);
  } finally {
    if (registry) await registry.terminate();
    try {
      fs.rmSync(temp, {recursive: true, force: true, maxRetries: 3});
    } catch (error) {
      console.error(`Warning: could not clean smoke-test directory ${temp}: ${error.message}`);
    }
  }
}

if (require.main === module) {
  (async () => {
    if (process.argv.length !== 3) throw new Error("Usage: npm run smoke -- <staged directory>");
    await smoke(path.resolve(process.argv[2]));
  })().catch(error => {
    console.error(error.stack);
    process.exitCode = 1;
  });
}
module.exports = {smoke};
