#!/usr/bin/env bash
# Fetch and verify the OpenSSL source archive before anything is extracted from it.
#
# Every workflow path goes through this script, so verification cannot be present on one platform and
# missing on another. It fails closed: an empty or malformed pin, a download error, or a digest mismatch
# stops the job before `tar` ever sees the bytes.
#
# Usage: scripts/fetch-openssl.sh <version> <expected-sha256> [dest-dir]
set -euo pipefail

version="${1:-}"
expected="${2:-}"
dest="${3:-.}"

if [ -z "${version}" ]; then
  echo "error: openssl version is required" >&2
  exit 2
fi
if [ -z "${expected}" ]; then
  echo "error: OPENSSL_SHA256 is not set; refusing to fetch an unverified archive" >&2
  echo "       pin it once, from an independently reviewed source, then commit the value" >&2
  exit 3
fi
# Lowercase 64 hex characters, nothing else: a truncated or "looks fine" pin is a configuration bug.
case "${expected}" in
  [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]*) ;;
  *) echo "error: OPENSSL_SHA256 must be 64 lowercase hex characters, got '${expected}'" >&2; exit 3 ;;
esac
if [ "${#expected}" -ne 64 ]; then
  echo "error: OPENSSL_SHA256 must be exactly 64 characters, got ${#expected}" >&2
  exit 3
fi

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT
archive="${tmp}/openssl-${version}.tar.gz"

fetch() {
  curl -fsSL --retry 3 --connect-timeout 20 -o "${archive}" "$1"
}

fetch "https://www.openssl.org/source/openssl-${version}.tar.gz" \
  || fetch "https://github.com/openssl/openssl/releases/download/openssl-${version}/openssl-${version}.tar.gz"

actual="$(sha256sum "${archive}" | awk '{print $1}')"
if [ "${actual}" != "${expected}" ]; then
  echo "error: OpenSSL archive digest mismatch" >&2
  echo "       expected ${expected}" >&2
  echo "       actual   ${actual}" >&2
  echo "       refusing to extract an unverified archive" >&2
  exit 4
fi

mkdir -p "${dest}"
tar -xzf "${archive}" -C "${dest}"
echo "ok: openssl-${version} verified (sha256 ${actual}) and extracted into ${dest}"
