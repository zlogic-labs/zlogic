use serde_json::Value;
use zlogic_protocol::usage::{ReportedCost, TokenUsage, UsageReport};

#[derive(Debug, Clone)]
pub struct UsageShape {
    pub input: &'static [&'static str],
    pub output: &'static [&'static str],
    pub cache_read: &'static [&'static str],
    pub cache_write: &'static [&'static str],
    pub reasoning: &'static [&'static str],
    pub cost: &'static [&'static str],
    pub input_includes_cache: bool,
    pub output_includes_reasoning: bool,
    pub currency: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct UsageOverrides {
    pub input: Option<String>,
    pub output: Option<String>,
    pub cache_read: Option<String>,
    pub cache_write: Option<String>,
    pub reasoning: Option<String>,
    pub cost: Option<String>,
    pub input_includes_cache: Option<bool>,
    pub output_includes_reasoning: Option<bool>,
}

pub const OPENAI: UsageShape = UsageShape {
    input: &["prompt_tokens", "input_tokens"],
    output: &["completion_tokens", "output_tokens"],
    cache_read: &["prompt_tokens_details.cached_tokens", "cached_tokens"],
    cache_write: &[],
    reasoning: &["completion_tokens_details.reasoning_tokens"],
    cost: &[],
    input_includes_cache: true,
    output_includes_reasoning: true,
    currency: "USD",
};

pub const RESPONSES: UsageShape = UsageShape {
    input: &["input_tokens"],
    output: &["output_tokens"],
    cache_read: &["input_tokens_details.cached_tokens"],
    cache_write: &[],
    reasoning: &["output_tokens_details.reasoning_tokens"],
    cost: &[],
    input_includes_cache: true,
    output_includes_reasoning: true,
    currency: "USD",
};

pub const DEEPSEEK: UsageShape = UsageShape {
    input: &["prompt_tokens"],
    output: &["completion_tokens"],
    cache_read: &[
        "prompt_cache_hit_tokens",
        "prompt_tokens_details.cached_tokens",
    ],
    cache_write: &[],
    reasoning: &["completion_tokens_details.reasoning_tokens"],
    cost: &[],
    input_includes_cache: true,
    output_includes_reasoning: true,
    currency: "USD",
};

pub const OPENROUTER: UsageShape = UsageShape {
    input: &["prompt_tokens"],
    output: &["completion_tokens"],
    cache_read: &["prompt_tokens_details.cached_tokens"],
    cache_write: &[],
    reasoning: &["completion_tokens_details.reasoning_tokens"],
    cost: &["cost"],
    input_includes_cache: true,
    output_includes_reasoning: true,
    currency: "USD",
};

pub const ANTHROPIC: UsageShape = UsageShape {
    input: &["input_tokens"],
    output: &["output_tokens"],
    cache_read: &["cache_read_input_tokens"],
    cache_write: &["cache_creation_input_tokens"],
    reasoning: &[],
    cost: &[],
    input_includes_cache: false,
    output_includes_reasoning: true,
    currency: "USD",
};

pub const GEMINI: UsageShape = UsageShape {
    input: &["promptTokenCount"],
    output: &["candidatesTokenCount"],
    cache_read: &["cachedContentTokenCount"],
    cache_write: &[],
    reasoning: &["thoughtsTokenCount"],
    cost: &[],
    input_includes_cache: true,
    output_includes_reasoning: false,
    currency: "USD",
};

pub const BEDROCK: UsageShape = UsageShape {
    input: &["inputTokens"],
    output: &["outputTokens"],
    cache_read: &["cacheReadInputTokens"],
    cache_write: &["cacheWriteInputTokens"],
    reasoning: &[],
    cost: &[],
    input_includes_cache: false,
    output_includes_reasoning: true,
    currency: "USD",
};

pub fn get_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

fn first_u64(root: &Value, paths: &[&str], override_path: Option<&str>) -> Option<u64> {
    if let Some(p) = override_path {
        return get_path(root, p).and_then(as_u64);
    }
    paths
        .iter()
        .find_map(|p| get_path(root, p).and_then(as_u64))
}

fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64))
}

pub fn extract(raw: &Value, shape: &UsageShape, ov: &UsageOverrides) -> UsageReport {
    let cache_read = first_u64(raw, shape.cache_read, ov.cache_read.as_deref());
    let cache_write = first_u64(raw, shape.cache_write, ov.cache_write.as_deref());
    let reported_input = first_u64(raw, shape.input, ov.input.as_deref()).unwrap_or(0);
    let reported_output = first_u64(raw, shape.output, ov.output.as_deref()).unwrap_or(0);
    let reasoning = first_u64(raw, shape.reasoning, ov.reasoning.as_deref());

    let output = if ov
        .output_includes_reasoning
        .unwrap_or(shape.output_includes_reasoning)
    {
        reported_output
    } else {
        reported_output.saturating_add(reasoning.unwrap_or(0))
    };

    let includes_cache = ov
        .input_includes_cache
        .unwrap_or(shape.input_includes_cache);
    let input = if includes_cache {
        reported_input
    } else {
        reported_input
            .saturating_add(cache_read.unwrap_or(0))
            .saturating_add(cache_write.unwrap_or(0))
    };

    let cost = ov
        .cost
        .as_deref()
        .map_or_else(
            || {
                shape
                    .cost
                    .iter()
                    .find_map(|p| get_path(raw, p).and_then(|v| v.as_f64()))
            },
            |p| get_path(raw, p).and_then(|v| v.as_f64()),
        )
        .map(|amount| ReportedCost {
            amount,
            currency: shape.currency.to_string(),
        });

    UsageReport {
        tokens: TokenUsage {
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
        },
        cost,
        raw: Some(raw.clone()),
    }
}

pub fn extract_checked(
    raw: &Value,
    shape: &UsageShape,
    ov: &UsageOverrides,
) -> Result<UsageReport, String> {
    validate_token_override(raw, "input", ov.input.as_deref(), true)?;
    validate_token_override(raw, "output", ov.output.as_deref(), true)?;
    validate_token_override(raw, "cache_read", ov.cache_read.as_deref(), false)?;
    validate_token_override(raw, "cache_write", ov.cache_write.as_deref(), false)?;
    validate_token_override(raw, "reasoning", ov.reasoning.as_deref(), false)?;
    validate_cost_override(raw, ov.cost.as_deref())?;
    Ok(extract(raw, shape, ov))
}

fn validate_token_override(
    raw: &Value,
    field: &str,
    path: Option<&str>,
    required: bool,
) -> Result<(), String> {
    let Some(path) = path else { return Ok(()) };
    validate_path_syntax(field, path)?;
    match get_path(raw, path) {
        Some(value) if is_non_negative_number(value) => Ok(()),
        Some(_) => Err(format!(
            "usage_fields.{field} points to {path:?} which is not a non-negative number; this usage was ignored"
        )),
        None if required => Err(format!(
            "usage_fields.{field} points to {path:?} which does not exist; this usage was ignored"
        )),
        None => Ok(()),
    }
}

fn is_non_negative_number(value: &Value) -> bool {
    value.as_u64().is_some()
        || value
            .as_f64()
            .is_some_and(|number| number.is_finite() && number >= 0.0)
}

fn validate_cost_override(raw: &Value, path: Option<&str>) -> Result<(), String> {
    let Some(path) = path else { return Ok(()) };
    validate_path_syntax("cost", path)?;
    match get_path(raw, path) {
        Some(value) if value.as_f64().is_some_and(f64::is_finite) => Ok(()),
        Some(_) => Err(format!(
            "usage_fields.cost points to {path:?} which is not a valid number; this usage was ignored"
        )),
        None => Ok(()),
    }
}

fn validate_path_syntax(field: &str, path: &str) -> Result<(), String> {
    if path.is_empty() || path.split('.').any(str::is_empty) {
        return Err(format!(
            "usage_fields.{field} path {path:?} is invalid; this usage was ignored"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_input_already_includes_cache() {
        let raw = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": { "cached_tokens": 800 }
        });
        let r = extract(&raw, &OPENAI, &UsageOverrides::default());
        assert_eq!(r.tokens.input, 1000, "cache must not be added twice");
        assert_eq!(r.tokens.cache_read, Some(800));
    }

    #[test]
    fn anthropic_input_excludes_cache_and_gets_normalized_up() {
        let raw = json!({
            "input_tokens": 200,
            "output_tokens": 50,
            "cache_read_input_tokens": 800,
            "cache_creation_input_tokens": 100
        });
        let r = extract(&raw, &ANTHROPIC, &UsageOverrides::default());
        assert_eq!(
            r.tokens.input, 1100,
            "total billable input = 200 + 800 + 100"
        );
        assert_eq!(r.tokens.cache_read, Some(800));
        assert_eq!(r.tokens.cache_write, Some(100));
    }

    #[test]
    fn deepseek_cache_hit_alias() {
        let raw = json!({
            "prompt_tokens": 900,
            "completion_tokens": 20,
            "prompt_cache_hit_tokens": 640,
            "prompt_cache_miss_tokens": 260
        });
        let r = extract(&raw, &DEEPSEEK, &UsageOverrides::default());
        assert_eq!(r.tokens.input, 900);
        assert_eq!(r.tokens.cache_read, Some(640));
    }

    #[test]
    fn deepseek_cache_hit_via_the_openai_mirror_only() {
        let raw = json!({
            "prompt_tokens": 900,
            "completion_tokens": 20,
            "prompt_tokens_details": { "cached_tokens": 640 }
        });
        let r = extract(&raw, &DEEPSEEK, &UsageOverrides::default());
        assert_eq!(
            r.tokens.input, 900,
            "input already includes cache; it must not be added again"
        );
        assert_eq!(r.tokens.cache_read, Some(640));
    }

    #[test]
    fn openrouter_reports_cost() {
        let raw = json!({ "prompt_tokens": 10, "completion_tokens": 5, "cost": 0.00123 });
        let r = extract(&raw, &OPENROUTER, &UsageOverrides::default());
        let c = r.cost.expect("the provider reports a cost");
        assert!((c.amount - 0.00123).abs() < f64::EPSILON);
        assert_eq!(c.currency, "USD");
    }

    #[test]
    fn gemini_output_absorbs_thoughts() {
        let raw = json!({
            "promptTokenCount": 100,
            "candidatesTokenCount": 40,
            "thoughtsTokenCount": 25,
            "totalTokenCount": 165
        });
        let r = extract(&raw, &GEMINI, &UsageOverrides::default());
        assert_eq!(
            r.tokens.output, 65,
            "40 visible + 25 thinking — this adds up with totalTokenCount"
        );
        assert_eq!(
            r.tokens.reasoning,
            Some(25),
            "after normalization reasoning is a subset of output; display only"
        );
        assert_eq!(
            r.tokens.input + r.tokens.output,
            165,
            "must agree with the provider's total"
        );
    }

    #[test]
    fn openai_output_already_includes_reasoning() {
        let raw = json!({
            "prompt_tokens": 100,
            "completion_tokens": 65,
            "completion_tokens_details": { "reasoning_tokens": 25 }
        });
        let r = extract(&raw, &OPENAI, &UsageOverrides::default());
        assert_eq!(r.tokens.output, 65, "reasoning must not be added twice");
        assert_eq!(r.tokens.reasoning, Some(25));
    }

    #[test]
    fn configured_path_does_not_fall_back() {
        let raw = json!({ "prompt_tokens": 100, "prompt_tokens_details": { "cached_tokens": 80 } });
        let ov = UsageOverrides {
            cache_read: Some("my.cached".into()),
            ..Default::default()
        };
        let r = extract(&raw, &OPENAI, &ov);
        assert_eq!(
            r.tokens.cache_read, None,
            "must not quietly fall back to openai's alias"
        );
    }

    #[test]
    fn checked_extraction_rejects_invalid_required_mapping() {
        let raw = json!({ "prompt_tokens": 10, "completion_tokens": 5 });
        let overrides = UsageOverrides {
            input: Some("wrong.input".into()),
            output: Some("completion_tokens".into()),
            ..Default::default()
        };
        let error = extract_checked(&raw, &OPENAI, &overrides).unwrap_err();
        assert!(error.contains("usage_fields.input"), "{error}");
    }

    #[test]
    fn checked_extraction_allows_absent_optional_mapping_but_rejects_wrong_type() {
        let raw = json!({ "prompt_tokens": 10, "completion_tokens": 5 });
        let overrides = UsageOverrides {
            cache_read: Some("details.cached".into()),
            ..Default::default()
        };
        assert!(extract_checked(&raw, &OPENAI, &overrides).is_ok());

        let raw = json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "details": { "cached": "not-a-number" }
        });
        let error = extract_checked(&raw, &OPENAI, &overrides).unwrap_err();
        assert!(error.contains("usage_fields.cache_read"), "{error}");
    }

    #[test]
    fn override_can_flip_the_cache_semantics() {
        let raw = json!({ "prompt_tokens": 200, "completion_tokens": 1, "my": { "cached": 800 } });
        let ov = UsageOverrides {
            cache_read: Some("my.cached".into()),
            input_includes_cache: Some(false),
            ..Default::default()
        };
        let r = extract(&raw, &OPENAI, &ov);
        assert_eq!(r.tokens.input, 1000);
    }

    #[test]
    fn raw_is_preserved_untouched() {
        let raw = json!({ "prompt_tokens": 1, "completion_tokens": 2, "vendor_extra": "keep me" });
        let r = extract(&raw, &OPENAI, &UsageOverrides::default());
        assert_eq!(r.raw.as_ref().unwrap()["vendor_extra"], "keep me");
    }

    #[test]
    fn dot_path_lookup() {
        let v = json!({ "a": { "b": { "c": 7 } } });
        assert_eq!(get_path(&v, "a.b.c").and_then(|x| x.as_u64()), Some(7));
        assert!(get_path(&v, "a.b.missing").is_none());
        assert!(get_path(&v, "nope.deep").is_none());
    }
}
