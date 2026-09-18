use std::sync::Arc;

use zlogic_protocol::config::{ClientSpec, ConfigError, ResolvedModel, Sdk};

use crate::anthropic::AnthropicClient;
use crate::bedrock::{BedrockClient, sigv4::Credentials};
use crate::chat::ChatClient;
use crate::chat::generic::GenericOpenAi;
use crate::chat::vendors::{
    DashScope, DeepSeek, Fireworks, Glm, OpenRouter, QwenLocal, VanillaOpenAi,
};
use crate::gemini::GeminiClient;
use crate::responses::ResponsesClient;
use crate::retry::RetryingClient;
use crate::transport::HttpTransport;
use crate::{Endpoint, LlmClient};

#[derive(Debug, Clone)]
pub struct BedrockAuth {
    pub region: String,
    pub credentials: Credentials,
}

pub struct ClientConfig {
    pub endpoint: Endpoint,
    pub max_output_tokens: Option<u64>,
    pub bedrock: Option<BedrockAuth>,
}

impl ClientConfig {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            max_output_tokens: None,
            bedrock: None,
        }
    }
}

pub fn default_base_url(sdk: Sdk) -> &'static str {
    match sdk {
        Sdk::OpenAiChat | Sdk::OpenAiResponses => "https://api.openai.com",
        Sdk::DeepSeek => "https://api.deepseek.com",
        Sdk::Glm => "https://open.bigmodel.cn/api/paas/v4",
        Sdk::DashScope => "https://dashscope.aliyuncs.com/compatible-mode/v1",
        Sdk::OpenRouter => "https://openrouter.ai/api/v1",
        Sdk::Fireworks => "https://api.fireworks.ai/inference/v1",
        Sdk::Anthropic => "https://api.anthropic.com",
        Sdk::Gemini => "https://generativelanguage.googleapis.com",
        Sdk::Bedrock => "https://bedrock-runtime.us-east-1.amazonaws.com",
        Sdk::OpenAiGeneric | Sdk::QwenLocal => "",
    }
}

pub fn create_client(
    spec: &ClientSpec,
    cfg: ClientConfig,
    transport: Arc<dyn HttpTransport>,
) -> Result<Arc<dyn LlmClient>, ConfigError> {
    let ClientConfig {
        endpoint,
        max_output_tokens,
        bedrock,
    } = cfg;

    let sdk = match spec {
        ClientSpec::OpenAiGeneric(dialect) => {
            let client = Arc::new(ChatClient::new(
                GenericOpenAi::new((**dialect).clone()),
                endpoint,
                transport,
            ));
            return Ok(detail(RetryingClient::new(client)));
        }
        ClientSpec::Builtin { sdk } => *sdk,
    };

    let client: Arc<dyn LlmClient> = match sdk {
        Sdk::OpenAiChat => Arc::new(ChatClient::new(VanillaOpenAi, endpoint, transport)),
        Sdk::DeepSeek => Arc::new(ChatClient::new(DeepSeek, endpoint, transport)),
        Sdk::Glm => Arc::new(ChatClient::new(Glm, endpoint, transport)),
        Sdk::DashScope => Arc::new(ChatClient::new(DashScope, endpoint, transport)),
        Sdk::QwenLocal => Arc::new(ChatClient::new(QwenLocal, endpoint, transport)),
        Sdk::OpenRouter => Arc::new(ChatClient::new(OpenRouter, endpoint, transport)),
        Sdk::Fireworks => Arc::new(ChatClient::new(Fireworks, endpoint, transport)),

        Sdk::OpenAiResponses => {
            Arc::new(ResponsesClient::new(endpoint, transport, max_output_tokens))
        }
        Sdk::Anthropic => Arc::new(AnthropicClient::new(endpoint, transport, max_output_tokens)),
        Sdk::Gemini => Arc::new(GeminiClient::new(endpoint, transport, max_output_tokens)),

        Sdk::Bedrock => {
            let auth = bedrock.ok_or_else(|| {
                ConfigError(
                    "sdk 'bedrock' requires a region and AWS credentials; it does not accept a Bearer token"
                        .into(),
                )
            })?;
            Arc::new(BedrockClient::new(
                endpoint,
                transport,
                auth.region,
                auth.credentials,
                max_output_tokens,
            ))
        }

        Sdk::OpenAiGeneric => unreachable!("handled above"),
    };

    Ok(detail(RetryingClient::new(client)))
}

fn detail(client: RetryingClient) -> Arc<dyn LlmClient> {
    Arc::new(crate::detail::DetailLoggingClient::new(Arc::new(client)))
}

pub fn from_resolved(
    model: &ResolvedModel,
    api_key: Option<String>,
    bedrock: Option<BedrockAuth>,
    transport: Arc<dyn HttpTransport>,
) -> Result<Arc<dyn LlmClient>, ConfigError> {
    let sdk = match &model.client {
        ClientSpec::Builtin { sdk } => *sdk,
        ClientSpec::OpenAiGeneric(_) => Sdk::OpenAiGeneric,
    };
    let base_url = model
        .base_url
        .clone()
        .unwrap_or_else(|| default_base_url(sdk).to_string());
    if base_url.is_empty() {
        return Err(ConfigError(format!(
            "sdk: {sdk:?} has no default endpoint; configure base_url"
        )));
    }

    let mut endpoint = Endpoint::new(base_url)
        .with_key(api_key)
        .with_capabilities(model.capabilities.clone());
    endpoint.network = model.network.clone();
    let w = &model.wiring;
    if let Some(h) = &w.auth_header {
        endpoint.auth = crate::AuthStyle::Header(h.clone());
    }
    endpoint.query = w
        .query
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    endpoint
        .extra_headers
        .extend(w.headers.iter().map(|(k, v)| (k.clone(), v.clone())));

    create_client(
        &model.client,
        ClientConfig {
            endpoint,
            max_output_tokens: model.max_output_tokens,
            bedrock,
        },
        transport,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::ReplayTransport;
    use zlogic_protocol::config::{GenericOpenAiDialect, ModelCapabilities, Pricing};
    use zlogic_protocol::message::Source;

    fn transport() -> Arc<dyn HttpTransport> {
        Arc::new(ReplayTransport::whole(""))
    }

    fn resolved(sdk: Sdk, base_url: Option<&str>) -> ResolvedModel {
        ResolvedModel {
            source: Source::new("p", "m"),
            wire_model: "m".into(),
            display_name: "m".into(),
            client: ClientSpec::Builtin { sdk },
            base_url: base_url.map(str::to_string),
            wiring: Default::default(),
            network: Default::default(),
            credential_refs: Vec::new(),
            context_window: 128_000,
            max_output_tokens: Some(4096),
            compaction_threshold: None,
            capabilities: ModelCapabilities::default(),
            pricing: None::<Pricing>,
            default_params: Default::default(),
            config_revision: 1,
        }
    }

    #[test]
    fn every_sdk_resolves_to_a_client() {
        for sdk in [
            Sdk::OpenAiChat,
            Sdk::OpenAiResponses,
            Sdk::DeepSeek,
            Sdk::Glm,
            Sdk::DashScope,
            Sdk::OpenRouter,
            Sdk::Fireworks,
            Sdk::Anthropic,
            Sdk::Gemini,
        ] {
            let m = resolved(sdk, Some("https://x.test"));
            assert!(
                from_resolved(&m, Some("k".into()), None, transport()).is_ok(),
                "{sdk:?} has no matching client"
            );
        }
    }

    #[test]
    fn self_hosted_sdks_require_an_explicit_base_url() {
        for sdk in [Sdk::OpenAiGeneric, Sdk::QwenLocal] {
            let m = resolved(sdk, None);
            assert!(
                from_resolved(&m, None, None, transport()).is_err(),
                "{sdk:?} has no default endpoint: this should error rather than build an empty URL"
            );
        }
    }

    #[test]
    fn bedrock_without_credentials_is_a_config_error_not_a_401() {
        let m = resolved(Sdk::Bedrock, Some("https://bedrock.test"));
        let Err(err) = from_resolved(&m, Some("bearer-token".into()), None, transport()) else {
            panic!(
                "bedrock does not take a Bearer token: this should be a config error at construction"
            );
        };
        assert!(err.0.contains("bedrock"));
    }

    #[test]
    fn bedrock_with_credentials_builds() {
        let m = resolved(Sdk::Bedrock, Some("https://bedrock.test"));
        let auth = BedrockAuth {
            region: "us-east-1".into(),
            credentials: Credentials {
                access_key_id: "AKID".into(),
                secret_access_key: "SECRET".into(),
                session_token: None,
            },
        };
        assert!(from_resolved(&m, None, Some(auth), transport()).is_ok());
    }

    #[test]
    fn generic_spec_carries_its_dialect_through() {
        let mut m = resolved(Sdk::OpenAiGeneric, Some("https://gw.corp.test/v1"));
        m.client = ClientSpec::OpenAiGeneric(Box::new(GenericOpenAiDialect {
            reasoning_carrier: Some("reasoning_content".into()),
            ..Default::default()
        }));
        assert!(from_resolved(&m, None, None, transport()).is_ok());
    }
}
