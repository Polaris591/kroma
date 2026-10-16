#!/usr/bin/env bash
#
# End-to-end TLS smoke for a built kroma binary.
#
# `--version` is not a TLS test, and `strings | grep OpenSSL` only proves a library went in. This
# script makes the artifact actually exercise its TLS backend against a local HTTPS server:
#
#   1. a certificate the client trusts MUST be accepted, the model turn MUST come back;
#   2. a certificate the client does not trust MUST be rejected (no turn, non-zero exit).
#
# It runs the artifact as-is, so `scripts/tls-smoke.sh <bin> qemu-aarch64-static` also smoke-tests a
# cross-built linux-arm64 binary. An Android device build cannot be run this way; see the release
# workflow for what is and is not covered there.
#
# Requires: openssl, python3, and a kroma binary whose transport is the OpenSSL FFI (any unix
# target except macOS).
#
# Usage: scripts/tls-smoke.sh <kroma-binary> [runner ...]
set -euo pipefail

BIN="${1:?usage: scripts/tls-smoke.sh <kroma-binary> [runner ...]}"
shift
[[ -x "${BIN}" ]] || { echo "error: ${BIN} is not an executable" >&2; exit 1; }
BIN="$(cd "$(dirname "${BIN}")" && pwd)/$(basename "${BIN}")"

command -v openssl >/dev/null || { echo "error: openssl is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }

WORK="$(mktemp -d)"
SERVER_PID=""
cleanup() {
  [[ -n "${SERVER_PID}" ]] && kill "${SERVER_PID}" 2>/dev/null || true
  rm -rf "${WORK}"
}
trap cleanup EXIT

MARKER="TLS_SMOKE_OK"
mkdir -p "${WORK}/ws" "${WORK}/state"

# A self-signed certificate for localhost: valid for the client once it is the configured CA
# bundle, and untrusted when it is not.
openssl req -x509 -newkey rsa:2048 -sha256 -days 2 -nodes \
  -keyout "${WORK}/key.pem" -out "${WORK}/cert.pem" \
  -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" >/dev/null 2>&1

cat > "${WORK}/server.py" <<'PY'
import http.server, json, ssl, sys

cert, key, ready_file = sys.argv[1], sys.argv[2], sys.argv[3]
body = json.dumps({
    "id": "tls-smoke",
    "object": "chat.completion",
    "model": "tls-smoke",
    "choices": [{"index": 0, "message": {"role": "assistant", "content": "TLS_SMOKE_OK"},
                 "finish_reason": "stop"}],
    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
}).encode()


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        self.rfile.read(length)
        with open(ready_file, "a", encoding="utf-8") as fh:
            fh.write("served %s\n" % self.request.version())
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(cert, key)
server.socket = context.wrap_socket(server.socket, server_side=True)
with open(ready_file, "w", encoding="utf-8") as fh:
    fh.write("ready %d\n" % server.server_address[1])
server.serve_forever()
PY

python3 "${WORK}/server.py" "${WORK}/cert.pem" "${WORK}/key.pem" "${WORK}/server.log" &
SERVER_PID=$!
for _ in $(seq 1 100); do
  grep -q '^ready ' "${WORK}/server.log" 2>/dev/null && break
  sleep 0.1
done
grep -q '^ready ' "${WORK}/server.log" || { echo "error: the TLS smoke server did not start" >&2; cat "${WORK}/server.log" >&2; exit 1; }
PORT="$(awk '/^ready /{print $2; exit}' "${WORK}/server.log")"
echo "tls-smoke: server on https://localhost:${PORT}"

run_kroma() {
  # $1: output log, $2: "trusted" | "untrusted", rest: optional runner (e.g. qemu-aarch64)
  local log="$1" trust="$2"
  shift 2
  local -a trust_env=()
  [[ "${trust}" == "trusted" ]] && trust_env=("KROMA_CA_BUNDLE=${WORK}/cert.pem")
  set +e
  timeout 180 env -u KROMA_INSECURE_TLS \
    KROMA_API_KEY=tls-smoke-key \
    KROMA_BASE_URL="https://localhost:${PORT}" \
    KROMA_MODEL=tls-smoke-model \
    XDG_STATE_HOME="${WORK}/state" \
    ${trust_env[@]+"${trust_env[@]}"} \
    "$@" \
    "${BIN}" run --task "reply with the smoke marker" \
      --workspace "${WORK}/ws" --policy read-only --output-format jsonl --quiet --no-stream \
    > "${log}" 2>&1
  local status=$?
  set -e
  return "${status}"
}

echo "tls-smoke: trusted certificate (expect success)"
if ! run_kroma "${WORK}/trusted.log" trusted "${@}"; then
  echo "error: the run against a trusted certificate failed" >&2
  tail -20 "${WORK}/trusted.log" >&2
  exit 1
fi
grep -q "${MARKER}" "${WORK}/trusted.log" || {
  echo "error: the trusted run produced no model turn; TLS did not work" >&2
  tail -20 "${WORK}/trusted.log" >&2
  exit 1
}
grep -q '^served TLSv1' "${WORK}/server.log" || {
  echo "error: the server never completed a TLS handshake" >&2
  cat "${WORK}/server.log" >&2
  exit 1
}

echo "tls-smoke: untrusted certificate (expect rejection)"
if run_kroma "${WORK}/untrusted.log" untrusted "${@}"; then
  echo "error: the run SUCCEEDED against an untrusted certificate; verification is not enforced" >&2
  exit 1
fi
if grep -q "${MARKER}" "${WORK}/untrusted.log"; then
  echo "error: the untrusted run still produced a model turn" >&2
  exit 1
fi
grep -Eqi 'TLS handshake failed|certificate' "${WORK}/untrusted.log" || {
  echo "error: the untrusted run failed for an unexpected reason (not certificate rejection)" >&2
  tail -20 "${WORK}/untrusted.log" >&2
  exit 1
}

echo "tls-smoke: PASS (trusted certificate accepted, untrusted certificate rejected)"
