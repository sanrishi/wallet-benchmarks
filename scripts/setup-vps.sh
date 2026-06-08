#!/usr/bin/env bash
# ============================================================
# Tari Wallet Benchmark Harness — Cloud VPS Setup Script
# ============================================================
# Run this on a fresh Ubuntu 22.04+ VPS (min 4GB RAM, 20GB disk).
# It installs deps, builds all required Tari binaries from source,
# clones the harness, and starts the base node syncing Esmeralda.
#
# Usage:
#   chmod +x setup-vps.sh && ./setup-vps.sh
#
# After the script completes, wait for the base node to sync,
# then start the benchmark (instructions printed at the end).
# ============================================================
set -euo pipefail

# --- Repos and paths ---
TARI_REPO="$HOME/tari"
CLI_REPO="$HOME/minotari-cli"
PP_REPO="$HOME/minotari_payment_processor"
HARNESS_REPO="$HOME/wallet-benchmarks"
BENCHMARK_LOG="$HOME/benchmark.log"
NODE_LOG="$HOME/tari-node.log"
TMUX_NODE="tari-node"
TMUX_BENCH="tari-bench"
CONFIG_SRC="https://raw.githubusercontent.com/sanrishi/wallet-benchmarks/feat/harness-implementation/config.toml"

echo "============================================"
echo " Step 1: System dependencies"
echo "============================================"
sudo apt-get update -qq
sudo apt-get install -y -qq \
    build-essential \
    pkg-config \
    libssl-dev \
    git \
    cmake \
    tmux \
    clang \
    curl \
    wget \
    unzip \
    jq

echo "============================================"
echo " Step 2: Install Rust"
echo "============================================"
if ! command -v cargo &>/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
set +u
source "$HOME/.cargo/env"
set -u
rustup default stable

echo "============================================"
echo " Step 3: Build minotari_node + minotari_console_wallet"
echo "         (from tari-project/tari — may take 1-2 hours)"
echo "============================================"
if [ ! -d "$TARI_REPO" ]; then
    git clone https://github.com/tari-project/tari.git "$TARI_REPO"
fi
cd "$TARI_REPO"
cargo build --release --bin minotari_node --bin minotari_console_wallet
echo "  ✓ minotari_node     at: $TARI_REPO/target/release/minotari_node"
echo "  ✓ minotari_console_wallet at: $TARI_REPO/target/release/minotari_console_wallet"

echo "============================================"
echo " Step 4: Build minotari CLI"
echo "         (from tari-project/minotari-cli)"
echo "============================================"
if [ ! -d "$CLI_REPO" ]; then
    git clone https://github.com/tari-project/minotari-cli.git "$CLI_REPO"
fi
cd "$CLI_REPO"
cargo build --release --bin minotari
echo "  ✓ minotari (CLI)    at: $CLI_REPO/target/release/minotari"

echo "============================================"
echo " Step 5: Build minotari_payment_processor"
echo "============================================"
if [ ! -d "$PP_REPO" ]; then
    git clone https://github.com/tari-project/minotari_payment_processor.git "$PP_REPO"
fi
cd "$PP_REPO"
cargo build --release --bin minotari_payment_processor
echo "  ✓ minotari_payment_processor at: $PP_REPO/target/release/minotari_payment_processor"

echo "============================================"
echo " Step 6: Clone benchmark harness"
echo "============================================"
if [ ! -d "$HARNESS_REPO" ]; then
    git clone https://github.com/sanrishi/wallet-benchmarks.git "$HARNESS_REPO"
fi
cd "$HARNESS_REPO"
git checkout feat/harness-implementation

# Write VPS-optimized config.toml
cat > config.toml << CONFIG_EOF
[benchmark]
a_fund = 300000000
c_min = 3
volume_target = 512
doubling_rounds = 6
fanout_outputs_per_tx = 8
concurrent_batches = [8, 16, 32, 64, 128]
s4_t_budget_secs = 900
s5_m = 100
s5_k = 10
tx_amount_ut = 200
fee_rate = "1"
scan_interval_secs = 1

[paths]
wallet_bin = "./bin/minotari_console_wallet"
minotari_bin = "./bin/minotari"
payment_processor_bin = "./bin/minotari_payment_processor"
console_wallet_bin = "./bin/minotari_console_wallet"

[network]
base_node_grpc_url = "http://127.0.0.1:18142"
base_node_http_url = "http://127.0.0.1:18142"
grpc_port = 18143

[passwords]
old_wallet = "benchmark123"
new_wallet = "benchmark_mode2_password_32_chars"
payment_processor = "benchmark_mode3_password_32_chars"
library_wallet = "benchmark_lib_password_32_chars"

[data]
old_wallet = "./wallet-data"
new_wallet = "./wallet-data-new"
payment_processor = "./wallet-data-pp"

[versions]
console_wallet = "$(cd $TARI_REPO && git log --oneline -1 | head -c 40 || echo 'unknown')"
minotari_cli = "$(cd $CLI_REPO && git log --oneline -1 | head -c 40 || echo 'unknown')"
base_node = "$(cd $TARI_REPO && git log --oneline -1 | head -c 40 || echo 'unknown')"
CONFIG_EOF

# Create symlinks to built binaries so config paths stay portable
mkdir -p bin
ln -sf "$TARI_REPO/target/release/minotari_node" bin/
ln -sf "$TARI_REPO/target/release/minotari_console_wallet" bin/
ln -sf "$CLI_REPO/target/release/minotari" bin/
ln -sf "$PP_REPO/target/release/minotari_payment_processor" bin/
echo "  ✓ Binary symlinks created in ./bin/"

echo "============================================"
echo " Step 7: Start base node sync on Esmeralda"
echo "============================================"
cd "$TARI_REPO"
tmux kill-session -t "$TMUX_NODE" 2>/dev/null || true
tmux new-session -d -s "$TMUX_NODE" \
    "./target/release/minotari_node --network esmeralda \
        --non-interactive-mode --disable-splash-screen \
    2>&1 | tee '$NODE_LOG'"
echo "  ✓ Base node syncing in tmux session: $TMUX_NODE"
echo "  ✓ Logs: tail -f $NODE_LOG"

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║                    SETUP COMPLETE                           ║"
echo "╠══════════════════════════════════════════════════════════════╣"
echo "║                                                              ║"
echo "║  WHAT TO DO NEXT:                                            ║"
echo "║                                                              ║"
echo "║  1. WAIT FOR BASE NODE TO SYNC                               ║"
echo "║     Check progress:                                          ║"
echo "║       tail -f $NODE_LOG                                      ║"
echo "║     The node is synced when tip height matches               ║"
echo "║     https://textexplore-esmeralda.tari.com/                  ║"
echo "║     (typically 672k+ blocks, may take 6-24 hours)            ║"
echo "║                                                              ║"
echo "║  2. SYNC COMPLETE? START THE BENCHMARK                       ║"
echo "║     tmux new-session -d -s $TMUX_BENCH 'bash -c \"           ║
echo "║       cd $HARNESS_REPO &&                                    ║
echo "║       ~/.cargo/bin/cargo run --release 2>&1                  ║
echo "║         | tee $BENCHMARK_LOG                                 ║
echo "║     \"'                                                       ║"
echo "║                                                              ║"
echo "║  3. MONITOR PROGRESS                                         ║"
echo "║     tmux attach -t $TMUX_BENCH              (view live)      ║"
echo "║     tail -f $BENCHMARK_LOG                   (check logs)    ║"
echo "║                                                              ║"
echo "║  4. WHEN BENCHMARK FINISHES                                  ║"
echo "║     The baseline_profile.json and full report                ║"
echo "║     will be in $HARNESS_REPO                                  ║"
echo "║     Copy the JSON files back to your fork and commit them.   ║"
echo "║                                                              ║"
echo "║  KNOWN ISSUES:                                               ║"
echo "║  - Build will fail on <4GB RAM VPS due to OOM during         ║"
echo "║    LLVM/linking. Add swap if needed:                         ║"
echo "║    sudo fallocate -l 4G /swapfile && sudo mkswap /swapfile && sudo swapon /swapfile    ║"
echo "║  - Base node takes 6-24 hours to sync Esmeralda from scratch ║"
echo "║  - The full benchmark matrix takes ~70 hours                  ║"
echo "║                                                              ║"
echo "╚══════════════════════════════════════════════════════════════╝"
