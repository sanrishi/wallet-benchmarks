mod config;
mod driver;
mod drivers;
mod metrics;
mod scenarios;

use std::fs::{read_to_string, File};
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use config::Config;
use driver::WalletDriver;
use drivers::new_wallet::NewWalletDriver;
use drivers::old_wallet::OldWalletDriver;
use drivers::payment_processor::PaymentProcessorDriver;
use metrics::BenchmarkReport;
use scenarios::run_all_scenarios;
use sysinfo::System;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_path = std::env::args()
        .skip_while(|a| a != "--config")
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let config_raw = read_to_string(&config_path)
        .with_context(|| format!("failed to read config from {}", config_path.display()))?;
    let config: Config = toml::from_str(&config_raw)
        .with_context(|| format!("failed to parse config from {}", config_path.display()))?;

    let config_snapshot = serde_json::to_value(&config).context("failed to serialize config")?;
    let mut system = System::new_all();
    system.refresh_all();

    let cpu_model = system
        .cpus()
        .first()
        .map(|cpu| cpu.brand().to_string())
        .filter(|brand| !brand.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let ram_kb = system.total_memory() / 1024;
    let os = System::long_os_version()
        .or_else(System::name)
        .unwrap_or_else(|| "unknown".to_string());

    let mut reports = Vec::new();

    let mut old_wallet = OldWalletDriver::new(
        PathBuf::from(&config.wallet_bin_path),
        PathBuf::from(&config.old_wallet_data_dir),
        config.grpc_port,
    );
    old_wallet.start().await?;
    let old_wallet_scenarios = run_all_scenarios(&old_wallet, &config).await?;
    old_wallet.stop();
    reports.push(BenchmarkReport {
        cpu_model: cpu_model.clone(),
        ram_kb,
        os: os.clone(),
        wallet_mode: old_wallet.mode_name().to_string(),
        config_snapshot: config_snapshot.clone(),
        scenarios: old_wallet_scenarios,
    });

    let new_wallet = NewWalletDriver::new(
        PathBuf::from(&config.new_wallet_data_dir),
        config.base_node_http_url.clone(),
    );
    let new_wallet_scenarios = run_all_scenarios(&new_wallet, &config).await?;
    reports.push(BenchmarkReport {
        cpu_model: cpu_model.clone(),
        ram_kb,
        os: os.clone(),
        wallet_mode: new_wallet.mode_name().to_string(),
        config_snapshot: config_snapshot.clone(),
        scenarios: new_wallet_scenarios,
    });

    let payment_processor = PaymentProcessorDriver::new(
        PathBuf::from(&config.payment_processor_data_dir),
        config.base_node_http_url.clone(),
    );
    let payment_processor_scenarios = run_all_scenarios(&payment_processor, &config).await?;
    reports.push(BenchmarkReport {
        cpu_model,
        ram_kb,
        os,
        wallet_mode: payment_processor.mode_name().to_string(),
        config_snapshot,
        scenarios: payment_processor_scenarios,
    });

    let report_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("baseline_profile.json");
    let report_file = File::create(&report_path)
        .with_context(|| format!("failed to create {}", report_path.display()))?;
    serde_json::to_writer_pretty(report_file, &reports)
        .with_context(|| format!("failed to write {}", report_path.display()))?;

    Ok(())
}
