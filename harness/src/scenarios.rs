use std::time::{Duration, Instant};

use anyhow::anyhow;
use futures::future::join_all;

use crate::config::Config;
use crate::driver::WalletDriver;
use crate::metrics::{ScenarioResult, TxMetrics};

const REDISCOVERY_TARGET: u64 = 512;
const POLL_INTERVAL_MS: u64 = 500;
const CONFIRMATION_TIMEOUT_SECS: u64 = 300;

pub async fn run_b0(driver: &dyn WalletDriver) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let scan_metrics = driver.scan_from_genesis().await?;
    let observed_balance = driver.get_balance().await?;

    Ok(ScenarioResult {
        scenario_name: "B0".to_string(),
        wall_clock_secs: started_at.elapsed().as_secs_f64(),
        total_fees: 0,
        success_count: 1,
        failure_count: 0,
        balance_delta: 0_i64.saturating_sub(observed_balance as i64),
        tx_metrics: Vec::new(),
        scan_metrics: Some(scan_metrics),
    })
}

pub async fn run_s0(driver: &dyn WalletDriver, config: &Config) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let h_birth = driver.get_tip_height().await?;
    let deadline = Instant::now() + Duration::from_secs(CONFIRMATION_TIMEOUT_SECS);
    loop {
        let balance = driver.get_balance().await?;
        if balance >= config.a_fund {
            break;
        }
        if Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
    let observed_balance = driver.get_balance().await?;
    let tip_after = driver.get_tip_height().await?;
    let _ = (h_birth, tip_after);

    Ok(ScenarioResult {
        scenario_name: "S0".to_string(),
        wall_clock_secs: started_at.elapsed().as_secs_f64(),
        total_fees: 0,
        success_count: u64::from(observed_balance >= config.a_fund),
        failure_count: u64::from(observed_balance < config.a_fund),
        balance_delta: config.a_fund as i64 - observed_balance as i64,
        tx_metrics: Vec::new(),
        scan_metrics: None,
    })
}

pub async fn run_s1(driver: &dyn WalletDriver, config: &Config) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let initial_balance = driver.get_balance().await?;
    let mut expected_balance = initial_balance;
    let mut tx_metrics = Vec::new();
    let fee_rate = parse_fee_rate(config);
    let amount_per_tx = (config.a_fund / config.volume_target.max(1)).max(1);
    let self_address = driver.get_self_address().await?;

    for round in 0..config.doubling_rounds {
        let round_tx_count = 1_u64 << round;
        for tx_index in 0..round_tx_count {
            let tx = attempt_send_single(
                driver,
                &self_address,
                amount_per_tx,
                fee_rate,
            )
            .await;
            if tx.success {
                expected_balance = expected_balance
                    .saturating_sub(amount_per_tx.saturating_add(tx.fee_paid));
            }
            let failed = !tx.success;
            tx_metrics.push(tx);
            if failed {
                let observed_balance = driver.get_balance().await?;
                return Ok(finalize_scenario(
                    "S1",
                    started_at,
                    expected_balance,
                    observed_balance,
                    tx_metrics,
                    None,
                ));
            }
        }
    }

    let fanout_tx_count = 1_u64 << config.doubling_rounds.saturating_sub(0);
    for index in 0..fanout_tx_count {
        let tx = attempt_send_single(
            driver,
            &self_address,
            amount_per_tx,
            fee_rate,
        )
        .await;
        if tx.success {
            expected_balance =
                expected_balance.saturating_sub(amount_per_tx.saturating_add(tx.fee_paid));
        }
        let failed = !tx.success;
        tx_metrics.push(tx);
        if failed {
            let observed_balance = driver.get_balance().await?;
            return Ok(finalize_scenario(
                "S1",
                started_at,
                expected_balance,
                observed_balance,
                tx_metrics,
                None,
            ));
        }
    }

    let observed_balance = driver.get_balance().await?;
    Ok(finalize_scenario(
        "S1",
        started_at,
        expected_balance,
        observed_balance,
        tx_metrics,
        None,
    ))
}

pub async fn run_s2(driver: &dyn WalletDriver) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let scan_metrics = driver.scan_from_genesis().await?;
    let observed_balance = driver.get_balance().await?;
    let expected_balance = observed_balance;
    let success_count = u64::from(scan_metrics.outputs_found >= REDISCOVERY_TARGET);
    let failure_count = u64::from(scan_metrics.outputs_found < REDISCOVERY_TARGET);

    Ok(ScenarioResult {
        scenario_name: "S2".to_string(),
        wall_clock_secs: started_at.elapsed().as_secs_f64(),
        total_fees: 0,
        success_count,
        failure_count,
        balance_delta: expected_balance as i64 - observed_balance as i64,
        tx_metrics: Vec::new(),
        scan_metrics: Some(scan_metrics),
    })
}

pub async fn run_s3(driver: &dyn WalletDriver, h_birth: u64) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let scan_metrics = driver.scan_from_birthday(h_birth).await?;
    let observed_balance = driver.get_balance().await?;
    let expected_balance = observed_balance;
    let success_count = u64::from(scan_metrics.outputs_found >= REDISCOVERY_TARGET);
    let failure_count = u64::from(scan_metrics.outputs_found < REDISCOVERY_TARGET);

    Ok(ScenarioResult {
        scenario_name: "S3".to_string(),
        wall_clock_secs: started_at.elapsed().as_secs_f64(),
        total_fees: 0,
        success_count,
        failure_count,
        balance_delta: expected_balance as i64 - observed_balance as i64,
        tx_metrics: Vec::new(),
        scan_metrics: Some(scan_metrics),
    })
}

pub async fn run_s4(driver: &dyn WalletDriver, config: &Config) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let initial_balance = driver.get_balance().await?;
    let mut expected_balance = initial_balance;
    let mut tx_metrics = Vec::new();
    let fee_rate = parse_fee_rate(config);
    let per_tx_timeout = Duration::from_secs(config.s4_t_budget_secs);
    let self_address = driver.get_self_address().await?;

    for batch_size in &config.concurrent_batches {
        let futures = (0..*batch_size)
            .map(|_| {
                let address = self_address.clone();
                async move {
                    match tokio::time::timeout(
                        per_tx_timeout,
                        driver.send_single(&address, 1, fee_rate),
                    )
                    .await
                    {
                        Ok(Ok(tx)) => tx,
                        Ok(Err(error)) => TxMetrics {
                            tx_id: String::new(),
                            construction_secs: 0.0,
                            broadcast_to_mempool_secs: 0.0,
                            broadcast_to_confirmed_secs: 0.0,
                            fee_paid: 0,
                            success: false,
                            error: Some(error.to_string()),
                        },
                        Err(_) => TxMetrics {
                            tx_id: String::new(),
                            construction_secs: per_tx_timeout.as_secs_f64(),
                            broadcast_to_mempool_secs: 0.0,
                            broadcast_to_confirmed_secs: 0.0,
                            fee_paid: 0,
                            success: false,
                            error: Some(format!(
                                "send_single timed out after {}s",
                                config.s4_t_budget_secs
                            )),
                        },
                    }
                }
            })
            .collect::<Vec<_>>();

        let batch_results = join_all(futures).await;
        for tx in batch_results {
            if tx.success {
                expected_balance =
                    expected_balance.saturating_sub(1_u64.saturating_add(tx.fee_paid));
            }
            tx_metrics.push(tx);
        }
    }

    let observed_balance = driver.get_balance().await?;
    Ok(finalize_scenario(
        "S4",
        started_at,
        expected_balance,
        observed_balance,
        tx_metrics,
        None,
    ))
}

pub async fn run_s5(driver: &dyn WalletDriver, config: &Config) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let initial_balance = driver.get_balance().await?;
    let mut expected_balance = initial_balance;
    let mut tx_metrics = Vec::new();
    let fee_rate = parse_fee_rate(config);
    let self_address = driver.get_self_address().await?;

    let num_batch_txs = config.s5_m / config.s5_k.max(1);
    for batch_index in 0..num_batch_txs {
        let recipients = (0..config.s5_k)
            .map(|_| (self_address.clone(), 1_u64))
            .collect::<Vec<_>>();
        let tx = attempt_send_batch(driver, recipients.clone(), fee_rate).await;
        if tx.success {
            let transferred: u64 = recipients.iter().map(|(_, amount)| *amount).sum();
            expected_balance = expected_balance.saturating_sub(transferred.saturating_add(tx.fee_paid));
        }
        tx_metrics.push(tx);
    }

    for index in 0..config.s5_m {
        let tx = attempt_send_single(
            driver,
            &self_address,
            1,
            fee_rate,
        )
        .await;
        if tx.success {
            expected_balance = expected_balance.saturating_sub(1_u64.saturating_add(tx.fee_paid));
        }
        tx_metrics.push(tx);
    }

    let observed_balance = driver.get_balance().await?;
    Ok(finalize_scenario(
        "S5",
        started_at,
        expected_balance,
        observed_balance,
        tx_metrics,
        None,
    ))
}

pub async fn run_s6(driver: &dyn WalletDriver) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let scan_metrics = driver.scan_from_genesis().await?;
    let observed_balance = driver.get_balance().await?;
    let expected_balance = observed_balance;

    Ok(ScenarioResult {
        scenario_name: "S6".to_string(),
        wall_clock_secs: started_at.elapsed().as_secs_f64(),
        total_fees: 0,
        success_count: 1,
        failure_count: 0,
        balance_delta: expected_balance as i64 - observed_balance as i64,
        tx_metrics: Vec::new(),
        scan_metrics: Some(scan_metrics),
    })
}

pub async fn run_s7(driver: &dyn WalletDriver, h_birth: u64) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let scan_metrics = driver.scan_from_birthday(h_birth).await?;
    let observed_balance = driver.get_balance().await?;
    let expected_balance = observed_balance;

    Ok(ScenarioResult {
        scenario_name: "S7".to_string(),
        wall_clock_secs: started_at.elapsed().as_secs_f64(),
        total_fees: 0,
        success_count: 1,
        failure_count: 0,
        balance_delta: expected_balance as i64 - observed_balance as i64,
        tx_metrics: Vec::new(),
        scan_metrics: Some(scan_metrics),
    })
}

pub async fn run_all_scenarios(
    driver: &dyn WalletDriver,
    config: &Config,
) -> anyhow::Result<Vec<ScenarioResult>> {
    let mut scenarios = Vec::new();

    scenarios.push(run_b0(driver).await?);
    scenarios.push(run_s0(driver, config).await?);
    scenarios.push(run_s1(driver, config).await?);

    let h_birth = driver.get_tip_height().await.unwrap_or(0);
    scenarios.push(run_s2(driver).await?);
    scenarios.push(run_s3(driver, h_birth).await?);
    scenarios.push(run_s4(driver, config).await?);
    scenarios.push(run_s5(driver, config).await?);

    let h_birth_after_s5 = driver.get_tip_height().await.unwrap_or(h_birth);
    scenarios.push(run_s6(driver).await?);
    scenarios.push(run_s7(driver, h_birth_after_s5).await?);

    Ok(scenarios)
}

fn finalize_scenario(
    scenario_name: &str,
    started_at: Instant,
    expected_balance: u64,
    observed_balance: u64,
    tx_metrics: Vec<TxMetrics>,
    scan_metrics: Option<crate::metrics::ScanMetrics>,
) -> ScenarioResult {
    let total_fees = tx_metrics.iter().map(|tx| tx.fee_paid).sum();
    let success_count = tx_metrics.iter().filter(|tx| tx.success).count() as u64;
    let failure_count = tx_metrics.len() as u64 - success_count;

    ScenarioResult {
        scenario_name: scenario_name.to_string(),
        wall_clock_secs: started_at.elapsed().as_secs_f64(),
        total_fees,
        success_count,
        failure_count,
        balance_delta: expected_balance as i64 - observed_balance as i64,
        tx_metrics,
        scan_metrics,
    }
}

async fn attempt_send_single(
    driver: &dyn WalletDriver,
    to_address: &str,
    amount_ut: u64,
    fee_rate: u64,
) -> TxMetrics {
    let started_at = Instant::now();
    match driver.send_single(to_address, amount_ut, fee_rate).await {
        Ok(tx) => tx,
        Err(error) => TxMetrics {
            tx_id: String::new(),
            construction_secs: started_at.elapsed().as_secs_f64(),
            broadcast_to_mempool_secs: 0.0,
            broadcast_to_confirmed_secs: 0.0,
            fee_paid: 0,
            success: false,
            error: Some(error.to_string()),
        },
    }
}

async fn attempt_send_batch(
    driver: &dyn WalletDriver,
    recipients: Vec<(String, u64)>,
    fee_rate: u64,
) -> TxMetrics {
    let started_at = Instant::now();
    match driver.send_batch(recipients, fee_rate).await {
        Ok(tx) => tx,
        Err(error) => TxMetrics {
            tx_id: String::new(),
            construction_secs: started_at.elapsed().as_secs_f64(),
            broadcast_to_mempool_secs: 0.0,
            broadcast_to_confirmed_secs: 0.0,
            fee_paid: 0,
            success: false,
            error: Some(error.to_string()),
        },
    }
}

async fn wait_for_tip_height(
    driver: &dyn WalletDriver,
    expected_tip: u64,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let current_tip = driver.get_tip_height().await?;
        if current_tip >= expected_tip {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(anyhow!(
                "tip height did not reach {expected_tip} before timeout"
            ));
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

fn parse_fee_rate(config: &Config) -> u64 {
    config.fee_rate.parse::<u64>().unwrap_or(0)
}

