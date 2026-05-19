use async_trait::async_trait;
use anyhow::anyhow;
use std::path::PathBuf;
use crate::driver::WalletDriver;
use crate::metrics::{ScanMetrics, TxMetrics};

pub struct PaymentProcessorDriver {
    pub data_dir: PathBuf,
    pub base_node_http_url: String,
}

impl PaymentProcessorDriver {
    pub fn new(data_dir: PathBuf, base_node_http_url: String) -> Self {
        Self { data_dir, base_node_http_url }
    }
}

#[async_trait]
impl WalletDriver for PaymentProcessorDriver {
    fn mode_name(&self) -> &str { "payment_processor" }

    async fn reset(&self) -> anyhow::Result<()> {
        if self.data_dir.exists() {
            std::fs::remove_dir_all(&self.data_dir)?;
        }
        std::fs::create_dir_all(&self.data_dir)?;
        Ok(())
    }

    async fn get_balance(&self) -> anyhow::Result<u64> {
        todo!("payment_processor get_balance")
    }

    async fn get_tip_height(&self) -> anyhow::Result<u64> {
        todo!("payment_processor get_tip_height")
    }

    async fn scan_from_genesis(&self) -> anyhow::Result<ScanMetrics> {
        todo!("payment_processor scan_from_genesis")
    }

    async fn scan_from_birthday(&self, _height: u64) -> anyhow::Result<ScanMetrics> {
        todo!("payment_processor scan_from_birthday")
    }

    async fn send_single(
        &self,
        _to_address: &str,
        _amount_ut: u64,
        _fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        todo!("payment_processor send_single: use individual send")
    }

    async fn send_batch(
        &self,
        recipients: Vec<(String, u64)>,
        fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        todo!("payment_processor send_batch: 1-to-many transaction")
    }
}
