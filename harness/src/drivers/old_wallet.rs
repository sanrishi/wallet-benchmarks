use std::fs;
use std::io::{BufReader, ErrorKind, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use async_trait::async_trait;
use reqwest::Client;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};


use crate::driver::WalletDriver;
use crate::drivers::shared;
use crate::drivers::shared::seed_words_with_birthday;
use crate::metrics::{ScanMetrics, TxMetrics};

// Include tonic-generated gRPC types from wallet.proto
#[allow(dead_code, clippy::doc_overindented_list_items)]
pub mod tari_rpc {
    tonic::include_proto!("tari.rpc");
}

pub struct OldWalletDriver {
    pub wallet_bin: PathBuf,
    pub data_dir: PathBuf,
    pub password: String,
    pub grpc_url: String,
    pub grpc_port: u16,
    pub base_node_url: String,
    pub confirmation_window: u64,
    pub startup_timeout_secs: u64,
    http_client: Client,
    seed_words: Mutex<String>,
    process: Mutex<Option<Child>>,
    stderr_capture: Mutex<Option<Arc<Mutex<String>>>>,
}

impl OldWalletDriver {
    pub fn new(
        wallet_bin: PathBuf,
        data_dir: PathBuf,
        password: String,
        grpc_port: u16,
        base_node_url: String,
        confirmation_window: u64,
        startup_timeout_secs: u64,
        config_seed: Option<String>,
    ) -> anyhow::Result<Self> {
        let grpc_url = format!("http://127.0.0.1:{}", grpc_port);
        fs::create_dir_all(&data_dir)?;
        let seed_words = Self::load_or_create_seed_words(&data_dir, config_seed.as_deref())?;
        Ok(Self {
            wallet_bin,
            data_dir,
            password,
            grpc_url,
            grpc_port,
            base_node_url,
            confirmation_window,
            startup_timeout_secs,
            http_client: Client::new(),
            seed_words: Mutex::new(seed_words),
            process: Mutex::new(None),
            stderr_capture: Mutex::new(None),
        })
    }

    fn load_or_create_seed_words(
        data_dir: &std::path::Path,
        config_seed: Option<&str>,
    ) -> anyhow::Result<String> {
        shared::load_or_create_seed_words(data_dir, config_seed)
    }

    pub fn set_seed_birthday(&self, birthday_days: u64) -> anyhow::Result<()> {
        let current = self.seed_words.lock().unwrap().clone();
        let updated = seed_words_with_birthday(&current, birthday_days)?;
        *self.seed_words.lock().unwrap() = updated;
        Ok(())
    }

    /// Spawn the wallet process and block until gRPC port responds or timeout
    pub async fn start(&self) -> anyhow::Result<()> {
        let stderr_capture = Arc::new(Mutex::new(String::new()));
        let stderr_capture_clone = stderr_capture.clone();
        let seed_words = self.seed_words.lock().unwrap().clone();

        let mut child = Command::new(&self.wallet_bin)
            .arg("--grpc-enabled")
            .arg("--grpc-address")
            .arg(format!("/ip4/127.0.0.1/tcp/{}", self.grpc_port))
            .arg("--password")
            .arg(&self.password)
            .arg("--seed-words")
            .arg(&seed_words)
            .arg(format!("--base-path={}", self.data_dir.display()))
            .arg("--non-interactive-mode")
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("failed to spawn wallet process: {e}"))?;

        // Read stderr in a background thread so the pipe does not fill up
        if let Some(stderr_handle) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut buffer = String::new();
                let mut reader = BufReader::new(stderr_handle);
                let _ = reader.read_to_string(&mut buffer);
                *stderr_capture_clone.lock().unwrap() = buffer;
            });
        }

        // Poll gRPC port until ready (max startup_timeout_secs)
        let deadline = Instant::now() + Duration::from_secs(self.startup_timeout_secs);
        loop {
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                let stderr = stderr_capture.lock().unwrap().clone();
                if !stderr.is_empty() {
                    return Err(anyhow!(
                        "wallet gRPC did not become ready within {}s. stderr:\n{stderr}",
                        self.startup_timeout_secs
                    ));
                }
                return Err(anyhow!(
                    "wallet gRPC did not become ready within {}s",
                    self.startup_timeout_secs
                ));
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    std::thread::sleep(Duration::from_millis(100));
                    let stderr = stderr_capture.lock().unwrap().clone();
                    if !stderr.is_empty() {
                        return Err(anyhow!(
                            "wallet process exited early with {status}. stderr:\n{stderr}"
                        ));
                    }
                    return Err(anyhow!("wallet process exited early with {status}"));
                }
                Ok(None) => {}
                Err(e) => return Err(anyhow!("failed to check wallet process status: {e}")),
            }
            if tokio::net::TcpStream::connect(format!("127.0.0.1:{}", self.grpc_port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        *self.process.lock().unwrap() = Some(child);
        *self.stderr_capture.lock().unwrap() = Some(stderr_capture);
        Ok(())
    }

    /// Return captured stderr output from the child process, if any
    pub fn captured_stderr(&self) -> String {
        self.stderr_capture
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|c| c.lock().ok())
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Kill the wallet process
    pub fn stop(&self) {
        if let Some(ref mut child) = *self.process.lock().unwrap() {
            let _ = child.kill();
            std::thread::sleep(Duration::from_millis(100));
            let _ = child.wait();
        }
        let captured = self.captured_stderr();
        if !captured.is_empty() {
            eprintln!("old_wallet stderr:\n{captured}");
        }
        std::thread::sleep(Duration::from_millis(500));
        *self.process.lock().unwrap() = None;
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

    async fn get_scanned_height(&self) -> anyhow::Result<u64> {
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::GetStateRequest;

        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        let resp = client.get_state(GetStateRequest {}).await?.into_inner();
        Ok(resp.scanned_height)
    }

    async fn get_utxo_count_via_grpc(&self) -> anyhow::Result<u64> {
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::{CoinBucket, CoinHistogramRequest};

        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        let histogram = client
            .coin_histogram(CoinHistogramRequest {
                buckets: vec![CoinBucket {
                    lower_bound: 0,
                    upper_bound: u64::MAX,
                }],
            })
            .await?
            .into_inner();
        let count: u64 = histogram.buckets.iter().map(|b| b.count).sum();
        Ok(count)
    }

    async fn get_base_node_tip_height(&self) -> anyhow::Result<u64> {
        shared::get_tip_height(&self.http_client, &self.base_node_url).await
    }

    /// Internal helper: restart the wallet with the given birthday and wait for it
    /// to finish scanning.  If `rescan_from_height` is Some and non-zero, a gRPC
    /// RescanWallet(height) is always issued after startup.  Non-zero heights work
    /// correctly upstream; height=0 is broken so genesis scans use height=1.
    async fn do_scan(
        &self,
        birthday_days: u64,
        rescan_from_height: Option<u64>,
    ) -> anyhow::Result<ScanMetrics> {
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::RescanWalletRequest;

        // 1. Stop existing process.
        self.stop();

        // 2. Wipe data dir.
        self.reset().await?;

        // 3. Set seed words with the requested birthday.
        self.set_seed_birthday(birthday_days)?;

        // 4. Record pre-scan tip and start the wallet.
        let h_tip_start = self.get_base_node_tip_height().await?;
        let target_tip = h_tip_start;
        self.start().await?;

        // 5. Issue gRPC RescanWallet to trigger a blockchain scan.  The
        //    non-zero height path works correctly upstream; for genesis scans
        //    we use height=1 (block 0 has no user funds) because height=0 is
        //    broken upstream and --seed-words alone no longer triggers a rescan
        //    in recent wallet versions.
        let rescan_height = match rescan_from_height {
            Some(fh) if fh > 0 => fh,
            _ => 1,
        };
        {
            let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
            client
                .rescan_wallet(RescanWalletRequest {
                    from_height: rescan_height,
                })
                .await?;
        }

        // 6. Wait for wallet to sync to tip.
        let pid = self
            .process
            .lock()
            .unwrap()
            .as_ref()
            .map(|child| Pid::from_u32(child.id()))
            .ok_or_else(|| anyhow!("wallet process is not running"))?;

        let started_at = Instant::now();
        let deadline = started_at + Duration::from_secs(1800);
        let mut system = System::new_all();
        let mut peak_rss_kb = 0_u64;
        let mut peak_cpu_percent = 0.0_f64;
        let mut h_tip_end;

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
        let effective_from = rescan_from_height.unwrap_or(0);
        let scanned_blocks = target_tip.saturating_sub(effective_from);
        let outputs_found = self.get_utxo_count_via_grpc().await?;
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
                return Err(anyhow!("transaction {tx_id} was not confirmed within 600s"));
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
            construction_secs: construction_finished_at
                .duration_since(started_at)
                .as_secs_f64(),
            broadcast_to_mempool_secs: broadcast_finished_at
                .duration_since(construction_finished_at)
                .as_secs_f64(),
            broadcast_to_confirmed_secs,
            fee_paid,
            success: true,
            error: None,
        })
    }

    async fn latest_incoming_tx_id(
        &self,
        expected_amount_ut: u64,
    ) -> anyhow::Result<Option<String>> {
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::GetAllCompletedTransactionsRequest;

        let mut client = WalletClient::connect(self.grpc_url.clone()).await?;
        #[allow(deprecated)]
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

#[async_trait]
impl WalletDriver for OldWalletDriver {
    fn mode_name(&self) -> &str {
        "old_wallet"
    }

    async fn reset(&self) -> anyhow::Result<()> {
        // reset() only handles filesystem; process lifecycle is
        // managed by stop()/start() separately.
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
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::GetBalanceRequest;

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
        // Full genesis scan: set birthday=0, restart wallet (--seed-words triggers
        // a full genesis scan at startup), wait for sync.  No RescanWallet call
        // (height=0 is broken upstream).
        self.do_scan(0, None).await
    }

    async fn scan_from_birthday(&self, height: u64) -> anyhow::Result<ScanMetrics> {
        // Birthday scan: restart wallet with birthday=0 seed words, then issue
        // RescanWallet(from_height=height) which works correctly for non-zero heights.
        self.do_scan(0, Some(height)).await
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
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::GetStateRequest;

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
            let state = client.get_state(GetStateRequest {}).await?.into_inner();
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
