use serde_json::{Map, Value, json};
use zlogic_protocol::config::ThinkingCapability;
use zlogic_protocol::llm::{ResponseFormat, ThinkingIntent, ThinkingMode};

use super::{ChatVendor, effort_budget, effort_for};
use crate::emitter::ReasoningRawSpec;
use crate::usage_map::{self, UsageShape};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingDecision {
    Omit,
    On,
    Off,
}

pub(crate) fn decide(
    intent: &ThinkingIntent,
    caps: &ThinkingCapability,
    warnings: &mut Vec<String>,
) -> ThinkingDecision {
    match intent.mode {
        ThinkingMode::Default => ThinkingDecision::Omit,
        ThinkingMode::On => {
            if caps.supported {
                ThinkingDecision::On
            } else {
                warnings.push("thinking requested but the model does not support it".into());
                ThinkingDecision::Omit
            }
        }
        ThinkingMode::Off => {
            if !caps.supported {
                ThinkingDecision::Omit
            } else if caps.can_disable {
                ThinkingDecision::Off
            } else {
                ThinkingDecision::Omit
            }
        }
    }
}

// ───────────────────────────── vanilla OpenAI ─────────────────────────────

pub struct VanillaOpenAi;

impl ChatVendor for VanillaOpenAi {
    fn name(&self) -> &'static str {
        "openai_chat"
    }

    fn reasoning_raw_spec(&self) -> ReasoningRawSpec {
        ReasoningRawSpec::None
    }

    fn map_thinking(
        &self,
        intent: &ThinkingIntent,
        caps: &ThinkingCapability,
        warnings: &mut Vec<String>,
    ) -> Map<String, Value> {
        let mut m = Map::new();
        match decide(intent, caps, warnings) {
            ThinkingDecision::Omit | ThinkingDecision::Off => {}
            ThinkingDecision::On => {
                if let Some(e) = effort_for(intent, &caps.efforts) {
                    m.insert("reasoning_effort".into(), json!(e.as_str()));
                }
            }
        }
        m
    }

    fn splice_reasoning(&self, _msg: &mut Map<String, Value>, _raw: &Value) {}

    fn transform_body(&self, body: &mut Map<String, Value>) {
        body.entry("store".to_string()).or_insert(json!(false));
    }

    fn supports_prompt_cache_key(&self) -> bool {
        true
    }
}

// ───────────────────────────── DeepSeek ─────────────────────────────

pub struct DeepSeek;

impl ChatVendor for DeepSeek {
    fn name(&self) -> &'static str {
        "deepseek"
    }

    fn reasoning_raw_spec(&self) -> ReasoningRawSpec {
        ReasoningRawSpec::Carrier("reasoning_content".into())
    }

    fn reasoning_delta<'a>(&self, delta: &'a Map<String, Value>) -> Option<&'a str> {
        delta.get("reasoning_content")?.as_str()
    }

    fn always_send_reasoning_field(&self) -> bool {
        true
    }

    fn map_thinking(
        &self,
        intent: &ThinkingIntent,
        caps: &ThinkingCapability,
        warnings: &mut Vec<String>,
    ) -> Map<String, Value> {
        let mut m = Map::new();
        match decide(intent, caps, warnings) {
            ThinkingDecision::Omit => {}
            ThinkingDecision::On => {
                m.insert("thinking".into(), json!({ "type": "enabled" }));
                if let Some(e) = effort_for(intent, &caps.efforts) {
                    m.insert("reasoning_effort".into(), json!(e.as_str()));
                }
            }
            ThinkingDecision::Off => {
                m.insert("thinking".into(), json!({ "type": "disabled" }));
            }
        }
        m
    }

    fn usage_shape(&self) -> &'static UsageShape {
        &usage_map::DEEPSEEK
    }

    fn response_format(&self, rf: &ResponseFormat, warnings: &mut Vec<String>) -> Value {
        if matches!(rf, ResponseFormat::JsonSchema { .. }) {
            warnings.push(
                "deepseek only accepts {\"type\":\"json_object\"}; the json_schema was dropped, \
                 put the expected field shape into the prompt"
                    .into(),
            );
        }
        json!({ "type": "json_object" })
    }
}

// ───────────────────────────── GLM ─────────────────────────────

pub struct Glm;

impl ChatVendor for Glm {
    fn name(&self) -> &'static str {
        "glm"
    }

    fn reasoning_raw_spec(&self) -> ReasoningRawSpec {
        ReasoningRawSpec::Carrier("reasoning_content".into())
    }

    fn reasoning_delta<'a>(&self, delta: &'a Map<String, Value>) -> Option<&'a str> {
        delta.get("reasoning_content")?.as_str()
    }

    fn map_thinking(
        &self,
        intent: &ThinkingIntent,
        caps: &ThinkingCapability,
        warnings: &mut Vec<String>,
    ) -> Map<String, Value> {
        let mut m = Map::new();
        match decide(intent, caps, warnings) {
            ThinkingDecision::Omit => {}
            ThinkingDecision::On => {
                m.insert("thinking".into(), json!({ "type": "enabled" }));
            }
            ThinkingDecision::Off => {
                m.insert("thinking".into(), json!({ "type": "disabled" }));
            }
        }
        m
    }
}

pub struct DashScope;

impl ChatVendor for DashScope {
    fn name(&self) -> &'static str {
        "dashscope"
    }

    fn reasoning_raw_spec(&self) -> ReasoningRawSpec {
        ReasoningRawSpec::Carrier("reasoning_content".into())
    }

    fn reasoning_delta<'a>(&self, delta: &'a Map<String, Value>) -> Option<&'a str> {
        delta.get("reasoning_content")?.as_str()
    }

    fn map_thinking(
        &self,
        intent: &ThinkingIntent,
        caps: &ThinkingCapability,
        warnings: &mut Vec<String>,
    ) -> Map<String, Value> {
        let mut m = Map::new();
        match decide(intent, caps, warnings) {
            ThinkingDecision::Omit => return m,
            ThinkingDecision::Off => {
                m.insert("enable_thinking".into(), json!(false));
                return m;
            }
            ThinkingDecision::On => {
                m.insert("enable_thinking".into(), json!(true));
            }
        }
        if caps.budget
            && let Some(b) = effort_budget(intent)
        {
            m.insert("thinking_budget".into(), json!(b));
        }
        m
    }
}

pub struct QwenLocal;

impl ChatVendor for QwenLocal {
    fn name(&self) -> &'static str {
        "qwen_local"
    }

    fn reasoning_raw_spec(&self) -> ReasoningRawSpec {
        ReasoningRawSpec::Carrier("reasoning_content".into())
    }

    fn reasoning_delta<'a>(&self, delta: &'a Map<String, Value>) -> Option<&'a str> {
        delta.get("reasoning_content")?.as_str()
    }

    fn think_tags(&self) -> bool {
        true
    }

    fn map_thinking(
        &self,
        intent: &ThinkingIntent,
        caps: &ThinkingCapability,
        warnings: &mut Vec<String>,
    ) -> Map<String, Value> {
        let mut m = Map::new();
        let enable = match decide(intent, caps, warnings) {
            ThinkingDecision::Omit => return m,
            ThinkingDecision::Off => false,
            ThinkingDecision::On => true,
        };
        m.insert(
            "chat_template_kwargs".into(),
            json!({ "enable_thinking": enable }),
        );
        m
    }
}

// ───────────────────────────── OpenRouter ─────────────────────────────

pub struct OpenRouter;

impl ChatVendor for OpenRouter {
    fn name(&self) -> &'static str {
        "openrouter"
    }

    fn reasoning_raw_spec(&self) -> ReasoningRawSpec {
        ReasoningRawSpec::Carrier("reasoning".into())
    }

    fn reasoning_delta<'a>(&self, delta: &'a Map<String, Value>) -> Option<&'a str> {
        delta.get("reasoning")?.as_str()
    }

    fn reasoning_raw_from_delta(
        &self,
        scratch: &mut Map<String, Value>,
        delta: &Map<String, Value>,
    ) -> Option<Value> {
        let incoming = delta.get("reasoning_details")?.as_array()?;
        let acc = scratch
            .entry("reasoning_details")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()?;

        for item in incoming {
            match item.get("index").and_then(|i| i.as_u64()) {
                Some(i) => {
                    let i = i as usize;
                    if acc.len() <= i {
                        acc.resize(i + 1, Value::Null);
                    }
                    merge_detail(&mut acc[i], item);
                }
                None => acc.push(item.clone()),
            }
        }

        let value = Value::Array(acc.iter().filter(|v| !v.is_null()).cloned().collect());
        Some(json!({ "carrier": "reasoning_details", "value": value }))
    }

    fn transform_body(&self, body: &mut Map<String, Value>) {
        body.insert("usage".into(), json!({ "include": true }));
    }

    fn usage_shape(&self) -> &'static UsageShape {
        &usage_map::OPENROUTER
    }

    fn map_thinking(
        &self,
        intent: &ThinkingIntent,
        caps: &ThinkingCapability,
        warnings: &mut Vec<String>,
    ) -> Map<String, Value> {
        let mut m = Map::new();
        let mut reasoning = Map::new();
        match decide(intent, caps, warnings) {
            ThinkingDecision::Omit => return m,
            ThinkingDecision::Off => {
                reasoning.insert("enabled".into(), json!(false));
            }
            ThinkingDecision::On => {
                reasoning.insert("enabled".into(), json!(true));
                if let Some(e) = effort_for(intent, &caps.efforts) {
                    reasoning.insert("effort".into(), json!(e.as_str()));
                }
                if let Some(b) = intent.budget_tokens {
                    reasoning.insert("max_tokens".into(), json!(b));
                }
            }
        }
        m.insert("reasoning".into(), Value::Object(reasoning));
        m
    }
}

fn merge_detail(slot: &mut Value, incoming: &Value) {
    if slot.is_null() {
        *slot = incoming.clone();
        return;
    }
    let (Some(dst), Some(src)) = (slot.as_object_mut(), incoming.as_object()) else {
        *slot = incoming.clone();
        return;
    };
    for (k, v) in src {
        match (dst.get_mut(k), v.as_str()) {
            (Some(Value::String(existing)), Some(add)) if k == "text" || k == "data" => {
                existing.push_str(add);
            }
            _ => {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

// ───────────────────────────── Fireworks ─────────────────────────────

pub struct Fireworks;

impl ChatVendor for Fireworks {
    fn name(&self) -> &'static str {
        "fireworks"
    }

    fn reasoning_raw_spec(&self) -> ReasoningRawSpec {
        ReasoningRawSpec::Carrier("reasoning_content".into())
    }

    fn reasoning_delta<'a>(&self, delta: &'a Map<String, Value>) -> Option<&'a str> {
        delta.get("reasoning_content")?.as_str()
    }

    fn map_thinking(
        &self,
        intent: &ThinkingIntent,
        caps: &ThinkingCapability,
        warnings: &mut Vec<String>,
    ) -> Map<String, Value> {
        let mut m = Map::new();
        match decide(intent, caps, warnings) {
            ThinkingDecision::Omit | ThinkingDecision::Off => {}
            ThinkingDecision::On => {
                if let Some(e) = effort_for(intent, &caps.efforts) {
                    m.insert("reasoning_effort".into(), json!(e.as_str()));
                }
            }
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::llm::Effort;

    fn caps(supported: bool, can_disable: bool) -> ThinkingCapability {
        ThinkingCapability {
            supported,
            can_disable,
            efforts: vec![],
            budget: false,
        }
    }

    fn intent(mode: ThinkingMode) -> ThinkingIntent {
        ThinkingIntent {
            mode,
            effort: None,
            budget_tokens: None,
        }
    }

    #[test]
    fn default_mode_sends_nothing() {
        let mut w = Vec::new();
        for v in [
            &DeepSeek as &dyn ChatVendor,
            &Glm,
            &DashScope,
            &QwenLocal,
            &OpenRouter,
            &Fireworks,
            &VanillaOpenAi,
        ] {
            let m = v.map_thinking(&intent(ThinkingMode::Default), &caps(true, true), &mut w);
            assert!(
                m.is_empty(),
                "{} must not send any thinking field under Default",
                v.name()
            );
        }
        assert!(w.is_empty());
    }

    #[test]
    fn off_on_a_model_that_cannot_disable_omits_and_lets_the_model_default() {
        let mut w = Vec::new();
        let m = VanillaOpenAi.map_thinking(&intent(ThinkingMode::Off), &caps(true, false), &mut w);
        assert!(
            m.is_empty(),
            "when it cannot be fully disabled, the lowest tier field would 400 at the gateway — send nothing and let the model use its default"
        );
        assert!(w.is_empty(), "no flooring any more, so no warning either");
    }

    #[test]
    fn off_on_a_disableable_model_sends_the_off_field() {
        let mut w = Vec::new();
        let m = DeepSeek.map_thinking(&intent(ThinkingMode::Off), &caps(true, true), &mut w);
        assert_eq!(m["thinking"], json!({ "type": "disabled" }));
        assert!(w.is_empty());
    }

    #[test]
    fn requesting_thinking_on_a_non_thinking_model_warns_and_omits() {
        let mut w = Vec::new();
        let m = DeepSeek.map_thinking(&intent(ThinkingMode::On), &caps(false, false), &mut w);
        assert!(m.is_empty());
        assert!(!w.is_empty());
    }

    #[test]
    fn vanilla_openai_opts_out_of_server_side_retention() {
        let mut body = Map::new();
        VanillaOpenAi.transform_body(&mut body);
        assert_eq!(body["store"], false);

        let mut body = Map::new();
        body.insert("store".into(), json!(true));
        VanillaOpenAi.transform_body(&mut body);
        assert_eq!(body["store"], true, "the configured value should win");
    }

    fn with_efforts(efforts: Vec<Effort>) -> ThinkingCapability {
        ThinkingCapability {
            supported: true,
            can_disable: true,
            efforts,
            budget: false,
        }
    }

    fn on(effort: Effort) -> ThinkingIntent {
        ThinkingIntent {
            mode: ThinkingMode::On,
            effort: Some(effort),
            budget_tokens: None,
        }
    }

    #[test]
    fn an_undeclared_ladder_sends_no_effort_at_all() {
        for v in [&VanillaOpenAi as &dyn ChatVendor, &OpenRouter, &Fireworks] {
            let mut w = Vec::new();
            let m = v.map_thinking(&on(Effort::Max), &caps(true, true), &mut w);
            let wire = serde_json::to_string(&m).unwrap();
            assert!(
                !wire.contains("effort"),
                "{} must not guess one when there is no tier declaration: {wire}",
                v.name()
            );
        }
    }

    #[test]
    fn a_declared_tier_is_used_verbatim() {
        let declared = with_efforts(vec![Effort::Low, Effort::Medium, Effort::High]);
        let mut w = Vec::new();
        let m = VanillaOpenAi.map_thinking(&on(Effort::High), &declared, &mut w);
        assert_eq!(m["reasoning_effort"], "high");

        let mut w = Vec::new();
        let m = DeepSeek.map_thinking(
            &on(Effort::Low),
            &with_efforts(vec![Effort::Low, Effort::High, Effort::Max]),
            &mut w,
        );
        assert_eq!(m["thinking"], json!({ "type": "enabled" }));
        assert_eq!(m["reasoning_effort"], "low");
        assert!(w.is_empty());
    }

    #[test]
    fn deepseek_effort_is_clamped_to_its_declared_ladder() {
        let mut w = Vec::new();
        let m = DeepSeek.map_thinking(
            &on(Effort::Low),
            &with_efforts(vec![Effort::High, Effort::Max]),
            &mut w,
        );
        assert_eq!(
            m["reasoning_effort"], "high",
            "low is not on the ladder, so it falls back to the lowest tier high"
        );

        let mut w = Vec::new();
        let m = DeepSeek.map_thinking(
            &on(Effort::Medium),
            &with_efforts(vec![Effort::Low, Effort::High, Effort::Max]),
            &mut w,
        );
        assert_eq!(
            m["reasoning_effort"], "low",
            "medium is not on the ladder, so it steps down to low"
        );

        let mut w = Vec::new();
        let m = DeepSeek.map_thinking(
            &on(Effort::XHigh),
            &with_efforts(vec![Effort::Low, Effort::High, Effort::Max]),
            &mut w,
        );
        assert_eq!(
            m["reasoning_effort"], "high",
            "xhigh is not on the ladder, so it steps down to high"
        );
    }

    #[test]
    fn deepseek_without_an_effort_ladder_sends_no_effort() {
        let mut w = Vec::new();
        let m = DeepSeek.map_thinking(&on(Effort::High), &caps(true, true), &mut w);
        assert_eq!(m["thinking"], json!({ "type": "enabled" }));
        assert!(!m.contains_key("reasoning_effort"), "{m:?}");
    }

    #[test]
    fn a_missing_tier_falls_back_downwards_not_upwards() {
        let opus_4_6 = with_efforts(vec![Effort::Low, Effort::Medium, Effort::High, Effort::Max]);
        let mut w = Vec::new();
        let m = VanillaOpenAi.map_thinking(&on(Effort::XHigh), &opus_4_6, &mut w);
        assert_eq!(m["reasoning_effort"], "high");
    }

    #[test]
    fn a_tier_below_the_floor_uses_the_floor() {
        let declared = with_efforts(vec![Effort::Medium, Effort::High]);
        let mut w = Vec::new();
        let m = VanillaOpenAi.map_thinking(&on(Effort::Minimal), &declared, &mut w);
        assert_eq!(m["reasoning_effort"], "medium");
    }

    #[test]
    fn the_undisableable_off_omits_instead_of_flooring() {
        let off = ThinkingIntent {
            mode: ThinkingMode::Off,
            effort: None,
            budget_tokens: None,
        };

        let mut w = Vec::new();
        let m = VanillaOpenAi.map_thinking(
            &off,
            &ThinkingCapability {
                supported: true,
                can_disable: false,
                efforts: vec![Effort::Low, Effort::High],
                budget: false,
            },
            &mut w,
        );
        assert!(
            m.is_empty(),
            "reasoning_effort must not be sent while it is gated"
        );
        assert!(w.is_empty());

        let mut w = Vec::new();
        let m = VanillaOpenAi.map_thinking(&off, &caps(true, false), &mut w);
        assert!(
            m.is_empty(),
            "with no declared tier, minimal must not be sent either"
        );
    }

    #[test]
    fn openrouter_always_asks_for_cost() {
        let mut body = Map::new();
        OpenRouter.transform_body(&mut body);
        assert_eq!(body["usage"], json!({ "include": true }));
    }

    #[test]
    fn openrouter_merges_reasoning_details_by_index() {
        let mut scratch = Map::new();
        let d1: Map<String, Value> = serde_json::from_value(json!({
            "reasoning_details": [{ "type": "reasoning.text", "text": "Think", "index": 0 }]
        }))
        .unwrap();
        let d2: Map<String, Value> = serde_json::from_value(json!({
            "reasoning_details": [
                { "type": "reasoning.text", "text": "ing…", "index": 0 },
                { "type": "reasoning.encrypted", "data": "enc", "index": 1 }
            ]
        }))
        .unwrap();

        OpenRouter.reasoning_raw_from_delta(&mut scratch, &d1);
        let raw = OpenRouter
            .reasoning_raw_from_delta(&mut scratch, &d2)
            .unwrap();

        assert_eq!(raw["carrier"], "reasoning_details");
        let arr = raw["value"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "order and count must be preserved");
        assert_eq!(
            arr[0]["text"], "Thinking…",
            "text fragments at the same index are joined"
        );
        assert_eq!(arr[1]["data"], "enc");
    }

    #[test]
    fn default_splice_handles_carrier_and_drops_unknown() {
        let mut msg = Map::new();
        DeepSeek.splice_reasoning(
            &mut msg,
            &json!({ "carrier": "reasoning_content", "value": "x" }),
        );
        assert_eq!(msg["reasoning_content"], "x");

        let mut msg = Map::new();
        DeepSeek.splice_reasoning(&mut msg, &json!({ "thoughtSignature": "sig" }));
        assert!(
            msg.is_empty(),
            "another vendor's raw payload must be discarded"
        );
    }

    #[test]
    fn openrouter_details_array_splices_back_verbatim() {
        let mut msg = Map::new();
        let details = json!([{ "type": "reasoning.text", "text": "t", "index": 0 }]);
        OpenRouter.splice_reasoning(
            &mut msg,
            &json!({ "carrier": "reasoning_details", "value": details.clone() }),
        );
        assert_eq!(msg["reasoning_details"], details);
    }

    #[test]
    fn dashscope_budget_only_when_the_model_takes_one() {
        let mut w = Vec::new();
        let i = ThinkingIntent {
            mode: ThinkingMode::On,
            effort: None,
            budget_tokens: Some(8000),
        };
        let with = ThinkingCapability {
            supported: true,
            can_disable: true,
            efforts: vec![],
            budget: true,
        };
        let m = DashScope.map_thinking(&i, &with, &mut w);
        assert_eq!(m["thinking_budget"], 8000);

        let m = DashScope.map_thinking(&i, &caps(true, true), &mut w);
        assert!(!m.contains_key("thinking_budget"));
    }

    #[test]
    fn qwen_local_uses_chat_template_kwargs() {
        let mut w = Vec::new();
        let m = QwenLocal.map_thinking(&intent(ThinkingMode::On), &caps(true, true), &mut w);
        assert_eq!(
            m["chat_template_kwargs"],
            json!({ "enable_thinking": true })
        );
    }

    #[test]
    fn deepseek_collapses_both_json_variants_to_json_object() {
        use zlogic_protocol::llm::ResponseFormat;

        let mut w = Vec::new();
        let v = DeepSeek.response_format(&ResponseFormat::Json, &mut w);
        assert_eq!(v, json!({ "type": "json_object" }));
        assert!(
            w.is_empty(),
            "the Json variant needs no downgrade, so nothing is recorded"
        );

        let mut w = Vec::new();
        let v = DeepSeek.response_format(
            &ResponseFormat::JsonSchema {
                name: "x".into(),
                schema: json!({ "type": "object" }),
                strict: true,
            },
            &mut w,
        );
        assert_eq!(v, json!({ "type": "json_object" }));
        assert!(
            !w.is_empty(),
            "dropping the schema must be recorded, never silent"
        );
        assert!(w[0].contains("json_object"), "{}", w[0]);
    }

    #[test]
    fn vanilla_openai_keeps_the_native_schema_shape() {
        use zlogic_protocol::llm::ResponseFormat;

        let mut w = Vec::new();
        let v = VanillaOpenAi.response_format(
            &ResponseFormat::JsonSchema {
                name: "verdict".into(),
                schema: json!({ "type": "object" }),
                strict: true,
            },
            &mut w,
        );
        assert_eq!(v["type"], "json_schema");
        assert_eq!(v["json_schema"]["name"], "verdict");
        assert!(w.is_empty());
    }
}
