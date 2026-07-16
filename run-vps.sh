#!/usr/bin/env bash
set -euo pipefail

MINER="${MINER:-0xe0124ead86bdc20cc675317bef95533020a6165f}"
NODE="${NODE:-https://emberchain.org}"
THREADS="${THREADS:-$(nproc)}"
BATCH_SIZE="${BATCH_SIZE:-25000}"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found. Install Rust first:"
  echo '  curl https://sh.rustup.rs -sSf | sh'
  echo '  source "$HOME/.cargo/env"'
  exit 1
fi

cargo build --release
exec ./target/release/ember-cpu-miner \
  --node "$NODE" \
  -j "$THREADS" \
  --batch-size "$BATCH_SIZE" \
  "$MINER"
