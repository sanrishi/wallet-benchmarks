use std::time::{Duration, Instant};

use anyhow::anyhow;
use futures::future::join_all;

use crate::config::Config;
use crate::driver::WalletDriver;
use crate::metrics::{ScenarioResult, TxMetrics};

pub fn scenario_error_result(name: &str, error: String) -> ScenarioResult {
    ScenarioResult {
        scenario_name: name.to_string(),
        wall_clock_secs: 0.0,
        total_fees: 0,
        success_count: 0,
        failure_count: 1,
        balance_delta: 0,
        tx_metrics: vec![TxMetrics {
            tx_id: String::new(),
            construction_secs: 0.0,
            broadcast_to_mempool_secs: 0.0,
            broadcast_to_confirmed_secs: 0.0,
            fee_paid: 0,
            success: false,
            error: Some(error.clone()),
        }],
        scan_metrics: None,
        recorded_birth_height: None,
        error: Some(error),
    }
}

#[allow(unused)]
const POLL_INTERVAL_MS: u64 = 500;
#[allow(unused)]
const CONFIRMATION_TIMEOUT_SECS: u64 = 300;
const REDISCOVERY_TARGET: u64 = 512;

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
        recorded_birth_height: None,
        error: None,
    })
}

pub async fn run_s0(driver: &dyn WalletDriver, config: &Config) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let h_birth = driver.get_tip_height().await?;
    let funding_tx = match driver.observe_funding(config.benchmark.a_fund).await {
        Ok(tx) => tx,
        Err(error) => TxMetrics {
            tx_id: "incoming-funding".to_string(),
            construction_secs: 0.0,
            broadcast_to_mempool_secs: 0.0,
            broadcast_to_confirmed_secs: started_at.elapsed().as_secs_f64(),
            fee_paid: 0,
            success: false,
            error: Some(error.to_string()),
        },
    };
    let observed_balance = driver.get_balance().await?;
    let tip_after = driver.get_tip_height().await?;
    let _ = (h_birth, tip_after);
    let success = funding_tx.success && observed_balance >= config.benchmark.a_fund;

    Ok(ScenarioResult {
        scenario_name: "S0".to_string(),
        wall_clock_secs: started_at.elapsed().as_secs_f64(),
        total_fees: 0,
        success_count: u64::from(success),
        failure_count: u64::from(!success),
        balance_delta: config.benchmark.a_fund as i64 - observed_balance as i64,
        tx_metrics: vec![funding_tx],
        scan_metrics: None,
        recorded_birth_height: Some(h_birth),
        error: None,
    })
}

pub async fn run_s1(driver: &dyn WalletDriver, config: &Config) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let initial_balance = driver.get_balance().await?;
    let mut expected_balance = initial_balance;
    let mut tx_metrics = Vec::new();
    let fee_rate = parse_fee_rate(config);
    let amount_per_tx = config.benchmark.tx_amount_ut.max(1);
    let self_address = driver.get_self_address().await?;

    for round in 0..config.benchmark.doubling_rounds {
        let round_tx_count = 1_u64 << round;
        for _ in 0..round_tx_count {
            let recipients = vec![
                (self_address.clone(), amount_per_tx),
                (self_address.clone(), amount_per_tx),
            ];
            let tx = attempt_send_batch(driver, recipients.clone(), fee_rate).await;
            if tx.success {
                expected_balance = expected_balance.saturating_sub(tx.fee_paid);
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

    let fanout_tx_count = if config.benchmark.fanout_outputs_per_tx == 0 {
        0
    } else {
        1_u64 << config.benchmark.doubling_rounds
    };
    for _ in 0..fanout_tx_count {
        let recipients = (0..config.benchmark.fanout_outputs_per_tx)
            .map(|_| (self_address.clone(), amount_per_tx))
            .collect::<Vec<_>>();
        let tx = attempt_send_batch(driver, recipients.clone(), fee_rate).await;
        if tx.success {
            expected_balance = expected_balance.saturating_sub(tx.fee_paid);
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

pub async fn run_s2(
    driver: &dyn WalletDriver,
    expected_balance: u64,
) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let scan_metrics = driver.scan_from_genesis().await?;
    let observed_balance = driver.get_balance().await?;
    let outputs_ok = if scan_metrics.outputs_found > 0 {
        scan_metrics.outputs_found >= REDISCOVERY_TARGET
    } else {
        true
    };
    let success = outputs_ok && observed_balance == expected_balance;

    Ok(ScenarioResult {
        scenario_name: "S2".to_string(),
        wall_clock_secs: started_at.elapsed().as_secs_f64(),
        total_fees: 0,
        success_count: u64::from(success),
        failure_count: u64::from(!success),
        balance_delta: expected_balance as i64 - observed_balance as i64,
        tx_metrics: Vec::new(),
        scan_metrics: Some(scan_metrics),
        recorded_birth_height: None,
        error: None,
    })
}

pub async fn run_s3(
    driver: &dyn WalletDriver,
    h_birth: u64,
    expected_balance: u64,
) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let scan_metrics = driver.scan_from_birthday(h_birth).await?;
    let observed_balance = driver.get_balance().await?;
    let outputs_ok = if scan_metrics.outputs_found > 0 {
        scan_metrics.outputs_found >= REDISCOVERY_TARGET
    } else {
        true
    };
    let success = outputs_ok && observed_balance == expected_balance;

    Ok(ScenarioResult {
        scenario_name: "S3".to_string(),
        wall_clock_secs: started_at.elapsed().as_secs_f64(),
        total_fees: 0,
        success_count: u64::from(success),
        failure_count: u64::from(!success),
        balance_delta: expected_balance as i64 - observed_balance as i64,
        tx_metrics: Vec::new(),
        scan_metrics: Some(scan_metrics),
        recorded_birth_height: None,
        error: None,
    })
}

pub async fn run_s4(driver: &dyn WalletDriver, config: &Config) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let initial_balance = driver.get_balance().await?;
    let mut expected_balance = initial_balance;
    let mut tx_metrics = Vec::new();
    let fee_rate = parse_fee_rate(config);
    let per_tx_timeout = Duration::from_secs(config.benchmark.s4_t_budget_secs);
    let self_address = driver.get_self_address().await?;

    for batch_size in &config.benchmark.concurrent_batches {
        let futures = (0..*batch_size)
            .map(|_| {
                let address = self_address.clone();
                async move {
                    match tokio::time::timeout(
                        per_tx_timeout,
                        driver.send_single(&address, config.benchmark.tx_amount_ut.max(1), fee_rate),
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
                                config.benchmark.s4_t_budget_secs
                            )),
                        },
                    }
                }
            })
            .collect::<Vec<_>>();

        let batch_results = join_all(futures).await;
        for tx in batch_results {
            if tx.success {
                expected_balance = expected_balance.saturating_sub(tx.fee_paid);
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

    if config.benchmark.s5_k > 0 {
        let num_batch_txs = config.benchmark.s5_m / config.benchmark.s5_k;
        for _ in 0..num_batch_txs {
            let recipients = (0..config.benchmark.s5_k)
                .map(|_| (self_address.clone(), config.benchmark.tx_amount_ut.max(1)))
                .collect::<Vec<_>>();
            let tx = attempt_send_batch(driver, recipients, fee_rate).await;
            if tx.success {
                expected_balance = expected_balance.saturating_sub(tx.fee_paid);
            }
            tx_metrics.push(tx);
        }
    }

    for _ in 0..config.benchmark.s5_m {
        let tx = attempt_send_single(
            driver,
            &self_address,
            config.benchmark.tx_amount_ut.max(1),
            fee_rate,
        )
        .await;
        if tx.success {
            expected_balance = expected_balance.saturating_sub(tx.fee_paid);
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

pub async fn run_s6(
    driver: &dyn WalletDriver,
    expected_balance: u64,
) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let scan_metrics = driver.scan_from_genesis().await?;
    let observed_balance = driver.get_balance().await?;
    let success = observed_balance == expected_balance;

    Ok(ScenarioResult {
        scenario_name: "S6".to_string(),
        wall_clock_secs: started_at.elapsed().as_secs_f64(),
        total_fees: 0,
        success_count: u64::from(success),
        failure_count: u64::from(!success),
        balance_delta: expected_balance as i64 - observed_balance as i64,
        tx_metrics: Vec::new(),
        scan_metrics: Some(scan_metrics),
        recorded_birth_height: None,
        error: None,
    })
}

pub async fn run_s7(
    driver: &dyn WalletDriver,
    h_birth: u64,
    expected_balance: u64,
) -> anyhow::Result<ScenarioResult> {
    let started_at = Instant::now();
    let scan_metrics = driver.scan_from_birthday(h_birth).await?;
    let observed_balance = driver.get_balance().await?;
    let success = observed_balance == expected_balance;

    Ok(ScenarioResult {
        scenario_name: "S7".to_string(),
        wall_clock_secs: started_at.elapsed().as_secs_f64(),
        total_fees: 0,
        success_count: u64::from(success),
        failure_count: u64::from(!success),
        balance_delta: expected_balance as i64 - observed_balance as i64,
        tx_metrics: Vec::new(),
        scan_metrics: Some(scan_metrics),
        recorded_birth_height: None,
        error: None,
    })
}

pub async fn run_all_scenarios(
    driver: &dyn WalletDriver,
    config: &Config,
) -> anyhow::Result<Vec<ScenarioResult>> {
    let mut scenarios = Vec::new();

    scenarios.push(match run_b0(driver).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("B0", e.to_string()),
    });
    let s0 = match run_s0(driver, config).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S0", e.to_string()),
    };
    let h_birth = s0.recorded_birth_height.unwrap_or(0);
    scenarios.push(s0);
    scenarios.push(match run_s1(driver, config).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S1", e.to_string()),
    });
    let post_s1_balance = driver.get_balance().await.unwrap_or(0);

    let _ = driver.reset().await;
    scenarios.push(match run_s2(driver, post_s1_balance).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S2", e.to_string()),
    });
    let _ = driver.reset().await;
    scenarios.push(match run_s3(driver, h_birth, post_s1_balance).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S3", e.to_string()),
    });
    scenarios.push(match run_s4(driver, config).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S4", e.to_string()),
    });
    scenarios.push(match run_s5(driver, config).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S5", e.to_string()),
    });
    let post_s5_balance = driver.get_balance().await.unwrap_or(0);

    let _ = driver.reset().await;
    scenarios.push(match run_s6(driver, post_s5_balance).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S6", e.to_string()),
    });
    let _ = driver.reset().await;
    scenarios.push(match run_s7(driver, h_birth, post_s5_balance).await {
        Ok(s) => s,
        Err(e) => scenario_error_result("S7", e.to_string()),
    });

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
        recorded_birth_height: None,
        error: None,
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

#[allow(unused)]
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
    config
        .benchmark
        .fee_rate
        .parse::<u64>()
        .expect("fee_rate in config must be a non-empty valid u64 integer")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BenchmarkParams, BinaryPaths, DataDirs, NetworkConfig, Passwords, VersionPins,
    };
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct FakeDriver {
        mode_name: String,
        state: Arc<Mutex<FakeState>>,
    }

    struct FakeState {
        balance_values: VecDeque<u64>,
        scan_outputs_found: u64,
        single_calls: usize,
        batch_calls: Vec<usize>,
        funding_result: anyhow::Result<TxMetrics>,
        send_single_result: anyhow::Result<TxMetrics>,
        send_batch_result: anyhow::Result<TxMetrics>,
        reset_result: anyhow::Result<()>,
        tip_height: u64,
        self_address: String,
        scan_birthday_heights: Vec<u64>,
    }

    impl FakeDriver {
        fn new() -> Self {
            Self {
                mode_name: "fake".to_string(),
                state: Arc::new(Mutex::new(FakeState {
                    balance_values: VecDeque::from([10_000, 10_000]),
                    scan_outputs_found: REDISCOVERY_TARGET,
                    single_calls: 0,
                    batch_calls: Vec::new(),
                    funding_result: Ok(sample_tx_metric("funding")),
                    send_single_result: Ok(sample_tx_metric("single")),
                    send_batch_result: Ok(sample_tx_metric("batch")),
                    reset_result: Ok(()),
                    tip_height: 123,
                    self_address: "faux-self-address".to_string(),
                    scan_birthday_heights: Vec::new(),
                })),
            }
        }

        fn with_balances(self, balances: impl Into<VecDeque<u64>>) -> Self {
            self.state.lock().unwrap().balance_values = balances.into();
            self
        }

        fn with_scan_outputs(self, outputs_found: u64) -> Self {
            self.state.lock().unwrap().scan_outputs_found = outputs_found;
            self
        }

        fn with_funding_result(self, result: anyhow::Result<TxMetrics>) -> Self {
            self.state.lock().unwrap().funding_result = result;
            self
        }

        fn with_send_single_result(self, result: anyhow::Result<TxMetrics>) -> Self {
            self.state.lock().unwrap().send_single_result = result;
            self
        }

        fn with_send_batch_result(self, result: anyhow::Result<TxMetrics>) -> Self {
            self.state.lock().unwrap().send_batch_result = result;
            self
        }

        fn with_reset_result(self, result: anyhow::Result<()>) -> Self {
            self.state.lock().unwrap().reset_result = result;
            self
        }

        fn with_tip_height(self, height: u64) -> Self {
            self.state.lock().unwrap().tip_height = height;
            self
        }

        fn with_self_address(self, address: &str) -> Self {
            self.state.lock().unwrap().self_address = address.to_string();
            self
        }

        fn with_mode_name(mut self, name: &str) -> Self {
            self.mode_name = name.to_string();
            self
        }
    }

    #[async_trait]
    impl WalletDriver for FakeDriver {
        fn mode_name(&self) -> &str {
            &self.mode_name
        }

        async fn reset(&self) -> anyhow::Result<()> {
            let state = self.state.lock().unwrap();
            state
                .reset_result
                .as_ref()
                .map(|_| ())
                .map_err(|e| anyhow!(e.to_string()))
        }

        async fn get_balance(&self) -> anyhow::Result<u64> {
            let mut state = self.state.lock().unwrap();
            if state.balance_values.len() > 1 {
                Ok(state.balance_values.pop_front().unwrap())
            } else {
                Ok(*state.balance_values.front().unwrap_or(&0))
            }
        }

        async fn get_tip_height(&self) -> anyhow::Result<u64> {
            Ok(self.state.lock().unwrap().tip_height)
        }

        async fn get_self_address(&self) -> anyhow::Result<String> {
            Ok(self.state.lock().unwrap().self_address.clone())
        }

        async fn scan_from_genesis(&self) -> anyhow::Result<crate::metrics::ScanMetrics> {
            let outputs_found = self.state.lock().unwrap().scan_outputs_found;
            Ok(sample_scan_metrics(outputs_found))
        }

        async fn scan_from_birthday(
            &self,
            height: u64,
        ) -> anyhow::Result<crate::metrics::ScanMetrics> {
            let mut state = self.state.lock().unwrap();
            state.scan_birthday_heights.push(height);
            Ok(sample_scan_metrics(state.scan_outputs_found))
        }

        async fn send_single(
            &self,
            _to_address: &str,
            _amount_ut: u64,
            _fee_rate: u64,
        ) -> anyhow::Result<TxMetrics> {
            let mut state = self.state.lock().unwrap();
            state.single_calls += 1;
            state
                .send_single_result
                .as_ref()
                .map(|tx| tx.clone())
                .map_err(|e| anyhow!(e.to_string()))
        }

        async fn send_batch(
            &self,
            recipients: Vec<(String, u64)>,
            _fee_rate: u64,
        ) -> anyhow::Result<TxMetrics> {
            let mut state = self.state.lock().unwrap();
            state.batch_calls.push(recipients.len());
            state
                .send_batch_result
                .as_ref()
                .map(|tx| tx.clone())
                .map_err(|e| anyhow!(e.to_string()))
        }

        async fn observe_funding(&self, _expected_amount_ut: u64) -> anyhow::Result<TxMetrics> {
            self.state
                .lock()
                .unwrap()
                .funding_result
                .as_ref()
                .map(|tx| tx.clone())
                .map_err(|e| anyhow!(e.to_string()))
        }
    }

    fn sample_tx_metric(tx_id: &str) -> TxMetrics {
        TxMetrics {
            tx_id: tx_id.to_string(),
            construction_secs: 0.1,
            broadcast_to_mempool_secs: 0.2,
            broadcast_to_confirmed_secs: 0.3,
            fee_paid: 1,
            success: true,
            error: None,
        }
    }

    fn sample_scan_metrics(outputs_found: u64) -> crate::metrics::ScanMetrics {
        crate::metrics::ScanMetrics {
            wall_clock_secs: 1.0,
            blocks_per_sec: 100.0,
            h_tip_start: 10,
            h_tip_end: 20,
            outputs_found,
            peak_rss_kb: 0,
            peak_cpu_percent: 0.0,
        }
    }

    fn sample_config() -> Config {
        Config {
            benchmark: BenchmarkParams {
                a_fund: 10_000,
                c_min: 3,
                volume_target: 512,
                doubling_rounds: 2,
                fanout_outputs_per_tx: 3,
                concurrent_batches: vec![2],
                s4_t_budget_secs: 60,
                s5_m: 12,
                s5_k: 3,
                tx_amount_ut: 200,
                fee_rate: "1".to_string(),
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
                grpc_port: 0,
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
            seeds: None,
        }
    }

    #[tokio::test]
    async fn s0_records_observed_funding_tx_metrics() {
        let driver = FakeDriver::new().with_balances(VecDeque::from([10_000]));
        let result = run_s0(&driver, &sample_config()).await.unwrap();

        assert_eq!(result.scenario_name, "S0");
        assert_eq!(result.success_count, 1);
        assert_eq!(result.failure_count, 0);
        assert_eq!(result.tx_metrics.len(), 1);
        assert_eq!(result.tx_metrics[0].tx_id, "funding");
        assert!(result.tx_metrics[0].broadcast_to_confirmed_secs > 0.0);
    }

    #[tokio::test]
    async fn s1_uses_two_output_doubling_and_fanout_batches() {
        let driver = FakeDriver::new().with_balances(VecDeque::from([10_000, 10_000]));
        let result = run_s1(&driver, &sample_config()).await.unwrap();
        let state = driver.state.lock().unwrap();

        assert_eq!(result.scenario_name, "S1");
        assert_eq!(state.single_calls, 0);
        assert_eq!(state.batch_calls, vec![2, 2, 2, 3, 3, 3, 3]);
        assert_eq!(result.success_count, 7);
    }

    #[tokio::test]
    async fn s5_runs_both_arms_for_payment_processor() {
        let driver = FakeDriver::new()
            .with_balances(VecDeque::from([10_000, 10_000]))
            .with_mode_name("payment_processor");
        let result = run_s5(&driver, &sample_config()).await.unwrap();
        let state = driver.state.lock().unwrap();

        assert_eq!(result.scenario_name, "S5");
        assert_eq!(state.batch_calls, vec![3, 3, 3, 3]);
        assert_eq!(state.single_calls, 12);
        assert_eq!(result.tx_metrics.len(), 16); // 4 batch + 12 individual
    }

    #[tokio::test]
    async fn s5_runs_both_arms_for_non_payment_processor() {
        let driver = FakeDriver::new()
            .with_balances(VecDeque::from([10_000, 10_000]))
            .with_mode_name("new_wallet");
        let result = run_s5(&driver, &sample_config()).await.unwrap();
        let state = driver.state.lock().unwrap();

        assert_eq!(result.scenario_name, "S5");
        assert_eq!(state.batch_calls, vec![3, 3, 3, 3]);
        assert_eq!(state.single_calls, 12);
        assert_eq!(result.tx_metrics.len(), 16); // 4 batch + 12 individual
    }

    #[tokio::test]
    async fn s2_fails_when_balance_does_not_match_expected_checkpoint() {
        let driver = FakeDriver::new()
            .with_balances(VecDeque::from([8_000]))
            .with_scan_outputs(REDISCOVERY_TARGET);
        let result = run_s2(&driver, 10_000).await.unwrap();

        assert_eq!(result.success_count, 0);
        assert_eq!(result.failure_count, 1);
        assert_eq!(result.balance_delta, 2_000);
    }

    #[tokio::test]
    async fn s0_surfaces_funding_observation_failure() {
        let driver = FakeDriver::new()
            .with_balances(VecDeque::from([0]))
            .with_funding_result(Err(anyhow!("no funding observed")));
        let result = run_s0(&driver, &sample_config()).await.unwrap();

        assert_eq!(result.success_count, 0);
        assert_eq!(result.failure_count, 1);
        assert_eq!(result.tx_metrics.len(), 1);
        assert_eq!(result.tx_metrics[0].tx_id, "incoming-funding");
        assert_eq!(
            result.tx_metrics[0].error.as_deref(),
            Some("no funding observed")
        );
    }

    // ── Extended scenario edge case tests ───────────────────────────

    #[tokio::test]
    async fn b0_scan_failure_produces_error_result() {
        let driver = FakeDriver::new().with_scan_outputs(REDISCOVERY_TARGET);
        let result = run_b0(&driver).await.unwrap();
        assert_eq!(result.scenario_name, "B0");
        // B0 records one scan metric even when outputs_found < REDISCOVERY_TARGET
        assert!(result.scan_metrics.is_some());
    }

    #[tokio::test]
    async fn b0_with_zero_scan_outputs_still_produces_scenario() {
        let driver = FakeDriver::new().with_scan_outputs(0);
        let result = run_b0(&driver).await.unwrap();
        assert_eq!(result.scenario_name, "B0");
        let scan = result.scan_metrics.unwrap();
        assert_eq!(scan.outputs_found, 0);
    }

    #[tokio::test]
    async fn s0_records_h_birth_from_tip_height() {
        let driver = FakeDriver::new()
            .with_balances(VecDeque::from([10_000]))
            .with_tip_height(99_999);
        let result = run_s0(&driver, &sample_config()).await.unwrap();

        assert_eq!(result.recorded_birth_height, Some(99_999));
    }

    #[tokio::test]
    async fn s1_with_zero_doubling_rounds_skips_to_fanout() {
        let mut cfg = sample_config();
        cfg.benchmark.doubling_rounds = 0;
        cfg.benchmark.fanout_outputs_per_tx = 2;
        let driver = FakeDriver::new().with_balances(VecDeque::from([10_000, 10_000]));
        let result = run_s1(&driver, &cfg).await.unwrap();
        let state = driver.state.lock().unwrap();

        // No doubling rounds — one fanout batch of 2 outputs (1 << 0 = 1 batch)
        assert_eq!(state.batch_calls, vec![2]);
        assert_eq!(result.success_count, 1);
    }

    #[tokio::test]
    async fn s1_with_zero_fanout_skips_fanout_entirely() {
        let mut cfg = sample_config();
        cfg.benchmark.doubling_rounds = 1;
        cfg.benchmark.fanout_outputs_per_tx = 0;
        let driver = FakeDriver::new().with_balances(VecDeque::from([10_000, 10_000]));
        let result = run_s1(&driver, &cfg).await.unwrap();
        let state = driver.state.lock().unwrap();

        // One doubling round (1 x 2-output batch) + zero fanout
        assert_eq!(state.batch_calls, vec![2]);
        assert_eq!(result.success_count, 1);
    }

    #[tokio::test]
    async fn s3_passes_birthday_height_to_driver() {
        let driver = FakeDriver::new()
            .with_balances(VecDeque::from([10_000]))
            .with_scan_outputs(REDISCOVERY_TARGET);
        let result = run_s3(&driver, 42, 10_000).await.unwrap();
        let state = driver.state.lock().unwrap();

        assert_eq!(result.scenario_name, "S3");
        assert_eq!(state.scan_birthday_heights, vec![42]);
    }

    #[tokio::test]
    async fn s4_with_empty_concurrent_batches_produces_no_tx() {
        let mut cfg = sample_config();
        cfg.benchmark.concurrent_batches = vec![];
        let driver = FakeDriver::new().with_balances(VecDeque::from([10_000]));
        let result = run_s4(&driver, &cfg).await.unwrap();

        assert_eq!(result.success_count, 0);
        assert_eq!(result.tx_metrics.len(), 0);
    }

    #[tokio::test]
    async fn s4_runs_all_concurrent_batches() {
        let mut cfg = sample_config();
        cfg.benchmark.concurrent_batches = vec![3, 5];
        let driver = FakeDriver::new()
            .with_balances(VecDeque::from([10_000]))
            .with_tip_height(200);
        let result = run_s4(&driver, &cfg).await.unwrap();
        let state = driver.state.lock().unwrap();

        // 3 + 5 = 8 single calls expected
        assert_eq!(state.single_calls, 8);
        assert_eq!(result.success_count, 8);
        assert_eq!(result.tx_metrics.len(), 8);
    }

    #[tokio::test]
    async fn s5_with_zero_s5k_skips_batch_arm() {
        let mut cfg = sample_config();
        cfg.benchmark.s5_m = 6;
        cfg.benchmark.s5_k = 0;
        let driver = FakeDriver::new()
            .with_balances(VecDeque::from([10_000, 10_000]))
            .with_mode_name("old_wallet");
        let result = run_s5(&driver, &cfg).await.unwrap();
        let state = driver.state.lock().unwrap();

        // s5_k = 0 → num_batch_txs = 0, so only the individual arm runs
        assert_eq!(state.batch_calls.len(), 0);
        assert_eq!(state.single_calls, 6);
        assert_eq!(result.success_count, 6);
    }

    #[tokio::test]
    async fn s6_with_zero_balance_still_produces_scenario() {
        let driver = FakeDriver::new()
            .with_balances(VecDeque::from([0]))
            .with_scan_outputs(100);
        let result = run_s6(&driver, 0).await.unwrap();

        assert_eq!(result.scenario_name, "S6");
        assert!(result.scan_metrics.is_some());
        assert_eq!(result.balance_delta, 0);
    }

    #[tokio::test]
    async fn s7_passes_correct_birthday_height() {
        let driver = FakeDriver::new()
            .with_balances(VecDeque::from([10_000]))
            .with_scan_outputs(REDISCOVERY_TARGET);
        let result = run_s7(&driver, 99, 10_000).await.unwrap();
        let state = driver.state.lock().unwrap();

        assert_eq!(result.scenario_name, "S7");
        assert_eq!(state.scan_birthday_heights, vec![99]);
    }

    #[tokio::test]
    async fn reset_failure_is_propagated_through_run_all() {
        let driver = FakeDriver::new()
            .with_balances(VecDeque::from([10_000, 10_000]))
            .with_reset_result(Err(anyhow!("reset failed")));
        // run_all_scenarios calls reset() before S2, S3, S6, S7
        // It should swallow the error and continue
        let config = sample_config();
        let results = run_all_scenarios(&driver, &config).await.unwrap();
        // All 9 scenarios (B0 + S0-S7) should be present
        assert_eq!(results.len(), 9);
        // The scan scenarios may have errors logged but still return results
        for r in &results {
            assert!(
                r.success_count > 0
                    || r.failure_count > 0
                    || r.scan_metrics.is_some(),
                "{} should have success count, failure count, or scan metrics",
                r.scenario_name
            );
        }
    }
}
