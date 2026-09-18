use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::llm::Effort;
use crate::message::Source;
use crate::usage::QuotaConfig;

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sdk {
    #[serde(rename = "openai_generic")]
    OpenAiGeneric,
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "deepseek")]
    DeepSeek,
    Glm,
    #[serde(rename = "dashscope")]
    DashScope,
    QwenLocal,
    #[serde(rename = "openrouter")]
    OpenRouter,
    Fireworks,
    Anthropic,
    Gemini,
    Bedrock,
}

impl Sdk {
    pub fn is_openai_like(self) -> bool {
        matches!(
            self,
            Sdk::OpenAiGeneric
                | Sdk::OpenAiChat
                | Sdk::DeepSeek
                | Sdk::Glm
                | Sdk::DashScope
                | Sdk::QwenLocal
                | Sdk::OpenRouter
                | Sdk::Fireworks
        )
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub provider_id: String,
    pub sdk: Sdk,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generic: Option<GenericOpenAiDialect>,
    #[serde(default, skip_serializing_if = "Wiring::is_empty")]
    pub wiring: Wiring,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub default_params: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quotas: Vec<QuotaConfig>,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    #[serde(default)]
    pub origin: ProviderOrigin,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderOrigin {
    #[default]
    Builtin,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Tier {
    Light,
    Main,
    Thinking,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Light, Tier::Main, Tier::Thinking];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Light => "light",
            Tier::Main => "main",
            Tier::Thinking => "thinking",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ModelConfig {
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub context_window: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_threshold: Option<u64>,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<Pricing>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub default_params: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub no_think_params: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk: Option<Sdk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quotas: Vec<QuotaConfig>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    #[serde(default)]
    pub thinking: ThinkingCapability,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThinkingCapability {
    pub supported: bool,
    #[serde(default)]
    pub can_disable: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<Effort>,
    #[serde(default)]
    pub budget: bool,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    pub input_per_m: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_m: Option<f64>,
    pub output_per_m: f64,
    #[serde(default = "usd")]
    pub currency: String,
}

fn usd() -> String {
    "USD".into()
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Wiring {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub query: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

impl Wiring {
    pub fn is_empty(&self) -> bool {
        self.auth_header.is_none() && self.query.is_empty() && self.headers.is_empty()
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_timeout_ms: Option<u64>,
}

impl NetworkConfig {
    pub fn is_empty(&self) -> bool {
        self.connect_timeout_ms.is_none() && self.read_timeout_ms.is_none()
    }

    pub fn overlay(&self, other: &NetworkConfig) -> NetworkConfig {
        NetworkConfig {
            connect_timeout_ms: other.connect_timeout_ms.or(self.connect_timeout_ms),
            read_timeout_ms: other.read_timeout_ms.or(self.read_timeout_ms),
        }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GenericOpenAiDialect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_carrier: Option<String>,
    #[serde(default)]
    pub think_tags: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_field: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub effort_map: BTreeMap<Effort, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_on: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_off: Option<Value>,
    #[serde(default)]
    pub usage_fields: UsageFieldMap,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_body: BTreeMap<String, Value>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageFieldMap {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_includes_cache: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_includes_reasoning: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "client", rename_all = "snake_case")]
pub enum ClientSpec {
    #[serde(rename = "openai_generic")]
    OpenAiGeneric(Box<GenericOpenAiDialect>),
    Builtin {
        sdk: Sdk,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

pub fn resolve_client(
    sdk: Sdk,
    generic: Option<&GenericOpenAiDialect>,
) -> Result<ClientSpec, ConfigError> {
    match (sdk, generic) {
        (Sdk::OpenAiGeneric, Some(d)) => Ok(ClientSpec::OpenAiGeneric(Box::new(d.clone()))),
        (Sdk::OpenAiGeneric, None) => Ok(ClientSpec::OpenAiGeneric(Box::default())),
        (other, None) => Ok(ClientSpec::Builtin { sdk: other }),
        (other, Some(_)) => Err(ConfigError(format!(
            "`generic` can only be set on sdk: openai_generic, current sdk is {other:?}; \
             named vendor differences belong in the client code, not in declarative overrides"
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedModel {
    pub source: Source,
    pub wire_model: String,
    pub display_name: String,
    pub client: ClientSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Wiring::is_empty")]
    pub wiring: Wiring,
    #[serde(default, skip_serializing_if = "NetworkConfig::is_empty")]
    pub network: NetworkConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_refs: Vec<String>,
    pub context_window: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_threshold: Option<u64>,
    pub capabilities: ModelCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<Pricing>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub default_params: BTreeMap<String, Value>,
    pub config_revision: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_dialect_only_on_generic_sdk() {
        let d = GenericOpenAiDialect {
            reasoning_carrier: Some("reasoning_content".into()),
            ..Default::default()
        };
        assert!(matches!(
            resolve_client(Sdk::OpenAiGeneric, Some(&d)),
            Ok(ClientSpec::OpenAiGeneric(_))
        ));
        assert!(resolve_client(Sdk::DeepSeek, Some(&d)).is_err());
        assert!(matches!(
            resolve_client(Sdk::DeepSeek, None),
            Ok(ClientSpec::Builtin { sdk: Sdk::DeepSeek })
        ));
    }

    #[test]
    fn openai_like_family() {
        assert!(Sdk::DeepSeek.is_openai_like());
        assert!(Sdk::OpenAiGeneric.is_openai_like());
        assert!(!Sdk::Anthropic.is_openai_like());
        assert!(!Sdk::Gemini.is_openai_like());
        assert!(!Sdk::OpenAiResponses.is_openai_like());
    }
}
