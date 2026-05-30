# XTM Wallet Benchmark Harness

Rust harness for the Tari wallet benchmark bounty (Issue `#1`). It runs the
required scenarios `B0`, `S0`–`S7` against three wallet modes and writes a
structured `baseline_profile.json` report.

## Prerequisites

### Hardware And OS

- Any system where Rust stable toolchain and the `minotari` stack can run
- At least 4 GB of RAM and 5 GB of free disk space
- Network access to an Esmeralda testnet base node

### Software

| Dependency | Minimum | How To Get |
|---|---|---|
| Rust toolchain | stable 1.80+ | `rustup install stable` |
| `minotari_console_wallet` | tag matching `config.toml` | Build from [tari-project/tari](https://github.com/tari-project/tari) |
| `minotari` CLI | tag matching `config.toml` | Same build from tari-project/tari |
| Esmeralda base node | tag matching `config.toml` | Same build OR use a public RPC endpoint |

### Building The `minotari` Stack (From tari-project/tari)

```bash
git clone https://github.com/tari-project/tari.git
cd tari
git checkout <tag-matching-config.toml>
cargo build --release -p minotari_console_wallet -p minotari --bin minotari
```

The binaries are at `target/release/minotari_console_wallet` and
`target/release/minotari`.  Copy or symlink them to a convenient location and
set `wallet_bin_path` / `minotari_bin_path` in `config.toml`.

If you already have access to a synced Esmeralda base node (public or local),
its gRPC port (default `18142`) is the `base_node_grpc_url`. The HTTP JSON-RPC
endpoint is normally the same host and port.

### Testnet Funds

Each of the three wallet modes needs **one UTXO of at least `a_fund`**
(current canonical: `300 T` = `300000000 µT`).  Obtain testnet Tari from:

- [Tari Esmeralda Faucet](https://faucet.esmeralda.tari.com/) (if available)
- Or ask in the Tari community Discord `#testnet-faucet` channel
- Or send from an existing wallet that has Esmeralda balances

## Quick Start

### 1. Clone And Build The Harness

```bash
git clone <your-fork-url> wallet-benchmarks
cd wallet-benchmarks
cargo build --release
```

The release binary is `target/release/harness.exe` (Windows) or
`target/release/harness` (Linux/macOS).

### 2. Edit `config.toml`

A sample `config.toml` ships with the repo. **Every setting must be correct
before running.**

#### Required Paths

| Setting | Example | Notes |
|---|---|---|
| `wallet_bin_path` | `"C:/tools/minotari_console_wallet.exe"` | Absolute or workspace-relative path to the old wallet binary |
| `minotari_bin_path` | `"C:/tools/minotari.exe"` | Absolute or workspace-relative path to the `minotari` CLI binary |
| `old_wallet_data_dir` | `"./wallet-data"` | Working directory for mode 1; **wiped** by scan scenarios |
| `new_wallet_data_dir` | `"./wallet-data-new"` | Working directory for mode 2; **wiped** by scan scenarios |
| `payment_processor_data_dir` | `"./wallet-data-pp"` | Working directory for mode 3; **wiped** by scan scenarios |

Use empty, throwaway directories. The harness resets wallet state as part of the
benchmark protocol.

#### Network Endpoints

| Setting | Example | Notes |
|---|---|---|
| `base_node_grpc_url` | `"http://rpc.esmeralda.tari.com:18142"` | Public or local base node gRPC endpoint |
| `base_node_http_url` | `"http://rpc.esmeralda.tari.com:18142"` | Public or local base node HTTP endpoint |
| `grpc_port` | `18143` | Local port for the spawned `minotari_console_wallet` gRPC server |

#### Version Pins

| Setting | Expected Value |
|---|---|
| `console_wallet_version` | The exact git tag or commit used to build `minotari_console_wallet` |
| `minotari_cli_version` | The exact git tag or commit used to build the `minotari` CLI |
| `base_node_version` | The exact git tag or commit of the base node you are connecting to |

These are recorded verbatim in the report so reviewers know what was tested.

#### Benchmark Parameters

| Setting | Default | Purpose |
|---|---|---|
| `a_fund` | `300000000` (`300 T`) | Funding amount each mode must receive before `S0` |
| `tx_amount_ut` | `200` | Per-transaction send amount in µT |
| `fee_rate` | `"1"` | Fee rate in µT/gram; must be a non-empty numeric string |
| `c_min` | `3` | Minimum confirmations to consider a transaction spendable |
| `volume_target` | `512` | Target UTXO count for scan rediscovery checks |
| `doubling_rounds` | `6` | Number of doubling rounds in `S1` |
| `fanout_outputs_per_tx` | `8` | Recipients per fan-out batch transaction in `S1` |
| `concurrent_batches` | `[8, 16, 32, 64, 128]` | Batch sizes for concurrent construction in `S4` |
| `s4_t_budget_secs` | `900` | Per-transaction timeout for `S4` concurrent sends |
| `s5_m` | `100` | Total transactions in `S5` (and per-arm count) |
| `s5_k` | `10` | Recipients per batch transaction in `S5` batch arm |

#### Passwords

| Setting | Notes |
|---|---|
| `old_wallet_password` | Passed to `minotari_console_wallet --password` |
| `new_wallet_password` | Used for wallet DB encryption (mode 2) |
| `payment_processor_password` | Used for wallet DB encryption (mode 3) |

Mode 2 and mode 3 passwords must be at least 16 characters (the Tari CLI wallet
enforces this).

### 3. Print The Three Funding Addresses

```bash
target/release/harness --config config.toml --print-addresses
```

Output (example):

```
old_wallet: 6b731a8e4a09e4c2f5e1c5c5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5b5
new_wallet: 7c842b9f5b1a2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c
payment_processor: 9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b9c8d7e6f5a4b3c2d1e0f
```

These addresses are **deterministic** from the seed words stored in each mode's
data directory. The same addresses will print every time until you wipe the data
directories.

### 4. Fund The Printed Addresses

Send exactly **one UTXO of `a_fund`** to each of the three addresses printed
above.  You can send the full amount in a single transaction per address.

After sending, wait a few minutes for the transactions to be confirmed on
Esmeralda.  You can verify with a block explorer or by querying the base node.

### 5. Run The Benchmark

```bash
target/release/harness --config config.toml
```

The harness runs **three modes sequentially** (this can take 1–4 hours):

1. **old_wallet** — `B0` → `S0` → `S1` → `S2` → `S3` → `S4` → `S5` → `S6` → `S7`
2. **new_wallet** — Same 9 scenarios
3. **payment_processor** — Same 9 scenarios

On success, `baseline_profile.json` is written to the current directory.

If a mode fails mid-run, the harness records whatever results it has for that
mode and continues to the next mode. It never exits without writing a report
unless the configuration itself is invalid.

## Scenario Reference

| Scenario | What It Measures | Balance Checkpoint |
|---|---|---|
| `B0` | Baseline scan time from genesis on an empty wallet | — |
| `S0` | Time to detect and confirm an incoming funding UTXO | `h_birth` (birthday height) |
| `S1` | UTXO build-up via doubling rounds + fan-out batches | Post-`S1` balance |
| `S2` | Full genesis re-scan after wipe; verifies recovered balance = post-`S1` | Post-`S1` balance |
| `S3` | Birthday re-scan after wipe; same checkpoint as `S2` | Post-`S1` balance |
| `S4` | Concurrent transaction construction at increasing batch sizes | — |
| `S5` | Throughput comparison: batch (1-to-many) vs individual sends | Post-`S5` balance |
| `S6` | Full genesis re-scan after wipe; verifies recovered balance = post-`S5` | Post-`S5` balance |
| `S7` | Birthday re-scan after wipe; same checkpoint as `S6` | Post-`S5` balance |

Scan scenarios (`S2`, `S3`, `S6`, `S7`) wipe the wallet data directory before
scanning.  The harness records whether the recovered balance matches the pre-wipe
checkpoint.  If it does not, the scenario is reported as failed.

## Output: `baseline_profile.json`

The report is a JSON array, one element per mode:

```json
[
  {
    "cpu_model": "AMD Ryzen 9 7950X",
    "ram_kb": 33554432,
    "os": "Windows 11 Pro",
    "disk_type": "unknown",
    "network_path": "remote:http://rpc.esmeralda.tari.com:18142",
    "console_wallet_version": "v1.8.0-pre.2",
    "minotari_cli_version": "v1.8.0-pre.2",
    "base_node_version": "v1.8.0-pre.2",
    "scan_delta_s2_minus_b0": 12.34,
    "scan_delta_s6_minus_s2": 5.67,
    "s5_throughput_multiplier": 2.45,
    "wallet_mode": "old_wallet",
    "config_snapshot": { ... },
    "scenarios": [
      {
        "scenario_name": "B0",
        "wall_clock_secs": 45.2,
        "total_fees": 0,
        "success_count": 1,
        "failure_count": 0,
        "balance_delta": 0,
        "tx_metrics": [],
        "scan_metrics": {
          "wall_clock_secs": 45.2,
          "blocks_per_sec": 125.5,
          "h_tip_start": 12345,
          "h_tip_end": 12350,
          "outputs_found": 512,
          "peak_rss_kb": 89000,
          "peak_cpu_percent": 12.3
        },
        "recorded_birth_height": null,
        "error": null
      }
    ]
  }
]
```

### Key Fields

| Field | Meaning |
|---|---|
| `scenarios[].wall_clock_secs` | Total elapsed wall time for the scenario |
| `scenarios[].success_count` / `failure_count` | Transaction-level pass/fail |
| `scenarios[].balance_delta` | `expected_balance - observed_balance` (0 = perfect reconciliation) |
| `scenarios[].tx_metrics[]` | Per-transaction timing (construction, mempool, confirmation) |
| `scenarios[].scan_metrics` | Scan resource usage including `peak_rss_kb` and `peak_cpu_percent` |
| `scenarios[].error` | Scenario-level error string if the scenario failed to complete |
| `scan_delta_s2_minus_b0` | How much slower (or faster) a genesis scan is after UTXO build-up vs baseline |
| `s5_throughput_multiplier` | Ratio of individual-send throughput to batch-send throughput in S5 |

## Architecture

### Three Wallet Modes

| Mode | Driver | Process Model | Transaction Path |
|---|---|---|---|
| `old_wallet` | `OldWalletDriver` | Long-lived `minotari_console_wallet` subprocess driven via gRPC | gRPC `Transfer`, gRPC confirmation polling |
| `new_wallet` | `NewWalletDriver` | Stateless `minotari` CLI per operation + daemon for status queries | CLI → offline sign → HTTP JSON-RPC submit → SQLite polling |
| `payment_processor` | `PaymentProcessorDriver` | Stateless `minotari` CLI (same as new_wallet) | Same, with multi-recipient batch support |

### Design Principles

- **No harness-level retry, backoff, or throttling:** wallet pain points (lock
  contention, rejects, stalls, timeouts) are surfaced as results, not hidden.
- **Real confirmation timing:** every transaction records how long it took to
  reach spendable depth—no hardcoded zeros.
- **Scan checkpoint validation:** post-wipe scans verify their recovered balance
  against a pre-wipe checkpoint, not against themselves.
- **Per-scenario fault isolation:** if a single scenario fails (subprocess crash,
  gRPC timeout, etc.), the harness catches the error, records a `scenario_error`,
  and continues to the next scenario. The mode still completes its remaining
  scenarios.

## Troubleshooting

### "address" or "bal" subcommand not found

The `minotari` CLI binary version is too old or not built with the required
subcommands. Rebuild `minotari` from the tag specified in `config.toml`.

### Wallet gRPC did not become ready within 120s

The `minotari_console_wallet` binary failed to start or connect to gRPC. Check:
- The binary exists at `wallet_bin_path`
- The `grpc_port` is not already in use
- The base node gRPC endpoint is reachable
- Run the binary manually to see its stderr output

### The benchmark runs but all scenarios fail with connection errors

- The Esmeralda base node endpoint may be down or unreachable
- Firewall rules may block outbound connections on the gRPC/HTTP port
- The base node version does not match the wallet binary version

### "password must be at least 16 characters"

Mode 2 and mode 3 passwords must be **16 characters or longer**. Update
`new_wallet_password` and `payment_processor_password` in `config.toml`.

### `baseline_profile.json` has `peak_rss_kb: 0` or `peak_cpu_percent: 0.0`

The `sysinfo` crate could not read process resource counters, which happens when:
- The process exited before the first poll (very short scans)
- The operating system does not expose memory/CPU per process (uncommon)
- The PID lookup failed (process table was refreshed before the process appeared)

This is a known limitation of the polling-based approach. Longer scans (e.g.
`B0` on a large chain) reliably produce non-zero values.

## Known Constraints

- A final proof-quality baseline requires funded Esmeralda wallets. Without L1
  testnet funds, `S0` and downstream funded scenarios cannot produce a real
  benchmark profile.
- `disk_type` is reported as `"unknown"` unless the harness is extended with
  host-specific disk inspection.
- Mode 2 and Mode 3 use `minotari` CLI subprocesses, not the wallet daemon.
  This is deliberate: it measures the user-facing CLI path, not a custom API.
