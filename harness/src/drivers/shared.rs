use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context;
use serde::Deserialize;
use tari_common_types::seeds::{
    cipher_seed::CipherSeed,
    mnemonic::{Mnemonic, MnemonicLanguage},
    seed_words::SeedWords,
};
use tari_transaction_components::tari_amount::MicroMinotari;

/// Seconds from Unix epoch (1970-01-01) to the Tari network genesis (2022-01-01 00:00:00 UTC).
pub const BIRTHDAY_GENESIS_FROM_UNIX_EPOCH: u64 = 1_640_995_200;
/// Seconds per day.
pub const SECS_PER_DAY: u64 = 86_400;
/// Average block time on Esmeralda testnet (seconds).
pub const AVG_BLOCK_SECS: u64 = 30;

/// Default wallet account name used by the `minotari` CLI.
pub const DEFAULT_ACCOUNT_NAME: &str = "default";

/// Convert a **block height** to the **birthday day count** expected by
/// `CipherSeed::change_birthday()`.
///
/// The birthday is stored as days since Unix epoch (1970-01-01).
///
/// When `block_height` is zero (genesis) this returns 0, which tells the
/// wallet to scan from the very beginning.
pub fn block_height_to_birthday(block_height: u64) -> u16 {
    if block_height == 0 {
        return 0;
    }
    let unix_timestamp = BIRTHDAY_GENESIS_FROM_UNIX_EPOCH + (block_height * AVG_BLOCK_SECS);
    (unix_timestamp / SECS_PER_DAY) as u16
}

/// Path to the `seed_words.txt` file inside a wallet data directory.
pub fn seed_words_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("seed_words.txt")
}

/// Re-encode a mnemonic seed phrase with a new birthday value.
///
/// `birthday` must be a day-count value as returned by
/// [`block_height_to_birthday`] — **not** a raw block height.
pub fn seed_words_with_birthday(seed_words: &str, birthday: u64) -> anyhow::Result<String> {
    let mnemonic =
        SeedWords::from_str(seed_words).context("failed to parse stored seed words")?;
    let mut seed = CipherSeed::from_mnemonic(&mnemonic, None)
        .context("failed to reconstruct cipher seed")?;
    let birthday =
        u16::try_from(birthday).context("birthday value exceeds u16 range")?;
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

/// Response from the base-node `/get_tip_info` HTTP endpoint.
#[derive(Debug, Deserialize)]
pub struct TipInfoResponse {
    pub metadata: TipMetadata,
}

#[derive(Debug, Deserialize)]
pub struct TipMetadata {
    pub best_block_height: u64,
}