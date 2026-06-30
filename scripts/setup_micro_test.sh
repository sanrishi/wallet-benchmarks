#!/usr/bin/env bash
set -euo pipefail

# ---- Minimal smoke-test setup for wallet-benchmarks ----
# Usage: bash scripts/setup_micro_test.sh
# Prerequisites: curl, unzip, and a 64-bit Linux environment (native or WSL).

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR"

# 1. Download pre-built binaries (Linux x86_64, Esmeralda, v5.3.1)
echo "Downloading Tari suite v5.3.1..."
curl -fL -o /tmp/tari_suite.zip \
  "https://github.com/tari-project/tari/releases/download/v5.3.1/tari_suite-5.3.1-esme-bc0e4f2-linux-x86_64.zip"

# 2. Extract into tools/
echo "Extracting to tools/..."
mkdir -p tools
unzip -o /tmp/tari_suite.zip -d tools/
chmod +x tools/*.exe 2>/dev/null || true
chmod +x tools/minotari_* 2>/dev/null || true

# 3. Update micro_config.toml with binary paths
echo "Updating micro_config.toml paths..."
ABS_TOOLS="$(cd tools && pwd)"
sed -i "s|wallet_bin = \"\"|wallet_bin = \"$ABS_TOOLS/minotari_console_wallet\"|" micro_config.toml
sed -i "s|minotari_bin = \"\"|minotari_bin = \"$ABS_TOOLS/minotari\"|" micro_config.toml
sed -i "s|payment_processor_bin = \"\"|payment_processor_bin = \"$ABS_TOOLS/minotari_payment_processor\"|" micro_config.toml
sed -i "s|console_wallet_bin = \"\"|console_wallet_bin = \"$ABS_TOOLS/minotari_console_wallet\"|" micro_config.toml

echo ""
echo "=== Setup complete ==="
echo ""
echo "Next steps:"
echo "  1. Send at least 100,000 uT (0.1 Tari) to your new_wallet address"
echo "     (run 'cargo run --release -- --config micro_config.toml --print-addresses' first)"
echo ""
echo "  2. Run the S1 smoke test:"
echo "     cargo run --release -- --config micro_config.toml --scenario S1"
echo ""
echo "  3. Share the JSON output with your mentor."
