//! |---|---|---|

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::define_enum_wire;

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaMetric {
    InputTokens,
    OutputTokens,
    TotalTokens,
    CostUsd,
    Requests,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaWeekStart {
    #[default]
    Mon,
    Sun,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaPeriod {
    Day,
    Week,
    Month,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QuotaWindow {
    Calendar {
        period: QuotaPeriod,
        #[serde(default = "utc_timezone")]
        timezone: String,
        #[serde(default)]
        week_start: QuotaWeekStart,
        #[serde(default = "first_day")]
        anchor_day: u8,
    },
    Rolling {
        hours: u32,
    },
}

fn utc_timezone() -> String {
    "UTC".into()
}

const fn first_day() -> u8 {
    1
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaConfig {
    pub label: String,
    pub metric: QuotaMetric,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    pub window: QuotaWindow,
    pub limit: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_at: Option<f64>,
}

impl QuotaConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.label.trim().is_empty() {
            return Err("quota.label must not be empty".into());
        }
        if !self.limit.is_finite() || self.limit <= 0.0 {
            return Err(format!(
                "quota.limit must be a positive number, got {}",
                self.limit
            ));
        }
        if self
            .warn
            .is_some_and(|value| !value.is_finite() || value <= 0.0 || value >= 1.0)
        {
            return Err("quota.warn must be a ratio greater than 0 and less than 1".into());
        }
        if self
            .warn_at
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return Err("quota.warn_at must be non-negative".into());
        }
        if let QuotaWindow::Rolling { hours } = &self.window
            && *hours == 0
        {
            return Err("rolling quota.hours must be at least 1".into());
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaState {
    Ok,
    Warn,
    Exceeded,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaScope {
    pub provider_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaStatus {
    pub label: String,
    pub scope: QuotaScope,
    pub metric: QuotaMetric,
    pub used: f64,
    pub limit: f64,
    pub pct: f64,
    pub state: QuotaState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_reset_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling_hours: Option<u32>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = add_opt(self.cache_read, other.cache_read);
        self.cache_write = add_opt(self.cache_write, other.cache_write);
        self.reasoning = add_opt(self.reasoning, other.reasoning);
    }
}

fn add_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0).saturating_add(y.unwrap_or(0))),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub tokens: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ReportedCost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportedCost {
    pub amount: f64,
    pub currency: String,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    ProviderReported,
    LocalPricing,
    Estimated,
}

define_enum_wire!(CostSource {
    ProviderReported => "provider_reported",
    LocalPricing => "local_pricing",
    Estimated => "estimated",
});

#[cfg(feature = "sql")]
crate::impl_enum_sql!(CostSource);

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostView {
    pub amount: f64,
    pub currency: String,
    pub source: CostSource,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostTotal {
    pub amount: f64,
    pub currency: String,
    pub source: CostSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unconverted: Vec<CurrencyAmount>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostTally {
    by_currency: std::collections::BTreeMap<String, f64>,
    source: Option<CostSource>,
}

impl CostTally {
    pub fn add(&mut self, amount: f64, currency: &str, source: Option<CostSource>) {
        let key = {
            let trimmed = currency.trim();
            if trimmed.is_empty() {
                "USD".to_string()
            } else {
                trimmed.to_ascii_uppercase()
            }
        };
        *self.by_currency.entry(key).or_default() += amount;
        self.source = Some(match (self.source, source) {
            (Some(a), Some(b)) if confidence(b) < confidence(a) => b,
            (Some(a), _) => a,
            (None, b) => b.unwrap_or(CostSource::LocalPricing),
        });
    }

    pub fn add_view(&mut self, view: &CostView) {
        self.add(view.amount, &view.currency, Some(view.source));
    }

    pub fn is_empty(&self) -> bool {
        self.by_currency.is_empty()
    }

    pub fn merge(&mut self, other: &CostTally) {
        for (currency, amount) in &other.by_currency {
            *self.by_currency.entry(currency.clone()).or_default() += amount;
        }
        self.source = match (self.source, other.source) {
            (Some(a), Some(b)) if confidence(b) < confidence(a) => Some(b),
            (Some(a), _) => Some(a),
            (None, b) => b,
        };
    }

    pub fn total(&self, cost: &crate::settings::CostConfig) -> Option<CostTotal> {
        let source = self.source?;
        let mut amount = 0.0;
        let mut unconverted = Vec::new();
        for (currency, sum) in &self.by_currency {
            match cost.convert(*sum, currency) {
                Some(converted) => amount += converted,
                None => unconverted.push(CurrencyAmount {
                    amount: *sum,
                    currency: currency.clone(),
                }),
            }
        }
        Some(CostTotal {
            amount,
            currency: cost.display(),
            source,
            unconverted,
        })
    }
}

fn confidence(source: CostSource) -> u8 {
    match source {
        CostSource::Estimated => 0,
        CostSource::LocalPricing => 1,
        CostSource::ProviderReported => 2,
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurrencyAmount {
    pub amount: f64,
    pub currency: String,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    pub used: Option<u64>,
    pub window: u64,
}

/// Stored as an explicit wire name. `Agent` carries a name, so it cannot use the fieldless
/// enum macro — it encodes as `agent:<name>`.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Main,
    Title,
    Compaction,
    Approval,
    ApprovalDeep,
    Utility,
    Agent(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_keeps_none_when_both_absent() {
        let mut a = TokenUsage {
            input: 10,
            output: 2,
            ..Default::default()
        };
        a.add(&TokenUsage {
            input: 5,
            output: 3,
            ..Default::default()
        });
        assert_eq!(a.input, 15);
        assert_eq!(a.output, 5);
        assert_eq!(a.cache_read, None);
    }

    #[test]
    fn add_promotes_none_to_zero_when_other_side_has_value() {
        let mut a = TokenUsage {
            input: 10,
            output: 2,
            ..Default::default()
        };
        a.add(&TokenUsage {
            input: 0,
            output: 0,
            cache_read: Some(7),
            ..Default::default()
        });
        assert_eq!(a.cache_read, Some(7));
    }
}

impl Purpose {
    pub fn as_wire(&self) -> String {
        match self {
            Purpose::Main => "main".into(),
            Purpose::Title => "title".into(),
            Purpose::Compaction => "compaction".into(),
            Purpose::Approval => "approval".into(),
            Purpose::ApprovalDeep => "approval_deep".into(),
            Purpose::Utility => "utility".into(),
            Purpose::Agent(name) => format!("agent:{name}"),
        }
    }

    pub fn parse_wire(s: &str) -> Option<Self> {
        Some(match s {
            "main" => Purpose::Main,
            "title" => Purpose::Title,
            "compaction" => Purpose::Compaction,
            "approval" => Purpose::Approval,
            "approval_deep" => Purpose::ApprovalDeep,
            "utility" => Purpose::Utility,
            other => Purpose::Agent(other.strip_prefix("agent:")?.to_string()),
        })
    }

    /// Whether this call is the main conversation. Compaction's trigger looks only at these.
    pub fn is_main(&self) -> bool {
        matches!(self, Purpose::Main)
    }
}

impl std::fmt::Display for Purpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_wire())
    }
}

#[cfg(feature = "sql")]
impl rusqlite::ToSql for Purpose {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.as_wire()))
    }
}

#[cfg(feature = "sql")]
impl rusqlite::types::FromSql for Purpose {
    fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let s = v.as_str()?;
        Self::parse_wire(s).ok_or_else(|| {
            rusqlite::types::FromSqlError::Other(Box::new(crate::ids::IdParseError {
                kind: "Purpose",
                value: s.to_string(),
            }))
        })
    }
}

#[cfg(test)]
mod purpose_tests {
    use super::*;

    #[test]
    fn purpose_round_trips_including_agent_names() {
        for p in [
            Purpose::Main,
            Purpose::Title,
            Purpose::Compaction,
            Purpose::Approval,
            Purpose::ApprovalDeep,
            Purpose::Utility,
            Purpose::Agent("researcher".into()),
        ] {
            assert_eq!(Purpose::parse_wire(&p.as_wire()).unwrap(), p);
        }
    }

    #[test]
    fn only_main_counts_as_the_main_conversation() {
        assert!(Purpose::Main.is_main());
        for p in [
            Purpose::Title,
            Purpose::Approval,
            Purpose::Agent("x".into()),
        ] {
            assert!(!p.is_main(), "{p} must not drive compaction");
        }
    }

    #[test]
    fn an_unprefixed_unknown_value_is_rejected() {
        assert!(Purpose::parse_wire("something_else").is_none());
        assert_eq!(
            Purpose::parse_wire("agent:reviewer"),
            Some(Purpose::Agent("reviewer".into()))
        );
    }
}
