use async_trait::async_trait;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use reqwest::Client;
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use tari_common::configuration::Network;
use tari_common_types::seeds::{
    cipher_seed::CipherSeed,
    mnemonic::{Mnemonic, MnemonicLanguage},
    seed_words::SeedWords,
};
use tari_transaction_components::{
    consensus::ConsensusConstantsBuilder,
    key_manager::{KeyManager, wallet_types::{SeedWordsWallet, WalletType}},
    offline_signing::{
        models::{PrepareOneSidedTransactionForSigningResult, TransactionResult},
        sign_locked_transaction,
    },
};

use crate::driver::WalletDriver;
use crate::metrics::{ScanMetrics, TxMetrics};

const DEFAULT_WALLET_PASSWORD: &str = "benchmark_mode2_password_32_chars";
const DEFAULT_ACCOUNT_NAME: &str = "default";

pub struct NewWalletDriver {
    pub minotari_bin: PathBuf,
    pub data_dir: PathBuf,
    pub base_node_url: String,
    password: String,
    seed_words: String,
}

impl NewWalletDriver {
    pub fn new(minotari_bin: PathBuf, data_dir: PathBuf, base_node_url: String) -> anyhow::Result<Self> {
        let seed_words = CipherSeed::random()
            .to_mnemonic(MnemonicLanguage::English, None)
            .context("failed to generate mnemonic seed words for new_wallet")?
            .join(" ")
            .reveal()
            .to_string();

        Ok(Self {
            minotari_bin,
            data_dir,
            base_node_url,
            password: DEFAULT_WALLET_PASSWORD.to_string(),
            seed_words,
        })
    }

    fn database_path(&self) -> PathBuf {
        self.data_dir.join("wallet.db")
    }

    fn next_temp_file(&self, stem: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        self.data_dir.join(format!("{stem}_{nanos}.json"))
    }

    fn run_cli_command(&self, args: &[&str]) -> anyhow::Result<String> {
        let output = Command::new(&self.minotari_bin)
            .args(args)
            .output()
            .with_context(|| {
                format!(
                    "failed to execute minotari CLI at {}",
                    self.minotari_bin.display()
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "minotari CLI command failed: {}",
                stderr.trim()
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn parse_balance_output(stdout: &str) -> anyhow::Result<u64> {
        let line = stdout
            .lines()
            .find(|line| line.contains("Balance at height"))
            .ok_or_else(|| anyhow!("balance output did not contain a balance line"))?;

        let amount = line
            .split(':')
            .next_back()
            .map(str::trim)
            .ok_or_else(|| anyhow!("balance output did not contain a parsable amount"))?;

        if let Some(value) = amount
            .strip_suffix("µT")
            .or_else(|| amount.strip_suffix("ÂµT"))
            .map(str::trim)
        {
            let value = value.replace(',', "");
            return value
                .parse::<u64>()
                .with_context(|| format!("failed to parse microTari balance from '{value}'"));
        }

        if let Some(value) = amount.strip_suffix('T').map(str::trim) {
            let value = value.replace(',', "");
            let tari = value
                .parse::<f64>()
                .with_context(|| format!("failed to parse Tari balance from '{value}'"))?;
            return Ok((tari * 1_000_000.0).round() as u64);
        }

        Err(anyhow!("unsupported balance output format: {amount}"))
    }

    fn ensure_wallet_initialized(&self) -> anyhow::Result<()> {
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
            &self.seed_words,
        ])?;

        Ok(())
    }

    fn key_manager(&self) -> anyhow::Result<KeyManager> {
        let mnemonic = SeedWords::from_str(&self.seed_words)
            .context("failed to parse stored seed words for new_wallet")?;
        let cipher_seed = CipherSeed::from_mnemonic(&mnemonic, None)
            .context("failed to reconstruct cipher seed for new_wallet")?;
        let wallet = WalletType::SeedWords(
            SeedWordsWallet::construct_new(cipher_seed)
                .map_err(|_| anyhow!("failed to construct seed-words wallet for new_wallet"))?,
        );
        KeyManager::new(wallet).context("failed to build key manager for new_wallet")
    }

    async fn submit_signed_transaction(
        &self,
        transaction: &serde_json::Value,
    ) -> anyhow::Result<BroadcastResponse> {
        let client = Client::new();
        let url = format!("{}/json_rpc", self.base_node_url.trim_end_matches('/'));
        let response = client
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

    fn get_unspent_output_count(&self) -> anyhow::Result<u64> {
        let connection = Connection::open(self.database_path())
            .context("failed to open new_wallet database for output counting")?;
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM outputs WHERE deleted_at IS NULL AND status = 'UNSPENT'",
                [],
                |row| row.get(0),
            )
            .context("failed to query unspent outputs from new_wallet database")?;
        Ok(count.max(0) as u64)
    }

    fn get_scanned_tip_height(&self) -> anyhow::Result<u64> {
        let connection = Connection::open(self.database_path())
            .context("failed to open new_wallet database for tip height")?;
        let height = connection
            .query_row("SELECT MAX(height) FROM scanned_tip_blocks", [], |row| row.get::<_, Option<u64>>(0))
            .optional()
            .context("failed to query scanned_tip_blocks from new_wallet database")?
            .flatten()
            .unwrap_or(0);
        Ok(height)
    }

    fn scan_from_height(&self, from_height: u64) -> anyhow::Result<ScanMetrics> {
        self.ensure_wallet_initialized()?;

        let database_path = self.database_path();
        let database_path = database_path
            .to_str()
            .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?;
        let base_node_url = self.base_node_url.as_str();
        let from_height_string = from_height.to_string();
        let h_tip_start = self.get_tip_height_blocking()?;
        let started_at = Instant::now();

        self.run_cli_command(&[
            "re-scan",
            "--database-path",
            database_path,
            "--password",
            &self.password,
            "--base-url",
            base_node_url,
            "--account-name",
            DEFAULT_ACCOUNT_NAME,
            "--rescan-from-height",
            &from_height_string,
        ])?;

        let wall_clock_secs = started_at.elapsed().as_secs_f64();
        let h_tip_end = self.get_tip_height_blocking()?;
        let scanned_tip_height = self.get_scanned_tip_height()?;
        let outputs_found = self.get_unspent_output_count()?;
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
            peak_rss_kb: 0,
            peak_cpu_percent: 0.0,
        })
    }

    fn get_tip_height_blocking(&self) -> anyhow::Result<u64> {
        let url = format!("{}/get_tip_info", self.base_node_url.trim_end_matches('/'));
        let response = reqwest::blocking::get(url)
            .context("failed to query get_tip_info with blocking client")?
            .error_for_status()
            .context("base node returned an HTTP error on blocking get_tip_info")?;
        let tip: TipInfoResponse = response
            .json()
            .context("failed to parse blocking get_tip_info response")?;
        Ok(tip.metadata.best_block_height)
    }
}

#[async_trait]
impl WalletDriver for NewWalletDriver {
    fn mode_name(&self) -> &str { "new_wallet" }

    async fn reset(&self) -> anyhow::Result<()> {
        if self.data_dir.exists() {
            std::fs::remove_dir_all(&self.data_dir)?;
        }
        std::fs::create_dir_all(&self.data_dir)?;
        Ok(())
    }

    async fn get_balance(&self) -> anyhow::Result<u64> {
        self.ensure_wallet_initialized()?;
        let database_path = self.database_path();
        let database_path = database_path
            .to_str()
            .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?;

        let stdout = self.run_cli_command(&[
            "balance",
            "--database-path",
            database_path,
            "--account-name",
            "default",
        ])?;

        Self::parse_balance_output(&stdout)
    }

    async fn get_tip_height(&self) -> anyhow::Result<u64> {
        let client = Client::new();
        let url = format!("{}/get_tip_info", self.base_node_url.trim_end_matches('/'));
        let tip: TipInfoResponse = client
            .get(url)
            .send()
            .await
            .context("failed to query get_tip_info")?
            .error_for_status()
            .context("base node returned an HTTP error on get_tip_info")?
            .json()
            .await
            .context("failed to parse get_tip_info response")?;

        Ok(tip.metadata.best_block_height)
    }

    async fn scan_from_genesis(&self) -> anyhow::Result<ScanMetrics> {
        self.scan_from_height(0)
    }

    async fn scan_from_birthday(&self, height: u64) -> anyhow::Result<ScanMetrics> {
        self.scan_from_height(height)
    }

    async fn send_single(
        &self,
        to_address: &str,
        amount_ut: u64,
        _fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.ensure_wallet_initialized()?;

        let database_path = self.database_path();
        let database_path = database_path
            .to_str()
            .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?;
        let output_file = self.next_temp_file("unsigned_tx");
        let output_file_str = output_file
            .to_str()
            .ok_or_else(|| anyhow!("unsigned transaction path is not valid UTF-8"))?;
        let recipient = format!("{to_address}::{amount_ut}");

        let construction_started = Instant::now();
        self.run_cli_command(&[
            "create-unsigned-transaction",
            "--database-path",
            database_path,
            "--password",
            &self.password,
            "--account-name",
            DEFAULT_ACCOUNT_NAME,
            "--recipient",
            &recipient,
            "--output-file",
            output_file_str,
        ])?;

        let unsigned_json = std::fs::read_to_string(&output_file)
            .with_context(|| format!("failed to read {}", output_file.display()))?;
        let unsigned_tx = PrepareOneSidedTransactionForSigningResult::from_json(&unsigned_json)
            .context("failed to parse unsigned transaction JSON")?;

        let key_manager = self.key_manager()?;
        let signed = sign_locked_transaction(
            &key_manager,
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

        let _ = std::fs::remove_file(&output_file);

        match submit_result {
            Ok(result) if result.accepted => Ok(TxMetrics {
                tx_id,
                construction_secs,
                broadcast_to_mempool_secs,
                broadcast_to_confirmed_secs: 0.0,
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

#[derive(Debug, Deserialize)]
struct TipInfoResponse {
    metadata: TipMetadata,
}

#[derive(Debug, Deserialize)]
struct TipMetadata {
    best_block_height: u64,
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
