//! Usage snapshots for /stats. Aggregation rules live in core; these are
//! the display-shaped DTOs the panel reads. Minimal.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageSnapshot {
    /// e.g. last-30-days daily cost, for the sparkline.
    pub daily_cost: Vec<f64>,
    pub today_cost: f64,
    pub total_cost: f64,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub request_count: u64,
    pub turn_count: u64,
    pub by_model: Vec<ModelUsage>,
    pub context_used_tokens: u64,
    pub context_limit_tokens: u64,
    /// 0.0..=1.0
    pub cache_hit_rate: f64,
    pub cache_savings: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    pub model: String,
    pub tokens: u64,
    /// 0.0..=1.0
    pub share: f64,
    pub cost: f64,
    /// TTFT p50 in ms (model distribution).
    pub ttft_p50_ms: Option<u32>,
    pub throughput_tok_s: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnUsage {
    pub turn_id: String,
    pub cost: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// Core-owned final statistics for one completed turn. The UI requests this after
/// `TurnDone`; it never derives these values from transient rendered rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnSummary {
    pub turn_id: String,
    pub rounds: Vec<RoundSummary>,
    pub thinking_chars: u64,
    pub tool_count: u64,
    pub failed_tool_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub context_used_tokens: u64,
    pub context_limit_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoundSummary {
    pub round_id: String,
    pub thinking_chars: u64,
    pub tool_count: u64,
    pub failed_tool_count: u64,
}
