use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use tokio::process::{Child, Command};

use crate::driver::WalletDriver;
use crate::drivers::new_wallet::NewWalletDriver;
use crate::drivers::shared;
use crate::drivers::shared::derive_wallet_keys;
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
struct HealthVersion {
    #[allow(dead_code)]
    version: String,
}

/// Driver for Mode 3 (payment_processor).
///
/// Spawns the actual `minotari_payment_processor` microservice binary as a
/// long-running daemon and drives it through its REST API for payment
/// operations.  Wallet-level operations (scan, balance, address) are delegated
/// to a [`NewWalletDriver`] inner instance (CLI-based).
pub struct PaymentProcessorDriver {
    inner: NewWalletDriver,
    pp_bin: PathBuf,
    data_dir: PathBuf,
    minotari_bin_path: PathBuf,
    password: String,
    base_node_url: String,
    confirmation_window: u64,
    http_client: Client,
    seed_words: String,
    daemon: Option<Child>,
    api_url: Option<String>,
    api_port: u16,
}

impl PaymentProcessorDriver {
    pub fn new(
        pp_bin: PathBuf,
        data_dir: PathBuf,
        minotari_bin: PathBuf,
        base_node_url: String,
        confirmation_window: u64,
        password: String,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create {}", data_dir.display()))?;

        let seed_words = Self::load_or_create_seed_words(&data_dir)?;

        let minotari_bin_path = minotari_bin.clone();
        let inner = NewWalletDriver::new_with_seed_words(
            minotari_bin,
            data_dir.clone(),
            base_node_url.clone(),
            confirmation_window,
            password.clone(),
            seed_words.clone(),
        )?;

        let pp_port = Self::find_free_port()?;

        Ok(Self {
            inner,
            pp_bin,
            data_dir,
            minotari_bin_path,
            password,
            base_node_url,
            confirmation_window,
            http_client: Client::new(),
            seed_words,
            daemon: None,
            api_url: None,
            api_port: pp_port,
        })
    }

    fn load_or_create_seed_words(data_dir: &std::path::Path) -> anyhow::Result<String> {
        shared::load_or_create_seed_words(data_dir)
    }

    fn find_free_port() -> anyhow::Result<u16> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        Ok(listener.local_addr()?.port())
    }

    /// Start the `minotari_payment_processor` daemon with proper env config.
    pub async fn start_daemon(&mut self) -> anyhow::Result<()> {
        let port = self.api_port;
        let api_url = format!("http://127.0.0.1:{port}");
        let db_path = self.data_dir.join("payments.db");
        let db_dir = self.data_dir.join("data");
        std::fs::create_dir_all(&db_dir)?;

        let db_url = format!("sqlite:{}", db_path.display());
        let (view_key_hex, spend_key_hex) = derive_wallet_keys(&self.seed_words)
            .context("failed to derive payment processor account keys")?;

        let mut child = Command::new(&self.pp_bin)
            .env("DATABASE_URL", &db_url)
            .env("TARI_NETWORK", "Esmeralda")
            .env("BASE_NODE", &self.base_node_url)
            .env("PAYMENT_RECEIVER", "http://127.0.0.1:1")
            .env("CONSOLE_WALLET_PATH", &self.minotari_bin_path)
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
                    "failed to spawn minotari_payment_processor at {}",
                    self.pp_bin.display()
                )
            })?;

        let deadline = Instant::now() + Duration::from_secs(60);
        let health_url = format!("{api_url}/health/version");
        loop {
            if Instant::now() > deadline {
                let _ = child.try_wait();
                return Err(anyhow!(
                    "minotari_payment_processor did not become ready within 60s at {health_url}"
                ));
            }
            if self.http_client.get(&health_url).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        self.daemon = Some(child);
        self.api_url = Some(api_url);
        Ok(())
    }

    pub async fn stop_daemon(&mut self) {
        if let Some(ref mut child) = self.daemon {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.daemon = None;
        self.api_url = None;
    }

    /// Send a single payment via the payment processor REST API.
    async fn api_send(&self, to_address: &str, amount_ut: u64) -> anyhow::Result<TxMetrics> {
        let api_url = self
            .api_url
            .as_ref()
            .ok_or_else(|| anyhow!("payment processor daemon is not running"))?;
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
            .context("payment processor API request failed")?;

        let construction_secs = started_at.elapsed().as_secs_f64();

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "payment processor rejected payment (HTTP {status}): {body}"
            ));
        }

        let payment: PaymentResponse = resp
            .json()
            .await
            .context("failed to parse payment processor response")?;

        // Poll for confirmation
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

    /// Send a batch payment via the payment processor REST API.
    async fn api_send_batch(&self, recipients: Vec<(String, u64)>) -> anyhow::Result<TxMetrics> {
        let api_url = self
            .api_url
            .as_ref()
            .ok_or_else(|| anyhow!("payment processor daemon is not running"))?;
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
            .context("payment processor batch API request failed")?;

        let construction_secs = started_at.elapsed().as_secs_f64();

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "payment processor rejected batch (HTTP {status}): {body}"
            ));
        }

        let batch: BulkPaymentResponse = resp
            .json()
            .await
            .context("failed to parse batch response")?;

        // Poll until all payments in the batch are confirmed
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
        if let Some(ref mut child) = self.daemon {
            let _ = child.kill();
            let _ = child.wait();
        }
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
        _amount_ut: u64,
        _fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        // The daemon must be running before we can use the API.
        // We cannot start it from &self (immutable ref), so if the caller
        // did not call start_daemon first, this will error.
        self.api_send(to_address, _amount_ut).await
    }

    async fn send_batch(
        &self,
        recipients: Vec<(String, u64)>,
        _fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.api_send_batch(recipients).await
    }

    async fn observe_funding(&self, expected_amount_ut: u64) -> anyhow::Result<TxMetrics> {
        self.inner.observe_funding(expected_amount_ut).await
    }
}
