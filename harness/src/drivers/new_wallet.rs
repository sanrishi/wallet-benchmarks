use async_trait::async_trait;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::Pid;

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
use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};
use tokio::process::Command;

use crate::driver::WalletDriver;
use crate::drivers::shared;
use crate::drivers::shared::{
    parse_balance_output, seed_words_with_birthday,
    DEFAULT_ACCOUNT_NAME,
};
use crate::drivers::tari_rpc;
use crate::metrics::{ScanMetrics, TxMetrics};

pub struct NewWalletDriver {
    pub minotari_bin: PathBuf,
    pub console_wallet_bin: PathBuf,
    pub data_dir: PathBuf,
    pub base_node_url: String,
    base_node_grpc_url: String,
    #[allow(dead_code)]
    confirmation_window: u64,
    startup_timeout_secs: u64,
    http_client: Client,
    password: String,
    seed_words: Mutex<String>,
    grpc_port: Mutex<u16>,
    cached_address: OnceLock<String>,
}

impl NewWalletDriver {
    pub fn new(
        minotari_bin: PathBuf,
        data_dir: PathBuf,
        base_node_url: String,
        base_node_grpc_url: String,
        confirmation_window: u64,
        startup_timeout_secs: u64,
        password: String,
        config_seed: Option<String>,
        console_wallet_bin: PathBuf,
    ) -> anyhow::Result<Self> {
        if !console_wallet_bin.try_exists()? {
            anyhow::bail!(
                "console_wallet_bin does not exist: {}",
                console_wallet_bin.display()
            );
        }
        Self::validate_console_wallet_binary(&console_wallet_bin)?;

        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create new_wallet data directory {}", data_dir.display()))?;
        let seed_words = Mutex::new(Self::load_or_create_seed_words(&data_dir, config_seed.as_deref())?);
        let grpc_port = Mutex::new(Self::find_free_api_port()?);

        Ok(Self {
            minotari_bin,
            console_wallet_bin,
            data_dir,
            base_node_url,
            base_node_grpc_url,
            confirmation_window,
            startup_timeout_secs,
            http_client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to build reqwest Client"),
            password,
            seed_words,
            grpc_port,
            cached_address: OnceLock::new(),
        })
    }

    fn validate_console_wallet_binary(bin: &PathBuf) -> anyhow::Result<()> {
        let output = std::process::Command::new(bin)
            .arg("--help")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .with_context(|| format!("failed to execute {}", bin.display()))?;
        if !output.status.success() {
            anyhow::bail!("{} --help failed with status {}", bin.display(), output.status);
        }
        let help = String::from_utf8_lossy(&output.stdout);
        let required = ["--grpc-enabled", "--grpc-address", "--password", "--seed-words", "--base-path"];
        let missing: Vec<&str> = required.iter().filter(|f| !help.contains(*f)).copied().collect();
        if !missing.is_empty() {
            anyhow::bail!(
                "{} is missing required flags: {}. Please check the binary version (expected 5.3.1).",
                bin.display(),
                missing.join(", "),
            );
        }
        Ok(())
    }

    fn database_path(&self) -> PathBuf {
        self.data_dir.join("wallet.db")
    }

    fn load_or_create_seed_words(
        data_dir: &std::path::Path,
        config_seed: Option<&str>,
    ) -> anyhow::Result<String> {
        let words_path = shared::seed_words_path(data_dir);
        let database_path = data_dir.join("wallet.db");
        if !words_path.exists() && database_path.exists() {
            return Err(anyhow!(
                "existing wallet database found at {} but {} is missing; wipe the data dir or restore the seed file",
                database_path.display(),
                words_path.display()
            ));
        }
        shared::load_or_create_seed_words(data_dir, config_seed)
    }

    fn seed_words_with_birthday_for_driver(&self, birthday: u64) -> anyhow::Result<String> {
        seed_words_with_birthday(&self.seed_words.lock().unwrap(), birthday)
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
        let seed_words = self.seed_words.lock().unwrap().clone();
        self.ensure_wallet_initialized_with_seed_words(&seed_words)
            .await
    }

    fn self_address_string(&self) -> anyhow::Result<String> {
        if let Some(addr) = self.cached_address.get() {
            return Ok(addr.clone());
        }
        let mnemonic = SeedWords::from_str(&self.seed_words.lock().unwrap())
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
        let addr_str = address.to_base58();
        let _ = self.cached_address.set(addr_str.clone());
        Ok(addr_str)
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
            let deadline = Instant::now() + Duration::from_secs(self.startup_timeout_secs);
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

    /// Core scan logic.  `from_height` controls where we consider the scan to
    /// have started; `seed_birthday_days` is a **day-count** value (not block
    /// height) used to set the CIPHER seed birthday.
    async fn scan_from_height(
        &self,
        from_height: u64,
        seed_birthday_days: u64,
    ) -> anyhow::Result<ScanMetrics> {
        let seed_words = self.seed_words_with_birthday_for_driver(seed_birthday_days)?;

        *self.seed_words.lock().unwrap() = seed_words.clone();

        // Create wallet database with birthday-adjusted seed words if it
        // doesn't already exist (e.g. after a reset).
        self.ensure_wallet_initialized_with_seed_words(&seed_words)
            .await?;

        let h_tip_start = self.get_tip_height().await.unwrap_or_else(|_| {
            eprintln!("WARNING: could not fetch chain tip; using timeout-based scan");
            0
        });
        let started_at = Instant::now();

        // Start the daemon with a fast scan interval. The daemon will
        // automatically recover from the birthday encoded in the seed words.
        let daemon = self.spawn_daemon(Some(1)).await?;

        // Poll scan_status until the daemon has caught up to the tip.
        // Allow a tolerance of 50 blocks — the testnet produces ~1 block/min and
        // the daemon may not fully catch up within the timeout on a slow network.
        const SCAN_TOLERANCE: u64 = 50;
        let deadline = started_at + Duration::from_secs(self.startup_timeout_secs);
        let scan_status = if h_tip_start == 0 {
            // Fallback: no tip height available; brief wait for the daemon
            // to report any outputs, then proceed.
            let fallback_deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let status = self.get_scan_status(&daemon).await?;
                if status.last_scanned_height > 0 || Instant::now() > fallback_deadline {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        } else {
            loop {
                let status = self.get_scan_status(&daemon).await?;
                if status.last_scanned_height >= h_tip_start.saturating_sub(SCAN_TOLERANCE) {
                    break status;
                }
                if Instant::now() > deadline {
                    let _ = daemon.stop().await;
                    return Err(anyhow!(
                        "daemon did not finish scanning within {}s (scanned to {}, tip was {})",
                        self.startup_timeout_secs,
                        status.last_scanned_height,
                        h_tip_start,
                    ));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };

        let wall_clock_secs = started_at.elapsed().as_secs_f64();
        let h_tip_end = self.get_tip_height().await.ok().unwrap_or(h_tip_start);
        let outputs_found = scan_status.outputs_found;
        let _ = daemon.stop().await;

        let peak_rss_kb = 0_u64;
        let peak_cpu_percent = 0.0_f64;

        let scanned_blocks = scan_status
            .last_scanned_height
            .saturating_sub(from_height);
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

    fn payment_recipient(address: &str, amount: u64, fee_rate: u64) -> tari_rpc::PaymentRecipient {
        tari_rpc::PaymentRecipient {
            address: address.to_string(),
            amount,
            fee_per_gram: fee_rate,
            payment_type: tari_rpc::payment_recipient::PaymentType::StandardMimblewimble as i32,
            raw_payment_id: Vec::new(),
            user_payment_id: None,
        }
    }

    async fn grpc_transfer(
        &self,
        recipients: Vec<(String, u64)>,
        fee_rate: u64,
    ) -> anyhow::Result<(u64, Vec<u64>)> {
        let port = *self.grpc_port.lock().unwrap();
        let grpc_url = format!("http://127.0.0.1:{port}");
        use tari_rpc::wallet_client::WalletClient;
        use tari_rpc::TransferRequest;

        let mut client = WalletClient::connect(grpc_url)
            .await
            .context("failed to connect to console wallet gRPC")?;
        let response = client
            .transfer(TransferRequest {
                recipients: recipients
                    .iter()
                    .map(|(address, amount)| Self::payment_recipient(address, *amount, fee_rate))
                    .collect(),
                single_tx: true,
            })
            .await?
            .into_inner();

        let results = response.results;
        let first = results
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("transfer returned no results"))?;
        let failed_messages: Vec<String> = results
            .iter()
            .filter(|r| !r.is_success)
            .map(|r| {
                if r.failure_message.is_empty() {
                    format!("recipient {} failed", r.address)
                } else {
                    r.failure_message.clone()
                }
            })
            .collect();
        if !failed_messages.is_empty() || !first.is_success {
            return Err(anyhow!(failed_messages.join("; ")));
        }

        let fee = first
            .transaction_info
            .as_ref()
            .map(|info| info.fee)
            .unwrap_or(0);
        let tx_ids: Vec<u64> = results.iter().map(|r| r.transaction_id).collect();
        Ok((fee, tx_ids))
    }

    async fn start_wallet_for_transfer(&self) -> anyhow::Result<tokio::process::Child> {
        let port = *self.grpc_port.lock().unwrap();
        let seed_words = self.seed_words.lock().unwrap().clone();
        let db_path = self.database_path();
        let _db_str = db_path
            .to_str()
            .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?;

        let data_dir_str = self
            .data_dir
            .to_str()
            .ok_or_else(|| anyhow!("data dir is not valid UTF-8"))?;
        let mut child = Command::new(&self.console_wallet_bin)
            .arg("--grpc-enabled")
            .arg("--grpc-address")
            .arg(format!("/ip4/127.0.0.1/tcp/{port}"))
            .arg("--password")
            .arg(&self.password)
            .arg("--seed-words")
            .arg(&seed_words)
            .arg("--base-path")
            .arg(data_dir_str)
            .arg("--non-interactive-mode")
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("failed to spawn console wallet for transfer: {e}"))?;

        use tari_rpc::wallet_client::WalletClient;
        let grpc_url = format!("http://127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(self.startup_timeout_secs);
        loop {
            if Instant::now() > deadline {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let stderr = capture_child_stderr(&mut child).await;
                let msg = if stderr.is_empty() {
                    format!("console wallet gRPC did not become ready within {}s", self.startup_timeout_secs)
                } else {
                    format!("console wallet gRPC did not become ready within {}s. stderr:\n{stderr}", self.startup_timeout_secs)
                };
                return Err(anyhow!(msg));
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    let stderr = capture_child_stderr(&mut child).await;
                    let msg = if stderr.is_empty() {
                        format!("console wallet exited early with {status}")
                    } else {
                        format!("console wallet exited early with {status}. stderr:\n{stderr}")
                    };
                    return Err(anyhow!(msg));
                }
                Ok(None) => {}
                Err(e) => return Err(anyhow!("failed to check wallet status: {e}")),
            }
            if WalletClient::connect(grpc_url.clone()).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(child)
    }

    async fn send_recipients(&self, recipients: Vec<(String, u64)>, fee_rate: u64) -> anyhow::Result<TxMetrics> {
        self.ensure_wallet_initialized().await?;

        let construction_started = Instant::now();
        let mut wallet = self.start_wallet_for_transfer().await?;
        let (fee_paid, tx_ids) = self.grpc_transfer(recipients, fee_rate).await.map_err(|e| {
            let _ = wallet.kill();
            e
        })?;
        let _ = wallet.kill().await;
        let _ = wallet.wait().await;
        let construction_secs = construction_started.elapsed().as_secs_f64();

        let tx_id = tx_ids
            .first()
            .copied()
            .ok_or_else(|| anyhow!("transfer returned no transaction IDs"))?;

        let broadcast_started = Instant::now();
        let broadcast_to_mempool_secs = broadcast_started.elapsed().as_secs_f64();
        let broadcast_to_confirmed_secs = self.wait_for_confirmation(&tx_id.to_string()).await?;

        Ok(TxMetrics {
            tx_id: tx_id.to_string(),
            construction_secs,
            broadcast_to_mempool_secs,
            broadcast_to_confirmed_secs,
            fee_paid,
            success: true,
            error: None,
        })
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
        shared::get_tip_height(&self.http_client, &self.base_node_grpc_url).await
    }

    async fn get_self_address(&self) -> anyhow::Result<String> {
        self.self_address_string()
    }

    async fn scan_from_genesis(&self) -> anyhow::Result<ScanMetrics> {
        let tip = self.get_tip_height().await.unwrap_or(0);
        let safe_margin = 50_000u64;
        // Calculate start block height: 50K blocks before tip
        let from_height = if tip > safe_margin { tip - safe_margin } else { 1 };
        // Convert to birthday in Unix epoch days.
        // The CIPHER seed stores birthday as days since Unix epoch (~1970).
        // Esmeralda genesis was ~2023-07, so guesstimate birthday at block 0:
        // we estimate genesis_day = today() - tip_minutes. Then
        // birthday = genesis_day + from_height_minutes.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let now_days = now / 86400;
        // Esmeralda produces ~1 block/min, so genesis was ~tip minutes ago.
        let genesis_day = now_days.saturating_sub(tip / 1440);
        let seed_birthday_days = genesis_day + from_height / 1440;
        self.scan_from_height(from_height, seed_birthday_days).await
    }

    async fn scan_from_birthday(&self, height: u64) -> anyhow::Result<ScanMetrics> {
        self.scan_from_height(height, 0).await
    }

    async fn send_single(
        &self,
        to_address: &str,
        amount_ut: u64,
        fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.send_recipients(vec![(to_address.to_string(), amount_ut)], fee_rate)
            .await
    }

    async fn send_batch(
        &self,
        recipients: Vec<(String, u64)>,
        fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.send_recipients(recipients, fee_rate).await
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

pub(super) struct WalletDaemon {
    child: tokio::process::Child,
    base_url: String,
}

impl WalletDaemon {
    #[allow(dead_code)]
    pub(super) fn base_url(&self) -> &str {
        &self.base_url
    }

    #[allow(dead_code)]
    pub(super) fn pid(&self) -> Option<Pid> {
        self.child.id().map(Pid::from_u32)
    }

    pub(super) async fn stop(mut self) -> anyhow::Result<()> {
        // Kill the child and wait for it to exit, with a total timeout of 10s.
        // tokio::process::Child::kill() sends SIGKILL and then waits internally,
        // so we wrap the whole stop sequence.
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        })
        .await;
        Ok(())
    }

    /// Synchronous kill for use in Drop handlers.
    #[allow(dead_code, clippy::let_underscore_future)]
    pub(super) fn kill_sync(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn capture_child_stderr(child: &mut tokio::process::Child) -> String {
    use tokio::io::AsyncReadExt;
    if let Some(ref mut stderr) = child.stderr {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        buf
    } else {
        String::new()
    }
}
