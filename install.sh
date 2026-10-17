#!/usr/bin/env bash
# Kroma v1.2.0 — Linux & macOS One-Line Installer
# Builds (or installs prebuilt) `kroma` into /usr/local/bin (or ~/.kroma/bin) so you can type
# `kroma` in any terminal to launch the interactive TUI in the current directory.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "==> Building Kroma v1.2.0 (release profile)..."
cargo build --workspace --release

BIN_SRC="$SCRIPT_DIR/target/release/kroma"
if [[ ! -f "$BIN_SRC" ]]; then
  echo "error: build did not produce $BIN_SRC" >&2
  exit 1
fi

if [[ -w "/usr/local/bin" ]]; then
  INSTALL_DIR="/usr/local/bin"
else
  INSTALL_DIR="$HOME/.kroma/bin"
  mkdir -p "$INSTALL_DIR"
fi

install -m 0755 "$BIN_SRC" "$INSTALL_DIR/kroma"
echo "==> Installed: $INSTALL_DIR/kroma ($( "$INSTALL_DIR/kroma" --version ))"

if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
  echo "==> Note: add '$INSTALL_DIR' to your PATH:"
  echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
fi
echo "==> Done! Open any project folder and run: kroma"
