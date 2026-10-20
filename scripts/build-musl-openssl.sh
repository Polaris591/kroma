#!/usr/bin/env bash
#
# Build a static OpenSSL for aarch64-unknown-linux-musl, which the linux-arm64 release binary
# needs (crates/kroma-core/src/provider/transport.rs links libssl/libcrypto on every unix target
# except macOS).
#
# Do not substitute a stub library here. The arm64 musl binary that shipped before this script
# existed was linked against stubs, and every HTTPS request failed at runtime with
# "TLS connection failed: TLS_client_method returned null" -- the binary had no OpenSSL symbols
# and no dynamic dependencies at all.
#
# Requires zig (used as the C compiler for the musl target) and an OpenSSL source tree.
#
# Usage: scripts/build-musl-openssl.sh <openssl-src-dir> [install-prefix]
set -euo pipefail

SRC="${1:?usage: build-musl-openssl.sh <openssl-src-dir> [install-prefix]}"
PREFIX="${2:-/opt/openssl-musl-aarch64}"

# Locate a *complete* zig installation. `pip install ziglang` ships the executable next to its own
# lib/ directory; copying just the executable elsewhere makes every invocation fail with
# "unable to find zig installation directory from executable path" (this is exactly how the release
# workflow used to break). ZIG_LIB_DIR is honoured by zig as an explicit override.
ZIG="${ZIG:-$(command -v zig || true)}"
if [[ -z "${ZIG}" ]]; then
  ZIG="$(python3 -c 'import os, ziglang; print(os.path.join(os.path.dirname(ziglang.__file__), "zig"))' 2>/dev/null || true)"
fi
[[ -n "${ZIG}" && -x "${ZIG}" ]] || {
  echo "error: zig not found (set ZIG=/path/to/zig; pip's ziglang package installs it next to lib/)" >&2
  exit 1
}
[[ -f "${SRC}/Configure" ]] || { echo "error: ${SRC} is not an OpenSSL source tree" >&2; exit 1; }

# A build cache under /var/cache (the previous default) is not writable by ordinary users or by
# GitHub's runner user, and a failed `mkdir -p` there used to abort the build with no explanation.
export ZIG_GLOBAL_CACHE_DIR="${ZIG_GLOBAL_CACHE_DIR:-${TMPDIR:-/tmp}/kroma-zig-cache}"
export ZIG_LOCAL_CACHE_DIR="${ZIG_LOCAL_CACHE_DIR:-${ZIG_GLOBAL_CACHE_DIR}}"
mkdir -p "${ZIG_GLOBAL_CACHE_DIR}" "${ZIG_LOCAL_CACHE_DIR}" || {
  echo "error: cannot create the zig cache directory ${ZIG_GLOBAL_CACHE_DIR} (set ZIG_GLOBAL_CACHE_DIR to a writable path)" >&2
  exit 1
}

CLEANUP_PATHS=()
cleanup() {
  for path in "${CLEANUP_PATHS[@]:-}"; do
    [[ -n "${path}" ]] && rm -rf "${path}"
  done
}
trap cleanup EXIT

WORK="$(mktemp -d)"
CLEANUP_PATHS+=("${WORK}")

# Prove the installation can actually compile for the target before configuring OpenSSL with it;
# a bare executable with no lib/ fails here in one line instead of deep inside `make`.
printf 'int main(void){return 0;}\n' > "${WORK}/zig-probe.c"
if ! "${ZIG}" cc -target aarch64-linux-musl -c "${WORK}/zig-probe.c" -o "${WORK}/zig-probe.o" > "${WORK}/zig-probe.log" 2>&1; then
  echo "error: the zig installation at ${ZIG} is incomplete" >&2
  echo "       point ZIG at the full pip install (…/site-packages/ziglang/zig), or set ZIG_LIB_DIR" >&2
  sed -n '1,5p' "${WORK}/zig-probe.log" >&2
  exit 1
fi
# Wrapper scripts: OpenSSL's build wants a plain command name for CC/AR/RANLIB/NM, and zig needs
# its cache directories in the environment.
# `zig cc` takes the target; the binutils-style subcommands (ar/ranlib/nm) do not.
for tool in cc ar ranlib nm; do
  target=""
  [ "${tool}" = "cc" ] && target="-target aarch64-linux-musl"
  cat > "${WORK}/zig-${tool}" <<EOF
#!/bin/sh
export ZIG_GLOBAL_CACHE_DIR="${ZIG_GLOBAL_CACHE_DIR}"
export ZIG_LOCAL_CACHE_DIR="${ZIG_LOCAL_CACHE_DIR}"
exec "${ZIG}" ${tool} ${target} "\$@"
EOF
  chmod +x "${WORK}/zig-${tool}"
done

export PATH="${WORK}:${PATH}"

cd "${SRC}"
make distclean >/dev/null 2>&1 || true
./Configure linux-aarch64 \
  --prefix="${PREFIX}" --openssldir="${PREFIX}" \
  no-shared no-tests no-apps no-docs -O2 \
  CC="zig-cc" AR="zig-ar" RANLIB="zig-ranlib" NM="zig-nm"
make -j"$(nproc)" build_sw
mkdir -p "${PREFIX}/lib"
cp libssl.a libcrypto.a "${PREFIX}/lib/"

# Prove the archives carry the implementation, not just symbol names. Tool output goes to files
# first: under `set -o pipefail`, `nm | grep -q` kills nm with SIGPIPE (141) as soon as grep has its
# match, which would report a perfectly good archive as broken.
VERIFY_DIR="$(mktemp -d)"
CLEANUP_PATHS+=("${VERIFY_DIR}")
nm --defined-only "${PREFIX}/lib/libssl.a" > "${VERIFY_DIR}/libssl.nm"
for sym in TLS_client_method SSL_CTX_new SSL_ctrl SSL_set1_host SSL_connect; do
  grep -qE " [Tt] ${sym}$" "${VERIFY_DIR}/libssl.nm" || {
    echo "error: ${sym} is not defined in the built libssl.a" >&2
    exit 1
  }
done
nm --defined-only "${PREFIX}/lib/libcrypto.a" > "${VERIFY_DIR}/libcrypto.nm"
grep -qE " [Tt] ERR_clear_error$" "${VERIFY_DIR}/libcrypto.nm" || {
  echo "error: ERR_clear_error is not defined in the built libcrypto.a" >&2
  exit 1
}
strings -a "${PREFIX}/lib/libcrypto.a" > "${VERIFY_DIR}/libcrypto.strings"
grep -q 'OpenSSL 3' "${VERIFY_DIR}/libcrypto.strings" || {
  echo "error: libcrypto.a carries no OpenSSL version string" >&2
  exit 1
}

echo "==> installed ${PREFIX}/lib/libssl.a and libcrypto.a"
echo "    build the target with: linker = zig cc -target aarch64-linux-musl -L${PREFIX}/lib"
echo "                           rustflags = [\"-C\", \"link-self-contained=no\"]"
