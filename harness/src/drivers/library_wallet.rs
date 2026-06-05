#![cfg(feature = "library_wallet")]

//! Library-based wallet driver using the `minotari_wallet` crate.
//!
//! When complete, this will replace the CLI subprocess calls in `new_wallet.rs`
//! with in-process library calls via `minotari_wallet::Wallet::start()`.
//!
//! ## Migration TODO
//!
//! - `minotari create`  → `read_or_create_master_seed()` + `Wallet::start()`
//! - `minotari balance` → `Wallet.output_manager_service` handle
//! - `minotari re-scan` → `utxo_scanner_service` handle
//! - `minotari daemon`  → `WalletConfig.grpc_enabled = true`
//! - `create-unsigned-transaction` → `transaction_service` handle
//!
//! ## Build prerequisites
//!
//! This module requires special build environment setup:
//!
//! ```powershell
//! .\setup-library-wallet.ps1   # run once per fresh clone
//! $env:RUSTFLAGS = "-C link-arg=/FORCE:MULTIPLE"
//! $env:PROTOC = "path\to\protoc.exe"   # or use protoc-bin-vendored
//! cargo check --features library_wallet
//! ```
//!
//! ## Risk
//!
//! `Wallet::start()` requires 13 complex parameters including backends, crypto
//! factories, consensus manager, etc.  Fill one field at a time and keep the
//! CLI fallback (`new_wallet.rs`) until the library path is fully verified.

use async_trait::async_trait;

use crate::driver::WalletDriver;
use crate::metrics::{ScanMetrics, TxMetrics};

pub struct LibraryWalletDriver;

impl LibraryWalletDriver {
    #[allow(unused_variables)]
    pub fn new(
        minotari_bin: std::path::PathBuf,
        data_dir: std::path::PathBuf,
        base_node_url: String,
        confirmation_window: u64,
        password: String,
    ) -> anyhow::Result<Self> {
        todo!("LibraryWalletDriver::new not yet implemented")
    }
}

#[async_trait]
impl WalletDriver for LibraryWalletDriver {
    fn mode_name(&self) -> &str {
        "library_wallet"
    }

    async fn reset(&self) -> anyhow::Result<()> {
        todo!("LibraryWalletDriver::reset not yet implemented")
    }

    async fn get_balance(&self) -> anyhow::Result<u64> {
        todo!("LibraryWalletDriver::get_balance not yet implemented")
    }

    async fn get_tip_height(&self) -> anyhow::Result<u64> {
        todo!("LibraryWalletDriver::get_tip_height not yet implemented")
    }

    async fn get_self_address(&self) -> anyhow::Result<String> {
        todo!("LibraryWalletDriver::get_self_address not yet implemented")
    }

    async fn scan_from_genesis(&self) -> anyhow::Result<ScanMetrics> {
        todo!("LibraryWalletDriver::scan_from_genesis not yet implemented")
    }

    async fn scan_from_birthday(&self, _height: u64) -> anyhow::Result<ScanMetrics> {
        todo!("LibraryWalletDriver::scan_from_birthday not yet implemented")
    }

    async fn send_single(
        &self,
        _to_address: &str,
        _amount_ut: u64,
        _fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        todo!("LibraryWalletDriver::send_single not yet implemented")
    }
}
