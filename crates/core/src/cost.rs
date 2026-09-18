//! Turning tokens into money.
//! Core owns this because only core has the model's pricing configuration. The LLM layer's job
//! ended at normalising field names and semantics — see `zlogic_protocol::usage`.
//! # A provider-reported cost always wins
//! OpenRouter (and gateways like it) bill their own way, with their own margin and their own
//! upstream routing. Local pricing would be a guess about somebody else's price list, so when the
//! provider states a figure we take it and record [`CostSource::ProviderReported`] — the only field
//! that makes a mismatched invoice diagnosable.
//! # Input is split, because the parts cost different amounts
//! `TokenUsage::input` is **total billed input**, cache included (that normalisation is what makes
//! Anthropic and OpenAI comparable at all). Pricing runs the other way: uncached input, cache reads
//! (much cheaper) and cache writes (Anthropic's ~1.25×) are separate rates, so the cache portions
//! are subtracted back out before the base rate is applied.
//! # No pricing means no number
//! Without a `pricing` block the answer is `None`, not an estimate. [`CostSource::Estimated`] exists
//! for a layer that has a defensible fallback rate; inventing one here would put a fabricated
//! figure in a spend report, which is worse than an honest blank.

use zlogic_protocol::config::Pricing;
use zlogic_protocol::usage::{CostSource, CostView, TokenUsage, UsageReport};

/// What one round cost.
pub fn cost_of(report: &UsageReport, pricing: Option<&Pricing>) -> Option<CostView> {
    if let Some(reported) = &report.cost {
        return Some(CostView {
            amount: reported.amount,
            currency: reported.currency.clone(),
            source: CostSource::ProviderReported,
        });
    }
    let p = pricing?;
    Some(CostView {
        amount: local_cost(&report.tokens, p),
        currency: p.currency.clone(),
        source: CostSource::LocalPricing,
    })
}

/// The local-pricing calculation, per million tokens.
pub fn local_cost(tokens: &TokenUsage, p: &Pricing) -> f64 {
    let cache_read = tokens.cache_read.unwrap_or(0);
    let cache_write = tokens.cache_write.unwrap_or(0);
    // Saturating: a provider whose cache figures exceed its own input total would otherwise wrap
    // into an astronomical bill.
    let uncached = tokens
        .input
        .saturating_sub(cache_read.saturating_add(cache_write));

    let per_m = |n: u64, rate: f64| (n as f64) * rate / 1_000_000.0;

    per_m(uncached, p.input_per_m)
        + per_m(cache_read, p.cached_input_per_m.unwrap_or(p.input_per_m))
        + per_m(cache_write, p.cache_write_per_m.unwrap_or(p.input_per_m))
        // Reasoning tokens are a subset of output and are already counted there.
        + per_m(tokens.output, p.output_per_m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::usage::ReportedCost;

    fn pricing() -> Pricing {
        Pricing {
            input_per_m: 3.0,
            cached_input_per_m: Some(0.3),
            cache_write_per_m: Some(3.75),
            output_per_m: 15.0,
            currency: "USD".into(),
        }
    }

    fn report(tokens: TokenUsage) -> UsageReport {
        UsageReport {
            tokens,
            cost: None,
            raw: None,
        }
    }

    #[test]
    fn each_part_of_the_input_is_charged_at_its_own_rate() {
        let tokens = TokenUsage {
            input: 100_000,
            output: 10_000,
            cache_read: Some(60_000),
            cache_write: Some(20_000),
            reasoning: Some(4_000),
        };
        // uncached 20k @3 + read 60k @0.3 + write 20k @3.75 + output 10k @15
        let expected = 0.06 + 0.018 + 0.075 + 0.15;
        assert!((local_cost(&tokens, &pricing()) - expected).abs() < 1e-9);
    }

    /// Reasoning tokens are informational: they are already inside `output`.
    #[test]
    fn reasoning_tokens_are_not_billed_twice() {
        let with = TokenUsage {
            input: 10,
            output: 100,
            reasoning: Some(90),
            ..Default::default()
        };
        let without = TokenUsage {
            output: 100,
            input: 10,
            ..Default::default()
        };
        assert_eq!(
            local_cost(&with, &pricing()),
            local_cost(&without, &pricing())
        );
    }

    /// The normalisation `input` carries makes this the correct arithmetic: cache is *inside* it.
    #[test]
    fn cache_is_subtracted_out_of_input_not_added_to_it() {
        let all_cached = TokenUsage {
            input: 50_000,
            output: 0,
            cache_read: Some(50_000),
            ..Default::default()
        };
        let none_cached = TokenUsage {
            input: 50_000,
            output: 0,
            ..Default::default()
        };
        assert!(
            local_cost(&all_cached, &pricing()) < local_cost(&none_cached, &pricing()) / 5.0,
            "a fully cached prompt must be an order of magnitude cheaper"
        );
    }

    /// A provider that reports cache totals larger than its own input must not produce a wild bill.
    #[test]
    fn inconsistent_provider_figures_cannot_wrap() {
        let bad = TokenUsage {
            input: 10,
            output: 0,
            cache_read: Some(999_999),
            ..Default::default()
        };
        let c = local_cost(&bad, &pricing());
        assert!(c.is_finite() && (0.0..1.0).contains(&c), "got {c}");
    }

    #[test]
    fn a_provider_reported_cost_wins_over_local_pricing() {
        let mut r = report(TokenUsage {
            input: 1_000_000,
            output: 0,
            ..Default::default()
        });
        r.cost = Some(ReportedCost {
            amount: 0.42,
            currency: "USD".into(),
        });

        let view = cost_of(&r, Some(&pricing())).unwrap();
        assert_eq!(
            view.amount, 0.42,
            "not the 3.0 local pricing would have produced"
        );
        assert_eq!(view.source, CostSource::ProviderReported);
    }

    /// The gateway case: no local pricing configured at all, but the provider stated a figure.
    #[test]
    fn a_reported_cost_needs_no_local_pricing() {
        let mut r = report(TokenUsage::default());
        r.cost = Some(ReportedCost {
            amount: 0.01,
            currency: "USD".into(),
        });
        assert_eq!(
            cost_of(&r, None).unwrap().source,
            CostSource::ProviderReported
        );
    }

    /// No pricing, no number. An invented rate in a spend report is worse than a blank.
    #[test]
    fn without_pricing_there_is_no_cost_rather_than_a_guess() {
        let r = report(TokenUsage {
            input: 1_000,
            output: 500,
            ..Default::default()
        });
        assert!(cost_of(&r, None).is_none());
    }

    #[test]
    fn a_missing_cache_rate_falls_back_to_the_input_rate() {
        let p = Pricing {
            input_per_m: 3.0,
            cached_input_per_m: None,
            cache_write_per_m: None,
            output_per_m: 15.0,
            currency: "USD".into(),
        };
        let split = TokenUsage {
            input: 100,
            output: 0,
            cache_read: Some(60),
            ..Default::default()
        };
        let flat = TokenUsage {
            input: 100,
            output: 0,
            ..Default::default()
        };
        assert!((local_cost(&split, &p) - local_cost(&flat, &p)).abs() < 1e-12);
    }

    #[test]
    fn the_currency_comes_from_the_price_list() {
        let p = Pricing {
            currency: "CNY".into(),
            ..pricing()
        };
        let view = cost_of(&report(TokenUsage::default()), Some(&p)).unwrap();
        assert_eq!(view.currency, "CNY");
        assert_eq!(view.source, CostSource::LocalPricing);
    }
}
