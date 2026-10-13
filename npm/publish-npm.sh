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

"${NPM_DIR}/build-npm-packages.sh"
VERSION="$(grep -m1 '^version = ' "${ROOT_DIR}/Cargo.toml" | sed 's/version = "\(.*\)"/\1/')"
npm publish "${DIST_DIR}/kroma-cli-${VERSION}.tgz" --access public --registry="${REGISTRY}"
echo "✓ Published all-in-one kroma-cli@${VERSION} to ${REGISTRY}!"
