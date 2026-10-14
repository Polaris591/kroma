#!/usr/bin/env bash
#
# Generate the link-only libssl/libcrypto stubs the Android build needs.
#
# Android ships libssl.so and libcrypto.so in /system/lib64, so a binary that links them resolves
# the real symbols at runtime -- but the NDK sysroot does not carry them, so `-lssl` fails at link
# time. These stubs declare exactly the symbols kroma calls (the extern block in
# crates/kroma-core/src/provider/transport.rs) with the matching SONAME, which is enough for the
# linker and correct at runtime.
#
# This is legitimate here and is NOT a way to work around a missing dependency: the stubs declare
# names, they do not implement behaviour. Linking the arm64 *musl* target against stubs is what
# produced a released binary whose every HTTPS request failed with "TLS_client_method returned
# null"; see npm/build-npm-packages.sh.
#
# Usage: scripts/gen-android-tls-stubs.sh [NDK_ROOT] [API_LEVEL]
set -euo pipefail

NDK_ROOT="${1:-${ANDROID_NDK_ROOT:-${ANDROID_NDK_HOME:-/opt/android-ndk/android-ndk-r26d}}}"
API_LEVEL="${2:-24}"
HOST_TAG="linux-x86_64"
LIB_DIR="${NDK_ROOT}/toolchains/llvm/prebuilt/${HOST_TAG}/sysroot/usr/lib/aarch64-linux-android/${API_LEVEL}"
CC="${NDK_ROOT}/toolchains/llvm/prebuilt/${HOST_TAG}/bin/aarch64-linux-android${API_LEVEL}-clang"

if [[ ! -x "${CC}" ]]; then
  echo "error: ${CC} not found; pass the NDK root as \$1" >&2
  exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT

cat > "${TMP}/ssl.c" <<'EOF'
#define F(n) void n(void) {}
F(TLS_client_method) F(SSL_CTX_new) F(SSL_CTX_free) F(SSL_CTX_set_default_verify_paths)
F(SSL_CTX_load_verify_locations) F(SSL_new) F(SSL_free) F(SSL_set_fd) F(SSL_set_verify)
F(SSL_ctrl) F(SSL_set1_host) F(SSL_get_verify_result) F(SSL_connect) F(SSL_read) F(SSL_write)
F(SSL_shutdown) F(SSL_get_error)
EOF
echo 'void ERR_clear_error(void) {}' > "${TMP}/crypto.c"

mkdir -p "${LIB_DIR}"
"${CC}" -shared -fPIC -nostdlib -Wl,-soname,libssl.so -o "${LIB_DIR}/libssl.so" "${TMP}/ssl.c"
"${CC}" -shared -fPIC -nostdlib -Wl,-soname,libcrypto.so -o "${LIB_DIR}/libcrypto.so" "${TMP}/crypto.c"
echo "==> wrote ${LIB_DIR}/libssl.so and libcrypto.so"
