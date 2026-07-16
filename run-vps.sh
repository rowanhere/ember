#!/usr/bin/env bash
set -euo pipefail

MINER="${MINER:-0xe0124ead86bdc20cc675317bef95533020a6165f}"
NODE="${NODE:-https://emberchain.org}"
THREADS="${THREADS:-$(nproc)}"
BATCH_SIZE="${BATCH_SIZE:-25000}"
CUDA="${CUDA:-0}"
CUDA_DEVICE="${CUDA_DEVICE:-0}"
CUDA_BATCH_SIZE="${CUDA_BATCH_SIZE:-67108864}"
CUDA_ARCH="${CUDA_ARCH:-sm_89}"
export CUDA_ARCH

if [ "${1:-}" = "--cuda" ]; then
  CUDA=1
  shift
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found. Install Rust first:"
  echo '  curl https://sh.rustup.rs -sSf | sh'
  echo '  source "$HOME/.cargo/env"'
  exit 1
fi

if [ "$CUDA" = "1" ]; then
  if ! command -v nvcc >/dev/null 2>&1; then
    echo "nvcc not found. Install the CUDA toolkit first." >&2
    exit 1
  fi

  cargo build --release --features cuda
  exec ./target/release/ember-cpu-miner \
    --cuda \
    --cuda-device "$CUDA_DEVICE" \
    --cuda-batch-size "$CUDA_BATCH_SIZE" \
    --node "$NODE" \
    "$MINER"
else
  cargo build --release
  exec ./target/release/ember-cpu-miner \
    --node "$NODE" \
    -j "$THREADS" \
    --batch-size "$BATCH_SIZE" \
    "$MINER"
fi
