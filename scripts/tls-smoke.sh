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
# The smoke is self-contained: it runs with an isolated HOME/XDG tree and an explicit small
# `--max-tokens` ceiling, so neither the developer's `~/.kroma/config.toml`, a `KROMA_*` variable, nor
# a missing owner budget can change the result. That matters because the owner-envelope gate refuses a
# paid run without a declared ceiling -- a clean CI machine has none, so the old script failed before
# it ever reached the network.
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
mkdir -p "${WORK}/ws" "${WORK}/state" "${WORK}/home" "${WORK}/config" "${WORK}/cache" "${WORK}/data"

# A test budget, not an operator budget. Since the owner-envelope gate (M1-06) a paid run refuses to
# start without a declared spend ceiling, so a smoke that inherits whatever the developer's machine
# happens to have configured either fails before TLS or silently spends a real allowance. This ceiling
# is fixed and only covers this smoke; `--max-tokens` is the explicit per-invocation layer, so no config
# file or environment variable can change it for this run.
#
# 50000 rather than "as small as possible": the untrusted stage is *expected* to fail its handshake,
# and each request attempt reserves the per-request output allowance (8192) on top of the prompt. A
# ceiling that only covers the successful turn runs out during that retry loop and the run then reports
# `budget_exhausted` instead of the certificate error the smoke is testing for.
SMOKE_MAX_TOKENS=50000

# Everything that could redirect configuration is cleared and re-pointed inside ${WORK}: a user
# `~/.kroma/config.toml`, a KROMA_* override, or an XDG directory must not be able to change what this
# smoke measures. HOME is *set* (not unset) because the config resolver falls back to the account
# database when HOME is empty, which would reach the developer's real `~/.kroma` again.
ISOLATED_ENV=(
  "HOME=${WORK}/home"
  "XDG_CONFIG_HOME=${WORK}/config"
  "XDG_STATE_HOME=${WORK}/state"
  "XDG_CACHE_HOME=${WORK}/cache"
  "XDG_DATA_HOME=${WORK}/data"
)
UNSET_ENV=(
  KROMA_HOME KROMA_POLICY KROMA_MAX_TOKENS KROMA_PROFILE KROMA_PROJECTION_PROFILE
  KROMA_ADAPTIVE_TOURNAMENT KROMA_OPENAI_BASE_URL KROMA_OPENAI_API_KEY
  KROMA_ANTHROPIC_API_KEY KROMA_VERIFY_COMMAND KROMA_VERIFY_TIMEOUT_SECS
  KROMA_MAX_SUBAGENTS KROMA_REASONING_EFFORT KROMA_PROTOCOL KROMA_INSECURE_TLS
  KROMA_CA_BUNDLE KROMA_DAEMON_TOKEN KROMA_GATE_MAX_RETRIES
)
unset_args=()
for name in "${UNSET_ENV[@]}"; do unset_args+=("-u" "${name}"); done

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
  timeout 180 env "${unset_args[@]}" "${ISOLATED_ENV[@]}" \
    KROMA_API_KEY=tls-smoke-key \
    KROMA_BASE_URL="https://localhost:${PORT}" \
    KROMA_MODEL=tls-smoke-model \
    ${trust_env[@]+"${trust_env[@]}"} \
    "$@" \
    "${BIN}" run --task "reply with the smoke marker" \
      --workspace "${WORK}/ws" --policy read-only --max-tokens "${SMOKE_MAX_TOKENS}" \
      --output-format jsonl --quiet --no-stream \
    > "${log}" 2>&1
  local status=$?
  set -e
  return "${status}"
}

# A run that stops before the network would otherwise be misreported as a TLS failure (or worse, as a
# TLS success for the untrusted case). Name the budget gate explicitly when it is the cause.
refuse_budget_failure() {
  local log="$1"
  if grep -qiE 'owner spend envelope|budget_exhausted|TokenBudgetExhausted' "${log}"; then
    echo "error: the run stopped on the token budget, not on TLS. scripts/tls-smoke.sh must pass its" >&2
    echo "error: own --max-tokens ceiling large enough for the expected handshake retry; this is a" >&2
    echo "error: script bug, not a TLS result." >&2
    tail -20 "${log}" >&2
    exit 1
  fi
}

echo "tls-smoke: trusted certificate (expect success)"
if ! run_kroma "${WORK}/trusted.log" trusted "${@}"; then
  echo "error: the run against a trusted certificate failed" >&2
  tail -20 "${WORK}/trusted.log" >&2
  refuse_budget_failure "${WORK}/trusted.log"
  exit 1
fi
refuse_budget_failure "${WORK}/trusted.log"
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
refuse_budget_failure "${WORK}/untrusted.log"
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
