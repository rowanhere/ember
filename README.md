# Ember CPU Miner

Standalone CPU miner for the browser mining API at `emberchain.org`.

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

## Run

```bash
./target/release/ember-cpu-miner -j "$(nproc)" 0xe0124ead86bdc20cc675317bef95533020a6165f
```

Options:

```bash
./target/release/ember-cpu-miner --node https://emberchain.org -j 16 --batch-size 25000 0xe0124ead86bdc20cc675317bef95533020a6165f
```

Dry-run without submitting found blocks:

```bash
./target/release/ember-cpu-miner --no-submit -j 2 --batch-size 1000 0xe0124ead86bdc20cc675317bef95533020a6165f
```

The miner prints scaled dashboard-style stats:

```text
[STATS] block #3965    diff 27164054  speed  449.0 KH/s avg  389.4 KH/s total    2.25 MH acc 0   stale 0   uptime 00:00:05 nonce~10691912333090672223
```

Keep it running after SSH disconnects:

```bash
tmux new -s ember
./target/release/ember-cpu-miner -j "$(nproc)" 0xe0124ead86bdc20cc675317bef95533020a6165f
```

Detach from tmux with `Ctrl+B`, then `D`. Reattach with `tmux attach -t ember`.
