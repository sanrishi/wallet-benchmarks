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
///
/// If `config_seed` is `Some`, it is used instead of generating a random seed,
/// making funding addresses reproducible across checkouts. The provided seed
/// is persisted to disk so subsequent runs are consistent even without the
/// config entry.
pub fn load_or_create_seed_words(
    data_dir: &std::path::Path,
    config_seed: Option<&str>,
) -> anyhow::Result<String> {
    let words_path = seed_words_path(data_dir);
    match std::fs::read_to_string(&words_path) {
        Ok(seed_words) => Ok(seed_words.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let seed_words = if let Some(provided) = config_seed {
                // Validate the provided seed is well-formed
                let mnemonic =
                    SeedWords::from_str(provided).context("invalid seed words in config")?;
                CipherSeed::from_mnemonic(&mnemonic, None)
                    .context("failed to reconstruct cipher seed from config")?;
                provided.to_string()
            } else {
                CipherSeed::random()
                    .to_mnemonic(MnemonicLanguage::English, None)?
                    .join(" ")
                    .reveal()
                    .to_string()
            };
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
    let amount = amount.replace(',', "");
    let amount = amount
        .trim_end()
        .trim_end_matches(|c: char| !c.is_ascii_digit() && c != '.' && c != ' ')
        .trim();
    let amount = MicroMinotari::from_str(amount)
        .with_context(|| format!("failed to parse balance amount '{amount}' from line: '{line}'"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_balance_output ─────────────────────────────────────────

    #[test]
    fn parse_balance_typical() {
        let out = "Balance at height 1234: 1,234,567 µT\n";
        assert_eq!(parse_balance_output(out).unwrap(), 1_234_567);
    }

    #[test]
    fn parse_balance_thousands_separator() {
        let out = "Balance at height 42: 1,000,000,000 µT\n";
        assert_eq!(parse_balance_output(out).unwrap(), 1_000_000_000);
    }

    #[test]
    fn parse_balance_zero() {
        let out = "Balance at height 0: 0 µT\n";
        assert_eq!(parse_balance_output(out).unwrap(), 0);
    }

    #[test]
    fn parse_balance_errors_when_line_missing() {
        let out = "some random output\n";
        assert!(parse_balance_output(out).is_err());
    }

    #[test]
    fn parse_balance_errors_on_empty() {
        assert!(parse_balance_output("").is_err());
    }

    #[test]
    fn parse_balance_handles_micro_symbol() {
        // The function replaces ÂµT -> µT, so test the actual
        // output that has the Unicode micro sign.
        let out = "Balance at height 5: 500 µT\n";
        assert_eq!(parse_balance_output(out).unwrap(), 500);
    }

    // ── seed_words_with_birthday ─────────────────────────────────────

    #[test]
    fn seed_words_with_birthday_round_trip() {
        let words = load_or_create_seed_words(&tempfile::TempDir::new().unwrap().path(), None).unwrap();
        let modified = seed_words_with_birthday(&words, 42).unwrap();
        assert!(!modified.is_empty());
        assert_ne!(modified, words, "birthday should change the seed encoding");
    }

    #[test]
    fn seed_words_with_birthday_genesis() {
        let words = load_or_create_seed_words(&tempfile::TempDir::new().unwrap().path(), None).unwrap();
        let genesis = seed_words_with_birthday(&words, 0).unwrap();
        assert!(!genesis.is_empty());
    }

    #[test]
    fn seed_words_with_birthday_errors_on_bad_seed() {
        assert!(seed_words_with_birthday("not valid seed words", 0).is_err());
    }

    // ── derive_wallet_keys ───────────────────────────────────────────

    #[test]
    fn derive_wallet_keys_is_deterministic() {
        let dir = tempfile::TempDir::new().unwrap();
        let words = load_or_create_seed_words(dir.path(), None).unwrap();
        let (vk1, sk1) = derive_wallet_keys(&words).unwrap();
        let (vk2, sk2) = derive_wallet_keys(&words).unwrap();
        assert_eq!(vk1, vk2, "view key must be deterministic");
        assert_eq!(sk1, sk2, "spend key must be deterministic");
    }

    #[test]
    fn derive_wallet_keys_returns_non_empty_hex() {
        let dir = tempfile::TempDir::new().unwrap();
        let words = load_or_create_seed_words(dir.path(), None).unwrap();
        let (view_key, spend_key) = derive_wallet_keys(&words).unwrap();
        assert!(!view_key.is_empty(), "view key hex should not be empty");
        assert!(!spend_key.is_empty(), "spend key hex should not be empty");
        // Basic hex sanity: all hex chars and even length
        assert!(view_key.len() % 2 == 0, "view key hex length should be even");
        assert!(spend_key.len() % 2 == 0, "spend key hex length should be even");
        assert!(
            view_key.chars().all(|c| c.is_ascii_hexdigit()),
            "view key should contain only hex chars"
        );
        assert!(
            spend_key.chars().all(|c| c.is_ascii_hexdigit()),
            "spend key should contain only hex chars"
        );
    }

    #[test]
    fn derive_wallet_keys_errors_on_bad_seed() {
        assert!(derive_wallet_keys("not valid seed words").is_err());
    }

    // ── load_or_create_seed_words ────────────────────────────────────

    #[test]
    fn creates_seed_words_file_when_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let words = load_or_create_seed_words(dir.path(), None).unwrap();
        assert!(!words.is_empty(), "should generate new seed words");
        let path = seed_words_path(dir.path());
        assert!(path.exists(), "seed_words.txt should be created on disk");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.trim(), words);
    }

    #[test]
    fn reads_existing_seed_words_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = seed_words_path(dir.path());
        let expected = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        std::fs::write(&path, expected).unwrap();
        let words = load_or_create_seed_words(dir.path(), None).unwrap();
        assert_eq!(words, expected);
    }

    // ── seed_words_path ──────────────────────────────────────────────

    // ── proptest: parse_balance_output ───────────────────────────────

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn parse_balance_any_non_negative_number(h in 0u64..10_000_000_000_000u64) {
            let with_commas = h.to_string()
                .as_bytes()
                .rchunks(3)
                .rev()
                .map(|chunk| std::str::from_utf8(chunk).unwrap())
                .collect::<Vec<_>>()
                .join(",");
            let output = format!("Balance at height 42: {with_commas} µT\n");
            let result = parse_balance_output(&output).unwrap();
            assert_eq!(result, h);
        }
    }

    #[test]
    fn seed_words_path_is_under_data_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = seed_words_path(dir.path());
        assert!(path.to_string_lossy().contains("seed_words.txt"));
        assert!(path.starts_with(dir.path()));
    }

    // ── Seed lifecycle (preservation across wipe / recovery) ─────────

    #[test]
    fn seed_words_persist_across_directory_wipe() {
        let dir = tempfile::TempDir::new().unwrap();
        let original = load_or_create_seed_words(dir.path(), None).unwrap();

        // Simulate wipe + restore: save seed words, delete dir, recreate
        let saved = original.clone();
        std::fs::remove_dir_all(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(seed_words_path(dir.path()), &saved).unwrap();

        let restored = load_or_create_seed_words(dir.path(), None).unwrap();
        assert_eq!(restored, original, "seed words must survive directory wipe");
    }

    #[test]
    fn derive_wallet_keys_recovers_same_wallet_after_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let words = load_or_create_seed_words(dir.path(), None).unwrap();

        // Persist seed words, then recover to a new directory
        let saved_words = words.clone();
        let recovered_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(seed_words_path(recovered_dir.path()), &saved_words).unwrap();
        let recovered = load_or_create_seed_words(recovered_dir.path(), None).unwrap();

        let (vk1, sk1) = derive_wallet_keys(&words).unwrap();
        let (vk2, sk2) = derive_wallet_keys(&recovered).unwrap();

        assert_eq!(vk1, vk2, "view key must match after seed recovery");
        assert_eq!(sk1, sk2, "spend key must match after seed recovery");
    }

    #[test]
    fn seed_words_with_birthday_produces_different_output_for_different_birthdays() {
        let dir = tempfile::TempDir::new().unwrap();
        let words = load_or_create_seed_words(dir.path(), None).unwrap();

        let b0 = seed_words_with_birthday(&words, 0).unwrap();
        let b1 = seed_words_with_birthday(&words, 10).unwrap();
        let b2 = seed_words_with_birthday(&words, 100).unwrap();

        assert_ne!(b0, b1, "different birthdays should produce different encodings");
        assert_ne!(b1, b2, "different birthdays should produce different encodings");
        assert_ne!(b0, b2, "different birthdays should produce different encodings");
    }

    #[test]
    fn same_birthday_produces_same_seed_encoding() {
        let dir = tempfile::TempDir::new().unwrap();
        let words = load_or_create_seed_words(dir.path(), None).unwrap();

        let a = seed_words_with_birthday(&words, 42).unwrap();
        let b = seed_words_with_birthday(&words, 42).unwrap();

        assert_eq!(a, b, "same birthday must produce identical seed encoding");
    }

    // ── wiremock: get_tip_height HTTP client ─────────────────────────

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn mock_tip_server(height: u64) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/get_tip_info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "metadata": { "best_block_height": height }
            })))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn get_tip_height_returns_height() {
        let server = mock_tip_server(123_456).await;
        let client = reqwest::Client::new();
        let height = get_tip_height(&client, &server.uri()).await.unwrap();
        assert_eq!(height, 123_456);
    }

    #[tokio::test]
    async fn get_tip_height_zero_returns_zero() {
        let server = mock_tip_server(0).await;
        let client = reqwest::Client::new();
        let height = get_tip_height(&client, &server.uri()).await.unwrap();
        assert_eq!(height, 0);
    }

    #[tokio::test]
    async fn get_tip_height_large_height() {
        let server = mock_tip_server(9_999_999).await;
        let client = reqwest::Client::new();
        let height = get_tip_height(&client, &server.uri()).await.unwrap();
        assert_eq!(height, 9_999_999);
    }

    #[tokio::test]
    async fn get_tip_height_errors_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/get_tip_info"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let result = get_tip_height(&client, &server.uri()).await;
        assert!(result.is_err(), "expected error for 404 response");
    }

    #[tokio::test]
    async fn get_tip_height_errors_on_bad_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/get_tip_info"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let result = get_tip_height(&client, &server.uri()).await;
        assert!(result.is_err(), "expected error for bad JSON");
    }

    #[tokio::test]
    async fn get_tip_height_errors_on_missing_field() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/get_tip_info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "metadata": {}
            })))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let result = get_tip_height(&client, &server.uri()).await;
        assert!(result.is_err(), "expected error when best_block_height is missing");
    }

    #[tokio::test]
    async fn get_tip_height_errors_on_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/get_tip_info"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let result = get_tip_height(&client, &server.uri()).await;
        assert!(result.is_err(), "expected error for 500 response");
    }

    #[tokio::test]
    async fn get_tip_height_errors_on_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/get_tip_info"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(10)))
            .mount(&server)
            .await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .unwrap();
        let result = get_tip_height(&client, &server.uri()).await;
        assert!(result.is_err(), "expected error for timeout");
    }
}

