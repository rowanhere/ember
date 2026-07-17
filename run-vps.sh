#!/usr/bin/env bash
set -euo pipefail

MINER="${MINER:-0xe0124ead86bdc20cc675317bef95533020a6165f}"
NODE="${NODE:-https://emberchain.org}"
THREADS="${THREADS:-$(nproc)}"
BATCH_SIZE="${BATCH_SIZE:-25000}"
CUDA="${CUDA:-0}"
CUDA_DEVICE="${CUDA_DEVICE:-0}"
CUDA_DEVICES="${CUDA_DEVICES:-}"
CUDA_BATCH_SIZE="${CUDA_BATCH_SIZE:-67108864}"
CUDA_ARCH="${CUDA_ARCH:-}"
AUTO_PULL="${AUTO_PULL:-1}"

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

if [ "$AUTO_PULL" = "1" ] && [ -d .git ] && command -v git >/dev/null 2>&1; then
  echo "[RUN] Pulling latest code..."
  git pull --ff-only
fi

if [ "$CUDA" = "1" ]; then
  if ! command -v nvcc >/dev/null 2>&1; then
    echo "nvcc not found. Install the CUDA toolkit first." >&2
    exit 1
  fi

  if [ -z "$CUDA_DEVICES" ]; then
    if command -v nvidia-smi >/dev/null 2>&1; then
      CUDA_DEVICES="$(nvidia-smi --query-gpu=index --format=csv,noheader | tr -d ' ' | paste -sd, -)"
    fi
    CUDA_DEVICES="${CUDA_DEVICES:-$CUDA_DEVICE}"
  fi

  if [ -z "$CUDA_ARCH" ]; then
    if command -v nvidia-smi >/dev/null 2>&1; then
      CUDA_COMPUTE_CAP="$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader 2>/dev/null | head -n 1 | tr -d ' .')"
      if [ -n "$CUDA_COMPUTE_CAP" ]; then
        CUDA_ARCH="sm_${CUDA_COMPUTE_CAP}"
      fi
    fi
    CUDA_ARCH="${CUDA_ARCH:-sm_89}"
  fi
  export CUDA_ARCH

  echo "[RUN] CUDA devices: $CUDA_DEVICES"
  echo "[RUN] CUDA arch: $CUDA_ARCH"
  cargo build --release --features cuda
  exec ./target/release/ember-cpu-miner \
    --cuda \
    --cuda-devices "$CUDA_DEVICES" \
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
