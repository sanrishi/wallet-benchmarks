use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::str::FromStr;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use anyhow::{anyhow, Context};
use reqwest::Client;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tari_common_types::seeds::{
    cipher_seed::CipherSeed,
    mnemonic::{Mnemonic, MnemonicLanguage},
};

use crate::driver::WalletDriver;
use crate::metrics::{ScanMetrics, TxMetrics};

// Include tonic-generated gRPC types from wallet.proto
pub mod tari_rpc {
    tonic::include_proto!("tari.rpc");
}

pub struct OldWalletDriver {
    pub wallet_bin: PathBuf,      // path to minotari_console_wallet binary
    pub data_dir: PathBuf,        // wallet data directory (wiped on reset)
    pub password: String,
    pub grpc_url: String,         // e.g. "http://127.0.0.1:18143"
    pub grpc_port: u16,
    pub base_node_url: String,
    pub confirmation_window: u64,
    http_client: Client,
    seed_words: String,
    process: Option<Child>,       // spawned wallet process
}

impl OldWalletDriver {
    pub fn new(
        wallet_bin: PathBuf,
        data_dir: PathBuf,
        password: String,
        grpc_port: u16,
        base_node_url: String,
        confirmation_window: u64,
    ) -> anyhow::Result<Self> {
        let grpc_url = format!("http://127.0.0.1:{}", grpc_port);
        fs::create_dir_all(&data_dir)?;
        let seed_words = Self::load_or_create_seed_words(&data_dir)?;
        Ok(Self {
            wallet_bin,
            data_dir,
            password,
            grpc_url,
            grpc_port,
            base_node_url,
            confirmation_window,
            http_client: Client::new(),
            seed_words,
            process: None,
        })
    }

    fn seed_words_path(data_dir: &std::path::Path) -> PathBuf {
        data_dir.join("seed_words.txt")
    }

    fn load_or_create_seed_words(data_dir: &std::path::Path) -> anyhow::Result<String> {
        let seed_words_path = Self::seed_words_path(data_dir);
        match fs::read_to_string(&seed_words_path) {
            Ok(seed_words) => Ok(seed_words.trim().to_string()),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let seed_words = CipherSeed::random()
                    .to_mnemonic(MnemonicLanguage::English, None)?
                    .join(" ")
                    .reveal()
                    .to_string();
                fs::write(&seed_words_path, &seed_words)?;
                Ok(seed_words)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn seed_words_with_birthday(seed_words: &str, birthday: u64) -> anyhow::Result<String> {
        let mnemonic = tari_common_types::seeds::seed_words::SeedWords::from_str(seed_words)?;
        let mut seed = CipherSeed::from_mnemonic(&mnemonic, None)?;
        let birthday = u16::try_from(birthday)
            .with_context(|| format!("birthday {birthday} exceeds u16 range for old_wallet"))?;
        seed.change_birthday(birthday);
        Ok(seed
            .to_mnemonic(MnemonicLanguage::English, None)?
            .join(" ")
            .reveal()
            .to_string())
    }

    pub fn set_seed_birthday(&mut self, birthday: u64) -> anyhow::Result<()> {
        self.seed_words = Self::seed_words_with_birthday(&self.seed_words, birthday)?;
        Ok(())
    }

    /// Spawn the wallet process and block until gRPC port responds or timeout
    pub async fn start(&mut self) -> anyhow::Result<()> {
        let child = Command::new(&self.wallet_bin)
            .arg("--grpc-enabled")
            .arg("--grpc-address")
            .arg(format!("/ip4/127.0.0.1/tcp/{}", self.grpc_port))
            .arg("--password")
            .arg(&self.password)
            .arg("--seed-words")
            .arg(&self.seed_words)
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
            let _ = child.wait();
        }
        std::thread::sleep(Duration::from_millis(500));
        self.process = None;
    }

    pub async fn get_wallet_address(&self) -> anyhow::Result<String> {
        use tari_rpc::wallet_client::WalletClient;

        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        let resp = client
            .get_complete_address(tari_rpc::Empty {})
            .await?
            .into_inner();

        if !resp.one_sided_address_base58.is_empty() {
            return Ok(resp.one_sided_address_base58);
        }
        if !resp.interactive_address_base58.is_empty() {
            return Ok(resp.interactive_address_base58);
        }

        Err(anyhow!("wallet returned no printable address"))
    }

    async fn get_unspent_output_count(&self) -> anyhow::Result<u64> {
        use tari_rpc::wallet_client::WalletClient;

        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        let resp = client.get_unspent_amounts(tari_rpc::Empty {}).await?.into_inner();
        Ok(resp.amount.len() as u64)
    }

    async fn get_scanned_height(&self) -> anyhow::Result<u64> {
        use tari_rpc::GetStateRequest;
        use tari_rpc::wallet_client::WalletClient;

        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        let resp = client.get_state(GetStateRequest {}).await?.into_inner();
        Ok(resp.scanned_height)
    }

    async fn get_base_node_tip_height(&self) -> anyhow::Result<u64> {
        let url = format!("{}/get_tip_info", self.base_node_url.trim_end_matches('/'));
        let tip: TipInfoResponse = self
            .http_client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(tip.metadata.best_block_height)
    }

    async fn scan_from_height(&self, from_height: u64) -> anyhow::Result<ScanMetrics> {
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::RescanWalletRequest;

        let h_tip_start = self.get_base_node_tip_height().await?;
        let target_tip = h_tip_start;
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

        loop {
            if Instant::now() > deadline {
                return Err(anyhow!(
                    "wallet scan did not reach target tip {target_tip} within 1800s"
                ));
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

            let scanned_height = self.get_scanned_height().await?;
            h_tip_end = self.get_base_node_tip_height().await?;
            if scanned_height >= target_tip {
                break;
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let wall_clock_secs = started_at.elapsed().as_secs_f64();
        let scanned_blocks = target_tip.saturating_sub(from_height);
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

    async fn wait_for_confirmation(&self, tx_id: u64) -> anyhow::Result<f64> {
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::GetTransactionInfoRequest;

        let started_at = Instant::now();
        let deadline = started_at + Duration::from_secs(600);
        let required_confirmations = self.confirmation_window.max(1);
        let confirmed_statuses = [
            tari_rpc::TransactionStatus::MinedConfirmed as i32,
            tari_rpc::TransactionStatus::MinedConfirmedLocked as i32,
            tari_rpc::TransactionStatus::OneSidedConfirmed as i32,
            tari_rpc::TransactionStatus::OneSidedConfirmedLocked as i32,
            tari_rpc::TransactionStatus::CoinbaseConfirmed as i32,
            tari_rpc::TransactionStatus::CoinbaseConfirmedLocked as i32,
        ];

        loop {
            if Instant::now() > deadline {
                return Err(anyhow!(
                    "transaction {tx_id} was not confirmed within 600s"
                ));
            }

            let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
            let response = client
                .get_transaction_info(GetTransactionInfoRequest {
                    transaction_ids: vec![tx_id],
                })
                .await?
                .into_inner();

            if let Some(info) = response.transactions.into_iter().next() {
                if confirmed_statuses.contains(&info.status) {
                    return Ok(started_at.elapsed().as_secs_f64());
                }

                if info.status == tari_rpc::TransactionStatus::Rejected as i32 {
                    let reason = if info.rejected_reason.is_empty() {
                        "transaction was rejected".to_string()
                    } else {
                        info.rejected_reason
                    };
                    return Err(anyhow!(reason));
                }

                if info.mined_in_block_height > 0 {
                    let tip = self.get_tip_height().await?;
                    let confirmation_height = info
                        .mined_in_block_height
                        .saturating_add(required_confirmations.saturating_sub(1));
                    if tip >= confirmation_height {
                        return Ok(started_at.elapsed().as_secs_f64());
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    fn payment_recipient(
        address: &str,
        amount_ut: u64,
        fee_rate: u64,
    ) -> tari_rpc::PaymentRecipient {
        tari_rpc::PaymentRecipient {
            address: address.to_string(),
            amount: amount_ut,
            fee_per_gram: fee_rate,
            payment_type: tari_rpc::payment_recipient::PaymentType::StandardMimblewimble as i32,
            raw_payment_id: Vec::new(),
            user_payment_id: None,
        }
    }

    async fn transfer_recipients(
        &self,
        recipients: Vec<(String, u64)>,
        fee_rate: u64,
        single_tx: bool,
    ) -> anyhow::Result<TxMetrics> {
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::TransferRequest;

        let started_at = Instant::now();
        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        let construction_finished_at = Instant::now();
        let response = client
            .transfer(TransferRequest {
                recipients: recipients
                    .iter()
                    .map(|(address, amount)| Self::payment_recipient(address, *amount, fee_rate))
                    .collect(),
                single_tx,
            })
            .await?
            .into_inner();
        let broadcast_finished_at = Instant::now();

        let results = response.results;
        let first = results
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("transfer returned no results"))?;
        let failed_messages = results
            .iter()
            .filter(|result| !result.is_success)
            .map(|result| {
                if result.failure_message.is_empty() {
                    format!("recipient {} failed", result.address)
                } else {
                    result.failure_message.clone()
                }
            })
            .collect::<Vec<_>>();
        let success = failed_messages.is_empty() && first.is_success;

        if !success {
            return Err(anyhow!(failed_messages.join("; ")));
        }

        let fee_paid = first
            .transaction_info
            .as_ref()
            .map(|info| info.fee)
            .unwrap_or(0);
        let tx_id = first.transaction_id.to_string();
        let broadcast_to_confirmed_secs = self.wait_for_confirmation(first.transaction_id).await?;

        Ok(TxMetrics {
            tx_id,
            construction_secs: construction_finished_at.duration_since(started_at).as_secs_f64(),
            broadcast_to_mempool_secs: broadcast_finished_at
                .duration_since(construction_finished_at)
                .as_secs_f64(),
            broadcast_to_confirmed_secs,
            fee_paid,
            success: true,
            error: None,
        })
    }

    async fn latest_incoming_tx_id(&self, expected_amount_ut: u64) -> anyhow::Result<Option<String>> {
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::GetAllCompletedTransactionsRequest;

        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        let response = client
            .get_all_completed_transactions(GetAllCompletedTransactionsRequest {
                offset: 0,
                limit: 50,
                status_bitflag: 0,
            })
            .await?
            .into_inner();

        Ok(response
            .transactions
            .into_iter()
            .filter(|tx| {
                tx.direction == tari_rpc::TransactionDirection::Inbound as i32
                    && tx.amount >= expected_amount_ut
            })
            .max_by_key(|tx| tx.timestamp)
            .map(|tx| tx.tx_id.to_string()))
    }
}

#[derive(Debug, serde::Deserialize)]
struct TipInfoResponse {
    metadata: TipMetadata,
}

#[derive(Debug, serde::Deserialize)]
struct TipMetadata {
    best_block_height: u64,
}

#[async_trait]
impl WalletDriver for OldWalletDriver {
    fn mode_name(&self) -> &str { "old_wallet" }

    async fn reset(&self) -> anyhow::Result<()> {
        // Caller must call stop() before reset() for old_wallet.
        // reset() only handles filesystem; process lifecycle is
        // managed by start()/stop() in main.rs.
        let mut last_error = None;
        for _ in 0..20 {
            if self.data_dir.exists() {
                match fs::remove_dir_all(&self.data_dir) {
                    Ok(()) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::NotFound | ErrorKind::PermissionDenied
                        ) || error.raw_os_error() == Some(32) =>
                    {
                        last_error = Some(error);
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                }
            }

            match fs::create_dir_all(&self.data_dir) {
                Ok(()) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    return Ok(());
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::AlreadyExists | ErrorKind::PermissionDenied
                    ) || error.raw_os_error() == Some(32) =>
                {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }

        Err(last_error
            .unwrap_or_else(|| std::io::Error::other("wallet data directory reset timed out"))
            .into())
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
        self.get_base_node_tip_height().await
    }

    async fn get_self_address(&self) -> anyhow::Result<String> {
        self.get_wallet_address().await
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
        self.transfer_recipients(vec![(to_address.to_string(), amount_ut)], fee_rate, true)
            .await
    }

    async fn send_batch(
        &self,
        recipients: Vec<(String, u64)>,
        fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.transfer_recipients(recipients, fee_rate, true).await
    }

    async fn observe_funding(&self, expected_amount_ut: u64) -> anyhow::Result<TxMetrics> {
        use tari_rpc::GetStateRequest;
        use tari_rpc::wallet_client::WalletClient;

        let started_at = Instant::now();
        let deadline = started_at + Duration::from_secs(600);
        let mut first_seen_pending_at = None;
        let mut observed_tx_id = None;

        loop {
            if Instant::now() > deadline {
                return Err(anyhow!(
                    "incoming funding of at least {expected_amount_ut} uT was not observed within 600s"
                ));
            }

            let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
            let state = client
                .get_state(GetStateRequest {})
                .await?
                .into_inner();
            let balance = state
                .balance
                .ok_or_else(|| anyhow!("wallet state returned no balance"))?;

            if observed_tx_id.is_none() {
                observed_tx_id = self.latest_incoming_tx_id(expected_amount_ut).await?;
            }

            if first_seen_pending_at.is_none()
                && (balance.pending_incoming_balance >= expected_amount_ut
                    || balance.available_balance >= expected_amount_ut)
            {
                first_seen_pending_at = Some(started_at.elapsed().as_secs_f64());
            }

            if balance.available_balance >= expected_amount_ut {
                let mempool_secs =
                    first_seen_pending_at.unwrap_or_else(|| started_at.elapsed().as_secs_f64());
                return Ok(TxMetrics {
                    tx_id: observed_tx_id.unwrap_or_else(|| "incoming-funding".to_string()),
                    construction_secs: 0.0,
                    broadcast_to_mempool_secs: mempool_secs,
                    broadcast_to_confirmed_secs: started_at.elapsed().as_secs_f64(),
                    fee_paid: 0,
                    success: true,
                    error: None,
                });
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}
