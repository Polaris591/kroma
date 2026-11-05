"use strict";
const fs = require("node:fs");
const path = require("node:path");
const {createHash} = require("node:crypto");
const {execFileSync, spawnSync} = require("node:child_process");
const {platforms, packageName, executable} = require("./platforms.cjs");
const {version: parseVersion} = require("./update.cjs");
const digest = file => createHash("sha256").update(fs.readFileSync(file)).digest("hex");
function archives(dir) {
  return fs.readdirSync(dir, {withFileTypes: true}).flatMap(e => {
    const file = path.join(dir, e.name);
    if (e.isSymbolicLink()) throw new Error("Release inputs must not contain symlinks");
    return e.isDirectory() ? archives(file) : e.name.endsWith(".tgz") ? [file] : [];
  });
}
function inspect(file) {
  const archive = path.basename(file);
  const tar = args => execFileSync("tar", args, {cwd: path.dirname(path.resolve(file)), encoding: "utf8", maxBuffer: 2 * 1024 * 1024, timeout: 30000});
  const entries = tar(["-tzf", archive]).trim().split(/\r?\n/);
  if (new Set(entries).size !== entries.length || entries.some(n => !n.startsWith("package/") || n.includes("..") || n.includes("\\"))) throw new Error("Unsafe or duplicate tar entries");
  if (tar(["-tvzf", archive]).trim().split(/\r?\n/).some(line => !/^[-d]/.test(line))) throw new Error("Non-regular release archive entry");
  const manifest = JSON.parse(tar(["-xOzf", archive, "package/package.json"]));
  const platform = platforms.find(p => packageName(p) === manifest.name);
  if (manifest.name !== "kroma" && !platform) throw new Error("Unexpected package name");
  if (manifest.scripts || manifest.dependencies || manifest.devDependencies) throw new Error("Unexpected scripts/dependencies in release package");
  const expected = ["package.json", "LICENSE", "README.md", ...(platform ? [`bin/${executable(platform)}`] : ["launcher.cjs", "platforms.cjs", "update.cjs"])].map(n => `package/${n}`).sort();
  const files = entries.filter(n => !n.endsWith("/")).sort();
  if (JSON.stringify(files) !== JSON.stringify(expected)) throw new Error("Unexpected release archive contents");
  if (manifest.license !== "MIT") throw new Error("Missing release license");
  if (platform) {
    if (JSON.stringify(manifest.os) !== JSON.stringify([platform.os]) || JSON.stringify(manifest.cpu) !== JSON.stringify([platform.cpu]) ||
        (platform.os === "linux" && JSON.stringify(manifest.libc) !== '["glibc"]') || manifest.optionalDependencies) throw new Error("Incorrect native platform metadata");
    if (platform.os !== "win32" && !tar(["-tvzf", archive]).split(/\r?\n/).some(line => /^-..x..x..x/.test(line) && line.endsWith(`package/bin/${executable(platform)}`))) throw new Error("Native executable permissions missing");
  } else {
    const expectedDeps = Object.fromEntries(platforms.map(p => [packageName(p), manifest.version]));
    if (JSON.stringify(Object.entries(manifest.optionalDependencies || {}).sort()) !== JSON.stringify(Object.entries(expectedDeps).sort())) throw new Error("Incomplete native dependencies");
  }
  return {name: manifest.name, version: manifest.version, file, sha256: digest(file)};
}
function validate(input, tag, channel) {
  if (!tag.startsWith("v")) throw new Error("Expected v<version> tag");
  const version = tag.slice(1), parsed = parseVersion(version);
  if (!["next", "latest"].includes(channel) || (channel === "latest" && parsed.pre)) throw new Error("Invalid release channel/version");
  const packages = new Map();
  for (const file of archives(input)) {
    const item = inspect(file);
    if (item.version !== version) throw new Error("Release package version does not match tag");
    if (path.basename(file) !== `${item.name}-${version}.tgz`) throw new Error("Unexpected tarball filename");
    if (packages.has(item.name)) {
      if (item.name !== "kroma" || packages.get(item.name).sha256 !== item.sha256) throw new Error("Duplicate or inconsistent release artifact");
    } else packages.set(item.name, item);
  }
  const names = [...platforms.map(packageName), "kroma"];
  if (packages.size !== names.length || names.some(n => !packages.has(n))) throw new Error("Release needs all five native packages and the main package");
  return {version, tag, channel, packages: names.map(n => packages.get(n))};
}
function collect(input, output, tag, channel) {
  if (fs.existsSync(output)) throw new Error("Release output must be a fresh directory");
  const release = validate(input, tag, channel);
  fs.mkdirSync(output, {recursive: true});
  for (const item of release.packages) {
    const filename = path.basename(item.file);
    fs.copyFileSync(item.file, path.join(output, filename));
    item.file = filename;
  }
  release.commit = process.env.RELEASE_COMMIT || null;
  fs.writeFileSync(path.join(output, "release-manifest.json"), JSON.stringify(release, null, 2) + "\n");
  fs.writeFileSync(path.join(output, "SHA256SUMS"), release.packages.map(p => `${p.sha256}  ${p.file}\n`).join(""));
  return release;
}
function publish(dir, execute = spawnSync) {
  if (process.env.KROMA_RELEASE_CONFIRMED !== "1") throw new Error("Publication requires explicit release confirmation");
  const manifest = JSON.parse(fs.readFileSync(path.join(dir, "release-manifest.json"), "utf8"));
  if ((process.env.RELEASE_TAG && manifest.tag !== process.env.RELEASE_TAG) ||
      (process.env.RELEASE_COMMIT && manifest.commit !== process.env.RELEASE_COMMIT) ||
      (process.env.RELEASE_CHANNEL && manifest.channel !== process.env.RELEASE_CHANNEL)) throw new Error("Release candidate differs from the approved source/channel");
  const release = validate(dir, manifest.tag, manifest.channel);
  for (const item of release.packages) {
    const old = manifest.packages.find(p => p.name === item.name);
    if (!old || old.sha256 !== item.sha256) throw new Error("Release checksum mismatch");
  }
  for (const item of release.packages) {
    const result = execute("npm", ["publish", item.file, "--access", "public", "--provenance", "--tag", release.channel, "--registry=https://registry.npmjs.org/"], {stdio: "inherit", shell: false});
    if (result.error || result.status !== 0) throw new Error(`Publication stopped at ${item.name}; do not rebuild/retry blindly. Audit already published versions.`);
  }
}
if (require.main === module) {
  try {
    const [command, ...args] = process.argv.slice(2);
    if (command === "collect" && args.length === 4) console.log(JSON.stringify(collect(...args), null, 2));
    else if (command === "publish" && args.length === 1) publish(path.resolve(args[0]));
    else throw new Error("Usage: node npm/release.cjs collect <artifacts> <fresh-output> <vVERSION> <next|latest> | publish <output>");
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
module.exports = {inspect, validate, collect, publish};
