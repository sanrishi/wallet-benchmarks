use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub benchmark: BenchmarkParams,
    pub paths: BinaryPaths,
    pub network: NetworkConfig,
    pub passwords: Passwords,
    pub data: DataDirs,
    pub versions: VersionPins,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BenchmarkParams {
    pub a_fund: u64,
    pub c_min: u64,
    pub volume_target: u64,
    pub doubling_rounds: u64,
    pub fanout_outputs_per_tx: u64,
    pub concurrent_batches: Vec<u64>,
    pub s4_t_budget_secs: u64,
    pub s5_m: u64,
    pub s5_k: u64,
    pub tx_amount_ut: u64,
    pub fee_rate: String,
    pub scan_interval_secs: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BinaryPaths {
    pub wallet_bin: String,
    pub minotari_bin: String,
    pub payment_processor_bin: String,
    pub console_wallet_bin: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct NetworkConfig {
    pub base_node_grpc_url: String,
    pub base_node_http_url: String,
    pub grpc_port: u16,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Passwords {
    pub old_wallet: String,
    pub new_wallet: String,
    pub payment_processor: String,
    pub library_wallet: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DataDirs {
    pub old_wallet: String,
    pub new_wallet: String,
    pub payment_processor: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct VersionPins {
    pub console_wallet: String,
    pub minotari_cli: String,
    pub base_node: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_toml() -> &'static str {
        r#"
[benchmark]
a_fund = 3000000000000
c_min = 4
volume_target = 512
doubling_rounds = 6
fanout_outputs_per_tx = 3
concurrent_batches = []
s4_t_budget_secs = 120
s5_m = 12
s5_k = 4
tx_amount_ut = 1000
fee_rate = "2"
scan_interval_secs = 1

[paths]
wallet_bin = "/usr/bin/minotari_console_wallet"
minotari_bin = "/usr/bin/minotari"
payment_processor_bin = "/usr/bin/minotari_payment_processor"
console_wallet_bin = "/usr/bin/minotari_console_wallet"

[network]
base_node_grpc_url = "grpc://localhost:18142"
base_node_http_url = "http://localhost:18143"
grpc_port = 18142

[passwords]
old_wallet = "test"
new_wallet = "test"
payment_processor = "test"
library_wallet = "test"

[data]
old_wallet = "/tmp/old"
new_wallet = "/tmp/new"
payment_processor = "/tmp/pp"

[versions]
console_wallet = "1.0.0"
minotari_cli = "1.0.0"
base_node = "1.0.0"
"#
    }

    /// Helper: parse TOML string into Config.
    fn parse(s: &str) -> Config {
        toml::from_str(s).expect("valid TOML config")
    }

    #[test]
    fn loads_full_config() {
        let c = parse(sample_toml());
        assert_eq!(c.benchmark.a_fund, 3_000_000_000_000);
        assert_eq!(c.benchmark.c_min, 4);
        assert_eq!(c.benchmark.volume_target, 512);
        assert_eq!(c.benchmark.doubling_rounds, 6);
        assert_eq!(c.benchmark.fanout_outputs_per_tx, 3);
        assert_eq!(c.benchmark.concurrent_batches, Vec::<u64>::new());
        assert_eq!(c.benchmark.s4_t_budget_secs, 120);
        assert_eq!(c.benchmark.s5_m, 12);
        assert_eq!(c.benchmark.s5_k, 4);
        assert_eq!(c.benchmark.tx_amount_ut, 1000);
        assert_eq!(c.benchmark.fee_rate, "2");
        assert_eq!(c.benchmark.scan_interval_secs, 1);
        assert_eq!(c.network.base_node_grpc_url, "grpc://localhost:18142");
        assert_eq!(c.network.base_node_http_url, "http://localhost:18143");
        assert_eq!(c.network.grpc_port, 18142);
        assert_eq!(c.passwords.old_wallet, "test");
        assert_eq!(c.passwords.new_wallet, "test");
        assert_eq!(c.passwords.payment_processor, "test");
        assert_eq!(c.passwords.library_wallet, "test");
        assert_eq!(c.paths.wallet_bin, "/usr/bin/minotari_console_wallet");
        assert_eq!(c.paths.minotari_bin, "/usr/bin/minotari");
        assert_eq!(c.paths.payment_processor_bin, "/usr/bin/minotari_payment_processor");
        assert_eq!(c.paths.console_wallet_bin, "/usr/bin/minotari_console_wallet");
        assert_eq!(c.data.old_wallet, "/tmp/old");
        assert_eq!(c.data.new_wallet, "/tmp/new");
        assert_eq!(c.data.payment_processor, "/tmp/pp");
        assert_eq!(c.versions.console_wallet, "1.0.0");
        assert_eq!(c.versions.minotari_cli, "1.0.0");
        assert_eq!(c.versions.base_node, "1.0.0");
    }

    #[test]
    fn errors_on_missing_required_fields() {
        let r: Result<Config, _> = toml::from_str("");
        assert!(r.is_err(), "empty TOML should fail without Default impl");

        let r: Result<Config, _> = toml::from_str("[benchmark]\na_fund = 1");
        assert!(r.is_err(), "partial TOML should fail");
    }

    #[test]
    fn errors_on_type_mismatch() {
        let r: Result<Config, _> = toml::from_str(
            &sample_toml().replace("grpc_port = 18142", "grpc_port = \"not-a-port\""),
        );
        assert!(r.is_err());
    }

    #[test]
    fn accepts_zero_values_for_all_numeric_fields() {
        let toml = sample_toml()
            .replace("a_fund = 3000000000000", "a_fund = 0")
            .replace("c_min = 4", "c_min = 0")
            .replace("volume_target = 512", "volume_target = 0")
            .replace("doubling_rounds = 6", "doubling_rounds = 0")
            .replace("fanout_outputs_per_tx = 3", "fanout_outputs_per_tx = 0")
            .replace("s4_t_budget_secs = 120", "s4_t_budget_secs = 0")
            .replace("s5_m = 12", "s5_m = 0")
            .replace("s5_k = 4", "s5_k = 0")
            .replace("tx_amount_ut = 1000", "tx_amount_ut = 0")
            .replace("scan_interval_secs = 1", "scan_interval_secs = 0")
            .replace("grpc_port = 18142", "grpc_port = 0");
        let c = parse(&toml);
        assert_eq!(c.benchmark.a_fund, 0);
        assert_eq!(c.benchmark.c_min, 0);
        assert_eq!(c.benchmark.volume_target, 0);
        assert_eq!(c.benchmark.doubling_rounds, 0);
        assert_eq!(c.benchmark.fanout_outputs_per_tx, 0);
        assert_eq!(c.benchmark.s4_t_budget_secs, 0);
        assert_eq!(c.benchmark.s5_m, 0);
        assert_eq!(c.benchmark.s5_k, 0);
        assert_eq!(c.benchmark.tx_amount_ut, 0);
        assert_eq!(c.benchmark.scan_interval_secs, 0);
        assert_eq!(c.network.grpc_port, 0);
    }

    #[test]
    fn accepts_empty_strings() {
        let toml = sample_toml()
            .replace("\"2\"", "\"\"")
            .replace("\"grpc://localhost:18142\"", "\"\"")
            .replace("\"http://localhost:18143\"", "\"\"")
            .replace("\"/usr/bin/minotari_console_wallet\"", "\"\"")
            .replace("\"/usr/bin/minotari\"", "\"\"")
            .replace("\"/usr/bin/minotari_payment_processor\"", "\"\"");
        let c = parse(&toml);
        assert_eq!(c.benchmark.fee_rate, "");
        assert_eq!(c.paths.wallet_bin, "");
        assert_eq!(c.paths.minotari_bin, "");
    }

    #[test]
    fn accepts_empty_concurrent_batches() {
        let c = parse(sample_toml());
        assert!(c.benchmark.concurrent_batches.is_empty());
    }

    #[test]
    fn accepts_populated_concurrent_batches() {
        let toml = sample_toml().replace("concurrent_batches = []", "concurrent_batches = [8, 16, 32]");
        let c = parse(&toml);
        assert_eq!(c.benchmark.concurrent_batches, vec![8, 16, 32]);
    }

    // ── proptest: config round-trip ──────────────────────────────────

    use proptest::prelude::*;

    fn arbitrary_string(max_len: usize) -> impl Strategy<Value = String> {
        "[a-zA-Z0-9/._:-]*".prop_filter("string within max_len", move |s| s.len() <= max_len)
    }

    prop_compose! {
        fn arbitrary_config()(
            a_fund in 0u64..1_000_000_000_000_000u64,
            c_min in 0u64..100u64,
            volume_target in 0u64..10_000u64,
            doubling_rounds in 0u64..20u64,
            fanout_outputs_per_tx in 0u64..100u64,
            concurrent_batches in proptest::collection::vec(0u64..100u64, 0..10),
            s4_t_budget_secs in 0u64..3600u64,
            s5_m in 0u64..1000u64,
            s5_k in 0u64..100u64,
            tx_amount_ut in 0u64..1_000_000u64,
            fee_rate in arbitrary_string(10),
            grpc_port in 0u16..65535u16,
        ) -> Config {
            Config {
                benchmark: BenchmarkParams {
                    a_fund, c_min, volume_target, doubling_rounds,
                    fanout_outputs_per_tx, concurrent_batches,
                    s4_t_budget_secs, s5_m, s5_k, tx_amount_ut,
                    fee_rate,
                    scan_interval_secs: 1,
                },
                paths: BinaryPaths {
                    wallet_bin: String::new(),
                    minotari_bin: String::new(),
                    payment_processor_bin: String::new(),
                    console_wallet_bin: String::new(),
                },
                network: NetworkConfig {
                    base_node_grpc_url: String::new(),
                    base_node_http_url: String::new(),
                    grpc_port,
                },
                passwords: Passwords {
                    old_wallet: String::new(),
                    new_wallet: String::new(),
                    payment_processor: String::new(),
                    library_wallet: String::new(),
                },
                data: DataDirs {
                    old_wallet: String::new(),
                    new_wallet: String::new(),
                    payment_processor: String::new(),
                },
                versions: VersionPins {
                    console_wallet: String::new(),
                    minotari_cli: String::new(),
                    base_node: String::new(),
                },
            }
        }
    }

    proptest! {
        #[test]
        fn config_toml_round_trip(cfg in arbitrary_config()) {
            let serialized = toml::to_string(&cfg).unwrap();
            let deserialized: Config = toml::from_str(&serialized).unwrap();
            assert_eq!(deserialized.benchmark.a_fund, cfg.benchmark.a_fund);
            assert_eq!(deserialized.benchmark.c_min, cfg.benchmark.c_min);
            assert_eq!(deserialized.benchmark.volume_target, cfg.benchmark.volume_target);
            assert_eq!(deserialized.benchmark.doubling_rounds, cfg.benchmark.doubling_rounds);
            assert_eq!(deserialized.benchmark.fanout_outputs_per_tx, cfg.benchmark.fanout_outputs_per_tx);
            assert_eq!(deserialized.benchmark.concurrent_batches, cfg.benchmark.concurrent_batches);
            assert_eq!(deserialized.benchmark.s4_t_budget_secs, cfg.benchmark.s4_t_budget_secs);
            assert_eq!(deserialized.benchmark.s5_m, cfg.benchmark.s5_m);
            assert_eq!(deserialized.benchmark.s5_k, cfg.benchmark.s5_k);
            assert_eq!(deserialized.benchmark.tx_amount_ut, cfg.benchmark.tx_amount_ut);
            assert_eq!(deserialized.benchmark.fee_rate, cfg.benchmark.fee_rate);
            assert_eq!(deserialized.network.grpc_port, cfg.network.grpc_port);
        }
    }
}
