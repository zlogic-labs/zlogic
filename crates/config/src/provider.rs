use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zlogic_credential::credential_candidates;
use zlogic_protocol::config::{
    ClientSpec, GenericOpenAiDialect, ModelCapabilities, NetworkConfig, Pricing, ResolvedModel,
    Sdk, ThinkingCapability, Tier, Wiring, resolve_client,
};
use zlogic_protocol::message::Source;
use zlogic_protocol::usage::QuotaConfig;

use crate::{ConfigError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdk: Option<Sdk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guide_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generic: Option<GenericOpenAiDialect>,
    #[serde(default)]
    pub wiring: Wiring,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,
    #[serde(default)]
    pub default_params: BTreeMap<String, Value>,
    #[serde(default)]
    pub quotas: Vec<QuotaConfig>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelSettings>,
    #[serde(default = "yes")]
    pub enabled: bool,

    #[serde(skip)]
    pub credential_ok: Option<bool>,
}

fn yes() -> bool {
    true
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            sdk: None,
            base_url: None,
            guide_url: None,
            generic: None,
            wiring: Wiring::default(),
            network: None,
            default_params: BTreeMap::new(),
            quotas: Vec::new(),
            models: BTreeMap::new(),
            enabled: true,
            credential_ok: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wire_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction_threshold: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing: Option<Pricing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,
    #[serde(default)]
    pub default_params: BTreeMap<String, Value>,
    #[serde(default)]
    pub no_think_params: BTreeMap<String, Value>,
    #[serde(default)]
    pub quotas: Vec<QuotaConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdk: Option<Sdk>,
}

fn builtin_context_window(sdk: Sdk) -> u64 {
    match sdk {
        Sdk::Anthropic | Sdk::Bedrock => 200_000,
        Sdk::Gemini => 128_000,
        Sdk::OpenAiChat | Sdk::OpenAiResponses => 128_000,
        Sdk::DeepSeek | Sdk::Glm | Sdk::DashScope | Sdk::OpenRouter | Sdk::Fireworks => 64_000,
        Sdk::OpenAiGeneric | Sdk::QwenLocal => 32_000,
    }
}

fn builtin_thinking(sdk: Sdk) -> ThinkingCapability {
    let supported = matches!(
        sdk,
        Sdk::Anthropic
            | Sdk::Bedrock
            | Sdk::OpenAiResponses
            | Sdk::Gemini
            | Sdk::DeepSeek
            | Sdk::Glm
            | Sdk::DashScope
            | Sdk::QwenLocal
            | Sdk::OpenRouter
    );
    ThinkingCapability {
        supported,
        can_disable: false,
        efforts: vec![],
        budget: false,
    }
}

const ENV_TABLE: &[(&str, Sdk, &[&str])] = &[
    ("openai", Sdk::OpenAiChat, &["OPENAI_API_KEY"]),
    ("anthropic", Sdk::Anthropic, &["ANTHROPIC_API_KEY"]),
    ("deepseek", Sdk::DeepSeek, &["DEEPSEEK_API_KEY"]),
    (
        "gemini",
        Sdk::Gemini,
        &[
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GOOGLE_GENERATIVE_AI_API_KEY",
        ],
    ),
    ("openrouter", Sdk::OpenRouter, &["OPENROUTER_API_KEY"]),
    ("dashscope", Sdk::DashScope, &["DASHSCOPE_API_KEY"]),
    ("glm", Sdk::Glm, &["ZHIPUAI_API_KEY", "GLM_API_KEY"]),
    ("fireworks", Sdk::Fireworks, &["FIREWORKS_API_KEY"]),
];

const FIREWORKS_GUIDE_URL: &str = "https://docs.fireworks.ai/getting-started/onboarding";
const BEDROCK_GUIDE_URL: &str =
    "https://docs.aws.amazon.com/IAM/latest/UserGuide/id_credentials_access-keys.html";

pub fn detect_providers(env: impl Fn(&str) -> Option<String>) -> Vec<(String, ProviderSettings)> {
    let present = |name: &str| env(name).is_some_and(|v| !v.trim().is_empty());
    let mut out = Vec::new();

    for (id, sdk, key_vars) in ENV_TABLE {
        if catalog_has(id) {
            continue;
        }
        if !key_vars.iter().any(|v| present(v)) {
            continue;
        }
        out.push((
            id.to_string(),
            ProviderSettings {
                sdk: Some(*sdk),
                guide_url: Some(FIREWORKS_GUIDE_URL.to_string()),
                ..Default::default()
            },
        ));
    }

    if present("AWS_ACCESS_KEY_ID") && present("AWS_SECRET_ACCESS_KEY") {
        out.push((
            "bedrock".to_string(),
            ProviderSettings {
                sdk: Some(Sdk::Bedrock),
                base_url: env("AWS_REGION")
                    .map(|r| format!("https://bedrock-runtime.{r}.amazonaws.com")),
                guide_url: Some(BEDROCK_GUIDE_URL.to_string()),
                ..Default::default()
            },
        ));
    }

    out
}

fn catalog_has(provider_id: &str) -> bool {
    let needle = format!("\n  {provider_id}:");
    crate::CATALOG.contains(&needle)
}

#[derive(Debug, Clone, Copy)]
pub struct ResolveOptions {
    pub default_compact_ratio: f32,
    pub auto_detect_env: bool,
    pub config_revision: u64,
}

pub fn resolve_model(
    provider_id: &str,
    provider: &ProviderSettings,
    model_id: &str,
    model: &ModelSettings,
    opts: ResolveOptions,
    warnings: &mut Vec<String>,
) -> Result<ResolvedModel> {
    let ResolveOptions {
        default_compact_ratio,
        auto_detect_env,
        config_revision,
    } = opts;
    let sdk = model.sdk.or(provider.sdk).ok_or_else(|| {
        ConfigError::Provider(format!("provider {provider_id} is missing an sdk"))
    })?;

    let client: ClientSpec = resolve_client(sdk, provider.generic.as_ref())
        .map_err(|e| ConfigError::Provider(format!("provider {provider_id}: {e}")))?;

    let context_window = match model.context_window {
        Some(w) => w,
        None => {
            let w = builtin_context_window(sdk);
            warnings.push(format!(
                "{provider_id}:{model_id} has no context_window configured; fell back to \
                 the conservative value {w}; configuring the real window would compact less often"
            ));
            w
        }
    };

    let compaction_threshold = model.compaction_threshold.or_else(|| {
        let ratio = default_compact_ratio.clamp(0.1, 0.95) as f64;
        Some((context_window as f64 * ratio).round() as u64)
    });

    let mut default_params = provider.default_params.clone();
    for (k, v) in &model.default_params {
        default_params.insert(k.clone(), v.clone());
    }

    let network = provider
        .network
        .as_ref()
        .unwrap_or(&NetworkConfig::default())
        .overlay(model.network.as_ref().unwrap_or(&NetworkConfig::default()));

    Ok(ResolvedModel {
        source: Source::new(provider_id, model_id),
        wire_model: model
            .wire_model
            .clone()
            .unwrap_or_else(|| model_id.to_string()),
        display_name: model
            .display_name
            .clone()
            .unwrap_or_else(|| format!("{provider_id}:{model_id}")),
        client,
        base_url: provider.base_url.clone(),
        wiring: provider.wiring.clone(),
        network,
        credential_refs: credential_candidates(provider_id, auto_detect_env)
            .iter()
            .map(ToString::to_string)
            .collect(),
        context_window,
        max_output_tokens: model.max_output_tokens,
        compaction_threshold,
        capabilities: ModelCapabilities {
            vision: model.vision,
            thinking: model
                .thinking
                .clone()
                .unwrap_or_else(|| builtin_thinking(sdk)),
        },
        pricing: model.pricing.clone(),
        default_params,
        config_revision,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zlogic_credential::CredentialRef;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn providers_in_the_catalog_are_left_to_the_catalog() {
        let found = detect_providers(env_of(&[
            ("OPENAI_API_KEY", "sk-x"),
            ("ANTHROPIC_API_KEY", "sk-y"),
            ("FIREWORKS_API_KEY", "sk-z"),
        ]));
        let ids: Vec<&str> = found.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            ["fireworks"],
            "only the one the catalog does not cover is left"
        );
        assert_eq!(found[0].1.sdk, Some(Sdk::Fireworks));
        assert_eq!(found[0].1.guide_url.as_deref(), Some(FIREWORKS_GUIDE_URL));
    }

    #[test]
    fn detection_never_carries_the_key_itself() {
        let found = detect_providers(env_of(&[("FIREWORKS_API_KEY", "sk-super-secret")]));
        assert!(
            !format!("{found:?}").contains("sk-super-secret"),
            "the key's value must never appear in the config structs"
        );
    }

    #[test]
    fn blank_env_values_do_not_count_as_configured() {
        assert!(detect_providers(env_of(&[("FIREWORKS_API_KEY", "   ")])).is_empty());
    }

    #[test]
    fn base_url_env_vars_no_longer_rewrite_a_builtin_endpoint() {
        let found = detect_providers(env_of(&[
            ("FIREWORKS_API_KEY", "k"),
            ("FIREWORKS_BASE_URL", "https://gw.corp"),
            ("OPENAI_BASE_URL", "https://gw.corp"),
        ]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.base_url, None);
    }

    #[test]
    fn bedrock_still_takes_its_region_from_the_environment() {
        let found = detect_providers(env_of(&[
            ("AWS_ACCESS_KEY_ID", "a"),
            ("AWS_SECRET_ACCESS_KEY", "b"),
            ("AWS_REGION", "ap-northeast-2"),
        ]));
        assert_eq!(found[0].0, "bedrock");
        assert_eq!(
            found[0].1.base_url.as_deref(),
            Some("https://bedrock-runtime.ap-northeast-2.amazonaws.com")
        );
        assert_eq!(found[0].1.guide_url.as_deref(), Some(BEDROCK_GUIDE_URL));
    }

    #[test]
    fn candidates_are_ordered_from_most_specific_to_least() {
        let c: Vec<String> = credential_candidates("anthropic", true)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            c,
            [
                "env:ZLOGIC_ANTHROPIC_API_KEY",
                "env:ANTHROPIC_API_KEY",
                "keyring:anthropic",
            ],
            "ANTHROPIC_API_KEY is already <PROVIDER>_API_KEY and must not be listed twice"
        );
    }

    #[test]
    fn every_known_alias_becomes_a_candidate() {
        let c: Vec<String> = credential_candidates("gemini", true)
            .iter()
            .map(ToString::to_string)
            .collect();
        for want in [
            "env:GEMINI_API_KEY",
            "env:GOOGLE_API_KEY",
            "env:GOOGLE_GENERATIVE_AI_API_KEY",
        ] {
            assert!(c.contains(&want.to_string()), "{c:?} is missing {want}");
        }
    }

    #[test]
    fn an_unknown_provider_id_still_gets_a_convention() {
        let c: Vec<String> = credential_candidates("my-gw.2", true)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            c,
            [
                "env:ZLOGIC_MY_GW_2_API_KEY",
                "env:MY_GW_2_API_KEY",
                "keyring:my-gw.2"
            ],
            "non-alphanumerics must become underscores, otherwise it is not a valid variable name"
        );
    }

    #[test]
    fn disabling_env_detection_leaves_only_the_keyring() {
        let c = credential_candidates("anthropic", false);
        assert_eq!(c, [CredentialRef::keyring("anthropic")]);
    }

    #[test]
    fn bedrock_needs_both_aws_variables() {
        assert!(detect_providers(env_of(&[("AWS_ACCESS_KEY_ID", "a")])).is_empty());
    }

    fn opts(revision: u64) -> ResolveOptions {
        ResolveOptions {
            default_compact_ratio: 0.8,
            auto_detect_env: true,
            config_revision: revision,
        }
    }

    #[test]
    fn network_timeouts_merge_provider_then_model_per_field() {
        let mut p = provider(Sdk::OpenAiChat);
        p.network = Some(NetworkConfig {
            connect_timeout_ms: Some(10_000),
            read_timeout_ms: Some(60_000),
        });
        let m = ModelSettings {
            network: Some(NetworkConfig {
                connect_timeout_ms: None,
                read_timeout_ms: Some(600_000),
            }),
            ..Default::default()
        };
        let mut warnings = Vec::new();
        let r = resolve_model("p", &p, "m", &m, opts(1), &mut warnings).unwrap();
        assert_eq!(r.network.connect_timeout_ms, Some(10_000));
        assert_eq!(r.network.read_timeout_ms, Some(600_000));
    }

    #[test]
    fn network_timeouts_default_to_unset() {
        let mut warnings = Vec::new();
        let r = resolve_model(
            "p",
            &provider(Sdk::OpenAiChat),
            "m",
            &ModelSettings::default(),
            opts(1),
            &mut warnings,
        )
        .unwrap();
        assert!(r.network.is_empty());
    }

    fn provider(sdk: Sdk) -> ProviderSettings {
        ProviderSettings {
            sdk: Some(sdk),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_fills_ids_and_defaults() {
        let mut w = Vec::new();
        let m = resolve_model(
            "deepseek",
            &provider(Sdk::DeepSeek),
            "deepseek-v4",
            &ModelSettings {
                context_window: Some(128_000),
                ..Default::default()
            },
            opts(7),
            &mut w,
        )
        .unwrap();

        assert_eq!(m.source, Source::new("deepseek", "deepseek-v4"));
        assert_eq!(
            m.wire_model, "deepseek-v4",
            "the default wire name = the config key"
        );
        assert_eq!(m.compaction_threshold, Some(102_400), "0.8 × the window");
        assert_eq!(m.config_revision, 7);
        assert!(w.is_empty());
    }

    #[test]
    fn missing_context_window_falls_back_conservatively_and_warns() {
        let mut w = Vec::new();
        let m = resolve_model(
            "corp",
            &provider(Sdk::OpenAiGeneric),
            "mystery",
            &ModelSettings::default(),
            opts(1),
            &mut w,
        )
        .unwrap();
        assert_eq!(m.context_window, 32_000);
        assert!(!w.is_empty(), "a fallback value must leave a trace");
    }

    #[test]
    fn model_level_settings_override_provider_level() {
        let mut p = provider(Sdk::DeepSeek);
        p.default_params
            .insert("temperature".into(), serde_json::json!(0.2));
        p.default_params
            .insert("top_p".into(), serde_json::json!(0.9));

        let mut ms = ModelSettings {
            context_window: Some(1000),
            ..Default::default()
        };
        ms.default_params
            .insert("temperature".into(), serde_json::json!(0.7));
        ms.sdk = Some(Sdk::Glm);

        let mut w = Vec::new();
        let m = resolve_model("p", &p, "m", &ms, opts(1), &mut w).unwrap();
        assert_eq!(m.default_params["temperature"], 0.7);
        assert_eq!(
            m.default_params["top_p"], 0.9,
            "the other provider-level entries must be kept"
        );
        assert!(matches!(m.client, ClientSpec::Builtin { sdk: Sdk::Glm }));
    }

    #[test]
    fn generic_dialect_on_a_named_sdk_is_a_config_error() {
        let mut p = provider(Sdk::DeepSeek);
        p.generic = Some(GenericOpenAiDialect::default());
        let mut w = Vec::new();
        assert!(
            resolve_model("p", &p, "m", &ModelSettings::default(), opts(1), &mut w).is_err(),
            "the declarative escape hatch belongs to openai_generic alone"
        );
    }

    #[test]
    fn missing_sdk_is_an_error() {
        let mut w = Vec::new();
        assert!(
            resolve_model(
                "p",
                &ProviderSettings::default(),
                "m",
                &ModelSettings::default(),
                opts(1),
                &mut w
            )
            .is_err()
        );
    }

    #[test]
    fn resolving_carries_candidate_handles_not_a_key() {
        let mut w = Vec::new();
        let m = resolve_model(
            "deepseek",
            &provider(Sdk::DeepSeek),
            "m",
            &ModelSettings::default(),
            opts(1),
            &mut w,
        )
        .unwrap();

        assert_eq!(
            m.credential_refs,
            [
                "env:ZLOGIC_DEEPSEEK_API_KEY",
                "env:DEEPSEEK_API_KEY",
                "keyring:deepseek"
            ]
        );
    }

    #[test]
    fn a_custom_provider_uses_the_same_convention() {
        let mut w = Vec::new();
        let mut p = provider(Sdk::OpenAiGeneric);
        p.base_url = Some("http://localhost:8000/v1".into());
        let m = resolve_model(
            "corp",
            &p,
            "internal",
            &ModelSettings {
                context_window: Some(32_000),
                ..Default::default()
            },
            opts(1),
            &mut w,
        )
        .unwrap();
        assert_eq!(
            m.credential_refs,
            [
                "env:ZLOGIC_CORP_API_KEY",
                "env:CORP_API_KEY",
                "keyring:corp"
            ]
        );
    }
}
