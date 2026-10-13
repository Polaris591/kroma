#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKG_DIR="${ROOT_DIR}/npm/kroma-cli"
DIST_DIR="${ROOT_DIR}/npm/dist"
VERSION="$(grep -m1 '^version = ' "${ROOT_DIR}/Cargo.toml" | sed 's/version = "\(.*\)"/\1/')"

python3 - "${PKG_DIR}/package.json" "${VERSION}" << 'PY'
import json, sys
p, ver = sys.argv[1], sys.argv[2]
with open(p, "r", encoding="utf-8") as f:
    d = json.load(f)
d["version"] = ver
with open(p, "w", encoding="utf-8") as f:
    json.dump(d, f, indent=2, ensure_ascii=False)
    f.write("\n")
PY

mkdir -p "${DIST_DIR}" \
         "${PKG_DIR}/bin/vendor/linux-x64" \
         "${PKG_DIR}/bin/vendor/win32-x64" \
         "${PKG_DIR}/bin/vendor/darwin-arm64" \
         "${PKG_DIR}/bin/vendor/darwin-x64"
rm -rf "${DIST_DIR:?}"/*

cp "${ROOT_DIR}/target/release/kroma" "${PKG_DIR}/bin/vendor/linux-x64/kroma"
chmod 0755 "${PKG_DIR}/bin/vendor/linux-x64/kroma"

cp "${ROOT_DIR}/target/x86_64-pc-windows-gnu/release/kroma.exe" "${PKG_DIR}/bin/vendor/win32-x64/kroma.exe"

cp "${ROOT_DIR}/target/aarch64-apple-darwin/release/kroma" "${PKG_DIR}/bin/vendor/darwin-arm64/kroma"
chmod 0755 "${PKG_DIR}/bin/vendor/darwin-arm64/kroma"

cp "${ROOT_DIR}/target/x86_64-apple-darwin/release/kroma" "${PKG_DIR}/bin/vendor/darwin-x64/kroma"
chmod 0755 "${PKG_DIR}/bin/vendor/darwin-x64/kroma"

(cd "${PKG_DIR}" && npm pack --pack-destination "${DIST_DIR}" --quiet >/dev/null)
rm -rf "${PKG_DIR}/bin/vendor"

ls -lh "${DIST_DIR}/kroma-cli-${VERSION}.tgz"
