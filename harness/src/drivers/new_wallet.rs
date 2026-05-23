use async_trait::async_trait;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant};

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
use tari_common_types::tari_address::{TariAddress, TariAddressFeatures};
use tari_transaction_components::{
    consensus::ConsensusConstantsBuilder,
    key_manager::{wallet_types::{SeedWordsWallet, WalletType}, KeyManager},
    offline_signing::{
        models::{PrepareOneSidedTransactionForSigningResult, TransactionResult},
        sign_locked_transaction,
    },
};
use tempfile::NamedTempFile;
use tokio::process::Command;

use crate::driver::WalletDriver;
use crate::metrics::{ScanMetrics, TxMetrics};

const DEFAULT_ACCOUNT_NAME: &str = "default";

pub struct NewWalletDriver {
    pub minotari_bin: PathBuf,
    pub data_dir: PathBuf,
    pub base_node_url: String,
    confirmation_window: u64,
    http_client: Client,
    password: String,
    seed_words: String,
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

        Ok(Self {
            minotari_bin,
            data_dir,
            base_node_url,
            confirmation_window,
            http_client: Client::new(),
            password,
            seed_words,
        })
    }

    fn database_path(&self) -> PathBuf {
        self.data_dir.join("wallet.db")
    }

    fn seed_words_path(data_dir: &std::path::Path) -> PathBuf {
        data_dir.join("seed_words.txt")
    }

    fn load_or_create_seed_words(data_dir: &std::path::Path) -> anyhow::Result<String> {
        let seed_words_path = Self::seed_words_path(data_dir);
        let database_path = data_dir.join("wallet.db");

        match fs::read_to_string(&seed_words_path) {
            Ok(seed_words) => return Ok(seed_words.trim().to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read {}", seed_words_path.display())
                });
            }
        }

        if database_path.exists() {
            return Err(anyhow!(
                "existing wallet database found at {} but {} is missing; wipe the data dir or restore the seed file",
                database_path.display(),
                seed_words_path.display()
            ));
        }

        let seed_words = CipherSeed::random()
            .to_mnemonic(MnemonicLanguage::English, None)
            .context("failed to generate mnemonic seed words for new_wallet")?
            .join(" ")
            .reveal()
            .to_string();
        fs::write(&seed_words_path, &seed_words)
            .with_context(|| format!("failed to write {}", seed_words_path.display()))?;
        Ok(seed_words)
    }

    async fn run_cli_command(&self, args: &[&str]) -> anyhow::Result<String> {
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

        let numeric = amount
            .chars()
            .take_while(|ch| ch.is_ascii_digit() || matches!(ch, '.' | ','))
            .collect::<String>();
        let suffix = amount[numeric.len()..].trim();
        let numeric = numeric.replace(',', "");

        if suffix == "T" {
            return Self::parse_tari_to_micro_tari(&numeric);
        }

        if suffix.contains('T') {
            return numeric
                .parse::<u64>()
                .with_context(|| format!("failed to parse microTari balance from '{amount}'"));
        }

        Err(anyhow!("unsupported balance output format: {amount}"))
    }

    fn parse_tari_to_micro_tari(value: &str) -> anyhow::Result<u64> {
        let (whole, fractional) = match value.split_once('.') {
            Some((whole, fractional)) => (whole.trim(), fractional.trim()),
            None => (value.trim(), ""),
        };

        let whole = whole
            .parse::<u64>()
            .with_context(|| format!("failed to parse Tari whole units from '{value}'"))?;

        let fractional_digits = fractional
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if fractional_digits.len() > 6 {
            return Err(anyhow!(
                "too many fractional Tari digits in '{value}', expected at most 6"
            ));
        }

        let mut fractional_padded = fractional_digits;
        while fractional_padded.len() < 6 {
            fractional_padded.push('0');
        }

        let fractional = if fractional_padded.is_empty() {
            0
        } else {
            fractional_padded
                .parse::<u64>()
                .with_context(|| format!("failed to parse Tari fractional units from '{value}'"))?
        };

        whole
            .checked_mul(1_000_000)
            .and_then(|base| base.checked_add(fractional))
            .ok_or_else(|| anyhow!("Tari balance overflow while converting '{value}'"))
    }

    async fn ensure_wallet_initialized(&self) -> anyhow::Result<()> {
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
        ])
        .await?;

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

    fn self_address_string(&self) -> anyhow::Result<String> {
        let mnemonic = SeedWords::from_str(&self.seed_words)
            .context("failed to parse stored seed words for new_wallet address")?;
        let cipher_seed = CipherSeed::from_mnemonic(&mnemonic, None)
            .context("failed to reconstruct cipher seed for new_wallet address")?;
        let wallet = WalletType::SeedWords(
            SeedWordsWallet::construct_new(cipher_seed)
                .map_err(|_| anyhow!("failed to construct seed-words wallet for new_wallet address"))?,
        );
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
        let response = self.http_client
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
            .query_row("SELECT MAX(height) FROM scanned_tip_blocks", [], |row| {
                row.get::<_, Option<u64>>(0)
            })
            .optional()
            .context("failed to query scanned_tip_blocks from new_wallet database")?
            .flatten()
            .unwrap_or(0);
        Ok(height)
    }

    async fn scan_from_height(&self, from_height: u64) -> anyhow::Result<ScanMetrics> {
        self.ensure_wallet_initialized().await?;

        let database_path = self.database_path();
        let database_path = database_path
            .to_str()
            .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?;
        let from_height_string = from_height.to_string();
        let h_tip_start = self.get_tip_height().await?;
        let started_at = Instant::now();

        self.run_cli_command(&[
            "re-scan",
            "--database-path",
            database_path,
            "--password",
            &self.password,
            "--base-url",
            self.base_node_url.as_str(),
            "--account-name",
            DEFAULT_ACCOUNT_NAME,
            "--rescan-from-height",
            &from_height_string,
        ])
        .await?;

        let wall_clock_secs = started_at.elapsed().as_secs_f64();
        let h_tip_end = self.get_tip_height().await?;
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

    fn tx_id_i64(tx_id: &str) -> anyhow::Result<i64> {
        let tx_id = tx_id
            .parse::<u64>()
            .with_context(|| format!("failed to parse tx id '{tx_id}' as u64"))?;
        Ok(tx_id as i64)
    }

    fn get_transaction_state(
        &self,
        tx_id: &str,
    ) -> anyhow::Result<Option<(String, Option<u64>, Option<u64>, Option<String>)>> {
        let connection = Connection::open(self.database_path())
            .context("failed to open new_wallet database for transaction status")?;
        let tx_id = Self::tx_id_i64(tx_id)?;
        connection
            .query_row(
                "SELECT status, mined_height, confirmation_height, last_rejected_reason FROM completed_transactions WHERE id = ?1",
                [tx_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<u64>>(1)?,
                        row.get::<_, Option<u64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .context("failed to query completed transaction state from new_wallet database")
    }

    async fn refresh_transaction_state(&self) -> anyhow::Result<()> {
        self.ensure_wallet_initialized().await?;
        let database_path = self.database_path();
        let database_path = database_path
            .to_str()
            .ok_or_else(|| anyhow!("database path is not valid UTF-8"))?;

        self.run_cli_command(&[
            "scan",
            "--database-path",
            database_path,
            "--password",
            &self.password,
            "--base-url",
            self.base_node_url.as_str(),
            "--account-name",
            DEFAULT_ACCOUNT_NAME,
            "--max-blocks-to-scan",
            "1",
        ])
        .await?;

        Ok(())
    }

    async fn wait_for_confirmation(&self, tx_id: &str) -> anyhow::Result<f64> {
        let started_at = Instant::now();
        let deadline = started_at + Duration::from_secs(600);

        loop {
            if Instant::now() > deadline {
                return Err(anyhow!(
                    "transaction {tx_id} was not confirmed within 600s"
                ));
            }

            self.refresh_transaction_state().await?;
            if let Some((status, mined_height, confirmation_height, rejected_reason)) =
                self.get_transaction_state(tx_id)?
            {
                match status.as_str() {
                    "mined_confirmed" => return Ok(started_at.elapsed().as_secs_f64()),
                    "rejected" => {
                        return Err(anyhow!(
                            rejected_reason.unwrap_or_else(|| "transaction was rejected".to_string())
                        ));
                    }
                    _ => {
                        if let Some(target_height) = confirmation_height {
                            let tip = self.get_tip_height().await?;
                            if tip >= target_height {
                                return Ok(started_at.elapsed().as_secs_f64());
                            }
                        } else if let Some(mined_height) = mined_height {
                            let tip = self.get_tip_height().await?;
                            let target_height = mined_height
                                .saturating_add(self.confirmation_window.max(1).saturating_sub(1));
                            if tip >= target_height {
                                return Ok(started_at.elapsed().as_secs_f64());
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    fn account_id(&self) -> anyhow::Result<i64> {
        let connection = Connection::open(self.database_path())
            .context("failed to open new_wallet database for account lookup")?;
        connection
            .query_row(
                "SELECT id FROM accounts WHERE friendly_name = ?1 LIMIT 1",
                [DEFAULT_ACCOUNT_NAME],
                |row| row.get(0),
            )
            .context("failed to query default account id from new_wallet database")
    }

    fn get_balance_state(&self) -> anyhow::Result<(u64, u64)> {
        let connection = Connection::open(self.database_path())
            .context("failed to open new_wallet database for balance state")?;
        let account_id = self.account_id()?;
        let (total_credits, total_debits): (i64, i64) = connection
            .query_row(
                "SELECT COALESCE(SUM(balance_credit), 0), COALESCE(SUM(balance_debit), 0) FROM balance_changes WHERE account_id = ?1",
                [account_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("failed to query balance aggregates from new_wallet database")?;
        let (locked_val, unconfirmed_val, locked_and_unconfirmed_val): (i64, i64, i64) = connection
            .query_row(
                "SELECT \
                    COALESCE(SUM(CASE WHEN status = 'locked' THEN value ELSE 0 END), 0), \
                    COALESCE(SUM(CASE WHEN confirmed_height IS NULL THEN value ELSE 0 END), 0), \
                    COALESCE(SUM(CASE WHEN status = 'locked' AND confirmed_height IS NULL THEN value ELSE 0 END), 0) \
                 FROM outputs WHERE account_id = ?1 AND deleted_at IS NULL AND is_burn = 0",
                [account_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .context("failed to query output totals from new_wallet database")?;

        let total_balance = (total_credits - total_debits).max(0) as u64;
        let unavailable = (locked_val + unconfirmed_val - locked_and_unconfirmed_val).max(0) as u64;
        let available = total_balance.saturating_sub(unavailable);
        let unconfirmed = unconfirmed_val.max(0) as u64;
        Ok((available, unconfirmed))
    }

    fn latest_incoming_transaction(
        &self,
        expected_amount_ut: u64,
    ) -> anyhow::Result<Option<(String, String)>> {
        let connection = Connection::open(self.database_path())
            .context("failed to open new_wallet database for displayed transaction lookup")?;
        let account_id = self.account_id()?;
        connection
            .query_row(
                "SELECT id, status FROM displayed_transactions \
                 WHERE account_id = ?1 AND direction = 'incoming' AND amount >= ?2 \
                 ORDER BY updated_at DESC, block_height DESC, id DESC LIMIT 1",
                rusqlite::params![account_id, expected_amount_ut as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .context("failed to query latest incoming displayed transaction from new_wallet database")
    }

    async fn send_recipients(
        &self,
        recipients: Vec<(String, u64)>,
    ) -> anyhow::Result<TxMetrics> {
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
    fn mode_name(&self) -> &str { "new_wallet" }

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

        let stdout = self.run_cli_command(&[
            "balance",
            "--database-path",
            database_path,
            "--account-name",
            DEFAULT_ACCOUNT_NAME,
        ])
        .await?;

        Self::parse_balance_output(&stdout)
    }

    async fn get_tip_height(&self) -> anyhow::Result<u64> {
        let url = format!("{}/get_tip_info", self.base_node_url.trim_end_matches('/'));
        let tip: TipInfoResponse = self.http_client
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

    async fn get_self_address(&self) -> anyhow::Result<String> {
        self.self_address_string()
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
        _fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.send_recipients(vec![(to_address.to_string(), amount_ut)]).await
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
        let mut first_seen_pending_at = None;

        loop {
            if Instant::now() > deadline {
                return Err(anyhow!(
                    "incoming funding of at least {expected_amount_ut} uT was not observed within 600s"
                ));
            }

            self.refresh_transaction_state().await?;
            let (available, unconfirmed) = self.get_balance_state()?;
            let incoming = self.latest_incoming_transaction(expected_amount_ut)?;

            if first_seen_pending_at.is_none()
                && (unconfirmed >= expected_amount_ut
                    || incoming
                        .as_ref()
                        .is_some_and(|(_, status)| status == "pending" || status == "unconfirmed" || status == "confirmed")
                    || available >= expected_amount_ut)
            {
                first_seen_pending_at = Some(started_at.elapsed().as_secs_f64());
            }

            if available >= expected_amount_ut
                || incoming
                    .as_ref()
                    .is_some_and(|(_, status)| status == "confirmed")
            {
                return Ok(TxMetrics {
                    tx_id: incoming
                        .map(|(tx_id, _)| tx_id)
                        .unwrap_or_else(|| "incoming-funding".to_string()),
                    construction_secs: 0.0,
                    broadcast_to_mempool_secs: first_seen_pending_at
                        .unwrap_or_else(|| started_at.elapsed().as_secs_f64()),
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
