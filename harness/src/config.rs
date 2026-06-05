use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
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
    pub base_node_grpc_url: String,
    pub base_node_http_url: String,
    pub console_wallet_version: String,
    pub minotari_cli_version: String,
    pub base_node_version: String,
    pub wallet_bin_path: String,
    pub minotari_bin_path: String,
    pub payment_processor_bin_path: String,
    pub old_wallet_password: String,
    pub new_wallet_password: String,
    pub payment_processor_password: String,
    pub old_wallet_data_dir: String,
    pub new_wallet_data_dir: String,
    pub payment_processor_data_dir: String,
    pub grpc_port: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_toml() -> &'static str {
        r#"
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
base_node_grpc_url = "grpc://localhost:18142"
base_node_http_url = "http://localhost:18143"
console_wallet_version = "1.0.0"
minotari_cli_version = "1.0.0"
base_node_version = "1.0.0"
wallet_bin_path = "/usr/bin/minotari_console_wallet"
minotari_bin_path = "/usr/bin/minotari"
payment_processor_bin_path = "/usr/bin/minotari_payment_processor"
old_wallet_password = "test"
new_wallet_password = "test"
payment_processor_password = "test"
old_wallet_data_dir = "/tmp/old"
new_wallet_data_dir = "/tmp/new"
payment_processor_data_dir = "/tmp/pp"
grpc_port = 18142
"#
    }

    /// Helper: parse TOML string into Config.
    fn parse(s: &str) -> Config {
        toml::from_str(s).expect("valid TOML config")
    }

    #[test]
    fn loads_full_config() {
        let c = parse(sample_toml());
        assert_eq!(c.a_fund, 3_000_000_000_000);
        assert_eq!(c.c_min, 4);
        assert_eq!(c.volume_target, 512);
        assert_eq!(c.doubling_rounds, 6);
        assert_eq!(c.fanout_outputs_per_tx, 3);
        assert_eq!(c.concurrent_batches, Vec::<u64>::new());
        assert_eq!(c.s4_t_budget_secs, 120);
        assert_eq!(c.s5_m, 12);
        assert_eq!(c.s5_k, 4);
        assert_eq!(c.tx_amount_ut, 1000);
        assert_eq!(c.fee_rate, "2");
        assert_eq!(c.base_node_grpc_url, "grpc://localhost:18142");
        assert_eq!(c.base_node_http_url, "http://localhost:18143");
        assert_eq!(c.grpc_port, 18142);
    }

    #[test]
    fn errors_on_missing_required_fields() {
        let r: Result<Config, _> = toml::from_str("");
        assert!(r.is_err(), "empty TOML should fail without Default impl");

        let r: Result<Config, _> = toml::from_str("a_fund = 1");
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
            .replace("grpc_port = 18142", "grpc_port = 0");
        let c = parse(&toml);
        assert_eq!(c.a_fund, 0);
        assert_eq!(c.c_min, 0);
        assert_eq!(c.grpc_port, 0);
        assert_eq!(c.s5_m, 0);
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
        assert_eq!(c.fee_rate, "");
        assert_eq!(c.wallet_bin_path, "");
    }

    #[test]
    fn accepts_empty_concurrent_batches() {
        let c = parse(sample_toml());
        assert!(c.concurrent_batches.is_empty());
    }

    #[test]
    fn accepts_populated_concurrent_batches() {
        let toml = sample_toml().replace("concurrent_batches = []", "concurrent_batches = [8, 16, 32]");
        let c = parse(&toml);
        assert_eq!(c.concurrent_batches, vec![8, 16, 32]);
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
                a_fund, c_min, volume_target, doubling_rounds,
                fanout_outputs_per_tx, concurrent_batches,
                s4_t_budget_secs, s5_m, s5_k, tx_amount_ut,
                fee_rate,
                base_node_grpc_url: String::new(),
                base_node_http_url: String::new(),
                console_wallet_version: String::new(),
                minotari_cli_version: String::new(),
                base_node_version: String::new(),
                wallet_bin_path: String::new(),
                minotari_bin_path: String::new(),
                payment_processor_bin_path: String::new(),
                old_wallet_password: String::new(),
                new_wallet_password: String::new(),
                payment_processor_password: String::new(),
                old_wallet_data_dir: String::new(),
                new_wallet_data_dir: String::new(),
                payment_processor_data_dir: String::new(),
                grpc_port,
            }
        }
    }

    proptest! {
        #[test]
        fn config_toml_round_trip(cfg in arbitrary_config()) {
            let serialized = toml::to_string(&cfg).unwrap();
            let deserialized: Config = toml::from_str(&serialized).unwrap();
            assert_eq!(deserialized.a_fund, cfg.a_fund);
            assert_eq!(deserialized.c_min, cfg.c_min);
            assert_eq!(deserialized.volume_target, cfg.volume_target);
            assert_eq!(deserialized.doubling_rounds, cfg.doubling_rounds);
            assert_eq!(deserialized.fanout_outputs_per_tx, cfg.fanout_outputs_per_tx);
            assert_eq!(deserialized.concurrent_batches, cfg.concurrent_batches);
            assert_eq!(deserialized.s4_t_budget_secs, cfg.s4_t_budget_secs);
            assert_eq!(deserialized.s5_m, cfg.s5_m);
            assert_eq!(deserialized.s5_k, cfg.s5_k);
            assert_eq!(deserialized.tx_amount_ut, cfg.tx_amount_ut);
            assert_eq!(deserialized.fee_rate, cfg.fee_rate);
            assert_eq!(deserialized.grpc_port, cfg.grpc_port);
        }
    }
}

