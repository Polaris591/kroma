"use strict";

const platforms = [
  {id: "win32-x64", os: "win32", cpu: "x64", targets: ["x86_64-pc-windows-msvc", "x86_64-pc-windows-gnu"]},
  {id: "linux-x64", os: "linux", cpu: "x64", targets: ["x86_64-unknown-linux-gnu"]},
  {id: "linux-arm64", os: "linux", cpu: "arm64", targets: ["aarch64-unknown-linux-gnu"]},
  {id: "darwin-x64", os: "darwin", cpu: "x64", targets: ["x86_64-apple-darwin"]},
  {id: "darwin-arm64", os: "darwin", cpu: "arm64", targets: ["aarch64-apple-darwin"]},
];

function forHost(os = process.platform, cpu = process.arch) {
  const platform = platforms.find(item => item.os === os && item.cpu === cpu);
  if (!platform) {
    throw new Error(`Unsupported platform ${os}/${cpu}. See https://github.com/Polaris591/kroma for source builds.`);
  }
  return platform;
}

function packageName(platform) {
  return `kroma-${platform.id}`;
}

function executable(platform) {
  return platform.os === "win32" ? "kroma.exe" : "kroma";
}

module.exports = {platforms, forHost, packageName, executable};
