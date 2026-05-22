use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use anyhow::anyhow;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

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
            .arg("--grpc-address")
            .arg(format!("/ip4/127.0.0.1/tcp/{}", self.grpc_port))
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

    async fn get_unspent_output_count(&self) -> anyhow::Result<u64> {
        use tari_rpc::wallet_client::WalletClient;

        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        let resp = client.get_unspent_amounts(tari_rpc::Empty {}).await?.into_inner();
        Ok(resp.amount.len() as u64)
    }

    async fn scan_from_height(&self, from_height: u64) -> anyhow::Result<ScanMetrics> {
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::RescanWalletRequest;

        let h_tip_start = self.get_tip_height().await?;
        let pid = self
            .process
            .as_ref()
            .map(|child| Pid::from_u32(child.id()))
            .ok_or_else(|| anyhow!("wallet process is not running"))?;

        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        client
            .rescan_wallet(RescanWalletRequest { from_height })
            .await?;

        let started_at = Instant::now();
        let deadline = started_at + Duration::from_secs(1800);
        let mut system = System::new_all();
        let mut peak_rss_kb = 0_u64;
        let mut peak_cpu_percent = 0.0_f64;
        let mut h_tip_end = h_tip_start;
        let mut last_height = None;
        let mut stable_polls = 0_u8;

        loop {
            if Instant::now() > deadline {
                return Err(anyhow!("wallet scan did not stabilize within 1800s"));
            }

            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                false,
                ProcessRefreshKind::nothing().with_memory().with_cpu(),
            );
            if let Some(process) = system.process(pid) {
                peak_rss_kb = peak_rss_kb.max(process.memory());
                peak_cpu_percent = peak_cpu_percent.max(process.cpu_usage() as f64);
            }

            let height = self.get_tip_height().await?;
            h_tip_end = height;

            if last_height == Some(height) {
                stable_polls = stable_polls.saturating_add(1);
            } else {
                stable_polls = 0;
                last_height = Some(height);
            }

            if stable_polls >= 3 {
                break;
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let wall_clock_secs = started_at.elapsed().as_secs_f64();
        let scanned_blocks = h_tip_end.saturating_sub(from_height);
        let outputs_found = self.get_unspent_output_count().await?;
        let blocks_per_sec = if wall_clock_secs > 0.0 {
            scanned_blocks as f64 / wall_clock_secs
        } else {
            0.0
        };

        Ok(ScanMetrics {
            wall_clock_secs,
            blocks_per_sec,
            h_tip_start,
            h_tip_end,
            outputs_found,
            peak_rss_kb,
            peak_cpu_percent,
        })
    }
}

#[async_trait]
impl WalletDriver for OldWalletDriver {
    fn mode_name(&self) -> &str { "old_wallet" }

    async fn reset(&self) -> anyhow::Result<()> {
        // Caller must call stop() before reset() for old_wallet.
        // reset() only handles filesystem; process lifecycle is
        // managed by start()/stop() in main.rs.
        if self.data_dir.exists() {
            std::fs::remove_dir_all(&self.data_dir)?;
        }
        std::fs::create_dir_all(&self.data_dir)?;
        Ok(())
    }

    async fn get_balance(&self) -> anyhow::Result<u64> {
        use tari_rpc::GetBalanceRequest;
        use tari_rpc::wallet_client::WalletClient;

        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        let resp = client
            .get_balance(GetBalanceRequest { payment_id: None })
            .await?
            .into_inner();
        Ok(resp.available_balance)
    }

    async fn get_tip_height(&self) -> anyhow::Result<u64> {
        use tari_rpc::GetStateRequest;
        use tari_rpc::wallet_client::WalletClient;

        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        let resp = client.get_state(GetStateRequest {}).await?.into_inner();
        Ok(resp.scanned_height)
    }

    async fn scan_from_genesis(&self) -> anyhow::Result<ScanMetrics> {
        self.scan_from_height(0).await
    }

    async fn scan_from_birthday(&self, height: u64) -> anyhow::Result<ScanMetrics> {
        self.scan_from_height(height).await
    }

    async fn send_single(
        &self,
        to_address: &str,
        amount_ut: u64,
        fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::{PaymentRecipient, TransferRequest};

        let started_at = Instant::now();
        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        let construction_finished_at = Instant::now();
        let resp = client
            .transfer(TransferRequest {
                recipients: vec![PaymentRecipient {
                    address: to_address.to_string(),
                    amount: amount_ut,
                    fee_per_gram: fee_rate,
                    payment_type: tari_rpc::payment_recipient::PaymentType::StandardMimblewimble
                        as i32,
                    raw_payment_id: Vec::new(),
                    user_payment_id: None,
                }],
                single_tx: true,
            })
            .await?
            .into_inner();
        let broadcast_finished_at = Instant::now();

        let result = resp
            .results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("transfer returned no results"))?;
        let fee_paid = result
            .transaction_info
            .as_ref()
            .map(|info| info.fee)
            .unwrap_or(0);
        let error = if result.is_success {
            None
        } else {
            Some(result.failure_message.clone())
        };

        Ok(TxMetrics {
            tx_id: result.transaction_id.to_string(),
            construction_secs: construction_finished_at.duration_since(started_at).as_secs_f64(),
            broadcast_to_mempool_secs: broadcast_finished_at
                .duration_since(construction_finished_at)
                .as_secs_f64(),
            broadcast_to_confirmed_secs: 0.0,
            fee_paid,
            success: result.is_success,
            error,
        })
    }
}
