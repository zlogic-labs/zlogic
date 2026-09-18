use serde::{Deserialize, Serialize};

/// The shell used by the built-in `shell` tool.
/// `Auto` is resolved once during engine bootstrap. Keeping the user's preference separate from
/// the resolved executable means the tool definition and the process launcher can share one
/// concrete backend without re-running detection or guessing from command text.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellPreference {
    #[default]
    Auto,
    GitBash,
    Ps7,
    Powershell,
    Cmd,
    Bash,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    #[default]
    Auto,
    Bypass,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    pub auto_title: AutoTitle,
    pub approval_mode: ApprovalMode,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutoTitle {
    pub enabled: bool,
    pub max_chars: usize,
    pub source_chars: usize,
}

impl Default for AutoTitle {
    fn default() -> Self {
        Self {
            enabled: true,
            max_chars: 30,
            source_chars: 600,
        }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub compact_ratio: f32,
    pub tail_turns: u32,
    pub overflow_retries: u32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            compact_ratio: 0.8,
            tail_turns: 1,
            overflow_retries: 1,
        }
    }
}

impl ContextConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(0.1..=0.95).contains(&self.compact_ratio) {
            return Err(format!(
                "context.compact_ratio must be between 0.1 and 0.95, got {}",
                self.compact_ratio
            ));
        }
        if self.tail_turns == 0 {
            return Err("context.tail_turns must be at least 1, otherwise compaction would also consume the current tool chain".into());
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    /// Shell backend for the `shell` tool. `Auto` prefers, on Windows:
    /// Git Bash → PowerShell 7 → Windows PowerShell → cmd. Other platforms use bash.
    pub default_shell: ShellPreference,
    pub max_result_chars: usize,
    pub timeout_secs: u64,
    pub web_search: WebSearchConfig,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            default_shell: ShellPreference::Auto,
            max_result_chars: 30_000,
            timeout_secs: 0,
            web_search: WebSearchConfig::default(),
        }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchConfig {
    pub provider: Option<String>,
    pub exa_url: Option<String>,
    pub parallel_url: Option<String>,
    pub timeout_secs: u64,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: None,
            exa_url: None,
            parallel_url: None,
            timeout_secs: 25,
        }
    }
}

impl WebSearchConfig {
    pub fn provider_id(&self) -> Option<String> {
        self.provider
            .as_ref()
            .map(|p| p.trim().to_ascii_lowercase())
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(p) = self.provider_id()
            && !matches!(p.as_str(), "exa" | "parallel")
        {
            return Err(format!(
                "tools.web_search.provider must be exa or parallel, got {:?}",
                self.provider.as_deref().unwrap_or_default()
            ));
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeConfig {
    pub dir: String,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            dir: "../{workspace}-worktrees".into(),
        }
    }
}

impl WorktreeConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.dir.trim().is_empty() {
            return Err("worktree.dir must not be empty".into());
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkSettings {
    pub proxy: String,
    pub no_proxy: Vec<String>,
}

impl NetworkSettings {
    pub fn proxy_url(&self) -> Option<&str> {
        let trimmed = self.proxy.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }

    pub fn no_proxy_list(&self) -> String {
        self.no_proxy
            .iter()
            .map(|entry| entry.trim())
            .filter(|entry| !entry.is_empty())
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn validate(&self) -> Result<(), String> {
        let Some(url) = self.proxy_url() else {
            return Ok(());
        };
        let Some((scheme, rest)) = url.split_once("://") else {
            return Err(format!(
                "network.proxy must include a scheme, e.g. http://127.0.0.1:7890 (got {url:?})"
            ));
        };
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
        ) {
            return Err(format!(
                "network.proxy scheme must be http, https or socks5, got {scheme:?}"
            ));
        }
        let host = rest.trim_end_matches('/');
        if host.is_empty() || host.contains(char::is_whitespace) || host.contains('/') {
            return Err(format!(
                "network.proxy must be host[:port] with no path, got {url:?}"
            ));
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// `error` / `warn` / `info` / `debug` / `trace`
    pub level: String,
    pub to_file: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            to_file: true,
        }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CostConfig {
    pub display_currency: String,
    pub rates: Vec<ExchangeRate>,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            display_currency: "USD".into(),
            rates: Vec::new(),
        }
    }
}

impl CostConfig {
    pub fn display(&self) -> String {
        let trimmed = self.display_currency.trim();
        if trimmed.is_empty() {
            "USD".into()
        } else {
            trimmed.to_ascii_uppercase()
        }
    }

    pub fn convert(&self, amount: f64, from: &str) -> Option<f64> {
        self.convert_to(amount, from, &self.display())
    }

    pub fn convert_to(&self, amount: f64, from: &str, to: &str) -> Option<f64> {
        let to = normalize_currency(to);
        let from = from.trim().to_ascii_uppercase();
        if from == to {
            return Some(amount);
        }
        for rate in &self.rates {
            let (rf, rt) = (rate.from(), rate.to());
            if rate.rate <= 0.0 || !rate.rate.is_finite() {
                continue;
            }
            if rf == from && rt == to {
                return Some(amount * rate.rate);
            }
            if rf == to && rt == from {
                return Some(amount / rate.rate);
            }
        }
        None
    }

    pub fn validate(&self) -> Result<(), String> {
        if !is_currency_code(&self.display()) {
            return Err(format!(
                "cost.display_currency must be a currency code (e.g. USD / CNY), got {:?}",
                self.display_currency
            ));
        }
        for rate in &self.rates {
            rate.validate()?;
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub max_rounds: u32,
    pub max_depth: u32,
    pub max_parallel_tools: usize,
    pub task_wait_secs: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_rounds: 150,
            max_depth: 2,
            max_parallel_tools: 16,
            task_wait_secs: 60,
        }
    }
}

impl LimitsConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_rounds == 0 {
            return Err(
                "limits.max_rounds must be at least 1, otherwise no round could ever run".into(),
            );
        }
        if self.max_depth == 0 {
            return Err("limits.max_depth must be at least 1 (don't enable the tool if you don't want sub-agents)".into());
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub per_turn: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub per_session: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub per_task: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub window: Option<BudgetWindow>,
    pub on_exceeded: BudgetAction,
}

impl BudgetConfig {
    pub fn is_set(&self) -> bool {
        self.per_turn.is_some()
            || self.per_session.is_some()
            || self.per_task.is_some()
            || self.window.is_some()
    }

    pub fn task_limit(&self) -> Option<f64> {
        self.per_task.or(self.per_turn)
    }

    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("per_turn", self.per_turn),
            ("per_session", self.per_session),
            ("per_task", self.per_task),
        ] {
            if let Some(amount) = value
                && !(amount.is_finite() && amount > 0.0)
            {
                return Err(format!(
                    "budget.{name} must be a positive number (omit the key for unlimited), got {amount}"
                ));
            }
        }
        if let Some(window) = &self.window {
            window.validate()?;
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetWindow {
    pub amount: f64,
    pub hours: u32,
}

impl BudgetWindow {
    pub fn validate(&self) -> Result<(), String> {
        if !(self.amount.is_finite() && self.amount > 0.0) {
            return Err(format!(
                "budget.window.amount must be a positive number, got {}",
                self.amount
            ));
        }
        if self.hours == 0 {
            return Err("budget.window.hours must be at least 1".into());
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetAction {
    #[default]
    Ask,
    Stop,
    Warn,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeRate {
    pub from: String,
    pub to: String,
    pub rate: f64,
}

impl ExchangeRate {
    pub fn from(&self) -> String {
        self.from.trim().to_ascii_uppercase()
    }

    pub fn to(&self) -> String {
        self.to.trim().to_ascii_uppercase()
    }

    pub fn validate(&self) -> Result<(), String> {
        let (from, to) = (self.from(), self.to());
        if !is_currency_code(&from) || !is_currency_code(&to) {
            return Err(format!(
                "exchange-rate currencies must be currency codes (e.g. USD / CNY), got {:?} → {:?}",
                self.from, self.to
            ));
        }
        if from == to {
            return Err(format!("the exchange rate {from} → {to} is meaningless"));
        }
        if !(self.rate.is_finite() && self.rate > 0.0) {
            return Err(format!(
                "the exchange rate {from} → {to} must be a positive number, got {}",
                self.rate
            ));
        }
        Ok(())
    }
}

fn is_currency_code(code: &str) -> bool {
    let mut chars = code.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && code.len() >= 2
        && code.len() <= 12
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn normalize_currency(code: &str) -> String {
    let trimmed = code.trim();
    if trimmed.is_empty() {
        "USD".into()
    } else {
        trimmed.to_ascii_uppercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proxy_needs_a_scheme_and_a_bare_host_and_port() {
        let ok = |proxy: &str| {
            NetworkSettings {
                proxy: proxy.into(),
                ..Default::default()
            }
            .validate()
        };
        assert!(ok("").is_ok());
        assert!(ok("  ").is_ok());
        assert!(ok("http://127.0.0.1:7890/").is_ok());
        assert!(ok("socks5://127.0.0.1:7891").is_ok());
        assert!(
            ok("127.0.0.1:7890").is_err(),
            "a missing scheme must not be guessed on the user's behalf"
        );
        assert!(ok("ftp://127.0.0.1:7890").is_err());
        assert!(ok("http://127.0.0.1:7890/pac").is_err());
        assert!(ok("http://127.0.0.1:78 90").is_err());
    }

    #[test]
    fn the_no_proxy_list_is_trimmed_and_joined() {
        let settings = NetworkSettings {
            proxy: "http://127.0.0.1:7890".into(),
            no_proxy: vec![" localhost ".into(), String::new(), "127.0.0.1".into()],
        };
        assert_eq!(settings.no_proxy_list(), "localhost,127.0.0.1");
    }

    #[test]
    fn defaults_are_sane() {
        let c = ContextConfig::default();
        assert!(c.validate().is_ok());
        assert!(
            c.compact_ratio < 1.0,
            "compacting only once 100% is reached is already too late"
        );
        assert!(c.tail_turns >= 1);
    }

    #[test]
    fn ratio_out_of_range_is_rejected() {
        for r in [0.0, 0.05, 1.0, 1.5, -1.0] {
            let c = ContextConfig {
                compact_ratio: r,
                ..Default::default()
            };
            assert!(c.validate().is_err(), "{r} must be rejected");
        }
    }

    #[test]
    fn zero_tail_turns_is_rejected() {
        let c = ContextConfig {
            tail_turns: 0,
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn approval_mode_defaults_to_auto() {
        assert_eq!(
            SessionConfig::default().approval_mode,
            ApprovalMode::Auto,
            "a full grant must never be on by default"
        );
    }

    #[test]
    fn sections_round_trip_through_yaml() {
        let s = SessionConfig::default();
        let y = serde_yaml_ng::to_string(&s).unwrap();
        assert_eq!(serde_yaml_ng::from_str::<SessionConfig>(&y).unwrap(), s);
    }

    #[test]
    fn default_shell_is_configurable_and_defaults_to_auto() {
        assert_eq!(ToolsConfig::default().default_shell, ShellPreference::Auto);
        let tools: ToolsConfig = serde_yaml_ng::from_str("default_shell: ps7\n").unwrap();
        assert_eq!(tools.default_shell, ShellPreference::Ps7);
        assert!(serde_yaml_ng::from_str::<ToolsConfig>("default_shell: zsh\n").is_err());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let y = "compact_ratio: 0.5\ntail_turn: 3\n"; // missing the s
        assert!(serde_yaml_ng::from_str::<ContextConfig>(y).is_err());
    }

    #[test]
    fn one_rate_serves_both_directions() {
        let cost = CostConfig {
            display_currency: "USD".into(),
            rates: vec![ExchangeRate {
                from: "USD".into(),
                to: "CNY".into(),
                rate: 7.0,
            }],
        };
        assert_eq!(cost.convert(1.0, "USD"), Some(1.0));
        assert_eq!(cost.convert(7.0, "CNY"), Some(1.0));

        let cost = CostConfig {
            display_currency: "CNY".into(),
            ..cost
        };
        assert_eq!(cost.convert(1.0, "USD"), Some(7.0));
        assert_eq!(cost.convert(7.0, "CNY"), Some(7.0));
    }

    #[test]
    fn currency_codes_are_matched_case_insensitively() {
        let cost = CostConfig {
            display_currency: " usd ".into(),
            rates: vec![ExchangeRate {
                from: "usd".into(),
                to: " cny".into(),
                rate: 7.0,
            }],
        };
        assert_eq!(cost.display(), "USD");
        assert_eq!(cost.convert(7.0, "cny"), Some(1.0));
    }

    #[test]
    fn a_missing_rate_cannot_be_converted() {
        let cost = CostConfig::default();
        assert_eq!(cost.convert(700.0, "CNY"), None);
        assert_eq!(
            cost.convert(1.0, "USD"),
            Some(1.0),
            "the same currency needs no rate"
        );
    }

    #[test]
    fn conversion_does_not_chain_through_a_third_currency() {
        let cost = CostConfig {
            display_currency: "JPY".into(),
            rates: vec![
                ExchangeRate {
                    from: "USD".into(),
                    to: "CNY".into(),
                    rate: 7.0,
                },
                ExchangeRate {
                    from: "CNY".into(),
                    to: "JPY".into(),
                    rate: 20.0,
                },
            ],
        };
        assert_eq!(cost.convert(1.0, "CNY"), Some(20.0));
        assert_eq!(cost.convert(1.0, "USD"), None, "USD→CNY→JPY is not derived");
    }

    #[test]
    fn a_nonsense_rate_is_rejected_and_never_used() {
        for rate in [0.0, -7.0, f64::NAN, f64::INFINITY] {
            let one = ExchangeRate {
                from: "USD".into(),
                to: "CNY".into(),
                rate,
            };
            assert!(one.validate().is_err(), "{rate} must be rejected");
            let cost = CostConfig {
                display_currency: "USD".into(),
                rates: vec![one],
            };
            assert_eq!(
                cost.convert(7.0, "CNY"),
                None,
                "{rate} must never be used for conversion"
            );
        }
    }

    #[test]
    fn a_self_referential_rate_is_rejected() {
        assert!(
            ExchangeRate {
                from: "USD".into(),
                to: "usd".into(),
                rate: 1.0,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn currency_codes_are_checked_loosely_not_against_a_list() {
        for ok in ["USD", "CNY", "USDT", "MY-CREDITS"] {
            let cost = CostConfig {
                display_currency: ok.into(),
                rates: Vec::new(),
            };
            assert!(cost.validate().is_ok(), "{ok} must be accepted");
        }
        for bad in ["", " ", "1", "U S D", "¥"] {
            let cost = CostConfig {
                display_currency: bad.into(),
                rates: Vec::new(),
            };
            if bad.trim().is_empty() {
                assert!(cost.validate().is_ok());
            } else {
                assert!(cost.validate().is_err(), "{bad:?} must be rejected");
            }
        }
    }

    #[test]
    fn the_default_needs_no_rates_at_all() {
        let cost = CostConfig::default();
        assert_eq!(
            cost.display(),
            "USD",
            "the built-in catalog prices are in USD"
        );
        assert!(cost.rates.is_empty());
        assert!(cost.validate().is_ok());
    }
}
