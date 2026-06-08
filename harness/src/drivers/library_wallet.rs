#![cfg(feature = "library_wallet")]

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use reqwest::Client;
use tari_common::configuration::Network;
use tari_common_types::{
    seeds::{
        cipher_seed::CipherSeed,
        mnemonic::Mnemonic,
        seed_words::SeedWords,
    },
    tari_address::{TariAddress, TariAddressFeatures},
    transaction::TxId,
};
use tari_transaction_components::{
    MicroMinotari,
    key_manager::wallet_types::{SeedWordsWallet, WalletType},
    transaction_components::{CoinBaseExtra, MemoField, OutputFeatures, OutputType, RangeProofType},
};
use tari_comms::peer_manager::{NodeIdentity, PeerFeatures};
use tari_p2p::auto_update::AutoUpdateConfig;
use tari_shutdown::Shutdown;
use tari_transaction_components::{
    consensus::ConsensusManager,
    crypto_factories::CryptoFactories,
};
use tari_utilities::SafePassword;
use tokio::sync::Mutex;
use url::Url;

use minotari_wallet::{
    WalletConfig,
    WalletSqlite,
    client::http_client_factory::{DefaultHttpClientFactory, HttpClientFactory},
    output_manager_service::{
        UtxoSelectionCriteria,
        storage::database::OutputManagerDatabase,
    },
    storage::{
        database::WalletDatabase,
        sqlite_utilities,
    },
    transaction_service::{
        error::TransactionServiceError,
        handle::TransactionServiceHandle,
        storage::models::CompletedTransaction,
    },
    utxo_scanner_service::{
        handle::UtxoScannerEvent,
        uxto_scanner_service_builder::{UtxoScannerMode, UtxoScannerServiceBuilder},
    },
    wallet::{derive_comms_secret_key, read_or_create_master_seed},
};

use crate::driver::WalletDriver;
use crate::drivers::shared;
use crate::metrics::{ScanMetrics, TxMetrics};

/// Library-mode wallet driver using in-process `minotari_wallet::Wallet`.
pub struct LibraryWalletDriver {
    wallet: Mutex<Option<WalletSqlite>>,
    shutdown: Mutex<Option<Shutdown>>,
    seed_words: String,
    data_dir: PathBuf,
    base_node_url: String,
    confirmation_window: u64,
    password: String,
    http_client: Client,
    cached_address: OnceLock<String>,
}

impl LibraryWalletDriver {
    #[allow(unused_variables)]
    pub fn new(
        minotari_bin: PathBuf,
        data_dir: PathBuf,
        base_node_url: String,
        confirmation_window: u64,
        password: String,
        config_seed: Option<String>,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create {}", data_dir.display()))?;
        let seed_words = shared::load_or_create_seed_words(&data_dir, config_seed.as_deref())?;
        Ok(Self {
            wallet: Mutex::new(None),
            shutdown: Mutex::new(None),
            seed_words,
            data_dir,
            base_node_url,
            confirmation_window,
            password,
            http_client: Client::new(),
            cached_address: OnceLock::new(),
        })
    }

    async fn ensure_wallet_started(&self) -> anyhow::Result<()> {
        if self.wallet.lock().await.is_some() {
            return Ok(());
        }
        let wallet = self.create_wallet(&self.seed_words).await?;
        *self.wallet.lock().await = Some(wallet);
        Ok(())
    }

    async fn create_wallet(&self, seed_words: &str) -> anyhow::Result<WalletSqlite> {
        let mnemonic = SeedWords::from_str(seed_words)
            .context("failed to parse seed words")?;
        let cipher_seed = CipherSeed::from_mnemonic(&mnemonic, None)
            .context("failed to reconstruct cipher seed from seed words")?;

        let mut config = WalletConfig::default();
        config.data_dir = self.data_dir.clone();
        config.db_file = self.data_dir.join("wallet.db");
        config.config_dir = self.data_dir.clone();
        config.network = Network::Esmeralda;
        config.http_server_url = self.base_node_url.clone();
        config.fallback_http_server_url = self.base_node_url.clone();
        config.grpc_enabled = false;
        config.password = Some(self.password.clone().into());

        let passphrase: SafePassword = self.password.clone().into();
        let (wallet_backend, transaction_backend, output_manager_backend, key_manager_backend) =
            sqlite_utilities::initialize_sqlite_database_backends(
                &config.db_file,
                passphrase,
                16,
            )
            .context("failed to initialize wallet database backends")?;

        let wallet_database = WalletDatabase::new(wallet_backend);
        let output_manager_database =
            OutputManagerDatabase::new(output_manager_backend.clone());

        let comms_secret_key = derive_comms_secret_key(&cipher_seed)
            .map_err(|e| anyhow::anyhow!("failed to derive comms secret key: {e}"))?;
        let node_identity = Arc::new(NodeIdentity::new(
            comms_secret_key,
            vec![],
            PeerFeatures::COMMUNICATION_CLIENT,
        ));

        let factories = CryptoFactories::default();
        let consensus_manager = ConsensusManager::builder(Network::Esmeralda).build();

        let shutdown = Shutdown::new();
        let shutdown_signal = shutdown.to_signal();

        let master_seed = read_or_create_master_seed(Some(cipher_seed), &wallet_database)
            .map_err(|e| anyhow::anyhow!("failed to read or create master seed: {e}"))?;

        let wallet = WalletSqlite::start(
            config,
            AutoUpdateConfig::default(),
            node_identity,
            consensus_manager,
            factories,
            wallet_database,
            output_manager_database,
            transaction_backend,
            output_manager_backend,
            key_manager_backend,
            shutdown_signal,
            master_seed,
            None,
        )
        .await
        .context("failed to start wallet")?;

        *self.shutdown.lock().await = Some(shutdown);

        Ok(wallet)
    }

    fn self_address_from_seed(&self) -> anyhow::Result<String> {
        if let Some(addr) = self.cached_address.get() {
            return Ok(addr.clone());
        }
        let mnemonic = SeedWords::from_str(&self.seed_words)
            .context("failed to parse seed words for address derivation")?;
        let cipher_seed = CipherSeed::from_mnemonic(&mnemonic, None)
            .context("failed to reconstruct cipher seed for address derivation")?;
        let wallet = WalletType::SeedWords(
            SeedWordsWallet::construct_new(cipher_seed)
                .map_err(|e| anyhow::anyhow!("failed to construct wallet for address: {e}"))?,
        );
        let address = TariAddress::new_dual_address(
            wallet.get_public_view_key(),
            wallet.get_public_spend_key(),
            Network::Esmeralda,
            TariAddressFeatures::create_one_sided_only(),
            None,
        )
        .context("failed to construct TariAddress")?;
        let addr_str = address.to_base58();
        let _ = self.cached_address.set(addr_str.clone());
        Ok(addr_str)
    }

    async fn run_recovery_scan(&self) -> anyhow::Result<ScanMetrics> {
        let mut wallet_lock = self.wallet.lock().await;
        let wallet = wallet_lock
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("wallet not initialized"))?;

        let factory = DefaultHttpClientFactory::new(
            Url::parse(&self.base_node_url)?,
            Url::parse(&self.base_node_url)?,
        );

        let scan_shutdown = Shutdown::new();
        let mut builder = UtxoScannerServiceBuilder::<DefaultHttpClientFactory>::default();
        builder.with_client_factory(factory);
        builder.with_mode(UtxoScannerMode::Recovery);
        let mut scanner = builder
            .build_with_wallet(wallet, scan_shutdown.to_signal())
            .await?;

        let mut event_rx = scanner.get_event_receiver();

        let h_tip_start = shared::get_tip_height(&self.http_client, &self.base_node_url).await?;
        let wall_start = Instant::now();

        scanner.run().await?;

        let event = tokio::time::timeout(Duration::from_secs(3600), event_rx.recv())
            .await
            .context("scan timed out (3600s)")?
            .context("scan event channel closed unexpectedly")?;

        let wall_secs = wall_start.elapsed().as_secs_f64();
        let h_tip_end = shared::get_tip_height(&self.http_client, &self.base_node_url).await?;

        match event {
            UtxoScannerEvent::Completed {
                num_recovered,
                final_height,
                ..
            } => {
                let blocks_scanned = final_height.saturating_sub(h_tip_start);
                let blocks_per_sec = if wall_secs > 0.0 {
                    blocks_scanned as f64 / wall_secs
                } else {
                    0.0
                };
                Ok(ScanMetrics {
                    wall_clock_secs: wall_secs,
                    blocks_per_sec,
                    h_tip_start,
                    h_tip_end,
                    outputs_found: num_recovered,
                    peak_rss_kb: 0,
                    peak_cpu_percent: 0.0,
                })
            },
            UtxoScannerEvent::ScanningRoundFailed {
                num_retries,
                retry_limit,
                error,
            } => Err(anyhow::anyhow!(
                "scan failed after {num_retries}/{retry_limit} retries: {error}"
            )),
            _ => Err(anyhow::anyhow!("unexpected scan event: {event:?}")),
        }
    }

    async fn stop_wallet(&self) {
        self.shutdown.lock().await.take();
        self.wallet.lock().await.take();
    }
}

#[async_trait]
impl WalletDriver for LibraryWalletDriver {
    fn mode_name(&self) -> &str {
        "library_wallet"
    }

    async fn reset(&self) -> anyhow::Result<()> {
        let saved_seed_words = self.seed_words.clone();

        self.stop_wallet().await;

        if self.data_dir.exists() {
            std::fs::remove_dir_all(&self.data_dir)
                .with_context(|| format!("failed to remove {}", self.data_dir.display()))?;
        }
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("failed to create {}", self.data_dir.display()))?;

        std::fs::write(shared::seed_words_path(&self.data_dir), &saved_seed_words)
            .with_context(|| "failed to persist seed words after wipe")?;

        Ok(())
    }

    async fn get_balance(&self) -> anyhow::Result<u64> {
        self.ensure_wallet_started().await?;
        let mut wallet = self.wallet.lock().await;
        let balance = wallet
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("wallet not initialized"))?
            .output_manager_service
            .get_balance()
            .await
            .context("failed to query wallet balance")?;
        Ok(balance.available_balance.as_u64())
    }

    async fn get_tip_height(&self) -> anyhow::Result<u64> {
        shared::get_tip_height(&self.http_client, &self.base_node_url).await
    }

    async fn get_self_address(&self) -> anyhow::Result<String> {
        self.self_address_from_seed()
    }

    async fn scan_from_genesis(&self) -> anyhow::Result<ScanMetrics> {
        let genesis_seed = shared::seed_words_with_birthday(&self.seed_words, 0)?;

        self.stop_wallet().await;

        if self.data_dir.exists() {
            std::fs::remove_dir_all(&self.data_dir)?;
        }
        std::fs::create_dir_all(&self.data_dir)?;

        std::fs::write(shared::seed_words_path(&self.data_dir), &genesis_seed)?;

        let wallet = self.create_wallet(&genesis_seed).await?;
        *self.wallet.lock().await = Some(wallet);

        self.run_recovery_scan().await
    }

    async fn scan_from_birthday(&self, height: u64) -> anyhow::Result<ScanMetrics> {
        self.ensure_wallet_started().await?;

        {
            let mut wallet_lock = self.wallet.lock().await;
            let wallet = wallet_lock
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("wallet not initialized"))?;
            wallet.db.clear_scanned_blocks_from_and_higher(height)?;
        }

        self.run_recovery_scan().await
    }

    async fn send_single(
        &self,
        to_address: &str,
        amount_ut: u64,
        fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.ensure_wallet_started().await?;

        let mut tx_handle = {
            let mut wallet = self.wallet.lock().await;
            wallet
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("wallet not initialized"))?
                .transaction_service
                .clone()
        };

        let destination = TariAddress::from_str(to_address)?;
        let started_at = Instant::now();

        let tx_id = tx_handle
            .send_one_sided_transaction(
                destination,
                MicroMinotari(amount_ut),
                UtxoSelectionCriteria::default(),
                OutputFeatures::new_current_version(
                    OutputType::Standard,
                    0,
                    CoinBaseExtra::default(),
                    None,
                    RangeProofType::BulletProofPlus,
                ),
                MicroMinotari(fee_rate),
                MemoField::new_empty(),
            )
            .await?;

        let broadcast_at = Instant::now();
        let construction_secs = broadcast_at.duration_since(started_at).as_secs_f64();

        let completed_tx = poll_transaction(&mut tx_handle, tx_id).await?;
        let confirmed_at = Instant::now();

        Ok(TxMetrics {
            tx_id: tx_id.to_string(),
            construction_secs,
            broadcast_to_mempool_secs: construction_secs,
            broadcast_to_confirmed_secs: confirmed_at.duration_since(started_at).as_secs_f64(),
            fee_paid: completed_tx.fee.as_u64(),
            success: true,
            error: None,
        })
    }

    async fn send_batch(
        &self,
        recipients: Vec<(String, u64)>,
        fee_rate: u64,
    ) -> anyhow::Result<TxMetrics> {
        self.ensure_wallet_started().await?;

        if recipients.is_empty() {
            return Err(anyhow::anyhow!("send_batch called with empty recipients"));
        }

        let mut tx_handle = {
            let mut wallet = self.wallet.lock().await;
            wallet
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("wallet not initialized"))?
                .transaction_service
                .clone()
        };

        let destinations: Vec<(TariAddress, MicroMinotari, MemoField)> = recipients
            .iter()
            .map(|(addr, amount)| {
                let address = TariAddress::from_str(addr)?;
                Ok((address, MicroMinotari(*amount), MemoField::new_empty()))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let started_at = Instant::now();
        let tx_ids = tx_handle
            .send_one_sided_multi_recipient_transaction(
                destinations,
                UtxoSelectionCriteria::default(),
                OutputFeatures::new_current_version(
                    OutputType::Standard,
                    0,
                    CoinBaseExtra::default(),
                    None,
                    RangeProofType::BulletProofPlus,
                ),
                MicroMinotari(fee_rate),
            )
            .await?;

        let broadcast_at = Instant::now();
        let construction_secs = broadcast_at.duration_since(started_at).as_secs_f64();

        let first_tx_id = tx_ids
            .first()
            .ok_or_else(|| anyhow::anyhow!("send_batch returned no tx_ids"))?;

        let completed_tx = poll_transaction(&mut tx_handle, *first_tx_id).await?;
        let confirmed_at = Instant::now();

        Ok(TxMetrics {
            tx_id: first_tx_id.to_string(),
            construction_secs,
            broadcast_to_mempool_secs: construction_secs,
            broadcast_to_confirmed_secs: confirmed_at.duration_since(started_at).as_secs_f64(),
            fee_paid: completed_tx.fee.as_u64(),
            success: true,
            error: None,
        })
    }

    async fn observe_funding(&self, expected_amount_ut: u64) -> anyhow::Result<TxMetrics> {
        self.ensure_wallet_started().await?;

        let started_at = Instant::now();
        let deadline = started_at + Duration::from_secs(600);

        loop {
            if Instant::now() > deadline {
                return Err(anyhow::anyhow!(
                    "incoming funding of at least {expected_amount_ut} uT was not observed within 600s"
                ));
            }
            let balance = self.get_balance().await?;
            if balance >= expected_amount_ut {
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

async fn poll_transaction(
    tx_handle: &mut TransactionServiceHandle,
    tx_id: TxId,
) -> anyhow::Result<CompletedTransaction> {
    let deadline = Instant::now() + Duration::from_secs(3600);
    loop {
        if Instant::now() > deadline {
            return Err(anyhow::anyhow!("transaction {tx_id} not confirmed within 3600s"));
        }
        match tx_handle.get_completed_transaction(tx_id).await {
            Ok(tx) => return Ok(tx),
            Err(TransactionServiceError::TransactionDoesNotExistError) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            },
            Err(e) => return Err(anyhow::anyhow!("transaction {tx_id} query failed: {e}")),
        }
    }
}
