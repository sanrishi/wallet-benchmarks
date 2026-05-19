use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use async_trait::async_trait;
use anyhow::anyhow;
use crate::driver::WalletDriver;
use crate::metrics::{ScanMetrics, TxMetrics};

// Include tonic-generated gRPC types from wallet.proto
pub mod tari_rpc {
    tonic::include_proto!("tari.rpc");
}

pub struct OldWalletDriver {
    pub wallet_bin: PathBuf,      // path to minotari_console_wallet binary
    pub data_dir: PathBuf,        // wallet data directory (wiped on reset)
    pub grpc_url: String,         // e.g. "http://127.0.0.1:18143"
    pub grpc_port: u16,
    process: Option<Child>,       // spawned wallet process
}

impl OldWalletDriver {
    pub fn new(wallet_bin: PathBuf, data_dir: PathBuf, grpc_port: u16) -> Self {
        let grpc_url = format!("http://127.0.0.1:{}", grpc_port);
        Self {
            wallet_bin,
            data_dir,
            grpc_url,
            grpc_port,
            process: None,
        }
    }

    /// Spawn the wallet process and block until gRPC port responds or timeout
    pub async fn start(&mut self) -> anyhow::Result<()> {
        let child = Command::new(&self.wallet_bin)
            .arg("--grpc-enabled")
            .arg(format!("--grpc-port={}", self.grpc_port))
            .arg(format!("--base-path={}", self.data_dir.display()))
            .arg("--non-interactive-mode")
            .spawn()?;
        self.process = Some(child);

        // Poll gRPC port until ready (max 120s)
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if Instant::now() > deadline {
                return Err(anyhow!("wallet gRPC did not become ready within 120s"));
            }
            if tokio::net::TcpStream::connect(
                format!("127.0.0.1:{}", self.grpc_port)
            ).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok(())
    }

    /// Kill the wallet process
    pub fn stop(&mut self) {
        if let Some(ref mut child) = self.process {
            let _ = child.kill();
        }
        self.process = None;
    }
}

#[async_trait]
impl WalletDriver for OldWalletDriver {
    fn mode_name(&self) -> &str { "old_wallet" }

    async fn reset(&self) -> anyhow::Result<()> {
        if self.data_dir.exists() {
            std::fs::remove_dir_all(&self.data_dir)?;
        }
        std::fs::create_dir_all(&self.data_dir)?;
        Ok(())
    }

    async fn get_balance(&self) -> anyhow::Result<u64> {
        todo!("gRPC GetBalance not yet implemented")
    }

    async fn get_tip_height(&self) -> anyhow::Result<u64> {
        todo!("gRPC GetTipHeight not yet implemented")
    }

    async fn scan_from_genesis(&self) -> anyhow::Result<ScanMetrics> {
        todo!("scan_from_genesis not yet implemented")
    }

    async fn scan_from_birthday(&self, _height: u64) -> anyhow::Result<ScanMetrics> {
        todo!("scan_from_birthday not yet implemented")
    }

    async fn send_single(
        &self,
        _to_address: &str,
        _amount_ut: u64,
        _fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        todo!("send_single not yet implemented")
    }
}
