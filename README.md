# Ember CPU Miner

Standalone CPU/CUDA miner for the browser mining API at `emberchain.org`.

It mirrors the live Web Worker:

- fetches `GET /api/mining/template?minerAddress=...`
- hashes `keccak256(JSON.stringify(headerWithNonce))`
- accepts hashes whose 256-bit value is `<= target`
- submits to `POST /api/mining/submit`

## Build on a VPS

```bash
sudo apt update
sudo apt install -y build-essential pkg-config
curl https://sh.rustup.rs -sSf | sh
source "$HOME/.cargo/env"

cd ember-cpu-miner
cargo build --release
```

CUDA build for an RTX 4090:

```bash
CUDA_ARCH=sm_89 cargo build --release --features cuda
```

## Run

```bash
./target/release/ember-cpu-miner -j "$(nproc)" 0xe0124ead86bdc20cc675317bef95533020a6165f
```

Run on an RTX 4090:

```bash
./run-vps.sh --cuda
```

`run-vps.sh` installs Rust/Cargo if missing, pulls latest code before building, auto-uses every GPU reported by `nvidia-smi`, and auto-detects `CUDA_ARCH` from GPU compute capability.

Manual CUDA run:

```bash
./target/release/ember-cpu-miner --cuda --cuda-device 0 0xe0124ead86bdc20cc675317bef95533020a6165f
```

Multiple GPUs:

```bash
./run-vps.sh --cuda
```

Override GPU selection:

```bash
CUDA_DEVICES=0,1 ./run-vps.sh --cuda
```

Manual multi-GPU run:

```bash
./target/release/ember-cpu-miner --cuda --cuda-devices 0,1 0xe0124ead86bdc20cc675317bef95533020a6165f
```

Options:

```bash
./target/release/ember-cpu-miner --node https://emberchain.org -j 16 --batch-size 25000 0xe0124ead86bdc20cc675317bef95533020a6165f
```

Dry-run without submitting found blocks:

```bash
./target/release/ember-cpu-miner --no-submit -j 2 --batch-size 1000 0xe0124ead86bdc20cc675317bef95533020a6165f
```

The miner prints an in-place dashboard:

```text
EMBER CPU MINER - DASHBOARD
======================================================================
Block          Difficulty         HashRate           Average
4002           32416993            450.6 KH/s          2.94 KH/s
----------------------------------------------------------------------
Checked          Uptime       Nonce                  Block Found
  2.00 KH        00:00:00     6726689996527822293    0
```

Keep it running after SSH disconnects:

```bash
tmux new -s ember
./target/release/ember-cpu-miner -j "$(nproc)" 0xe0124ead86bdc20cc675317bef95533020a6165f
```

Detach from tmux with `Ctrl+B`, then `D`. Reattach with `tmux attach -t ember`.
