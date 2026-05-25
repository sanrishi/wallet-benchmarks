# XTM Wallet Benchmark Harness

Rust harness for the Tari wallet benchmark bounty in Issue `#1`. It runs the
required scenarios `B0`, `S0`-`S7` and writes a structured
`baseline_profile.json` report.

## What The Harness Does

- `old_wallet`: spawns `minotari_console_wallet`, waits for gRPC readiness, and
  drives it over gRPC
- `new_wallet`: uses a stateless `minotari` CLI subprocess for wallet
  initialization, scan, balance, and unsigned transaction drafting, then signs
  locally and broadcasts over base-node HTTP JSON-RPC
- `payment_processor`: same stateless `minotari` subprocess model as
  `new_wallet`, with multi-recipient batch send support for S5

No harness-level retry, backoff, or artificial serialization is added to hide
wallet contention or failure behavior.

## Prerequisites

- Rust stable toolchain
- Access to Esmeralda testnet
- A `minotari_console_wallet` binary that matches the version recorded in
  `config.toml`
- A `minotari` CLI binary that matches the version recorded in `config.toml`
- Reachable Esmeralda base node gRPC and HTTP endpoints
- L1 Esmeralda testnet funds for the three benchmark wallets

## Configuration

Edit `config.toml` before running:

- `wallet_bin_path`: absolute or repo-relative path to `minotari_console_wallet`
- `minotari_bin_path`: absolute or repo-relative path to the `minotari` CLI
- `old_wallet_data_dir`: working directory for Mode 1; it will be wiped by scan
  scenarios
- `new_wallet_data_dir`: working directory for Mode 2; it will be wiped by scan
  scenarios
- `payment_processor_data_dir`: working directory for Mode 3; it will be wiped
  by scan scenarios
- `base_node_grpc_url`: Esmeralda base node gRPC endpoint
- `base_node_http_url`: Esmeralda base node HTTP endpoint
- `console_wallet_version`: exact tag or commit used for `minotari_console_wallet`
- `minotari_cli_version`: exact tag or commit used for `minotari`
- `base_node_version`: exact tag or commit used for the base node
- `grpc_port`: local gRPC port for the spawned console wallet
- `old_wallet_password`: password passed to `minotari_console_wallet`
- `new_wallet_password`: password used for Mode 2 wallet storage
- `payment_processor_password`: password used for Mode 3 wallet storage
- `tx_amount_ut`: per-transaction send amount used by `S1`, `S4`, and `S5`
- `fee_rate`: optional explicit fee rate in `uT/g`; leave empty to use current
  driver defaults
- `a_fund`: funding amount per mode
- `c_min`, `volume_target`, `doubling_rounds`, `fanout_outputs_per_tx`,
  `concurrent_batches`, `s4_t_budget_secs`, `s5_m`, `s5_k`: benchmark control
  parameters recorded into the output profile

Use empty, throwaway data directories. The harness resets wallet state as part
of the benchmark protocol.

## Step-By-Step Runbook

### 1. Build The Harness

```bash
cargo build --release
```

### 2. Print The Three Funding Addresses

Use the built harness to initialize wallet state and print the deterministic
address for each benchmark mode:

```bash
./target/release/harness --print-addresses
./target/release/harness --config /path/to/config.toml --print-addresses
```

Expected stdout format:

```text
old_wallet: <base58 Tari address>
new_wallet: <base58 Tari address>
payment_processor: <base58 Tari address>
```

This step is safe to repeat. Mode 2 and Mode 3 persist their seed words in
`seed_words.txt` inside the configured data directories so the same addresses
are reused until those directories are wiped.

### 3. Fund The Printed Addresses

Fund each of the three printed addresses with at least `a_fund` on Esmeralda.
Use the same recipient set that you printed in Step 2.

Current funding expectation:

- `old_wallet`: one UTXO of `a_fund` (current canonical config: `300T`)
- `new_wallet`: one UTXO of `a_fund` (current canonical config: `300T`)
- `payment_processor`: one UTXO of `a_fund` (current canonical config: `300T`)

Current canonical run tuning:

- `tx_amount_ut = 200`
- `a_fund = 300000000` (`300T`)

The funding step is external to the measurement. Wait until the funding
transactions are visible on the network before starting the timed benchmark run.

### 4. Run The Benchmark

```bash
./target/release/harness
./target/release/harness --config /path/to/config.toml
```

The harness will:

1. run `B0` for `old_wallet`
2. run `S0`-`S7` for `old_wallet`
3. run `B0`-`S7` for `new_wallet`
4. run `B0`-`S7` for `payment_processor`
5. write `baseline_profile.json` in the current working directory

If a mode fails, the harness still writes whatever partial results it has for
that mode instead of exiting without a report.

## Output

`baseline_profile.json` contains:

- CPU, RAM, and OS details from the local machine
- configured software version pins for wallet binaries and base node
- full config snapshot
- per-mode scenario results
- per-scenario scan metrics and transaction metrics
- balance reconciliation deltas
- computed scan deltas:
  - `scan_delta_s2_minus_b0`
  - `scan_delta_s6_minus_s2`
- computed `s5_throughput_multiplier` when enough S5 transaction data exists

## Known Constraints

- A final proof-quality baseline requires funded Esmeralda wallets. Without L1
  testnet funds, `S0` and the downstream funded scenarios cannot produce a real
  final benchmark profile.
- Mode 2 and Mode 3 currently use a stateless `minotari` CLI subprocess for
  wallet operations plus local offline signing and HTTP submission. They do not
  keep a background wallet daemon alive.
- `disk_type` is currently reported as `"unknown"` unless the harness is
  extended with host-specific disk inspection.

## Wallet Modes

- `old_wallet`: `minotari_console_wallet` via gRPC
- `new_wallet`: stateless `minotari` subprocess, local offline signing, HTTP
  broadcast
- `payment_processor`: stateless `minotari` subprocess with batch 1-to-many
  transaction support
