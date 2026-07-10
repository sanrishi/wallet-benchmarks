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
#[cfg(feature = "library_wallet")]
use drivers::library_wallet::LibraryWalletDriver;
use drivers::old_wallet::OldWalletDriver;
use drivers::payment_processor::PaymentProcessorDriver;
use metrics::{BenchmarkReport, ScenarioResult};
use scenarios::*;
use sysinfo::System;

struct OldWalletGuard<'a> {
    driver: &'a OldWalletDriver,
}

impl Drop for OldWalletGuard<'_> {
    fn drop(&mut self) {
        self.driver.stop();
    }
}

async fn run_old_wallet_scenarios(
    old_wallet: &OldWalletDriver,
    config: &Config,
) -> Vec<ScenarioResult> {
    let mut scenarios = Vec::new();

    scenarios.push(match run_b0(old_wallet).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("B0", e.to_string()),
    });
    print_old_wallet_address(old_wallet).await;
    let _guard = OldWalletGuard { driver: old_wallet };

    let s0 = match run_s0(old_wallet, config).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S0", e.to_string()),
    };
    let h_birth = s0.recorded_birth_height.unwrap_or(0);
    scenarios.push(s0);
    scenarios.push(match run_s1(old_wallet, config).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S1", e.to_string()),
    });
    let post_s1_balance = old_wallet.get_balance().await.unwrap_or(0);

    // Wait for any pending S1 transactions before the reconstruction test
    if let Err(e) = old_wallet
        .await_all_pending(config.benchmark.confirmation_timeout_secs)
        .await
    {
        eprintln!("pending S1 transactions did not clear in time: {e}");
    }

    scenarios.push(match run_s2(old_wallet, post_s1_balance).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S2", e.to_string()),
    });

    scenarios.push(
        match run_s3(old_wallet, h_birth, post_s1_balance).await {
            Ok(s) => s,
            Err(e) => scenario_error_result("S3", e.to_string()),
        },
    );

    scenarios.push(match run_s4(old_wallet, config).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S4", e.to_string()),
    });
    scenarios.push(match run_s5(old_wallet, config).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S5", e.to_string()),
    });
    let post_s5_balance = old_wallet.get_balance().await.unwrap_or(0);

    // Wait for any pending S5 transactions before the reconstruction test
    if let Err(e) = old_wallet
        .await_all_pending(config.benchmark.confirmation_timeout_secs)
        .await
    {
        eprintln!("pending S5 transactions did not clear in time: {e}");
    }

    scenarios.push(match run_s6(old_wallet, post_s5_balance).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S6", e.to_string()),
    });

    scenarios.push(
        match run_s7(old_wallet, h_birth, post_s5_balance).await {
            Ok(s) => s,
            Err(e) => scenario_error_result("S7", e.to_string()),
        },
    );

    scenarios
}

async fn print_old_wallet_address(old_wallet: &OldWalletDriver) {
    match old_wallet.get_wallet_address().await {
        Ok(address) => println!("B0 old_wallet address: {address}"),
        Err(error) => eprintln!("B0 old_wallet address unavailable: {error}"),
    }
}

async fn print_all_wallet_addresses(config: &Config, use_library_wallet: bool) -> anyhow::Result<()> {
    let old_wallet = OldWalletDriver::new(
        require_nonempty_path("wallet_bin", &config.paths.wallet_bin)?,
        require_nonempty_path("old_wallet", &config.data.old_wallet)?,
        config.passwords.old_wallet.clone(),
        config.network.grpc_port,
        config.network.base_node_http_url.clone(),
        config.benchmark.c_min,
        config.benchmark.startup_timeout_secs,
        config.benchmark.confirmation_timeout_secs,
        config_seed_for(config, "old_wallet"),
    )?;
    old_wallet.start().await?;
    let old_wallet_result = old_wallet.get_self_address().await;
    old_wallet.stop();
    println!("old_wallet: {}", old_wallet_result?);

    if use_library_wallet {
        #[cfg(feature = "library_wallet")]
        {
            let library_wallet = LibraryWalletDriver::new(
                require_nonempty_path("minotari_bin", &config.paths.minotari_bin)?,
                require_nonempty_path("new_wallet", &config.data.new_wallet)?,
                config.network.base_node_http_url.clone(),
                config.benchmark.c_min,
                config.benchmark.startup_timeout_secs,
                config.passwords.library_wallet.clone(),
                config_seed_for(config, "library_wallet"),
            )?;
            println!("library_wallet: {}", library_wallet.get_self_address().await?);
        }
        #[cfg(not(feature = "library_wallet"))]
        {
            let _ = config;
            eprintln!("--library-wallet flag requires the library_wallet feature");
        }
    } else {
        let new_wallet = NewWalletDriver::new(
            require_nonempty_path("minotari_bin", &config.paths.minotari_bin)?,
            require_nonempty_path("new_wallet", &config.data.new_wallet)?,
            config.network.base_node_http_url.clone(),
            config.network.base_node_grpc_url.clone(),
            config.benchmark.c_min,
            config.benchmark.startup_timeout_secs,
            config.passwords.new_wallet.clone(),
            config_seed_for(config, "new_wallet"),
            require_nonempty_path("console_wallet_bin", &config.paths.console_wallet_bin)?,
        )?;
        println!("new_wallet: {}", new_wallet.get_self_address().await?);
    }

    let payment_processor = PaymentProcessorDriver::new(
        require_nonempty_path(
            "payment_processor_bin",
            &config.paths.payment_processor_bin,
        )?,
        require_nonempty_path(
            "payment_processor",
            &config.data.payment_processor,
        )?,
        require_nonempty_path("minotari_bin", &config.paths.minotari_bin)?,
        require_nonempty_path("console_wallet_bin", &config.paths.console_wallet_bin)?,
        config.network.base_node_http_url.clone(),
        config.benchmark.c_min,
        config.benchmark.startup_timeout_secs,
        config.passwords.payment_processor.clone(),
        config_seed_for(config, "payment_processor"),
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

fn config_seed_for(config: &Config, mode: &str) -> Option<String> {
    config.seeds.as_ref().and_then(|s| match mode {
        "old_wallet" => s.old_wallet.clone(),
        "new_wallet" => s.new_wallet.clone(),
        "payment_processor" => s.payment_processor.clone(),
        "library_wallet" => s.library_wallet.clone(),
        _ => None,
    })
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

#[allow(clippy::too_many_arguments)]
fn build_report(
    cpu_model: String,
    ram_kb: u64,
    os: String,
    network_path: String,
    wallet_mode: String,
    config_snapshot: serde_json::Value,
    scenarios: Vec<ScenarioResult>,
    config: &Config,
    setup_error: Option<String>,
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
            let num_batch_txs = if config.benchmark.s5_k > 0 {
                (config.benchmark.s5_m / config.benchmark.s5_k) as usize
            } else {
                0
            };
            let num_individual_txs = config.benchmark.s5_m as usize;
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
        console_wallet_version: config.versions.console_wallet.clone(),
        minotari_cli_version: config.versions.minotari_cli.clone(),
        base_node_version: config.versions.base_node.clone(),
        scan_delta_s2_minus_b0,
        scan_delta_s6_minus_s2,
        s5_throughput_multiplier,
        wallet_mode,
        config_snapshot,
        scenarios,
        error: setup_error,
    }
}

async fn run_new_wallet(config: &Config) -> (String, Vec<ScenarioResult>) {
    let new_wallet = (|| -> anyhow::Result<NewWalletDriver> {
        Ok(NewWalletDriver::new(
            require_nonempty_path("minotari_bin", &config.paths.minotari_bin)?,
            require_nonempty_path("new_wallet", &config.data.new_wallet)?,
            config.network.base_node_http_url.clone(),
            config.network.base_node_grpc_url.clone(),
            config.benchmark.c_min,
            config.benchmark.startup_timeout_secs,
            config.passwords.new_wallet.clone(),
            config_seed_for(config, "new_wallet"),
            require_nonempty_path("console_wallet_bin", &config.paths.console_wallet_bin)?,
        )?)
    })();
    match new_wallet {
        Ok(new_wallet) => {
            if let Ok(addr) = new_wallet.get_self_address().await {
                println!("new_wallet address: {addr}");
            }
            let mode_name = new_wallet.mode_name().to_string();
            let scenarios = match run_all_scenarios(&new_wallet, config).await {
                Ok(scenarios) => scenarios,
                Err(error) => {
                    eprintln!("new_wallet scenarios failed: {error}");
                    vec![]
                }
            };
            (mode_name, scenarios)
        }
        Err(error) => {
            eprintln!("new_wallet initialization failed: {error}");
            ("new_wallet".to_string(), vec![])
        }
    }
}

async fn run_single_scenario(config: &Config, name: &str) -> anyhow::Result<()> {
    let new_wallet = NewWalletDriver::new(
        require_nonempty_path("minotari_bin", &config.paths.minotari_bin)?,
        require_nonempty_path("new_wallet", &config.data.new_wallet)?,
        config.network.base_node_http_url.clone(),
        config.network.base_node_grpc_url.clone(),
        config.benchmark.c_min,
        config.benchmark.startup_timeout_secs,
        config.passwords.new_wallet.clone(),
        config_seed_for(config, "new_wallet"),
        require_nonempty_path("console_wallet_bin", &config.paths.console_wallet_bin)?,
    )?;
    if let Ok(addr) = new_wallet.get_self_address().await {
        println!("new_wallet address: {addr}");
    }

    println!("Running B0 (scan)...");
    let _b0 = run_b0(&new_wallet).await.map_err(|e| {
        eprintln!("B0 failed: {e}");
        e
    })?;

    let result = match name {
        "S0" => run_s0(&new_wallet, config).await?,
        "S1" => run_s1(&new_wallet, config).await?,
        "S4" => run_s4(&new_wallet, config).await?,
        "S5" => run_s5(&new_wallet, config).await?,
        other => anyhow::bail!("unsupported single scenario: {other} (use S0, S1, S4, or S5)"),
    };

    println!("{} result: {}", name, serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(feature = "library_wallet")]
async fn run_library_wallet(config: &Config) -> (String, Vec<ScenarioResult>) {
    let library_wallet = (|| -> anyhow::Result<LibraryWalletDriver> {
        Ok(LibraryWalletDriver::new(
            require_nonempty_path("minotari_bin", &config.paths.minotari_bin)?,
            require_nonempty_path("new_wallet", &config.data.new_wallet)?,
            config.network.base_node_http_url.clone(),
            config.benchmark.c_min,
            config.benchmark.startup_timeout_secs,
            config.passwords.library_wallet.clone(),
            config_seed_for(config, "library_wallet"),
        )?)
    })();
    match library_wallet {
        Ok(library_wallet) => {
            let mode_name = library_wallet.mode_name().to_string();
            let scenarios = match run_all_scenarios(&library_wallet, config).await {
                Ok(scenarios) => scenarios,
                Err(error) => {
                    eprintln!("library_wallet scenarios failed: {error}");
                    vec![]
                }
            };
            (mode_name, scenarios)
        }
        Err(error) => {
            eprintln!("library_wallet initialization failed: {error}");
            ("library_wallet".to_string(), vec![])
        }
    }
}

#[cfg(not(feature = "library_wallet"))]
async fn run_library_wallet(_config: &Config) -> (String, Vec::<ScenarioResult>) {
    eprintln!("--library-wallet flag requires the library_wallet feature: cargo check --features library_wallet");
    ("library_wallet".to_string(), vec![])
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = std::env::args().collect::<Vec<_>>();
    let print_addresses = args.iter().any(|arg| arg == "--print-addresses");
    let use_library_wallet = args.iter().any(|arg| arg == "--library-wallet");
    let scenario_name = args.iter().skip_while(|a| a.as_str() != "--scenario").nth(1).cloned();
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
        print_all_wallet_addresses(&config, use_library_wallet).await?;
        return Ok(());
    }

    if let Some(ref name) = scenario_name {
        return run_single_scenario(&config, name).await;
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

    let old_wallet = OldWalletDriver::new(
        require_nonempty_path("wallet_bin", &config.paths.wallet_bin)?,
        require_nonempty_path("old_wallet", &config.data.old_wallet)?,
        config.passwords.old_wallet.clone(),
        config.network.grpc_port,
        config.network.base_node_http_url.clone(),
        config.benchmark.c_min,
        config.benchmark.startup_timeout_secs,
        config.benchmark.confirmation_timeout_secs,
        config_seed_for(&config, "old_wallet"),
    )?;
    let old_wallet_scenarios = run_old_wallet_scenarios(&old_wallet, &config).await;
    reports.push(build_report(
        cpu_model.clone(),
        ram_kb,
        os.clone(),
        format!("remote:{}", config.network.base_node_grpc_url),
        old_wallet.mode_name().to_string(),
        config_snapshot.clone(),
        old_wallet_scenarios,
        &config,
        None,
    ));

    let (new_wallet_mode_name, new_wallet_scenarios) = if use_library_wallet {
        run_library_wallet(&config).await
    } else {
        run_new_wallet(&config).await
    };
    reports.push(build_report(
        cpu_model.clone(),
        ram_kb,
        os.clone(),
        format!("remote:{}", config.network.base_node_grpc_url),
        new_wallet_mode_name,
        config_snapshot.clone(),
        new_wallet_scenarios,
        &config,
        None,
    ));

    let (payment_processor_mode_name, payment_processor_scenarios, payment_processor_error) =
        match PaymentProcessorDriver::new(
            require_nonempty_path(
                "payment_processor_bin",
                &config.paths.payment_processor_bin,
            )?,
            require_nonempty_path(
                "payment_processor",
                &config.data.payment_processor,
            )?,
            require_nonempty_path("minotari_bin", &config.paths.minotari_bin)?,
            require_nonempty_path("console_wallet_bin", &config.paths.console_wallet_bin)?,
            config.network.base_node_http_url.clone(),
            config.benchmark.c_min,
            config.benchmark.startup_timeout_secs,
            config.passwords.payment_processor.clone(),
            config_seed_for(&config, "payment_processor"),
        ) {
            Ok(mut pp) => match pp.start_daemon().await {
                Ok(()) => {
                    if let Ok(addr) = pp.get_self_address().await {
                        println!("payment_processor address: {addr}");
                    }
                    let mode_name = pp.mode_name().to_string();
                    let (scenarios, err) = match run_all_scenarios(&pp, &config).await {
                        Ok(s) => (s, None),
                        Err(e) => {
                            let msg = format!("payment_processor scenarios failed: {e}");
                            eprintln!("{msg}");
                            (vec![], Some(msg))
                        }
                    };
                    let _ = pp.stop_daemon().await;
                    (mode_name, scenarios, err)
                }
                Err(e) => {
                    let msg = format!("payment_processor daemon start failed: {e}");
                    eprintln!("{msg}");
                    (pp.mode_name().to_string(), vec![], Some(msg))
                }
            },
            Err(error) => {
                let msg = format!("payment_processor initialization failed: {error}");
                eprintln!("{msg}");
                ("payment_processor".to_string(), vec![], Some(msg))
            }
        };
    reports.push(build_report(
        cpu_model,
        ram_kb,
        os,
        format!("remote:{}", config.network.base_node_grpc_url),
        payment_processor_mode_name,
        config_snapshot,
        payment_processor_scenarios,
        &config,
        payment_processor_error,
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
