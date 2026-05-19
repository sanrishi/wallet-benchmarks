# XTM Wallet Benchmark Harness

## Prerequisites
- Rust toolchain (stable)
- Access to Esmeralda testnet
- A funded minotari_console_wallet binary

## Configuration
Edit `config.toml` before running:
- `wallet_bin_path`  absolute path to minotari_console_wallet binary
- `old_wallet_data_dir`  directory for old wallet data (will be wiped)
- `new_wallet_data_dir`  directory for new wallet data (will be wiped)
- `payment_processor_data_dir`  directory for payment processor data
- `base_node_grpc_url`  e.g. "http://127.0.0.1:18142"
- `base_node_http_url`  e.g. "http://127.0.0.1:9000"
- `grpc_port`  wallet gRPC port, default 18143
- `fee_rate`  fee rate in uT/g, leave empty to use wallet default
- `a_fund`  funding amount in uT per mode (default 10000)

## Funding
Before running, fund each wallet mode's address with `a_fund` uT
from the Esmeralda faucet or a miner wallet.
Seed strategy: fresh seed per wallet mode.

## Running
cargo build --release
./target/release/harness
Results are written to `baseline_profile.json`.

## Output
Structured JSON report containing:
- Hardware and OS info
- Pinned software versions
- Per-scenario metrics for all three wallet modes
- Balance reconciliation deltas
- Scan timing comparisons

## Wallet Modes
- `old_wallet`  minotari_console_wallet via gRPC
- `new_wallet`  minotari-cli library with HTTP RPC
- `payment_processor`  batch 1-to-many transactions
