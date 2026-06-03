use async_trait::async_trait;
use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;

use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use anyhow::{anyhow, Context};
use reqwest::Client;
use serde::Deserialize;
use tari_common::configuration::Network;
use tari_common_types::seeds::{
    cipher_seed::CipherSeed,
    mnemonic::Mnemonic,
    seed_words::SeedWords,
};
use tari_common_types::tari_address::{TariAddress, TariAddressFeatures};
use tari_transaction_components::{
    consensus::ConsensusConstantsBuilder,
    key_manager::{
        wallet_types::{SeedWordsWallet, WalletType},
        KeyManager,
    },
    offline_signing::{
        models::{PrepareOneSidedTransactionForSigningResult, TransactionResult},
        sign_locked_transaction,
    },
};
use tempfile::NamedTempFile;
use tokio::process::Command;

use crate::driver::WalletDriver;
use crate::drivers::shared;
use crate::drivers::shared::{
    parse_balance_output, seed_words_with_birthday,
    DEFAULT_ACCOUNT_NAME,
};
use crate::metrics::{ScanMetrics, TxMetrics};

pub struct NewWalletDriver {
    pub minotari_bin: PathBuf,
    pub data_dir: PathBuf,
    pub base_node_url: String,
    confirmation_window: u64,
    http_client: Client,
    password: String,
    seed_words: String,
    key_manager: KeyManager,
}

impl NewWalletDriver {
    pub fn new(
        minotari_bin: PathBuf,
        data_dir: PathBuf,
        base_node_url: String,
        confirmation_window: u64,
        password: String,
    ) -> anyhow::Result<Self> {
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create {}", data_dir.display()))?;
        let seed_words = Self::load_or_create_seed_words(&data_dir)?;
        let key_manager = Self::build_key_manager(&seed_words)?;

        Ok(Self {
            minotari_bin,
            data_dir,
            base_node_url,
            confirmation_window,
            http_client: Client::new(),
            password,
            seed_words,
            key_manager,
        })
    }

    /// Construct a driver with externally-provided seed words (avoids
    /// re-creating them from the data directory).
    pub fn new_with_seed_words(
        minotari_bin: PathBuf,
        data_dir: PathBuf,
        base_node_url: String,
        confirmation_window: u64,
        password: String,
        seed_words: String,
    ) -> anyhow::Result<Self> {
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create {}", data_dir.display()))?;
        let key_manager = Self::build_key_manager(&seed_words)?;
        Ok(Self {
            minotari_bin,
            data_dir,
            base_node_url,
            confirmation_window,
            http_client: Client::new(),
            password,
            seed_words,
            key_manager,
        })
    }

    fn database_path(&self) -> PathBuf {
        self.data_dir.join("wallet.db")
    }

    fn load_or_create_seed_words(data_dir: &std::path::Path) -> anyhow::Result<String> {
        let words_path = shared::seed_words_path(data_dir);
        let database_path = data_dir.join("wallet.db");
        if !words_path.exists() && database_path.exists() {
            return Err(anyhow!(
                "existing wallet database found at {} but {} is missing; wipe the data dir or restore the seed file",
                database_path.display(),
                words_path.display()
            ));
        }
        shared::load_or_create_seed_words(data_dir)
    }

    fn seed_words_with_birthday_for_driver(&self, birthday: u64) -> anyhow::Result<String> {
        seed_words_with_birthday(&self.seed_words, birthday)
    }

    async fn ensure_wallet_initialized_with_seed_words(
        &self,
        seed_words: &str,
    ) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("failed to create {}", self.data_dir.display()))?;

        if self.database_path().exists() {
            return Ok(());
        }

        let database_path = self.database_path();
        let database_path = database_path
            .to_str()
            .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?;

        self.run_cli_command(&[
            "create",
            "--password",
            &self.password,
            "--database-path",
            database_path,
            "--account-name",
            DEFAULT_ACCOUNT_NAME,
            "--seed-words",
            seed_words,
        ])
        .await?;

        Ok(())
    }

    pub async fn run_cli_command(&self, args: &[&str]) -> anyhow::Result<String> {
        let output = Command::new(&self.minotari_bin)
            .args(args)
            .output()
            .await
            .with_context(|| {
                format!(
                    "failed to execute minotari CLI at {}",
                    self.minotari_bin.display()
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("minotari CLI command failed: {}", stderr.trim()));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn ensure_wallet_initialized(&self) -> anyhow::Result<()> {
        self.ensure_wallet_initialized_with_seed_words(&self.seed_words)
            .await
    }

    fn build_key_manager(seed_words: &str) -> anyhow::Result<KeyManager> {
        let mnemonic = SeedWords::from_str(seed_words)
            .context("failed to parse stored seed words for new_wallet")?;
        let cipher_seed = CipherSeed::from_mnemonic(&mnemonic, None)
            .context("failed to reconstruct cipher seed for new_wallet")?;
        let wallet = WalletType::SeedWords(
            SeedWordsWallet::construct_new(cipher_seed)
                .map_err(|_| anyhow!("failed to construct seed-words wallet for new_wallet"))?,
        );
        KeyManager::new(wallet).context("failed to build key manager for new_wallet")
    }

    fn key_manager(&self) -> &KeyManager {
        &self.key_manager
    }

    fn self_address_string(&self) -> anyhow::Result<String> {
        let mnemonic = SeedWords::from_str(&self.seed_words)
            .context("failed to parse stored seed words for new_wallet address")?;
        let cipher_seed = CipherSeed::from_mnemonic(&mnemonic, None)
            .context("failed to reconstruct cipher seed for new_wallet address")?;
        let wallet =
            WalletType::SeedWords(SeedWordsWallet::construct_new(cipher_seed).map_err(|_| {
                anyhow!("failed to construct seed-words wallet for new_wallet address")
            })?);
        let address = TariAddress::new_dual_address(
            wallet.get_public_view_key(),
            wallet.get_public_spend_key(),
            Network::Esmeralda,
            TariAddressFeatures::create_one_sided_only(),
            None,
        )
        .context("failed to construct new_wallet self address")?;
        Ok(address.to_base58())
    }

    async fn submit_signed_transaction(
        &self,
        transaction: &serde_json::Value,
    ) -> anyhow::Result<BroadcastResponse> {
        let url = format!("{}/json_rpc", self.base_node_url.trim_end_matches('/'));
        let response = self
            .http_client
            .post(url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": "1",
                "method": "submit_transaction",
                "params": { "transaction": transaction }
            }))
            .send()
            .await
            .context("failed to submit transaction to base node")?
            .error_for_status()
            .context("base node returned an HTTP error on submit_transaction")?;

        let rpc: JsonRpcResponse<BroadcastResponse> = response
            .json()
            .await
            .context("failed to parse submit_transaction response")?;

        match (rpc.result, rpc.error) {
            (Some(result), _) => Ok(result),
            (None, Some(error)) => Err(anyhow!("submit_transaction failed: {error}")),
            (None, None) => Err(anyhow!("submit_transaction returned no result")),
        }
    }

    /// Find a free ephemeral port.
    ///
    /// # TOCTOU note
    ///
    /// The kernel guarantees that a bound `TcpListener` will not be handed
    /// out to another `bind()` caller, so the returned port will be free
    /// at the moment of the call.  A brief race still exists between
    /// `drop(listener)` and the child process calling `bind()`, but on
    /// Windows and Linux the kernel's TIME_WAIT / TCP-TW-reuse behaviour
    /// makes actual collisions vanishingly rare in practice.  If the
    /// child does fail with EADDRINUSE, the caller should retry with a
    /// fresh port.
    fn find_free_api_port() -> anyhow::Result<u16> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("failed to bind an ephemeral port for minotari daemon")?;
        let port = listener
            .local_addr()
            .context("failed to read ephemeral port for minotari daemon")?
            .port();
        drop(listener);
        Ok(port)
    }

    /// Spawn the `minotari daemon` subprocess and wait for its API to become
    /// reachable.
    ///
    /// Retries with a fresh port if the first attempt fails (TOCTOU
    /// mitigation).
    pub(super) async fn spawn_daemon(&self, scan_interval_secs: Option<u64>) -> anyhow::Result<WalletDaemon> {
        self.ensure_wallet_initialized().await?;

        for attempt in 0..5 {
            let port = Self::find_free_api_port()?;
            let database_path = self.database_path();
            let database_path = database_path
                .to_str()
                .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?;
            let mut args = vec![
                "daemon".to_string(),
                "--password".to_string(),
                self.password.clone(),
                "--database-path".to_string(),
                database_path.to_string(),
                "--api-port".to_string(),
                port.to_string(),
                "--base-url".to_string(),
                self.base_node_url.clone(),
            ];
            if let Some(interval) = scan_interval_secs {
                args.push("--scan-interval-secs".to_string());
                args.push(interval.to_string());
            }
            let child = Command::new(&self.minotari_bin)
                .args(&args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| {
                    format!(
                        "failed to spawn minotari daemon at {}",
                        self.minotari_bin.display()
                    )
                })?;
            let daemon = WalletDaemon {
                child,
                base_url: format!("http://127.0.0.1:{port}"),
            };
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut ready = false;
            while Instant::now() <= deadline {
                if self
                    .http_client
                    .get(format!("{}/version", daemon.base_url))
                    .send()
                    .await
                    .and_then(|response| response.error_for_status())
                    .is_ok()
                {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            if ready {
                return Ok(daemon);
            }
            // Port may have collided; kill the child and retry with a new port.
            let _ = daemon.stop().await;
            if attempt < 4 {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }

        Err(anyhow!(
            "minotari daemon did not become ready after 5 port-retry attempts"
        ))
    }

    async fn get_scan_status(&self, daemon: &WalletDaemon) -> anyhow::Result<ScanStatusResponse> {
        self.http_client
            .get(format!(
                "{}/accounts/{}/scan_status",
                daemon.base_url, DEFAULT_ACCOUNT_NAME
            ))
            .send()
            .await
            .context("failed to query minotari scan_status API")?
            .error_for_status()
            .context("minotari scan_status API returned an HTTP error")?
            .json()
            .await
            .context("failed to parse minotari scan_status response")
    }

    async fn get_completed_transaction(
        &self,
        daemon: &WalletDaemon,
        tx_id: &str,
    ) -> anyhow::Result<Option<CompletedTransactionResponse>> {
        let transactions: Vec<CompletedTransactionResponse> = self
            .http_client
            .get(format!(
                "{}/accounts/{}/completed_transactions?limit=1000&offset=0",
                daemon.base_url, DEFAULT_ACCOUNT_NAME
            ))
            .send()
            .await
            .context("failed to query minotari completed_transactions API")?
            .error_for_status()
            .context("minotari completed_transactions API returned an HTTP error")?
            .json()
            .await
            .context("failed to parse minotari completed_transactions response")?;
        Ok(transactions
            .into_iter()
            .find(|transaction| Self::tx_id_matches(&transaction.id, tx_id)))
    }

    fn tx_id_matches(value: &serde_json::Value, expected: &str) -> bool {
        if let Some(id) = value.as_u64() {
            return expected.parse::<u64>().ok() == Some(id);
        }
        if let Some(id) = value.as_str() {
            return id == expected;
        }
        value
            .as_object()
            .and_then(|object| object.values().next())
            .and_then(|value| value.as_u64())
            .zip(expected.parse::<u64>().ok())
            .map(|(actual, expected)| actual == expected)
            .unwrap_or(false)
    }

    /// Core scan logic.  `from_height` controls where `--rescan-from-height`
    /// starts; `seed_birthday_days` is a **day-count** value (not block
    /// height) used to set the CIPHER seed birthday.
    async fn scan_from_height(
        &self,
        from_height: u64,
        seed_birthday_days: u64,
    ) -> anyhow::Result<ScanMetrics> {
        let seed_words = self.seed_words_with_birthday_for_driver(seed_birthday_days)?;

        // Ensure the birthday encoded in the seed words takes effect by
        // recreating the wallet if the database already exists with a
        // different (original) birthday.
        if self.database_path().exists() {
            std::fs::remove_file(self.database_path()).with_context(|| {
                format!(
                    "failed to remove wallet database at {}",
                    self.database_path().display()
                )
            })?;
        }
        self.ensure_wallet_initialized_with_seed_words(&seed_words)
            .await?;

        let database_path = self.database_path();
        let database_path = database_path
            .to_str()
            .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?;
        let from_height_string = from_height.to_string();
        let h_tip_start = self.get_tip_height().await?;
        let started_at = Instant::now();

        // Spawn the re-scan CLI process directly so we can track its PID
        let mut child = Command::new(&self.minotari_bin)
            .arg("re-scan")
            .arg("--database-path")
            .arg(database_path)
            .arg("--password")
            .arg(&self.password)
            .arg("--base-url")
            .arg(self.base_node_url.as_str())
            .arg("--account-name")
            .arg(DEFAULT_ACCOUNT_NAME)
            .arg("--rescan-from-height")
            .arg(&from_height_string)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!("failed to spawn re-scan at {}", self.minotari_bin.display())
            })?;

        let cli_pid = child.id().map(Pid::from_u32);
        let mut peak_rss_kb = 0_u64;
        let mut peak_cpu_percent = 0.0_f64;
        let mut system = System::new_all();
        let deadline = started_at + Duration::from_secs(1800);

        let cli_output = loop {
            match child.try_wait() {
                Ok(Some(_)) => break child.wait_with_output().await?,
                Ok(None) => {
                    if Instant::now() > deadline {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        return Err(anyhow!("re-scan did not complete within 1800s"));
                    }
                    if let Some(pid) = cli_pid {
                        system.refresh_processes_specifics(
                            ProcessesToUpdate::Some(&[pid]),
                            false,
                            ProcessRefreshKind::nothing().with_memory().with_cpu(),
                        );
                        if let Some(process) = system.process(pid) {
                            peak_rss_kb = peak_rss_kb.max(process.memory());
                            peak_cpu_percent = peak_cpu_percent.max(process.cpu_usage() as f64);
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => return Err(anyhow!("failed to poll re-scan process: {e}")),
            }
        };

        if !cli_output.status.success() {
            let stderr = String::from_utf8_lossy(&cli_output.stderr);
            return Err(anyhow!("re-scan failed: {}", stderr.trim()));
        }

        let wall_clock_secs = started_at.elapsed().as_secs_f64();
        let h_tip_end = self.get_tip_height().await?;
        let daemon = self.spawn_daemon(None).await?;

        // Track the daemon process briefly while querying scan status
        if let Some(daemon_pid) = daemon.pid() {
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[daemon_pid]),
                false,
                ProcessRefreshKind::nothing().with_memory().with_cpu(),
            );
            if let Some(process) = system.process(daemon_pid) {
                peak_rss_kb = peak_rss_kb.max(process.memory());
                peak_cpu_percent = peak_cpu_percent.max(process.cpu_usage() as f64);
            }
        }

        let scan_status = self.get_scan_status(&daemon).await?;
        let outputs_found = scan_status.outputs_found;
        let _ = daemon.stop().await;
        let scanned_tip_height = scan_status.last_scanned_height;
        let scanned_blocks = scanned_tip_height.saturating_sub(from_height);
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

    async fn wait_for_confirmation(&self, tx_id: &str) -> anyhow::Result<f64> {
        let started_at = Instant::now();
        let deadline = started_at + Duration::from_secs(600);
        let daemon = self.spawn_daemon(Some(1)).await?;

        loop {
            if Instant::now() > deadline {
                let _ = daemon.stop().await;
                return Err(anyhow!("transaction {tx_id} was not confirmed within 600s"));
            }

            if let Some(transaction) = self.get_completed_transaction(&daemon, tx_id).await? {
                match transaction.status.as_str() {
                    "mined_confirmed" => {
                        let elapsed = started_at.elapsed().as_secs_f64();
                        let _ = daemon.stop().await;
                        return Ok(elapsed);
                    }
                    "canceled" => {
                        let _ = daemon.stop().await;
                        return Err(anyhow!("transaction was canceled"));
                    }
                    "rejected" => {
                        let _ = daemon.stop().await;
                        return Err(anyhow!(transaction
                            .last_rejected_reason
                            .unwrap_or_else(|| "transaction was rejected".to_string())));
                    }
                    _ => {}
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn send_recipients(&self, recipients: Vec<(String, u64)>) -> anyhow::Result<TxMetrics> {
        self.ensure_wallet_initialized().await?;

        let database_path = self.database_path();
        let database_path = database_path
            .to_str()
            .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?;
        let output_file = NamedTempFile::new_in(&self.data_dir)
            .context("failed to create temporary unsigned transaction file")?;
        let output_file_str = output_file
            .path()
            .to_str()
            .ok_or_else(|| anyhow!("unsigned transaction path is not valid UTF-8"))?;
        let recipient_specs = recipients
            .iter()
            .map(|(address, amount)| format!("{address}::{amount}"))
            .collect::<Vec<_>>();

        let construction_started = Instant::now();
        let mut args = vec![
            "create-unsigned-transaction".to_string(),
            "--database-path".to_string(),
            database_path.to_string(),
            "--password".to_string(),
            self.password.clone(),
            "--account-name".to_string(),
            DEFAULT_ACCOUNT_NAME.to_string(),
            "--confirmation-window".to_string(),
            self.confirmation_window.max(1).to_string(),
        ];
        for recipient in &recipient_specs {
            args.push("--recipient".to_string());
            args.push(recipient.clone());
        }
        args.push("--output-file".to_string());
        args.push(output_file_str.to_string());
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        self.run_cli_command(&arg_refs).await?;

        let unsigned_json = std::fs::read_to_string(output_file.path())
            .with_context(|| format!("failed to read {}", output_file.path().display()))?;
        let unsigned_tx = PrepareOneSidedTransactionForSigningResult::from_json(&unsigned_json)
            .context("failed to parse unsigned transaction JSON")?;

        let key_manager = self.key_manager();
        let signed = sign_locked_transaction(
            key_manager,
            ConsensusConstantsBuilder::new(Network::Esmeralda).build(),
            Network::Esmeralda,
            unsigned_tx,
        )
        .context("failed to offline-sign locked transaction")?;
        let construction_secs = construction_started.elapsed().as_secs_f64();

        let tx_id = signed.signed_transaction.tx_id.to_string();
        let fee_paid = signed
            .signed_transaction
            .transaction
            .body()
            .kernels()
            .iter()
            .map(|kernel| kernel.fee.as_u64())
            .sum();
        let transaction_value = serde_json::to_value(&signed.signed_transaction.transaction)
            .context("failed to serialize signed transaction for HTTP submit")?;

        let broadcast_started = Instant::now();
        let submit_result = self.submit_signed_transaction(&transaction_value).await;
        let broadcast_to_mempool_secs = broadcast_started.elapsed().as_secs_f64();

        match submit_result {
            Ok(result) if result.accepted => Ok(TxMetrics {
                tx_id: tx_id.clone(),
                construction_secs,
                broadcast_to_mempool_secs,
                broadcast_to_confirmed_secs: self.wait_for_confirmation(&tx_id).await?,
                fee_paid,
                success: true,
                error: None,
            }),
            Ok(result) => Err(anyhow!(
                "transaction rejected by base node: {}",
                result.rejection_reason
            )),
            Err(error) => Err(error),
        }
    }
}

#[async_trait]
impl WalletDriver for NewWalletDriver {
    fn mode_name(&self) -> &str {
        "new_wallet"
    }

    async fn reset(&self) -> anyhow::Result<()> {
        if self.data_dir.exists() {
            std::fs::remove_dir_all(&self.data_dir)?;
        }
        std::fs::create_dir_all(&self.data_dir)?;
        Ok(())
    }

    async fn get_balance(&self) -> anyhow::Result<u64> {
        self.ensure_wallet_initialized().await?;
        let database_path = self.database_path();
        let database_path = database_path
            .to_str()
            .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?;

        let stdout = self
            .run_cli_command(&[
                "balance",
                "--database-path",
                database_path,
                "--account-name",
                DEFAULT_ACCOUNT_NAME,
            ])
            .await?;

        parse_balance_output(&stdout)
    }

    async fn get_tip_height(&self) -> anyhow::Result<u64> {
        shared::get_tip_height(&self.http_client, &self.base_node_url).await
    }

    async fn get_self_address(&self) -> anyhow::Result<String> {
        self.self_address_string()
    }

    async fn scan_from_genesis(&self) -> anyhow::Result<ScanMetrics> {
        // Seed birthday = 0 (genesis), rescan-from-height = 0
        self.scan_from_height(0, 0).await
    }

    async fn scan_from_birthday(&self, height: u64) -> anyhow::Result<ScanMetrics> {
        self.scan_from_height(height, 0).await
    }

    async fn send_single(
        &self,
        to_address: &str,
        amount_ut: u64,
        _fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.send_recipients(vec![(to_address.to_string(), amount_ut)])
            .await
    }

    async fn send_batch(
        &self,
        recipients: Vec<(String, u64)>,
        _fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.send_recipients(recipients).await
    }

    async fn observe_funding(&self, expected_amount_ut: u64) -> anyhow::Result<TxMetrics> {
        let started_at = Instant::now();
        let deadline = started_at + Duration::from_secs(600);
        let daemon = self.spawn_daemon(Some(1)).await?;

        loop {
            if Instant::now() > deadline {
                let _ = daemon.stop().await;
                return Err(anyhow!(
                    "incoming funding of at least {expected_amount_ut} uT was not observed within 600s"
                ));
            }

            let available = self.get_balance().await?;
            if available >= expected_amount_ut {
                let elapsed = started_at.elapsed().as_secs_f64();
                let _ = daemon.stop().await;
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

#[derive(Debug, Deserialize)]
struct ScanStatusResponse {
    last_scanned_height: u64,
    /// Some daemon versions report discovered outputs in scan status.
    #[serde(default)]
    outputs_found: u64,
}

#[derive(Debug, Deserialize)]
struct CompletedTransactionResponse {
    id: serde_json::Value,
    status: String,
    last_rejected_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BroadcastResponse {
    accepted: bool,
    #[serde(default)]
    rejection_reason: String,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<String>,
}

pub(super) struct WalletDaemon {
    child: tokio::process::Child,
    base_url: String,
}

impl WalletDaemon {
    pub(super) fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(super) fn pid(&self) -> Option<Pid> {
        self.child.id().map(Pid::from_u32)
    }

    pub(super) async fn stop(mut self) -> anyhow::Result<()> {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        Ok(())
    }

    /// Synchronous kill for use in Drop handlers.
    pub(super) fn kill_sync(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
