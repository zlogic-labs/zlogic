use serde_json::{Map, Value, json};
use zlogic_protocol::config::{GenericOpenAiDialect, ThinkingCapability};
use zlogic_protocol::llm::ThinkingIntent;

use super::ChatVendor;
use super::effort_for;
use super::vendors::{ThinkingDecision, decide};
use crate::emitter::ReasoningRawSpec;
use crate::usage_map::{self, UsageOverrides, UsageShape};

pub struct GenericOpenAi {
    dialect: GenericOpenAiDialect,
}

impl GenericOpenAi {
    pub fn new(dialect: GenericOpenAiDialect) -> Self {
        Self { dialect }
    }
}

impl ChatVendor for GenericOpenAi {
    fn name(&self) -> &'static str {
        "openai_generic"
    }

    fn reasoning_raw_spec(&self) -> ReasoningRawSpec {
        match &self.dialect.reasoning_carrier {
            Some(c) => ReasoningRawSpec::Carrier(c.clone()),
            None => ReasoningRawSpec::None,
        }
    }

    fn reasoning_delta<'a>(&self, delta: &'a Map<String, Value>) -> Option<&'a str> {
        let carrier = self.dialect.reasoning_carrier.as_deref()?;
        delta.get(carrier)?.as_str()
    }

    fn think_tags(&self) -> bool {
        self.dialect.think_tags
    }

    fn map_thinking(
        &self,
        intent: &ThinkingIntent,
        caps: &ThinkingCapability,
        warnings: &mut Vec<String>,
    ) -> Map<String, Value> {
        let decision = decide(intent, caps, warnings);
        let chosen = match decision {
            ThinkingDecision::Omit => None,
            ThinkingDecision::Off => self.dialect.thinking_off.as_ref(),
            ThinkingDecision::On => self.dialect.thinking_on.as_ref(),
        };
        let mut out = match chosen.and_then(|v| v.as_object()) {
            Some(obj) => obj.clone(),
            None => {
                if chosen.is_some() {
                    warnings.push(
                        "generic dialect thinking_on/off must be a JSON object; ignored".into(),
                    );
                }
                Map::new()
            }
        };

        if matches!(decision, ThinkingDecision::On)
            && let Some(e) = effort_for(intent, &caps.efforts)
        {
            if let Some(field) = &self.dialect.effort_field {
                out.insert(field.clone(), json!(e.as_str()));
            }
            if let Some(patch) = self.dialect.effort_map.get(&e) {
                match patch.as_object() {
                    Some(obj) => {
                        for (k, v) in obj {
                            out.insert(k.clone(), v.clone());
                        }
                    }
                    None => warnings.push(
                        "generic dialect effort_map entries must be JSON objects; ignored".into(),
                    ),
                }
            }
        }
        out
    }

    fn transform_body(&self, body: &mut Map<String, Value>) {
        for (k, v) in &self.dialect.extra_body {
            body.insert(k.clone(), v.clone());
        }
    }

    fn usage_shape(&self) -> &'static UsageShape {
        &usage_map::OPENAI
    }

    fn usage_overrides(&self) -> UsageOverrides {
        let f = &self.dialect.usage_fields;
        UsageOverrides {
            input: f.input.clone(),
            output: f.output.clone(),
            cache_read: f.cache_read.clone(),
            cache_write: f.cache_write.clone(),
            reasoning: f.reasoning.clone(),
            cost: f.cost.clone(),
            input_includes_cache: f.input_includes_cache,
            output_includes_reasoning: f.output_includes_reasoning,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zlogic_protocol::config::UsageFieldMap;
    use zlogic_protocol::llm::ThinkingMode;

    fn dialect() -> GenericOpenAiDialect {
        GenericOpenAiDialect {
            reasoning_carrier: Some("reasoning_content".into()),
            think_tags: false,
            effort_field: None,
            effort_map: Default::default(),
            thinking_on: Some(json!({ "enable_thinking": true })),
            thinking_off: Some(json!({ "enable_thinking": false })),
            usage_fields: UsageFieldMap {
                cache_read: Some("my.cached".into()),
                input_includes_cache: Some(false),
                ..Default::default()
            },
            extra_body: [("x_gateway".to_string(), json!("on"))]
                .into_iter()
                .collect(),
        }
    }

    fn caps() -> ThinkingCapability {
        ThinkingCapability {
            supported: true,
            can_disable: true,
            efforts: vec![],
            budget: false,
        }
    }

    #[test]
    fn declared_carrier_drives_both_parse_and_replay() {
        let v = GenericOpenAi::new(dialect());
        assert_eq!(
            v.reasoning_raw_spec(),
            ReasoningRawSpec::Carrier("reasoning_content".into())
        );

        let delta: Map<String, Value> =
            serde_json::from_value(json!({ "reasoning_content": "thinking" })).unwrap();
        assert_eq!(v.reasoning_delta(&delta), Some("thinking"));

        let mut msg = Map::new();
        v.splice_reasoning(
            &mut msg,
            &json!({ "carrier": "reasoning_content", "value": "thinking" }),
        );
        assert_eq!(msg["reasoning_content"], "thinking");
    }

    #[test]
    fn no_carrier_means_no_replayable_payload() {
        let mut d = dialect();
        d.reasoning_carrier = None;
        let v = GenericOpenAi::new(d);
        assert_eq!(v.reasoning_raw_spec(), ReasoningRawSpec::None);
        let delta: Map<String, Value> =
            serde_json::from_value(json!({ "reasoning_content": "x" })).unwrap();
        assert_eq!(v.reasoning_delta(&delta), None);
    }

    #[test]
    fn declared_thinking_fields_are_used_verbatim() {
        let v = GenericOpenAi::new(dialect());
        let mut w = Vec::new();
        let on = v.map_thinking(
            &ThinkingIntent {
                mode: ThinkingMode::On,
                effort: None,
                budget_tokens: None,
            },
            &caps(),
            &mut w,
        );
        assert_eq!(on["enable_thinking"], true);

        let off = v.map_thinking(
            &ThinkingIntent {
                mode: ThinkingMode::Off,
                effort: None,
                budget_tokens: None,
            },
            &caps(),
            &mut w,
        );
        assert_eq!(off["enable_thinking"], false);

        let dflt = v.map_thinking(&ThinkingIntent::default(), &caps(), &mut w);
        assert!(dflt.is_empty(), "nothing is sent under Default");
    }

    #[test]
    fn a_declared_effort_field_carries_the_chosen_tier() {
        use zlogic_protocol::llm::Effort;
        let mut d = dialect();
        d.effort_field = Some("reasoning_effort".into());
        d.thinking_on = None;
        let v = GenericOpenAi::new(d);
        let mut w = Vec::new();
        let caps = ThinkingCapability {
            supported: true,
            can_disable: true,
            efforts: vec![Effort::Low, Effort::Medium, Effort::High],
            budget: false,
        };

        let on = v.map_thinking(
            &ThinkingIntent {
                mode: ThinkingMode::On,
                effort: Some(Effort::High),
                budget_tokens: None,
            },
            &caps,
            &mut w,
        );
        assert_eq!(on["reasoning_effort"], "high");

        let coerced = v.map_thinking(
            &ThinkingIntent {
                mode: ThinkingMode::On,
                effort: Some(Effort::Max),
                budget_tokens: None,
            },
            &caps,
            &mut w,
        );
        assert_eq!(
            coerced["reasoning_effort"], "high",
            "max should be coerced to high"
        );

        let off = v.map_thinking(
            &ThinkingIntent {
                mode: ThinkingMode::Off,
                effort: Some(Effort::High),
                budget_tokens: None,
            },
            &caps,
            &mut w,
        );
        assert_eq!(off["enable_thinking"], false);
        assert!(!off.contains_key("reasoning_effort"));
    }

    #[test]
    fn without_an_effort_field_no_tier_is_sent() {
        use zlogic_protocol::llm::Effort;
        let v = GenericOpenAi::new(dialect());
        let mut w = Vec::new();
        let caps = ThinkingCapability {
            supported: true,
            can_disable: true,
            efforts: vec![Effort::High],
            budget: false,
        };
        let on = v.map_thinking(
            &ThinkingIntent {
                mode: ThinkingMode::On,
                effort: Some(Effort::High),
                budget_tokens: None,
            },
            &caps,
            &mut w,
        );
        assert_eq!(on.keys().collect::<Vec<_>>(), ["enable_thinking"]);
    }

    #[test]
    fn effort_map_patch_wins_over_fixed_values_for_that_tier() {
        use zlogic_protocol::llm::Effort;
        let mut d = dialect();
        d.effort_field = Some("reasoning_effort".into());
        d.effort_map = [
            (
                Effort::High,
                json!({ "thinking_budget": 32768, "enable_thinking": "deep" }),
            ),
            (Effort::Low, json!({ "thinking_budget": 1024 })),
        ]
        .into_iter()
        .collect();
        let v = GenericOpenAi::new(d);
        let mut w = Vec::new();
        let caps = ThinkingCapability {
            supported: true,
            can_disable: true,
            efforts: vec![Effort::Low, Effort::High],
            budget: false,
        };

        let on = v.map_thinking(
            &ThinkingIntent {
                mode: ThinkingMode::On,
                effort: Some(Effort::High),
                budget_tokens: None,
            },
            &caps,
            &mut w,
        );
        assert_eq!(on["thinking_budget"], 32768);
        assert_eq!(
            on["reasoning_effort"], "high",
            "the mapping stacks with effort_field"
        );
        assert_eq!(
            on["enable_thinking"], "deep",
            "on a shared key the mapping wins over thinking_on"
        );

        let coerced = v.map_thinking(
            &ThinkingIntent {
                mode: ThinkingMode::On,
                effort: Some(Effort::Max),
                budget_tokens: None,
            },
            &caps,
            &mut w,
        );
        assert_eq!(coerced["thinking_budget"], 32768);

        let off = v.map_thinking(
            &ThinkingIntent {
                mode: ThinkingMode::Off,
                effort: Some(Effort::High),
                budget_tokens: None,
            },
            &caps,
            &mut w,
        );
        assert!(!off.contains_key("thinking_budget"));
        assert!(
            w.is_empty(),
            "an object patch must not produce a warning: {w:?}"
        );
    }

    #[test]
    fn non_object_effort_map_entry_is_ignored_with_a_warning() {
        use zlogic_protocol::llm::Effort;
        let mut d = dialect();
        d.effort_map = [(Effort::High, json!("high"))].into_iter().collect();
        let v = GenericOpenAi::new(d);
        let mut w = Vec::new();
        let caps = ThinkingCapability {
            supported: true,
            can_disable: true,
            efforts: vec![Effort::High],
            budget: false,
        };
        let on = v.map_thinking(
            &ThinkingIntent {
                mode: ThinkingMode::On,
                effort: Some(Effort::High),
                budget_tokens: None,
            },
            &caps,
            &mut w,
        );
        assert_eq!(on.keys().collect::<Vec<_>>(), ["enable_thinking"]);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("effort_map"), "{w:?}");
    }

    #[test]
    fn extra_body_is_merged() {
        let v = GenericOpenAi::new(dialect());
        let mut body = Map::new();
        v.transform_body(&mut body);
        assert_eq!(body["x_gateway"], "on");
    }

    #[test]
    fn declared_usage_paths_do_not_fall_back_to_openai_aliases() {
        let v = GenericOpenAi::new(dialect());
        let raw = json!({
            "prompt_tokens": 200,
            "completion_tokens": 10,
            "prompt_tokens_details": { "cached_tokens": 999 }
        });
        let r = v.read_usage(&raw).unwrap();
        assert_eq!(r.tokens.cache_read, None);
        assert_eq!(r.tokens.input, 200);

        let raw = json!({ "prompt_tokens": 200, "completion_tokens": 10, "my": { "cached": 800 } });
        let r = v.read_usage(&raw).unwrap();
        assert_eq!(r.tokens.cache_read, Some(800));
        assert_eq!(r.tokens.input, 1000);
    }

    #[test]
    fn invalid_required_usage_path_is_ignored_instead_of_becoming_zero_usage() {
        let mut config = dialect();
        config.usage_fields.input = Some("missing.prompt_tokens".into());
        let v = GenericOpenAi::new(config);
        let raw = json!({ "prompt_tokens": 200, "completion_tokens": 10 });

        let error = v.read_usage(&raw).unwrap_err();
        assert!(error.contains("usage_fields.input"), "{error}");
        assert!(error.contains("this usage was ignored"), "{error}");
    }
}
