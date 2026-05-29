use std::path::PathBuf;

use async_trait::async_trait;
use anyhow::Context;

use crate::driver::WalletDriver;
use crate::drivers::new_wallet::NewWalletDriver;
use crate::metrics::{ScanMetrics, TxMetrics};

/// Payment-processor wallet mode.
///
/// This driver wraps [`NewWalletDriver`] and reuses its full implementation
/// (stateless `minotari` CLI subprocess, local offline signing, HTTP
/// JSON-RPC broadcast).  The only difference from `new_wallet` is
///
/// - `mode_name()` returns `"payment_processor"`
/// - `send_batch()` is used for multi-recipient batches in scenario `S5`
///
/// All other `WalletDriver` methods delegate directly to the inner
/// `NewWalletDriver`.
pub struct PaymentProcessorDriver {
    inner: NewWalletDriver,
}

impl PaymentProcessorDriver {
    pub fn new(
        minotari_bin: PathBuf,
        data_dir: PathBuf,
        base_node_url: String,
        confirmation_window: u64,
        password: String,
    ) -> anyhow::Result<Self> {
        let inner = NewWalletDriver::new(
            minotari_bin,
            data_dir,
            base_node_url,
            confirmation_window,
            password,
        )
        .context("failed to create new_wallet driver for payment_processor")?;
        Ok(Self { inner })
    }
}

#[async_trait]
impl WalletDriver for PaymentProcessorDriver {
    fn mode_name(&self) -> &str {
        "payment_processor"
    }

    async fn reset(&self) -> anyhow::Result<()> {
        self.inner.reset().await
    }

    async fn get_balance(&self) -> anyhow::Result<u64> {
        self.inner.get_balance().await
    }

    async fn get_tip_height(&self) -> anyhow::Result<u64> {
        self.inner.get_tip_height().await
    }

    async fn get_self_address(&self) -> anyhow::Result<String> {
        self.inner.get_self_address().await
    }

    async fn scan_from_genesis(&self) -> anyhow::Result<ScanMetrics> {
        self.inner.scan_from_genesis().await
    }

    async fn scan_from_birthday(&self, height: u64) -> anyhow::Result<ScanMetrics> {
        self.inner.scan_from_birthday(height).await
    }

    async fn send_single(
        &self,
        to_address: &str,
        amount_ut: u64,
        fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.inner.send_single(to_address, amount_ut, fee_rate).await
    }

    async fn send_batch(
        &self,
        recipients: Vec<(String, u64)>,
        fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.inner.send_batch(recipients, fee_rate).await
    }

    async fn observe_funding(&self, expected_amount_ut: u64) -> anyhow::Result<TxMetrics> {
        self.inner.observe_funding(expected_amount_ut).await
    }
}