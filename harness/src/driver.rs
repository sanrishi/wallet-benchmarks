use crate::metrics::{ScanMetrics, TxMetrics};
use async_trait::async_trait;

#[async_trait]
pub trait WalletDriver: Sync {
    /// Return the wallet mode name e.g. "old_wallet", "new_wallet", "payment_processor"
    fn mode_name(&self) -> &str;

    /// Wipe wallet data directory and reset state
    async fn reset(&self) -> anyhow::Result<()>;

    /// Get current spendable balance in uT
    async fn get_balance(&self) -> anyhow::Result<u64>;

    /// Get current chain tip height as seen by wallet
    async fn get_tip_height(&self) -> anyhow::Result<u64>;

    /// Trigger a scan from genesis (birthday = 0)
    async fn scan_from_genesis(&self) -> anyhow::Result<ScanMetrics>;

    /// Trigger a scan from a given birthday height
    async fn scan_from_birthday(&self, height: u64) -> anyhow::Result<ScanMetrics>;

    /// Send a single transaction, return metrics. No retry, no backoff.
    async fn send_single(
        &self,
        to_address: &str,
        amount_ut: u64,
        fee_rate: u64,
    ) -> anyhow::Result<TxMetrics>;

    /// Send a batch (1-to-many) transaction. Only used by payment_processor mode.
    /// Default impl returns an error so old/new wallet modes don't need to implement it.
    async fn send_batch(
        &self,
        _recipients: Vec<(String, u64)>,
        _fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        Err(anyhow::anyhow!("send_batch not supported by this driver"))
    }
}
