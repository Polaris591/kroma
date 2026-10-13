#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NPM_DIR="${ROOT_DIR}/npm"
DIST_DIR="${NPM_DIR}/dist"
VERSION="$(grep -m1 '^version = ' "${ROOT_DIR}/Cargo.toml" | sed 's/version = "\(.*\)"/\1/')"

echo "[1/4] Syncing npm package versions to ${VERSION}..."
for pkg in "${NPM_DIR}"/*/package.json; do
  python3 - "$pkg" "$VERSION" << 'PY'
import json, sys
path, ver = sys.argv[1], sys.argv[2]
with open(path, "r", encoding="utf-8") as f:
    data = json.load(f)
data["version"] = ver
if "optionalDependencies" in data:
    for k in data["optionalDependencies"]:
        data["optionalDependencies"][k] = ver
with open(path, "w", encoding="utf-8") as f:
    json.dump(data, f, indent=2, ensure_ascii=False)
    f.write("\n")
PY
done

mkdir -p "${DIST_DIR}"
rm -rf "${DIST_DIR:?}"/*

LINUX_BIN="${ROOT_DIR}/target/release/kroma"
WIN_BIN="${ROOT_DIR}/target/x86_64-pc-windows-gnu/release/kroma.exe"

echo "[2/4] Packing platform-specific npm packages (@kroma-ai/cli-linux-x64 & @kroma-ai/cli-win32-x64)..."
cp "${LINUX_BIN}" "${NPM_DIR}/cli-linux-x64/bin/kroma"
chmod 0755 "${NPM_DIR}/cli-linux-x64/bin/kroma"
(cd "${NPM_DIR}/cli-linux-x64" && npm pack --pack-destination "${DIST_DIR}" --quiet >/dev/null)

cp "${WIN_BIN}" "${NPM_DIR}/cli-win32-x64/bin/kroma.exe"
(cd "${NPM_DIR}/cli-win32-x64" && npm pack --pack-destination "${DIST_DIR}" --quiet >/dev/null)

echo "[3/4] Packing standalone universal offline package (kroma-cli-${VERSION}-standalone.tgz with Linux + Windows binaries)..."
WORK_STANDALONE="$(mktemp -d)"
cp -r "${NPM_DIR}/kroma-cli/"* "${WORK_STANDALONE}/"
mkdir -p "${WORK_STANDALONE}/bin/vendor/linux-x64" "${WORK_STANDALONE}/bin/vendor/win32-x64"
cp "${LINUX_BIN}" "${WORK_STANDALONE}/bin/vendor/linux-x64/kroma"
chmod 0755 "${WORK_STANDALONE}/bin/vendor/linux-x64/kroma"
cp "${WIN_BIN}" "${WORK_STANDALONE}/bin/vendor/win32-x64/kroma.exe"
python3 - "${WORK_STANDALONE}/package.json" << 'PY'
import json, sys
p = sys.argv[1]
with open(p, "r", encoding="utf-8") as f:
    d = json.load(f)
d.pop("optionalDependencies", None)
with open(p, "w", encoding="utf-8") as f:
    json.dump(d, f, indent=2, ensure_ascii=False)
    f.write("\n")
PY
(cd "${WORK_STANDALONE}" && npm pack --pack-destination "${DIST_DIR}" --quiet >/dev/null)
mv "${DIST_DIR}/kroma-cli-${VERSION}.tgz" "${DIST_DIR}/kroma-cli-${VERSION}-standalone.tgz"
rm -rf "${WORK_STANDALONE}"

echo "[4/4] Packing registry launcher package (kroma-cli-${VERSION}.tgz)..."
rm -rf "${NPM_DIR}/kroma-cli/bin/vendor"
(cd "${NPM_DIR}/kroma-cli" && npm pack --pack-destination "${DIST_DIR}" --quiet >/dev/null)

ls -lh "${DIST_DIR}"
