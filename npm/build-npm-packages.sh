#!/usr/bin/env bash
#
# Packs the npm tarball from already-built release binaries. It does NOT build them; see
# README/"Releasing" for the per-target requirements, which are:
#
#   linux-x64      cargo build --release (host OpenSSL is enough)
#   win32-x64      cargo build --release --target x86_64-pc-windows-gnu  (schannel; no OpenSSL)
#   linux-arm64    cargo build --release --target aarch64-unknown-linux-musl
#                  Needs a static OpenSSL for aarch64-musl and a musl toolchain. The
#                  arm64 musl binary previously shipped with *stub* SSL symbols and every HTTPS
#                  request failed with "TLS_client_method returned null" -- never link this target
#                  against a stub library. Zig's bundled musl plus an OpenSSL built for
#                  aarch64-musl (see /opt/ossl/openssl-3.5.7) is the known-good combination.
#   android-arm64  cargo build --release --target aarch64-linux-android
#                  The NDK sysroot ships no libssl/libcrypto. Link-only stubs whose SONAME
#                  matches Android's system libraries are enough (they are resolved at runtime
#                  from /system/lib64); see the stubs in the NDK's
#                  sysroot/usr/lib/aarch64-linux-android/<api>/ directory.
#   darwin-*       cargo build --release --target {aarch64,x86_64}-apple-darwin
#                  Requires a real macOS SDK: macOS uses native-tls, so the link pulls in
#                  Security.framework and CoreFoundation.framework.
#
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
         "${PKG_DIR}/bin/vendor/darwin-x64" \
         "${PKG_DIR}/bin/vendor/android-arm64" \
         "${PKG_DIR}/bin/vendor/linux-arm64"
rm -rf "${DIST_DIR:?}"/*

# 1. Linux x64
cp "${ROOT_DIR}/target/release/kroma" "${PKG_DIR}/bin/vendor/linux-x64/kroma"
chmod 0755 "${PKG_DIR}/bin/vendor/linux-x64/kroma"

# 2. Windows x64
cp "${ROOT_DIR}/target/x86_64-pc-windows-gnu/release/kroma.exe" "${PKG_DIR}/bin/vendor/win32-x64/kroma.exe"

# 3. macOS Apple Silicon & Intel
#
# Cross-compiling for macOS needs a real SDK: since the Gate-3 audit moved macOS to native-tls,
# the link pulls in Security.framework and CoreFoundation.framework, and Apple's .tbd stubs carry
# per-symbol availability data that a hand-made stub cannot reproduce. Rather than link against a
# fake stub -- which is how the arm64 musl binary ended up shipping with a TLS backend that could
# never work -- refuse loudly and leave the previous artifact in place if one exists.
copy_macos() {
  local target="$1" vendor="$2"
  if [[ ! -f "${ROOT_DIR}/target/${target}/release/kroma" ]]; then
    echo "error: ${target} binary missing." >&2
    echo "       Build it on a Mac, or install the macOS SDK and set" >&2
    echo "       CARGO_TARGET_$(echo "${target}" | tr 'a-z-' 'A-Z_')_LINKER." >&2
    return 1
  fi
  local bin="${ROOT_DIR}/target/${target}/release/kroma"
  # A stale artifact is worse than a missing one: it silently ships code that predates the
  # current source. Say so instead of hiding it.
  local newest_src
  newest_src="$(find "${ROOT_DIR}/crates" -name '*.rs' -newer "${bin}" -print -quit)"
  if [[ -n "${newest_src}" ]]; then
    echo "warning: ${vendor} binary is older than the current sources (e.g. ${newest_src#"${ROOT_DIR}/"})." >&2
    echo "         It does NOT contain the current code. Rebuild it on a Mac before publishing." >&2
  fi
  cp "${bin}" "${PKG_DIR}/bin/vendor/${vendor}/kroma"
  chmod 0755 "${PKG_DIR}/bin/vendor/${vendor}/kroma"
}
copy_macos aarch64-apple-darwin darwin-arm64
copy_macos x86_64-apple-darwin darwin-x64

# 4. Android Termux ARM64 (Official NDK Bionic dynamically linked for Android 24+)
cp "${ROOT_DIR}/target/aarch64-linux-android/release/kroma" "${PKG_DIR}/bin/vendor/android-arm64/kroma"
chmod 0755 "${PKG_DIR}/bin/vendor/android-arm64/kroma"

# 5. Linux ARM64 (musl static)
cp "${ROOT_DIR}/target/aarch64-unknown-linux-musl/release/kroma" "${PKG_DIR}/bin/vendor/linux-arm64/kroma"
chmod 0755 "${PKG_DIR}/bin/vendor/linux-arm64/kroma"

(cd "${PKG_DIR}" && npm pack --pack-destination "${DIST_DIR}" --quiet >/dev/null)
rm -rf "${PKG_DIR}/bin/vendor"

ls -lh "${DIST_DIR}/kroma-cli-${VERSION}.tgz"
