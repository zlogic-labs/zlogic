//! Model-manager DTOs + config mutate envelope. Minimal.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Light,
    Main,
    Thinking,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KeyStatus {
    Present, // ✓
    Missing, // ✗
    Env,     // env
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub name: String,
    pub provider: String,
    pub tier: Tier,
    pub vision: bool,
    pub key: KeyStatus,
    /// e.g. "$0.55/$2.19" or "—".
    pub price: String,
    /// Total context window in tokens. Drives auto-compaction, so it's a first-class
    /// per-model field (0 = unknown). Displayed as e.g. "128K".
    #[serde(default)]
    pub context_window: u32,
    pub is_current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub name: String,
    pub sdk: String,
    pub base_url: Option<String>,
    pub key: KeyStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEntry {
    pub provider: String,
    pub sdk: String,
    pub status: KeyStatus,
    pub preview: Option<String>,
    pub storage: Option<String>,
    pub env_var: Option<String>,
}

/// Result of the `t` connectivity test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnResult {
    pub ok: bool,
    pub latency_ms: u32,
    pub detail: String,
}

/// Provider / model / role / key snapshot (tabs). Opaque-ish.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigView {
    pub providers: Vec<String>,
    pub current_model: Option<String>,
}

/// All config writes funnel through one enum so the trait doesn't grow a method per op.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ConfigOp {
    AddModel {
        provider: String,
        model: String,
    },
    RemoveModel {
        model: String,
    },
    SetProvider {
        name: String,
        sdk: String,
        base_url: String,
    },
    KeySet {
        provider: String,
        value: String,
        storage: String,
    },
    KeyDelete {
        provider: String,
    },
}
