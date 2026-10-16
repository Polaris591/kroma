#!/usr/bin/env bash
#
# Build a static OpenSSL for aarch64-linux-android, which the android-arm64 release binary links
# (crates/kroma-core/src/provider/transport.rs binds libssl/libcrypto on every unix target except
# macOS).
#
# Do NOT satisfy those symbols with the system BoringSSL. Android's internal BoringSSL is not an
# OpenSSL ABI: Google removed low-level entry points such as SSL_ctrl (which this crate calls for
# SNI), and the NDK does not export libssl/libcrypto to third-party code at all. A generated
# link-only stub therefore only postpones the failure to the first HTTPS request on a real device.
# The previous `scripts/gen-android-tls-stubs.sh` did exactly that and has been deleted.
#
# Usage: scripts/build-android-openssl.sh <openssl-src-dir> [install-prefix] [api-level]
#
# Requires: an Android NDK r22+ (no `platforms/` directory, but `meta/platforms.json`) or a
# standalone LLVM toolchain from `.../toolchains/llvm/prebuilt/<host>`, and an OpenSSL 3.x source
# tree. Set ANDROID_NDK_ROOT to either root; the toolchain directory is then auto-detected.
set -euo pipefail

SRC="${1:?usage: build-android-openssl.sh <openssl-src-dir> [install-prefix] [api-level]}"
PREFIX="${2:-$PWD/openssl-android}"
API="${3:-24}"
NDK_INPUT="${ANDROID_NDK_ROOT:-${ANDROID_NDK_HOME:-/opt/android-ndk/android-ndk-r26d}}"
HOST_TAG="${NDK_HOST_TAG:-linux-x86_64}"

[[ -f "${SRC}/Configure" ]] || { echo "error: ${SRC} is not an OpenSSL source tree" >&2; exit 1; }

# OpenSSL's android-* targets require ANDROID_NDK_ROOT to name a root they recognize: a full NDK
# (ships `source.properties`) or a standalone toolchain (ships `AndroidVersion.txt`). Accept all
# three shapes people actually have: a full NDK root, a standalone toolchain root, or the nested
# `toolchains/llvm/prebuilt/<host>` extraction that some installations carry without NDK metadata.
CLEANUP_PATHS=()
cleanup() {
  for path in "${CLEANUP_PATHS[@]:-}"; do
    [[ -n "${path}" ]] && rm -rf "${path}"
  done
}
trap cleanup EXIT
TOOLCHAIN="${NDK_INPUT}/toolchains/llvm/prebuilt/${HOST_TAG}"
if [[ -f "${NDK_INPUT}/source.properties" ]]; then
  NDK_ROOT="${NDK_INPUT}"
elif [[ -f "${NDK_INPUT}/AndroidVersion.txt" ]]; then
  NDK_ROOT="${NDK_INPUT}"
  TOOLCHAIN="${NDK_INPUT}"
elif [[ -f "${TOOLCHAIN}/AndroidVersion.txt" ]]; then
  # Only the LLVM toolchain is present. Point OpenSSL at a synthesised root around it, so that the
  # canonical `android-arm64` configure path (not a hand-rolled linux-aarch64 one) still runs.
  SYNTHETIC_ROOT="$(mktemp -d)"
  CLEANUP_PATHS+=("${SYNTHETIC_ROOT}")
  mkdir -p "${SYNTHETIC_ROOT}/toolchains/llvm"
  ln -s "${NDK_INPUT}/toolchains/llvm/prebuilt" "${SYNTHETIC_ROOT}/toolchains/llvm/prebuilt"
  : > "${SYNTHETIC_ROOT}/source.properties"
  NDK_ROOT="${SYNTHETIC_ROOT}"
  TOOLCHAIN="${SYNTHETIC_ROOT}/toolchains/llvm/prebuilt/${HOST_TAG}"
else
  echo "error: ${NDK_INPUT} is neither a full NDK nor a standalone LLVM toolchain" >&2
  echo "       (set ANDROID_NDK_ROOT to the NDK root or to toolchains/llvm/prebuilt/<host>)" >&2
  exit 1
fi
[[ -x "${TOOLCHAIN}/bin/aarch64-linux-android${API}-clang" ]] || {
  echo "error: ${TOOLCHAIN}/bin/aarch64-linux-android${API}-clang not found (unsupported API level ${API}?)" >&2
  exit 1
}

# OpenSSL's android-* targets discover the NDK toolchain from ANDROID_NDK_ROOT plus a clang on PATH.
export ANDROID_NDK_ROOT="${NDK_ROOT}"
export PATH="${TOOLCHAIN}/bin:${PATH}"

cd "${SRC}"
make distclean >/dev/null 2>&1 || true
./Configure android-arm64 \
  -D__ANDROID_API__="${API}" \
  --prefix="${PREFIX}" --openssldir="${PREFIX}" \
  no-shared no-tests no-apps no-docs -O2
make -j"$(nproc)" build_sw

mkdir -p "${PREFIX}/lib"
cp libssl.a libcrypto.a "${PREFIX}/lib/"

# Prove the archives carry the implementation, not just symbol names: every entry point the crate
# binds must be defined in the archive, and the version string the release gate greps for must be
# present. A stub library would fail both checks. The tool output goes to files first: under
# `set -o pipefail`, `nm | grep -q` kills nm with SIGPIPE (141) as soon as grep has its match, which
# would report a perfectly good archive as broken.
VERIFY_DIR="$(mktemp -d)"
CLEANUP_PATHS+=("${VERIFY_DIR}")
"${TOOLCHAIN}/bin/llvm-nm" --defined-only "${PREFIX}/lib/libssl.a" > "${VERIFY_DIR}/libssl.nm"
for sym in TLS_client_method SSL_CTX_new SSL_ctrl SSL_set1_host SSL_connect; do
  grep -qE " [Tt] ${sym}$" "${VERIFY_DIR}/libssl.nm" || {
    echo "error: ${sym} is not defined in the built libssl.a" >&2
    exit 1
  }
done
"${TOOLCHAIN}/bin/llvm-nm" --defined-only "${PREFIX}/lib/libcrypto.a" > "${VERIFY_DIR}/libcrypto.nm"
grep -qE " [Tt] ERR_clear_error$" "${VERIFY_DIR}/libcrypto.nm" || {
  echo "error: ERR_clear_error is not defined in the built libcrypto.a" >&2
  exit 1
}
strings -a "${PREFIX}/lib/libcrypto.a" > "${VERIFY_DIR}/libcrypto.strings"
grep -q 'OpenSSL 3' "${VERIFY_DIR}/libcrypto.strings" || {
  echo "error: libcrypto.a carries no OpenSSL version string" >&2
  exit 1
}

echo "==> installed ${PREFIX}/lib/libssl.a and libcrypto.a (aarch64-linux-android, API ${API})"
echo "    link the target with -L${PREFIX}/lib so these static archives satisfy -lssl/-lcrypto"
