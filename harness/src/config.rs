use serde::Deserialize;

#[derive(Debug, Deserialize)]
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
    pub fee_rate: String,
    pub base_node_grpc_url: String,
    pub base_node_http_url: String,
}
