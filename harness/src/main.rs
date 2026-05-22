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
use drivers::old_wallet::OldWalletDriver;
use metrics::{BenchmarkReport, ScenarioResult};
use scenarios::{run_b0, run_s0, run_s1, run_s2, run_s3, run_s4, run_s5, run_s6, run_s7};
use sysinfo::System;

struct OldWalletGuard<'a> {
    driver: &'a mut OldWalletDriver,
}

impl Drop for OldWalletGuard<'_> {
    fn drop(&mut self) {
        self.driver.stop();
    }
}

async fn run_old_wallet_scenarios(
    old_wallet: &mut OldWalletDriver,
    config: &Config,
) -> anyhow::Result<Vec<ScenarioResult>> {
    old_wallet.start().await?;
    let mut guard = OldWalletGuard { driver: old_wallet };
    let mut scenarios = Vec::new();
    scenarios.push(run_b0(&*guard.driver).await?);
    scenarios.push(run_s0(&*guard.driver, config).await?);
    scenarios.push(run_s1(&*guard.driver, config).await?);

    let h_birth = guard.driver.get_tip_height().await.unwrap_or(0);

    guard.driver.stop();
    guard.driver.reset().await?;
    guard.driver.start().await?;
    scenarios.push(run_s2(&*guard.driver).await?);

    guard.driver.stop();
    guard.driver.reset().await?;
    guard.driver.start().await?;
    scenarios.push(run_s3(&*guard.driver, h_birth).await?);

    scenarios.push(run_s4(&*guard.driver, config).await?);
    scenarios.push(run_s5(&*guard.driver, config).await?);

    let h_birth_after_s5 = guard.driver.get_tip_height().await.unwrap_or(h_birth);

    guard.driver.stop();
    guard.driver.reset().await?;
    guard.driver.start().await?;
    scenarios.push(run_s6(&*guard.driver).await?);

    guard.driver.stop();
    guard.driver.reset().await?;
    guard.driver.start().await?;
    scenarios.push(run_s7(&*guard.driver, h_birth_after_s5).await?);

    Ok(scenarios)
}

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
        config.old_wallet_password.clone(),
        config.grpc_port,
    );
    let old_wallet_scenarios = match run_old_wallet_scenarios(&mut old_wallet, &config).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("old_wallet failed: {e}");
            vec![]
        }
    };
    reports.push(BenchmarkReport {
        cpu_model: cpu_model.clone(),
        ram_kb,
        os: os.clone(),
        disk_type: "unknown".to_string(),
        network_path: format!("remote:{}", config.base_node_grpc_url),
        console_wallet_version: "pinned-see-README".to_string(),
        minotari_cli_version: "pinned-see-README".to_string(),
        base_node_version: "pinned-see-README".to_string(),
        scan_delta_s2_minus_b0: None,
        scan_delta_s6_minus_s2: None,
        s5_throughput_multiplier: None,
        wallet_mode: old_wallet.mode_name().to_string(),
        config_snapshot: config_snapshot.clone(),
        scenarios: old_wallet_scenarios,
    });

    // Mode 2 is disabled for MVP baseline runs until NewWalletDriver is implemented.
    // let new_wallet = NewWalletDriver::new(
    //     PathBuf::from(&config.new_wallet_data_dir),
    //     config.base_node_http_url.clone(),
    // );
    // let new_wallet_scenarios = run_all_scenarios(&new_wallet, &config).await?;
    // reports.push(BenchmarkReport {
    //     cpu_model: cpu_model.clone(),
    //     ram_kb,
    //     os: os.clone(),
    //     disk_type: "unknown".to_string(),
    //     network_path: format!("remote:{}", config.base_node_grpc_url),
    //     console_wallet_version: "pinned-see-README".to_string(),
    //     minotari_cli_version: "pinned-see-README".to_string(),
    //     base_node_version: "pinned-see-README".to_string(),
    //     scan_delta_s2_minus_b0: None,
    //     scan_delta_s6_minus_s2: None,
    //     s5_throughput_multiplier: None,
    //     wallet_mode: new_wallet.mode_name().to_string(),
    //     config_snapshot: config_snapshot.clone(),
    //     scenarios: new_wallet_scenarios,
    // });

    // Mode 3 is disabled for MVP baseline runs until PaymentProcessorDriver is implemented.
    // let payment_processor = PaymentProcessorDriver::new(
    //     PathBuf::from(&config.payment_processor_data_dir),
    //     config.base_node_http_url.clone(),
    // );
    // let payment_processor_scenarios = run_all_scenarios(&payment_processor, &config).await?;
    // reports.push(BenchmarkReport {
    //     cpu_model,
    //     ram_kb,
    //     os,
    //     disk_type: "unknown".to_string(),
    //     network_path: format!("remote:{}", config.base_node_grpc_url),
    //     console_wallet_version: "pinned-see-README".to_string(),
    //     minotari_cli_version: "pinned-see-README".to_string(),
    //     base_node_version: "pinned-see-README".to_string(),
    //     scan_delta_s2_minus_b0: None,
    //     scan_delta_s6_minus_s2: None,
    //     s5_throughput_multiplier: None,
    //     wallet_mode: payment_processor.mode_name().to_string(),
    //     config_snapshot,
    //     scenarios: payment_processor_scenarios,
    // });

    let report_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("baseline_profile.json");
    let report_file = File::create(&report_path)
        .with_context(|| format!("failed to create {}", report_path.display()))?;
    serde_json::to_writer_pretty(report_file, &reports)
        .with_context(|| format!("failed to write {}", report_path.display()))?;

    Ok(())
}
