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
ZIG="${ZIG:-$(command -v zig || echo /usr/local/lib/python3.13/dist-packages/ziglang/zig)}"

[[ -x "${ZIG}" ]] || { echo "error: zig not found (set ZIG=/path/to/zig)" >&2; exit 1; }
[[ -f "${SRC}/Configure" ]] || { echo "error: ${SRC} is not an OpenSSL source tree" >&2; exit 1; }

export ZIG_GLOBAL_CACHE_DIR="${ZIG_GLOBAL_CACHE_DIR:-/var/cache/zig}"
export ZIG_LOCAL_CACHE_DIR="${ZIG_LOCAL_CACHE_DIR:-/var/cache/zig}"
mkdir -p "${ZIG_GLOBAL_CACHE_DIR}"

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
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

echo "==> installed ${PREFIX}/lib/libssl.a and libcrypto.a"
echo "    build the target with: linker = zig cc -target aarch64-linux-musl -L${PREFIX}/lib"
echo "                           rustflags = [\"-C\", \"link-self-contained=no\"]"
