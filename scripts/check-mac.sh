#!/usr/bin/env bash
# ReelVault macOS baseline check. Run from the repo root: scripts/check-mac.sh
set -euo pipefail
cd "$(dirname "$0")/.."

if [ -f "$HOME/.cargo/env" ]; then
  . "$HOME/.cargo/env"
fi

echo "[1/5] cargo fmt --check"
cargo fmt --check

echo "[2/5] cargo check"
cargo check

echo "[3/5] cargo test"
cargo test

echo "[4/5] python unit tests"
python3 -m unittest discover -s tests -v

echo "[5/5] web js syntax"
node --check web/app.js

echo "check-mac: all steps green"
