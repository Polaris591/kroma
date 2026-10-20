#!/usr/bin/env bash
#
# Packs the npm tarball from already-built release binaries. It does NOT build them; the per-target
# requirements are:
#
#   linux-x64      cargo build --release (host OpenSSL is enough)
#   win32-x64      cargo build --release --target x86_64-pc-windows-gnu  (schannel; no OpenSSL)
#   linux-arm64    cargo build --release --target aarch64-unknown-linux-musl
#                  Needs a static OpenSSL for aarch64-musl and a musl toolchain. The
#                  arm64 musl binary previously shipped with *stub* SSL symbols and every HTTPS
#                  request failed with "TLS_client_method returned null" -- never link this target
#                  against a stub library. Build the real one with scripts/build-musl-openssl.sh.
#   android-arm64  cargo build --release --target aarch64-linux-android
#                  Needs a real static OpenSSL built with the NDK; see
#                  scripts/build-android-openssl.sh. Do NOT satisfy the -lssl/-lcrypto link with
#                  link-only stubs: Android's system libraries are BoringSSL, which is not an
#                  OpenSSL ABI (SSL_ctrl, which this crate calls for SNI, was removed), and the NDK
#                  does not expose libssl/libcrypto to third-party code at all. The previous stub
#                  generator, scripts/gen-android-tls-stubs.sh, has been deleted for that reason.
#   darwin-*       cargo build --release --target {aarch64,x86_64}-apple-darwin
#                  Requires a real macOS SDK: macOS uses native-tls, so the link pulls in
#                  Security.framework and CoreFoundation.framework.
#
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKG_DIR="${ROOT_DIR}/npm/kroma-cli"
DIST_DIR="${ROOT_DIR}/npm/dist"
VERSION="$(grep -m1 '^version = ' "${ROOT_DIR}/Cargo.toml" | sed 's/version = "\(.*\)"/\1/')"

fail() {
  echo "error: $*" >&2
  exit 1
}

# Files are copied between different GitHub jobs. Their mtimes are not comparable to a new
# checkout's source mtimes, so a cross-run `find -newer` check would give false positives and
# false assurances. Build jobs must produce binary artifacts in the SAME run; the signed candidate
# later binds the commit, job/run identity, and exact bytes. Never reuse binaries from a prior run.
require_built() {
  local binary="$1" label="$2"
  [[ -s "${binary}" && -f "${binary}" && ! -L "${binary}" ]] || \
    fail "${label}: missing, empty or symlink binary at ${binary}; rebuild it in this job"
}

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
require_built "${ROOT_DIR}/target/release/kroma" "linux-x64"
cp "${ROOT_DIR}/target/release/kroma" "${PKG_DIR}/bin/vendor/linux-x64/kroma"
chmod 0755 "${PKG_DIR}/bin/vendor/linux-x64/kroma"

# 2. Windows x64
require_built "${ROOT_DIR}/target/x86_64-pc-windows-gnu/release/kroma.exe" "win32-x64"
cp "${ROOT_DIR}/target/x86_64-pc-windows-gnu/release/kroma.exe" "${PKG_DIR}/bin/vendor/win32-x64/kroma.exe"

# 3. macOS Apple Silicon & Intel
#
# Cross-compiling for macOS needs a real SDK: since the Gate-3 audit moved macOS to native-tls,
# the link pulls in Security.framework and CoreFoundation.framework, and Apple's .tbd stubs carry
# per-symbol availability data that a hand-made stub cannot reproduce. Rather than link against a
# fake stub -- which is how the arm64 musl binary ended up shipping with a TLS backend that could
# never work -- require a freshly built binary and refuse anything else.
copy_macos() {
  local target="$1" vendor="$2"
  local bin="${ROOT_DIR}/target/${target}/release/kroma"
  require_built "${bin}" "${vendor}"
  cp "${bin}" "${PKG_DIR}/bin/vendor/${vendor}/kroma"
  chmod 0755 "${PKG_DIR}/bin/vendor/${vendor}/kroma"
}
copy_macos aarch64-apple-darwin darwin-arm64
copy_macos x86_64-apple-darwin darwin-x64

# 4. Android Termux ARM64 (static OpenSSL, linked for Android 24+)
require_built "${ROOT_DIR}/target/aarch64-linux-android/release/kroma" "android-arm64"
cp "${ROOT_DIR}/target/aarch64-linux-android/release/kroma" "${PKG_DIR}/bin/vendor/android-arm64/kroma"
chmod 0755 "${PKG_DIR}/bin/vendor/android-arm64/kroma"

# 5. Linux ARM64 (musl static, real OpenSSL)
require_built "${ROOT_DIR}/target/aarch64-unknown-linux-musl/release/kroma" "linux-arm64"
cp "${ROOT_DIR}/target/aarch64-unknown-linux-musl/release/kroma" "${PKG_DIR}/bin/vendor/linux-arm64/kroma"
chmod 0755 "${PKG_DIR}/bin/vendor/linux-arm64/kroma"

(cd "${PKG_DIR}" && npm pack --pack-destination "${DIST_DIR}" --quiet >/dev/null)
rm -rf "${PKG_DIR}/bin/vendor"

ls -lh "${DIST_DIR}/kroma-cli-${VERSION}.tgz"
