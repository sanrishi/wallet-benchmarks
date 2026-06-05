use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use tokio::process::{Child, Command};

use std::str::FromStr;

use tari_common::configuration::Network;
use tari_common_types::seeds::{
    cipher_seed::CipherSeed,
    mnemonic::Mnemonic,
    seed_words::SeedWords,
};
use tari_common_types::tari_address::{TariAddress, TariAddressFeatures};
use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};

use crate::driver::WalletDriver;
use crate::drivers::shared;
use crate::drivers::shared::{derive_wallet_keys, seed_words_with_birthday, DEFAULT_ACCOUNT_NAME};
use crate::metrics::{ScanMetrics, TxMetrics};

#[derive(Debug, Deserialize)]
struct PaymentResponse {
    payment_id: String,
    status: String,
    recipient_address: String,
    amount: i64,
    failure_reason: Option<String>,
    #[allow(dead_code)]
    mined_height: Option<i64>,
    #[allow(dead_code)]
    mined_timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct BulkPaymentResponse {
    batch_id: String,
    status: String,
    payments: Vec<PaymentResponse>,
}

#[derive(Debug, Deserialize)]
struct ScanStatusResponse {
    last_scanned_height: u64,
    #[serde(default)]
    outputs_found: u64,
}

#[derive(Debug, Deserialize)]
struct BalanceResponse {
    available_balance: u64,
}

/// Approximate number of blocks per day on Esmeralda (~120s blocks).
const BLOCKS_PER_DAY: u64 = 720;

/// Driver for Mode 3 (payment_processor).
///
/// Manages TWO parallel daemons directly (no NewWalletDriver wrapper):
/// 1. A `minotari daemon` (PR/account daemon) providing the HTTP REST API
///    for balance queries, scan status, and address derivation.
/// 2. The `minotari_payment_processor` microservice orchestrating payments.
///
/// Wallet-level operations (scan, balance, address) use the PR daemon's
/// HTTP REST API. Payment operations use the PP daemon's REST API.
///
/// Seed words are held in memory and preserved across data directory wipes
/// to ensure the funding UTXO set is recoverable after reset.
pub struct PaymentProcessorDriver {
    pp_bin: PathBuf,
    minotari_bin: PathBuf,
    console_wallet_bin_path: PathBuf,
    data_dir: PathBuf,
    base_node_url: String,
    confirmation_window: u64,
    password: String,
    http_client: Client,
    seed_words: Mutex<String>,
    pp_daemon: Mutex<Option<Child>>,
    pr_daemon: Mutex<Option<Child>>,
    api_url: Mutex<Option<String>>,
    api_port: u16,
    pr_port: u16,
    pr_url: Mutex<Option<String>>,
}

impl PaymentProcessorDriver {
    pub fn new(
        pp_bin: PathBuf,
        data_dir: PathBuf,
        minotari_bin: PathBuf,
        console_wallet_bin: PathBuf,
        base_node_url: String,
        confirmation_window: u64,
        password: String,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create {}", data_dir.display()))?;

        let seed_words = Self::load_or_create_seed_words(&data_dir)?;
        let pp_port = Self::find_free_port()?;
        let pr_port = Self::find_free_port()?;

        Ok(Self {
            pp_bin,
            data_dir,
            minotari_bin,
            console_wallet_bin_path: console_wallet_bin,
            base_node_url,
            confirmation_window,
            password,
            http_client: Client::new(),
            seed_words: Mutex::new(seed_words),
            pp_daemon: Mutex::new(None),
            pr_daemon: Mutex::new(None),
            api_url: Mutex::new(None),
            api_port: pp_port,
            pr_port,
            pr_url: Mutex::new(None),
        })
    }

    fn load_or_create_seed_words(data_dir: &std::path::Path) -> anyhow::Result<String> {
        shared::load_or_create_seed_words(data_dir)
    }

    fn find_free_port() -> anyhow::Result<u16> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        Ok(listener.local_addr()?.port())
    }

    fn database_path(&self) -> PathBuf {
        self.data_dir.join("wallet.db")
    }

    // ── Wallet (PR daemon) lifecycle ─────────────────────────────────

    /// Create a wallet database from seed words, encoding `birthday_days`
    /// into the cipher seed so the daemon knows where to start scanning.
    async fn create_wallet(&self, birthday_days: u64) -> anyhow::Result<()> {
        let seed = {
            let words = self.seed_words.lock().unwrap();
            seed_words_with_birthday(&words, birthday_days)?
        };
        let db_path = self.database_path();
        let db_str = db_path
            .to_str()
            .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?;

        let status = Command::new(&self.minotari_bin)
            .arg("create")
            .arg("--password")
            .arg(&self.password)
            .arg("--database-path")
            .arg(db_str)
            .arg("--account-name")
            .arg(DEFAULT_ACCOUNT_NAME)
            .arg("--seed-words")
            .arg(&seed)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .with_context(|| {
                format!(
                    "failed to run minotari create at {}",
                    self.minotari_bin.display()
                )
            })?;

        if !status.success() {
            return Err(anyhow!("minotari create failed with status {status}"));
        }
        Ok(())
    }

    /// Spawn the `minotari daemon` (PR account daemon) and wait for its
    /// HTTP API to become reachable.
    async fn start_pr_daemon(&self) -> anyhow::Result<()> {
        let db_str = self
            .database_path()
            .to_str()
            .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?
            .to_string();
        let port = self.pr_port;

        for attempt in 0..5 {
            let port = if attempt == 0 { port } else { Self::find_free_port()? };

            let mut child = Command::new(&self.minotari_bin)
                .arg("daemon")
                .arg("--password")
                .arg(&self.password)
                .arg("--database-path")
                .arg(&db_str)
                .arg("--api-port")
                .arg(port.to_string())
                .arg("--base-url")
                .arg(&self.base_node_url)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| {
                    format!(
                        "failed to spawn PR daemon at {}",
                        self.minotari_bin.display()
                    )
                })?;

            let base_url = format!("http://127.0.0.1:{port}");
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if Instant::now() > deadline {
                    break;
                }
                if self
                    .http_client
                    .get(format!("{base_url}/version"))
                    .send()
                    .await
                    .is_ok()
                {
                    // Child is ready; store handles with minimal lock time.
                    *self.pr_daemon.lock().unwrap() = Some(child);
                    *self.pr_url.lock().unwrap() = Some(base_url);
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            // Port may have collided (TIME_WAIT) — kill and retry
            let _ = child.try_wait();
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        Err(anyhow!(
            "PR daemon did not become ready after 5 port-retry attempts"
        ))
    }

    async fn stop_pr_daemon(&self) {
        let child_opt = self.pr_daemon.lock().unwrap().take();
        if let Some(mut child) = child_opt {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.pr_url.lock().unwrap().take();
    }

    // ── Payment Processor (PP daemon) lifecycle ──────────────────────

    /// Spawn the `minotari_payment_processor` and wait for its API.
    async fn start_pp_daemon(&self) -> anyhow::Result<()> {
        let port = self.api_port;
        let api_url = format!("http://127.0.0.1:{port}");
        let db_path = self.data_dir.join("payments.db");
        let db_dir = self.data_dir.join("data");
        std::fs::create_dir_all(&db_dir)?;
        let db_url = format!("sqlite:{}", db_path.display());

        let pr_url = self
            .pr_url
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("PR daemon URL not set before starting PP daemon"))?;

        let (view_key_hex, spend_key_hex) = {
            let words = self.seed_words.lock().unwrap();
            derive_wallet_keys(&words)?
        };

        let mut child = Command::new(&self.pp_bin)
            .env("DATABASE_URL", &db_url)
            .env("TARI_NETWORK", "Esmeralda")
            .env("BASE_NODE", &self.base_node_url)
            .env("PAYMENT_RECEIVER", &pr_url)
            .env("CONSOLE_WALLET_PATH", &self.console_wallet_bin_path)
            .env("CONSOLE_WALLET_BASE_PATH", &self.data_dir)
            .env("CONSOLE_WALLET_PASSWORD", &self.password)
            .env("LISTEN_PORT", port.to_string())
            .env("LISTEN_IP", "127.0.0.1")
            .env("ACCOUNTS__DEFAULT__NAME", "default")
            .env("ACCOUNTS__DEFAULT__VIEW_KEY", &view_key_hex)
            .env("ACCOUNTS__DEFAULT__PUBLIC_SPEND_KEY", &spend_key_hex)
            .env(
                "CONFIRMATION_CHECKER_REQUIRED_CONFIRMATIONS",
                self.confirmation_window.to_string(),
            )
            .env("REVEAL_PII", "1")
            .env("MAX_INPUT_COUNT_PER_TX", "400")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn PP daemon at {}",
                    self.pp_bin.display()
                )
            })?;

        let deadline = Instant::now() + Duration::from_secs(60);
        let health_url = format!("{api_url}/health/version");
        loop {
            if Instant::now() > deadline {
                let _ = child.try_wait();
                return Err(anyhow!(
                    "PP daemon did not become ready within 60s at {health_url}"
                ));
            }
            if self.http_client.get(&health_url).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        *self.pp_daemon.lock().unwrap() = Some(child);
        *self.api_url.lock().unwrap() = Some(api_url);
        Ok(())
    }

    async fn stop_pp_daemon(&self) {
        let child_opt = self.pp_daemon.lock().unwrap().take();
        if let Some(mut child) = child_opt {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.api_url.lock().unwrap().take();
    }

    /// Ensure the PP daemon is running (lazy restart after reset).
    async fn ensure_pp_daemon_running(&self) -> anyhow::Result<()> {
        if self.pp_daemon.lock().unwrap().is_some() {
            return Ok(());
        }
        self.start_pp_daemon().await
    }

    /// Public entry point — start both daemons.
    pub async fn start_daemon(&mut self) -> anyhow::Result<()> {
        if !self.database_path().exists() {
            self.create_wallet(0).await?;
        }
        self.start_pr_daemon().await?;
        self.start_pp_daemon().await?;
        Ok(())
    }

    /// Public entry point — stop both daemons.
    pub async fn stop_daemon(&mut self) {
        self.stop_pp_daemon().await;
        self.stop_pr_daemon().await;
    }

    // ── PR daemon HTTP helpers ───────────────────────────────────────

    async fn get_scan_status(&self) -> anyhow::Result<ScanStatusResponse> {
        let pr_url = self
            .pr_url
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("PR daemon not running"))?;
        self.http_client
            .get(format!(
                "{pr_url}/accounts/{DEFAULT_ACCOUNT_NAME}/scan_status"
            ))
            .send()
            .await
            .context("failed to query PR daemon scan_status")?
            .error_for_status()
            .context("PR daemon scan_status returned HTTP error")?
            .json()
            .await
            .context("failed to parse PR daemon scan_status response")
    }

    async fn get_balance_from_pr(&self) -> anyhow::Result<u64> {
        let pr_url = self
            .pr_url
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("PR daemon not running"))?;
        let resp: BalanceResponse = self
            .http_client
            .get(format!(
                "{pr_url}/accounts/{DEFAULT_ACCOUNT_NAME}/balance"
            ))
            .send()
            .await
            .context("failed to query PR daemon balance")?
            .error_for_status()
            .context("PR daemon balance returned HTTP error")?
            .json()
            .await
            .context("failed to parse PR daemon balance response")?;
        Ok(resp.available_balance)
    }

    // ── Internal scan logic ──────────────────────────────────────────

    /// Core scan: stop PR daemon, recreate the wallet with `birthday_days`,
    /// restart PR daemon, and wait for the scan to reach the current tip.
    async fn scan_from_height(
        &self,
        from_height: u64,
        birthday_days: u64,
    ) -> anyhow::Result<ScanMetrics> {
        let h_tip_start = shared::get_tip_height(&self.http_client, &self.base_node_url).await?;
        let started_at = Instant::now();

        self.stop_pr_daemon().await;

        let db_path = self.database_path();
        if db_path.exists() {
            std::fs::remove_file(&db_path)
                .with_context(|| format!("failed to remove {}", db_path.display()))?;
        }

        self.create_wallet(birthday_days).await?;
        self.start_pr_daemon().await?;

        let deadline = started_at + Duration::from_secs(3600);
        loop {
            let status = self.get_scan_status().await?;
            if status.last_scanned_height >= h_tip_start {
                let wall_clock_secs = started_at.elapsed().as_secs_f64();
                let h_tip_end =
                    shared::get_tip_height(&self.http_client, &self.base_node_url).await?;
                let scanned_blocks = h_tip_end.saturating_sub(from_height);
                let blocks_per_sec = if wall_clock_secs > 0.0 {
                    scanned_blocks as f64 / wall_clock_secs
                } else {
                    0.0
                };
                return Ok(ScanMetrics {
                    wall_clock_secs,
                    blocks_per_sec,
                    h_tip_start,
                    h_tip_end,
                    outputs_found: status.outputs_found,
                    peak_rss_kb: 0,
                    peak_cpu_percent: 0.0,
                });
            }
            if Instant::now() > deadline {
                return Err(anyhow!(
                    "scan from height {from_height} did not reach tip within 3600s"
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    // ── PP REST API helpers (unchanged logic) ────────────────────────

    async fn api_send(&self, to_address: &str, amount_ut: u64) -> anyhow::Result<TxMetrics> {
        let api_url = self
            .api_url
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("PP daemon not running"))?;
        let client_id = format!("bench-single-{}", Instant::now().elapsed().as_nanos());

        let started_at = Instant::now();
        let resp = self
            .http_client
            .post(format!("{api_url}/v1/payments"))
            .json(&serde_json::json!({
                "client_id": client_id,
                "account_name": "default",
                "recipient_address": to_address,
                "amount": amount_ut as i64,
            }))
            .send()
            .await
            .context("PP API request failed")?;

        let construction_secs = started_at.elapsed().as_secs_f64();

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("PP rejected payment (HTTP {status}): {body}"));
        }

        let payment: PaymentResponse = resp
            .json()
            .await
            .context("failed to parse PP payment response")?;

        let confirm_deadline = started_at + Duration::from_secs(600);
        let tx_id = payment.payment_id.clone();
        let mut broadcast_to_mempool_secs = 0.0;
        let mut broadcast_to_confirmed_secs = 0.0;

        loop {
            if Instant::now() > confirm_deadline {
                return Err(anyhow!("payment {tx_id} was not confirmed within 600s"));
            }
            let status_resp = self
                .http_client
                .get(format!("{api_url}/v1/payments/{tx_id}"))
                .send()
                .await
                .context("failed to poll payment status")?;

            if let Ok(status_payment) = status_resp.json::<PaymentResponse>().await {
                match status_payment.status.as_str() {
                    "Completed" | "Mined" | "Confirmed" => {
                        broadcast_to_confirmed_secs = started_at.elapsed().as_secs_f64();
                        if broadcast_to_mempool_secs == 0.0 {
                            broadcast_to_mempool_secs = broadcast_to_confirmed_secs;
                        }
                        break;
                    }
                    "Failed" | "Cancelled" => {
                        let reason = status_payment.failure_reason.unwrap_or_default();
                        return Err(anyhow!("payment {tx_id} failed: {reason}"));
                    }
                    _ => {}
                }
            }
            if broadcast_to_mempool_secs == 0.0 && started_at.elapsed().as_secs_f64() > 5.0 {
                broadcast_to_mempool_secs = started_at.elapsed().as_secs_f64();
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }

        Ok(TxMetrics {
            tx_id,
            construction_secs,
            broadcast_to_mempool_secs,
            broadcast_to_confirmed_secs,
            fee_paid: 0,
            success: true,
            error: None,
        })
    }

    async fn api_send_batch(&self, recipients: Vec<(String, u64)>) -> anyhow::Result<TxMetrics> {
        let api_url = self
            .api_url
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("PP daemon not running"))?;
        let batch_id = format!("bench-batch-{}", Instant::now().elapsed().as_nanos());

        let items: Vec<serde_json::Value> = recipients
            .iter()
            .map(|(addr, amount)| {
                serde_json::json!({
                    "client_id": format!("{}-item-{}", batch_id, Instant::now().elapsed().as_nanos()),
                    "recipient_address": addr,
                    "amount": *amount as i64,
                })
            })
            .collect();

        let started_at = Instant::now();
        let resp = self
            .http_client
            .post(format!("{api_url}/v1/payment-batches"))
            .json(&serde_json::json!({
                "account_name": "default",
                "items": items,
            }))
            .send()
            .await
            .context("PP batch API request failed")?;

        let construction_secs = started_at.elapsed().as_secs_f64();

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("PP rejected batch (HTTP {status}): {body}"));
        }

        let batch: BulkPaymentResponse = resp
            .json()
            .await
            .context("failed to parse batch response")?;

        let confirm_deadline = started_at + Duration::from_secs(600);
        let mut broadcast_to_mempool_secs = 0.0;
        let mut broadcast_to_confirmed_secs = 0.0;
        let payment_ids: Vec<String> = batch
            .payments
            .iter()
            .map(|p| p.payment_id.clone())
            .collect();
        let tx_id = batch.batch_id;

        loop {
            if Instant::now() > confirm_deadline {
                return Err(anyhow!("batch {tx_id} was not fully confirmed within 600s"));
            }

            let mut all_done = true;
            let mut any_failed = false;
            let mut fail_reason = String::new();

            for pid in &payment_ids {
                let status_resp = self
                    .http_client
                    .get(format!("{api_url}/v1/payments/{pid}"))
                    .send()
                    .await;

                if let Ok(resp) = status_resp {
                    if let Ok(payment) = resp.json::<PaymentResponse>().await {
                        match payment.status.as_str() {
                            "Completed" | "Mined" | "Confirmed" => continue,
                            "Failed" | "Cancelled" => {
                                any_failed = true;
                                fail_reason = payment.failure_reason.unwrap_or_default();
                                break;
                            }
                            _ => all_done = false,
                        }
                    }
                }
            }

            if any_failed {
                return Err(anyhow!("batch payment failed: {fail_reason}"));
            }
            if all_done {
                broadcast_to_confirmed_secs = started_at.elapsed().as_secs_f64();
                if broadcast_to_mempool_secs == 0.0 {
                    broadcast_to_mempool_secs = broadcast_to_confirmed_secs;
                }
                break;
            }

            if broadcast_to_mempool_secs == 0.0 && started_at.elapsed().as_secs_f64() > 5.0 {
                broadcast_to_mempool_secs = started_at.elapsed().as_secs_f64();
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }

        Ok(TxMetrics {
            tx_id,
            construction_secs,
            broadcast_to_mempool_secs,
            broadcast_to_confirmed_secs,
            fee_paid: 0,
            success: true,
            error: None,
        })
    }
}

impl Drop for PaymentProcessorDriver {
    fn drop(&mut self) {
        // Mutex::get_mut is safe in Drop because &mut self guarantees no
        // other references exist.
        if let Some(ref mut child) = *self.pp_daemon.get_mut().unwrap() {
            let _ = child.kill();
            let _ = child.try_wait();
        }
        if let Some(ref mut child) = *self.pr_daemon.get_mut().unwrap() {
            let _ = child.kill();
            let _ = child.try_wait();
        }
    }
}

#[async_trait]
impl WalletDriver for PaymentProcessorDriver {
    fn mode_name(&self) -> &str {
        "payment_processor"
    }

    async fn reset(&self) -> anyhow::Result<()> {
        // Stop daemons so in-memory file handles are released before wipe.
        self.stop_pp_daemon().await;
        self.stop_pr_daemon().await;

        // Wipe everything except the seed_words.txt file (preserved in
        // self.seed_words as well, but keeping the on-disk copy is a
        // safety net).
        let seed_path = shared::seed_words_path(&self.data_dir);
        let seed_backup = std::fs::read_to_string(&seed_path).ok();

        if self.data_dir.exists() {
            std::fs::remove_dir_all(&self.data_dir)?;
        }
        std::fs::create_dir_all(&self.data_dir)?;

        // Restore seed words file from memory backup.
        if let Some(ref words) = seed_backup {
            std::fs::write(&seed_path, words)?;
        }

        // The in-memory self.seed_words is unchanged, so subsequent
        // scan_* / create_wallet calls will recover the exact same wallet.
        Ok(())
    }

    async fn get_balance(&self) -> anyhow::Result<u64> {
        self.get_balance_from_pr().await
    }

    async fn get_tip_height(&self) -> anyhow::Result<u64> {
        shared::get_tip_height(&self.http_client, &self.base_node_url).await
    }

    async fn get_self_address(&self) -> anyhow::Result<String> {
        let words = self.seed_words.lock().unwrap();
        let mnemonic =
            SeedWords::from_str(&words).context("failed to parse seed words for address")?;
        let cipher_seed = CipherSeed::from_mnemonic(&mnemonic, None)
            .context("failed to reconstruct cipher seed for address")?;
        let wallet = WalletType::SeedWords(
            SeedWordsWallet::construct_new(cipher_seed)
                .map_err(|_| anyhow!("failed to construct wallet for address"))?,
        );
        let address = TariAddress::new_dual_address(
            wallet.get_public_view_key(),
            wallet.get_public_spend_key(),
            Network::Esmeralda,
            TariAddressFeatures::create_one_sided_only(),
            None,
        )
        .context("failed to construct self address")?;
        Ok(address.to_base58())
    }

    async fn scan_from_genesis(&self) -> anyhow::Result<ScanMetrics> {
        self.scan_from_height(0, 0).await
    }

    async fn scan_from_birthday(&self, height: u64) -> anyhow::Result<ScanMetrics> {
        let birthday_days = height / BLOCKS_PER_DAY;
        self.scan_from_height(height, birthday_days).await
    }

    async fn send_single(
        &self,
        to_address: &str,
        amount_ut: u64,
        _fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.ensure_pp_daemon_running().await?;
        self.api_send(to_address, amount_ut).await
    }

    async fn send_batch(
        &self,
        recipients: Vec<(String, u64)>,
        _fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.ensure_pp_daemon_running().await?;
        self.api_send_batch(recipients).await
    }

    async fn observe_funding(&self, expected_amount_ut: u64) -> anyhow::Result<TxMetrics> {
        let started_at = Instant::now();
        let deadline = started_at + Duration::from_secs(600);

        loop {
            if Instant::now() > deadline {
                return Err(anyhow!(
                    "incoming funding of at least {expected_amount_ut} uT was not observed within 600s"
                ));
            }

            let available = self.get_balance_from_pr().await?;
            if available >= expected_amount_ut {
                let elapsed = started_at.elapsed().as_secs_f64();
                return Ok(TxMetrics {
                    tx_id: "incoming-funding".to_string(),
                    construction_secs: 0.0,
                    broadcast_to_mempool_secs: elapsed,
                    broadcast_to_confirmed_secs: elapsed,
                    fee_paid: 0,
                    success: true,
                    error: None,
                });
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}
