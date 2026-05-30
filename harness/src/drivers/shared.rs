use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context;
use reqwest::Client;
use serde::Deserialize;
use tari_common_types::seeds::{
    cipher_seed::CipherSeed,
    mnemonic::{Mnemonic, MnemonicLanguage},
    seed_words::SeedWords,
};
use tari_transaction_components::{
    key_manager::wallet_types::{SeedWordsWallet, WalletType},
    tari_amount::MicroMinotari,
};

/// Default wallet account name used by the `minotari` CLI.
pub const DEFAULT_ACCOUNT_NAME: &str = "default";

/// Path to the `seed_words.txt` file inside a wallet data directory.
pub fn seed_words_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("seed_words.txt")
}

/// Load existing seed words from disk, or generate and persist a fresh set.
pub fn load_or_create_seed_words(data_dir: &std::path::Path) -> anyhow::Result<String> {
    let words_path = seed_words_path(data_dir);
    match std::fs::read_to_string(&words_path) {
        Ok(seed_words) => Ok(seed_words.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let seed_words = CipherSeed::random()
                .to_mnemonic(MnemonicLanguage::English, None)?
                .join(" ")
                .reveal()
                .to_string();
            std::fs::write(&words_path, &seed_words)?;
            Ok(seed_words)
        }
        Err(e) => Err(e.into()),
    }
}

/// Re-encode a mnemonic seed phrase with a new birthday value.
///
/// `birthday` is a day-count value (days since Unix epoch). Pass 0 for
/// no date filter (scan from genesis height via `--rescan-from-height`).
pub fn seed_words_with_birthday(seed_words: &str, birthday: u64) -> anyhow::Result<String> {
    let mnemonic = SeedWords::from_str(seed_words).context("failed to parse stored seed words")?;
    let mut seed =
        CipherSeed::from_mnemonic(&mnemonic, None).context("failed to reconstruct cipher seed")?;
    let birthday = u16::try_from(birthday).context("birthday value exceeds u16 range")?;
    seed.change_birthday(birthday);
    seed.to_mnemonic(MnemonicLanguage::English, None)
        .map(|m| m.join(" ").reveal().to_string())
        .context("failed to serialize seed words")
}

/// Parse the `Balance at height` line from a `minotari balance` CLI output.
pub fn parse_balance_output(stdout: &str) -> anyhow::Result<u64> {
    let line = stdout
        .lines()
        .find(|line| line.contains("Balance at height"))
        .ok_or_else(|| anyhow::anyhow!("balance output did not contain a balance line"))?;

    let amount = line
        .split(':')
        .next_back()
        .map(str::trim)
        .ok_or_else(|| anyhow::anyhow!("balance output did not contain a parsable amount"))?;
    let amount = amount.replace(',', "").replace("ÂµT", "µT");
    let amount = MicroMinotari::from_str(&amount)
        .with_context(|| format!("failed to parse balance amount '{amount}'"))?;
    Ok(amount.as_u64())
}

/// Derive the view key and public spend key hex strings from wallet seed words.
pub fn derive_wallet_keys(seed_words: &str) -> anyhow::Result<(String, String)> {
    use tari_utilities::byte_array::ByteArray;

    let mnemonic =
        SeedWords::from_str(seed_words).context("failed to parse seed words for key derivation")?;
    let cipher_seed = CipherSeed::from_mnemonic(&mnemonic, None)
        .context("failed to reconstruct cipher seed for key derivation")?;
    let wallet = WalletType::SeedWords(
        SeedWordsWallet::construct_new(cipher_seed)
            .map_err(|e| anyhow::anyhow!("failed to construct wallet for key derivation: {e}"))?,
    );
    let view_key = wallet.get_public_view_key();
    let spend_key = wallet.get_public_spend_key();
    let view_key_bytes = view_key.as_bytes();
    let spend_key_bytes = spend_key.as_bytes();
    let view_key_hex = view_key_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let spend_key_hex = spend_key_bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok((view_key_hex, spend_key_hex))
}

/// Query the base node for the current chain tip height.
pub async fn get_tip_height(http_client: &Client, base_node_url: &str) -> anyhow::Result<u64> {
    let url = format!("{}/get_tip_info", base_node_url.trim_end_matches('/'));
    let tip: TipInfoResponse = http_client
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

/// Response from the base-node `/get_tip_info` HTTP endpoint.
#[derive(Debug, Deserialize)]
pub struct TipInfoResponse {
    pub metadata: TipMetadata,
}

#[derive(Debug, Deserialize)]
pub struct TipMetadata {
    pub best_block_height: u64,
}
