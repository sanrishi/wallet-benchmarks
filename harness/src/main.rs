mod config;
mod driver;
mod drivers;
mod metrics;
mod scenarios;

use std::fs::{read_to_string, File};
use std::path::PathBuf;

use anyhow::Context;
use config::Config;
use driver::WalletDriver;
use drivers::new_wallet::NewWalletDriver;
use drivers::old_wallet::OldWalletDriver;
use drivers::payment_processor::PaymentProcessorDriver;
use metrics::{BenchmarkReport, ScenarioResult};
use scenarios::*;
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
) -> Vec<ScenarioResult> {
    let mut scenarios = Vec::new();

    let _ = restart_old_wallet_for_scan(old_wallet).await;
    print_old_wallet_address(old_wallet).await;
    let guard = OldWalletGuard { driver: old_wallet };

    scenarios.push(match run_b0(&*guard.driver).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("B0", e.to_string()),
    });
    let s0 = match run_s0(&*guard.driver, config).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S0", e.to_string()),
    };
    let h_birth = s0.recorded_birth_height.unwrap_or(0);
    scenarios.push(s0);
    scenarios.push(match run_s1(&*guard.driver, config).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S1", e.to_string()),
    });
    let post_s1_balance = guard.driver.get_balance().await.unwrap_or(0);

    let _ = restart_old_wallet_for_scan(guard.driver).await;
    scenarios.push(match run_s2(&*guard.driver, post_s1_balance).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S2", e.to_string()),
    });

    let _ = restart_old_wallet_for_scan(guard.driver).await;
    scenarios.push(
        match run_s3(&*guard.driver, h_birth, post_s1_balance).await {
            Ok(s) => s,
            Err(e) => scenario_error_result("S3", e.to_string()),
        },
    );

    scenarios.push(match run_s4(&*guard.driver, config).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S4", e.to_string()),
    });
    scenarios.push(match run_s5(&*guard.driver, config).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S5", e.to_string()),
    });
    let post_s5_balance = guard.driver.get_balance().await.unwrap_or(0);

    let _ = restart_old_wallet_for_scan(guard.driver).await;
    scenarios.push(match run_s6(&*guard.driver, post_s5_balance).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S6", e.to_string()),
    });

    let _ = restart_old_wallet_for_scan(guard.driver).await;
    scenarios.push(
        match run_s7(&*guard.driver, h_birth, post_s5_balance).await {
            Ok(s) => s,
            Err(e) => scenario_error_result("S7", e.to_string()),
        },
    );

    scenarios
}

async fn restart_old_wallet_for_scan(
    old_wallet: &mut OldWalletDriver,
) -> anyhow::Result<()> {
    old_wallet.stop();
    old_wallet.reset().await?;
    old_wallet.set_seed_birthday()?;
    old_wallet.start().await
}

async fn print_old_wallet_address(old_wallet: &OldWalletDriver) {
    match old_wallet.get_wallet_address().await {
        Ok(address) => println!("B0 old_wallet address: {address}"),
        Err(error) => eprintln!("B0 old_wallet address unavailable: {error}"),
    }
}

async fn print_all_wallet_addresses(config: &Config) -> anyhow::Result<()> {
    let mut old_wallet = OldWalletDriver::new(
        require_nonempty_path("wallet_bin_path", &config.wallet_bin_path)?,
        require_nonempty_path("old_wallet_data_dir", &config.old_wallet_data_dir)?,
        config.old_wallet_password.clone(),
        config.grpc_port,
        config.base_node_http_url.clone(),
        config.c_min,
    )?;
    old_wallet.start().await?;
    let old_wallet_result = old_wallet.get_self_address().await;
    old_wallet.stop();
    println!("old_wallet: {}", old_wallet_result?);

    let new_wallet = NewWalletDriver::new(
        require_nonempty_path("minotari_bin_path", &config.minotari_bin_path)?,
        require_nonempty_path("new_wallet_data_dir", &config.new_wallet_data_dir)?,
        config.base_node_http_url.clone(),
        config.c_min,
        config.new_wallet_password.clone(),
    )?;
    println!("new_wallet: {}", new_wallet.get_self_address().await?);

    let payment_processor = PaymentProcessorDriver::new(
        require_nonempty_path(
            "payment_processor_bin_path",
            &config.payment_processor_bin_path,
        )?,
        require_nonempty_path(
            "payment_processor_data_dir",
            &config.payment_processor_data_dir,
        )?,
        require_nonempty_path("minotari_bin_path", &config.minotari_bin_path)?,
        require_nonempty_path("wallet_bin_path", &config.wallet_bin_path)?,
        config.base_node_http_url.clone(),
        config.c_min,
        config.payment_processor_password.clone(),
    )?;
    println!(
        "payment_processor: {}",
        payment_processor.get_self_address().await?
    );

    Ok(())
}

fn require_nonempty_path(label: &str, value: &str) -> anyhow::Result<PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("{label} must not be empty"));
    }
    Ok(PathBuf::from(trimmed))
}

fn scan_duration(scenarios: &[ScenarioResult], name: &str) -> Option<f64> {
    scenarios
        .iter()
        .find(|scenario| scenario.scenario_name == name)
        .and_then(|scenario| scenario.scan_metrics.as_ref())
        .map(|scan| scan.wall_clock_secs)
}

fn tx_duration_sum(metrics: &[metrics::TxMetrics]) -> f64 {
    metrics
        .iter()
        .map(|tx| {
            tx.construction_secs + tx.broadcast_to_mempool_secs + tx.broadcast_to_confirmed_secs
        })
        .sum()
}

fn build_report(
    cpu_model: String,
    ram_kb: u64,
    os: String,
    network_path: String,
    wallet_mode: String,
    config_snapshot: serde_json::Value,
    scenarios: Vec<ScenarioResult>,
    config: &Config,
) -> BenchmarkReport {
    let scan_delta_s2_minus_b0 = match (
        scan_duration(&scenarios, "B0"),
        scan_duration(&scenarios, "S2"),
    ) {
        (Some(b0), Some(s2)) => Some(s2 - b0),
        _ => None,
    };
    let scan_delta_s6_minus_s2 = match (
        scan_duration(&scenarios, "S2"),
        scan_duration(&scenarios, "S6"),
    ) {
        (Some(s2), Some(s6)) => Some(s6 - s2),
        _ => None,
    };
    let s5_throughput_multiplier = scenarios
        .iter()
        .find(|scenario| scenario.scenario_name == "S5")
        .and_then(|scenario| {
            let num_batch_txs = (config.s5_m / config.s5_k.max(1)) as usize;
            let num_individual_txs = config.s5_m as usize;
            if scenario.tx_metrics.len() < num_batch_txs + num_individual_txs || num_batch_txs == 0
            {
                return None;
            }
            let batch_duration = tx_duration_sum(&scenario.tx_metrics[..num_batch_txs]);
            let individual_duration = tx_duration_sum(
                &scenario.tx_metrics[num_batch_txs..num_batch_txs + num_individual_txs],
            );
            if batch_duration > 0.0 {
                Some(individual_duration / batch_duration)
            } else {
                None
            }
        });

    BenchmarkReport {
        cpu_model,
        ram_kb,
        os,
        disk_type: "unknown".to_string(),
        network_path,
        console_wallet_version: config.console_wallet_version.clone(),
        minotari_cli_version: config.minotari_cli_version.clone(),
        base_node_version: config.base_node_version.clone(),
        scan_delta_s2_minus_b0,
        scan_delta_s6_minus_s2,
        s5_throughput_multiplier,
        wallet_mode,
        config_snapshot,
        scenarios,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let print_addresses = args.iter().any(|arg| arg == "--print-addresses");
    let config_path = args
        .iter()
        .skip_while(|a| a.as_str() != "--config")
        .nth(1)
        .map(|arg| PathBuf::from(arg.as_str()))
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let config_raw = read_to_string(&config_path)
        .with_context(|| format!("failed to read config from {}", config_path.display()))?;
    let config: Config = toml::from_str(&config_raw)
        .with_context(|| format!("failed to parse config from {}", config_path.display()))?;

    if print_addresses {
        print_all_wallet_addresses(&config).await?;
        return Ok(());
    }

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
        require_nonempty_path("wallet_bin_path", &config.wallet_bin_path)?,
        require_nonempty_path("old_wallet_data_dir", &config.old_wallet_data_dir)?,
        config.old_wallet_password.clone(),
        config.grpc_port,
        config.base_node_http_url.clone(),
        config.c_min,
    )?;
    let old_wallet_scenarios = run_old_wallet_scenarios(&mut old_wallet, &config).await;
    reports.push(build_report(
        cpu_model.clone(),
        ram_kb,
        os.clone(),
        format!("remote:{}", config.base_node_grpc_url),
        old_wallet.mode_name().to_string(),
        config_snapshot.clone(),
        old_wallet_scenarios,
        &config,
    ));

    let new_wallet = NewWalletDriver::new(
        require_nonempty_path("minotari_bin_path", &config.minotari_bin_path)?,
        require_nonempty_path("new_wallet_data_dir", &config.new_wallet_data_dir)?,
        config.base_node_http_url.clone(),
        config.c_min,
        config.new_wallet_password.clone(),
    );
    let (new_wallet_mode_name, new_wallet_scenarios) = match new_wallet {
        Ok(new_wallet) => {
            let mode_name = new_wallet.mode_name().to_string();
            let scenarios = match run_all_scenarios(&new_wallet, &config).await {
                Ok(scenarios) => scenarios,
                Err(error) => {
                    eprintln!("new_wallet failed: {error}");
                    vec![]
                }
            };
            (mode_name, scenarios)
        }
        Err(error) => {
            eprintln!("new_wallet initialization failed: {error}");
            ("new_wallet".to_string(), vec![])
        }
    };
    reports.push(build_report(
        cpu_model.clone(),
        ram_kb,
        os.clone(),
        format!("remote:{}", config.base_node_grpc_url),
        new_wallet_mode_name,
        config_snapshot.clone(),
        new_wallet_scenarios,
        &config,
    ));

    let (payment_processor_mode_name, payment_processor_scenarios) =
        match PaymentProcessorDriver::new(
            require_nonempty_path(
                "payment_processor_bin_path",
                &config.payment_processor_bin_path,
            )?,
            require_nonempty_path(
                "payment_processor_data_dir",
                &config.payment_processor_data_dir,
            )?,
            require_nonempty_path("minotari_bin_path", &config.minotari_bin_path)?,
            require_nonempty_path("wallet_bin_path", &config.wallet_bin_path)?,
            config.base_node_http_url.clone(),
            config.c_min,
            config.payment_processor_password.clone(),
        ) {
            Ok(mut pp) => match pp.start_daemon().await {
                Ok(()) => {
                    let mode_name = pp.mode_name().to_string();
                    let scenarios = match run_all_scenarios(&pp, &config).await {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("payment_processor scenarios failed: {e}");
                            vec![]
                        }
                    };
                    let _ = pp.stop_daemon().await;
                    (mode_name, scenarios)
                }
                Err(e) => {
                    eprintln!("payment_processor daemon start failed: {e}");
                    (pp.mode_name().to_string(), vec![])
                }
            },
            Err(error) => {
                eprintln!("payment_processor initialization failed: {error}");
                ("payment_processor".to_string(), vec![])
            }
        };
    reports.push(build_report(
        cpu_model,
        ram_kb,
        os,
        format!("remote:{}", config.base_node_grpc_url),
        payment_processor_mode_name,
        config_snapshot,
        payment_processor_scenarios,
        &config,
    ));

    let report_path = std::env::current_dir()
        .context("failed to resolve current working directory for report output")?
        .join("baseline_profile.json");
    let report_file = File::create(&report_path)
        .with_context(|| format!("failed to create {}", report_path.display()))?;
    serde_json::to_writer_pretty(report_file, &reports)
        .with_context(|| format!("failed to write {}", report_path.display()))?;

    Ok(())
}
