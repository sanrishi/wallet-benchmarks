use async_trait::async_trait;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
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
use tari_transaction_components::consensus::ConsensusConstantsBuilder;
use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};
use tari_transaction_components::key_manager::KeyManager;
use tari_transaction_components::offline_signing::models::PrepareOneSidedTransactionForSigningResult;
use tari_transaction_components::offline_signing::sign_locked_transaction;
use tari_utilities::ByteArray;
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
    base_node_grpc_url: String,
    #[allow(dead_code)]
    confirmation_window: u64,
    startup_timeout_secs: u64,
    http_client: Client,
    password: String,
    seed_words: Mutex<String>,
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
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create new_wallet data directory {}", data_dir.display()))?;
        let seed_words = Mutex::new(Self::load_or_create_seed_words(&data_dir, config_seed.as_deref())?);

        Ok(Self {
            minotari_bin,
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
            cached_address: OnceLock::new(),
        })
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

    /// Reconstruct the in-process `KeyManager` from the stored seed words.
    ///
    /// This mirrors the daemon's `AccountRow::get_key_manager` (same
    /// `KeyManager::new(WalletType::SeedWords(SeedWordsWallet::construct_new(cipher_seed)))`
    /// construction), so keys derived here match the account the daemon scans
    /// and drafts transactions for.
    fn key_manager(&self) -> anyhow::Result<KeyManager> {
        let mnemonic = SeedWords::from_str(&self.seed_words.lock().unwrap())
            .context("failed to parse stored seed words for new_wallet key manager")?;
        let cipher_seed = CipherSeed::from_mnemonic(&mnemonic, None)
            .context("failed to reconstruct cipher seed for new_wallet key manager")?;
        let wallet =
            WalletType::SeedWords(SeedWordsWallet::construct_new(cipher_seed).map_err(|_| {
                anyhow!("failed to construct seed-words wallet for new_wallet key manager")
            })?);
        KeyManager::new(wallet).context("failed to construct new_wallet key manager")
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

    /// Wait for the broadcast transaction to be mined by polling the base
    /// node's HTTP `/transactions` query endpoint, keyed by the kernel's
    /// excess signature (nonce + signature bytes).
    ///
    /// The daemon does not expose a broadcast/finalize endpoint, so it will
    /// never record our transaction in `completed_transactions`; the base node
    /// is the authoritative source of truth for mempool/mined status.
    async fn wait_for_confirmation(
        &self,
        tx_id: u64,
        excess_sig_nonce_hex: String,
        excess_sig_sig_hex: String,
    ) -> anyhow::Result<f64> {
        let started_at = Instant::now();
        let deadline = started_at + Duration::from_secs(600);

        loop {
            if Instant::now() > deadline {
                return Err(anyhow!(
                    "transaction {tx_id} was not confirmed within 600s (last status from base node)"
                ));
            }

            let url = format!(
                "{}/transactions?excess_sig_nonce={}&excess_sig_sig={}",
                self.base_node_url.trim_end_matches('/'),
                excess_sig_nonce_hex,
                excess_sig_sig_hex,
            );
            let response = self
                .http_client
                .get(&url)
                .send()
                .await
                .context("failed to query base node transaction status")?
                .error_for_status()
                .context("base node transaction status query returned an HTTP error")?;
            let body = response
                .text()
                .await
                .context("failed to read base node transaction status response")?;
            let status: TxQueryResponse = serde_json::from_str(&body)
                .with_context(|| format!("failed to parse base node transaction status response: {body}"))?;

            match status.location.as_str() {
                "Mined" => return Ok(started_at.elapsed().as_secs_f64()),
                "None" | "NotStored" => {
                    // Not seen yet; keep polling.
                }
                "InMempool" => {
                    // In the mempool; keep polling until mined.
                }
                other => {
                    return Err(anyhow!("transaction {tx_id} is in unexpected state {other:?}"));
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Send one or more one-sided payments.
    ///
    /// Flow (no old wallet, no `minotari_wallet` library):
    /// 1. Ask the `minotari daemon` REST API to scan and draft the
    ///    transaction (`create_unsigned_transaction`). The daemon only scans
    ///    the chain with the account's view key and selects inputs — it does
    ///    not sign.
    /// 2. Reconstruct the in-process `KeyManager` from the seed words and sign
    ///    the locked payload with `tari_transaction_components`.
    /// 3. Submit the signed transaction directly to the base node via its HTTP
    ///    JSON-RPC endpoint.
    /// 4. Poll the base node `/transactions` endpoint until the kernel is mined.
    async fn send_recipients(&self, recipients: Vec<(String, u64)>, _fee_rate: u64) -> anyhow::Result<TxMetrics> {
        self.ensure_wallet_initialized().await?;

        let daemon = self.spawn_daemon(Some(1)).await?;
        let result = self.send_with_daemon(&daemon, recipients).await;
        let _ = daemon.stop().await;
        result
    }

    async fn send_with_daemon(
        &self,
        daemon: &WalletDaemon,
        recipients: Vec<(String, u64)>,
    ) -> anyhow::Result<TxMetrics> {
        // 1. Draft via the daemon REST API. The daemon hardcodes its fee per
        //    gram (DEFAULT_FEE_PER_GRAM), so the harness `fee_rate` cannot be
        //    applied; the actual fee is read from the prepared payload.
        let construction_started = Instant::now();
        let request = serde_json::json!({
            "recipients": recipients.iter().map(|(address, amount)| serde_json::json!({
                "address": address,
                "amount": amount,
            })).collect::<Vec<_>>(),
            "seconds_to_lock_utxos": 3600,
        });
        let response = self
            .http_client
            .post(format!(
                "{}/accounts/{}/create_unsigned_transaction",
                daemon.base_url,
                DEFAULT_ACCOUNT_NAME
            ))
            .json(&request)
            .send()
            .await
            .context("failed to call daemon create_unsigned_transaction API")?
            .error_for_status()
            .context("daemon create_unsigned_transaction API returned an HTTP error")?;
        let response_body = response
            .text()
            .await
            .context("failed to read daemon create_unsigned_transaction response")?;
        let prepared: PrepareOneSidedTransactionForSigningResult = serde_json::from_str(&response_body)
            .with_context(|| {
                format!(
                    "failed to parse daemon create_unsigned_transaction response: {}",
                    &response_body[..response_body.len().min(1000)]
                )
            })?;
        let tx_id: u64 = prepared.tx_id.into();
        let fee_paid = prepared.info.fee.as_u64();

        // 2. Sign in-process.
        let key_manager = self.key_manager()?;
        let consensus_constants = ConsensusConstantsBuilder::new(Network::Esmeralda).build();
        let signed = sign_locked_transaction(&key_manager, consensus_constants, Network::Esmeralda, prepared)
            .map_err(|error| anyhow!("failed to sign locked transaction: {error}"))?;
        let transaction = signed.signed_transaction.transaction;
        let construction_secs = construction_started.elapsed().as_secs_f64();

        // 3. Submit to the base node.
        let kernel = transaction
            .body()
            .kernels()
            .first()
            .ok_or_else(|| anyhow!("signed transaction has no kernel"))?;
        let excess_sig_nonce_hex = hex::encode(kernel.excess_sig.get_compressed_public_nonce().as_bytes());
        let excess_sig_sig_hex = hex::encode(kernel.excess_sig.get_signature().as_bytes());

        let broadcast_started = Instant::now();
        let submission = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "submit_transaction",
            "params": { "transaction": &transaction },
        });
        let submit_response = self
            .http_client
            .post(format!("{}/json_rpc", self.base_node_url.trim_end_matches('/')))
            .json(&submission)
            .send()
            .await
            .context("failed to submit transaction to base node")?;
        let submit_status = submit_response.status();
        let submit_body = submit_response
            .text()
            .await
            .context("failed to read base node submit_transaction response")?;
        if !submit_status.is_success() {
            return Err(anyhow!(
                "base node submit_transaction returned HTTP {submit_status}: {submit_body}"
            ));
        }
        let submit_value: serde_json::Value = serde_json::from_str(&submit_body)
            .with_context(|| format!("failed to parse submit_transaction response: {submit_body}"))?;
        if let Some(error) = submit_value.get("error") {
            let message = error
                .as_str()
                .or_else(|| error.get("message").and_then(|message| message.as_str()))
                .unwrap_or("unknown JSON-RPC error");
            return Err(anyhow!("base node rejected transaction {tx_id}: {message}"));
        }
        let accepted = submit_value
            .get("result")
            .and_then(|result| result.get("accepted"))
            .and_then(|accepted| accepted.as_bool())
            .ok_or_else(|| anyhow!("submit_transaction response missing result.accepted: {submit_body}"))?;
        if !accepted {
            let reason = submit_value
                .get("result")
                .and_then(|result| result.get("rejection_reason"))
                .and_then(|reason| reason.as_str())
                .unwrap_or("unknown");
            return Err(anyhow!("transaction {tx_id} was rejected by the network: {reason}"));
        }
        let broadcast_to_mempool_secs = broadcast_started.elapsed().as_secs_f64();

        // 4. Wait for the kernel to be mined.
        let broadcast_to_confirmed_secs =
            self.wait_for_confirmation(tx_id, excess_sig_nonce_hex, excess_sig_sig_hex)
                .await?;

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
        let from_height = if tip > safe_margin { tip - safe_margin } else { 1 };
        // CIPHER seed birthday is days since Tari epoch (2023-01-01).
        // Esmeralda genesis block 0 was ~195 days after Tari epoch (July 2023).
        // So the birthday for a block at `h` is: genesis_birthday + h / blocks_per_day.
        // We use 1440 = 1 block/min as an approximation.
        // genesis_birthday is approximately 195 (July 2023).
        let genesis_birthday: u64 = 195;
        let seed_birthday_days = genesis_birthday + from_height / 1440;
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

/// Base node `/transactions` query response.
#[derive(Debug, Deserialize)]
struct TxQueryResponse {
    /// One of "None", "NotStored", "InMempool" or "Mined".
    location: String,
    #[allow(dead_code)]
    mined_height: Option<u64>,
    #[allow(dead_code)]
    mined_header_hash: Option<String>,
    #[allow(dead_code)]
    mined_timestamp: Option<String>,
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
