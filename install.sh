#!/usr/bin/env bash
# Harness v1.1.0 — Linux & macOS One-Line Installer
# Builds (or installs prebuilt) `harness` into ~/.local/bin or /usr/local/bin so you can type `harness` in any terminal.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "==> Building Harness v1.1.0 (release profile)..."
cargo build --workspace --release

BIN_SRC="$SCRIPT_DIR/target/release/harness"
if [[ ! -f "$BIN_SRC" ]]; then
  echo "error: build did not produce $BIN_SRC" >&2
  exit 1
fi

if [[ -w "/usr/local/bin" ]]; then
  INSTALL_DIR="/usr/local/bin"
else
  INSTALL_DIR="$HOME/.local/bin"
  mkdir -p "$INSTALL_DIR"
fi

install -m 0755 "$BIN_SRC" "$INSTALL_DIR/harness"
echo "==> Installed: $INSTALL_DIR/harness ($( "$INSTALL_DIR/harness" --version ))"

if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
  echo "==> Note: add '$INSTALL_DIR' to your PATH:"
  echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
fi
echo "==> Done! Open any project folder and run: harness"
