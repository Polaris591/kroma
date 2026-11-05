"use strict";

const fs = require("node:fs");
const path = require("node:path");
const {spawnSync} = require("node:child_process");
const {platforms, forHost, packageName, executable} = require("./platforms.cjs");

const root = path.resolve(__dirname, "..");

function cargoVersion() {
  const manifest = fs.readFileSync(path.join(root, "Cargo.toml"), "utf8");
  const section = manifest.match(/^\[package\]\s*\r?\n([\s\S]*?)(?=^\[|$(?![\s\S]))/m)?.[1];
  const version = section?.match(/^version\s*=\s*"([^"]+)"\s*$/m)?.[1];
  if (!version || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error("Cargo.toml must contain a valid package version.");
  }
  return version;
}

function stage({target, binary, out}) {
  const platform = platforms.find(item => item.targets.includes(target));
  if (!platform) throw new Error(`Unsupported Rust target: ${target}`);
  if (platform.id !== forHost().id) {
    throw new Error("Stage on the target platform so the actual binary can be verified before packaging.");
  }
  const version = cargoVersion();
  const source = path.resolve(binary);
  const result = spawnSync(source, ["--version"], {encoding: "utf8", timeout: 15000});
  if (result.error || result.status !== 0 || !result.stdout.startsWith(`kroma ${version} (`)) {
    throw new Error(`Binary must run and report kroma ${version}: ${result.error?.message || result.stderr || result.stdout}`);
  }
  const output = path.resolve(out);
  const mainDir = path.join(output, "kroma");
  const nativeDir = path.join(output, packageName(platform));
  if (fs.existsSync(mainDir) || fs.existsSync(nativeDir)) {
    throw new Error("Output package directories already exist; use a fresh --out directory.");
  }
  const shared = {
    version,
    license: "MIT",
    repository: {type: "git", url: "git+https://github.com/Polaris591/kroma.git"},
    homepage: "https://github.com/Polaris591/kroma#readme",
    bugs: {url: "https://github.com/Polaris591/kroma/issues"},
    publishConfig: {access: "public"},
  };
  fs.mkdirSync(mainDir, {recursive: true});
  fs.mkdirSync(path.join(nativeDir, "bin"), {recursive: true});
  for (const dir of [mainDir, nativeDir]) {
    fs.copyFileSync(path.join(root, "LICENSE"), path.join(dir, "LICENSE"));
    fs.copyFileSync(path.join(__dirname, "README.md"), path.join(dir, "README.md"));
  }
  fs.copyFileSync(path.join(__dirname, "launcher.cjs"), path.join(mainDir, "launcher.cjs"));
  fs.copyFileSync(path.join(__dirname, "platforms.cjs"), path.join(mainDir, "platforms.cjs"));
  fs.copyFileSync(path.join(__dirname, "update.cjs"), path.join(mainDir, "update.cjs"));
  fs.chmodSync(path.join(mainDir, "launcher.cjs"), 0o755);
  const destination = path.join(nativeDir, "bin", executable(platform));
  fs.copyFileSync(source, destination);
  fs.chmodSync(destination, 0o755);
  const main = {
    ...shared,
    name: "kroma",
    description: "An observable Rust AI coding agent with a terminal UI, MCP tools and file checkpoints",
    engines: {node: ">=22"},
    bin: {kroma: "launcher.cjs"},
    files: ["launcher.cjs", "platforms.cjs", "update.cjs", "LICENSE", "README.md"],
    optionalDependencies: Object.fromEntries(platforms.map(item => [packageName(item), version])),
  };
  const native = {
    ...shared,
    name: packageName(platform),
    description: `Kroma native binary for ${platform.id}; installed by kroma`,
    os: [platform.os],
    cpu: [platform.cpu],
    ...(platform.os === "linux" ? {libc: ["glibc"]} : {}),
    files: ["bin", "LICENSE", "README.md"],
  };
  for (const [dir, manifest] of [[mainDir, main], [nativeDir, native]]) {
    fs.writeFileSync(path.join(dir, "package.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  }
  return {version, platform: platform.id, mainDir, nativeDir};
}

function parseArgs(args) {
  const options = {};
  for (let index = 0; index < args.length; index += 2) {
    const key = args[index];
    const value = args[index + 1];
    if (!["--target", "--binary", "--out"].includes(key) || !value || value.startsWith("--") || options[key.slice(2)]) {
      throw new Error("Usage: npm run stage -- --target <Rust triple> --binary <executable> --out <fresh directory>");
    }
    options[key.slice(2)] = value;
  }
  if (!options.target || !options.binary || !options.out) throw new Error("--target, --binary and --out are required.");
  return options;
}

if (require.main === module) {
  try {
    console.log(JSON.stringify(stage(parseArgs(process.argv.slice(2))), null, 2));
  } catch (error) {
    console.error(`Kroma packaging: ${error.message}`);
    process.exitCode = 1;
  }
}
module.exports = {cargoVersion, parseArgs, stage};
