#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanMetrics {
    pub wall_clock_secs: f64,
    pub blocks_per_sec: f64,
    pub h_tip_start: u64,
    pub h_tip_end: u64,
    pub outputs_found: u64,
    pub peak_rss_kb: u64,
    pub peak_cpu_percent: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TxMetrics {
    pub tx_id: String,
    pub construction_secs: f64,
    pub broadcast_to_mempool_secs: f64,
    pub broadcast_to_confirmed_secs: f64,
    pub fee_paid: u64,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct ScenarioResult {
    pub scenario_name: String,
    pub wall_clock_secs: f64,
    pub total_fees: u64,
    pub success_count: u64,
    pub failure_count: u64,
    pub balance_delta: i64,
    pub tx_metrics: Vec<TxMetrics>,
    pub scan_metrics: Option<ScanMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_birth_height: Option<u64>,
}

#[derive(Debug, serde::Serialize)]
pub struct BenchmarkReport {
    pub cpu_model: String,
    pub ram_kb: u64,
    pub os: String,
    pub disk_type: String,
    pub network_path: String,
    pub console_wallet_version: String,
    pub minotari_cli_version: String,
    pub base_node_version: String,
    pub scan_delta_s2_minus_b0: Option<f64>,
    pub scan_delta_s6_minus_s2: Option<f64>,
    pub s5_throughput_multiplier: Option<f64>,
    pub wallet_mode: String,
    pub config_snapshot: serde_json::Value,
    pub scenarios: Vec<ScenarioResult>,
}
