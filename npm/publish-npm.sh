#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NPM_DIR="${ROOT_DIR}/npm"
DIST_DIR="${NPM_DIR}/dist"
REGISTRY="${KROMA_NPM_REGISTRY:-https://registry.npmjs.org}"

if [[ -n "${NPM_TOKEN:-}" ]]; then
  NPMRC_TMP="$(mktemp)"
  trap 'rm -f "${NPMRC_TMP}"' EXIT
  echo "//registry.npmjs.org/:_authToken=${NPM_TOKEN}" > "${NPMRC_TMP}"
  export NPM_CONFIG_USERCONFIG="${NPMRC_TMP}"
fi

echo "[1/3] Checking npm authentication on ${REGISTRY}..."
NPM_USER="$(npm whoami --registry="${REGISTRY}")"
echo "  Authenticated as: ${NPM_USER}"

echo "[2/3] Building fresh npm tarballs..."
"${NPM_DIR}/build-npm-packages.sh"

VERSION="$(grep -m1 '^version = ' "${ROOT_DIR}/Cargo.toml" | sed 's/version = "\(.*\)"/\1/')"

echo "[3/3] Publishing platform binary packages & main launcher (v${VERSION})..."
for tarball in \
  "${DIST_DIR}/kroma-cli-linux-x64-${VERSION}.tgz" \
  "${DIST_DIR}/kroma-cli-win32-x64-${VERSION}.tgz" \
  "${DIST_DIR}/kroma-cli-${VERSION}.tgz"; do
  echo "  -> publishing $(basename "${tarball}")..."
  npm publish "${tarball}" --access public --registry="${REGISTRY}"
done

echo "✓ Published kroma-cli@${VERSION} to ${REGISTRY}!"
echo "  Install globally: npm install -g kroma-cli"
echo "  Upgrade anytime:  kroma upgrade"
