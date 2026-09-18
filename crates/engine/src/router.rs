//! ```text
//!         │
//! ```
//! ```text
//! ```

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use zlogic_config::{AppConfig, SESSION};
use zlogic_core::AuxModel;
use zlogic_credential::CredentialStore;
use zlogic_llm::LlmClient;
use zlogic_llm::factory::{BedrockAuth, from_resolved};
use zlogic_llm::transport::HttpTransport;
use zlogic_protocol::config::{ClientSpec, ResolvedModel, Sdk, Tier};
use zlogic_protocol::llm::{ThinkingIntent, ThinkingMode};
use zlogic_protocol::usage::Purpose;

use crate::{EngineError, Result};

#[derive(Clone)]
pub struct Routed {
    pub model: ResolvedModel,
    pub client: Arc<dyn LlmClient>,
    pub thinking: ThinkingIntent,
    pub via: String,
}

impl Routed {
    pub fn model_ref(&self) -> String {
        format!(
            "{}:{}",
            self.model.source.provider_id, self.model.source.model_id
        )
    }

    pub fn aux(&self) -> AuxModel {
        AuxModel {
            model: self.model.clone(),
            client: self.client.clone(),
        }
    }
}

pub struct ModelRouter {
    config: RwLock<Arc<AppConfig>>,
    transport: Arc<dyn HttpTransport>,
    keys: Arc<dyn CredentialStore>,
}

impl ModelRouter {
    pub fn new(
        config: Arc<AppConfig>,
        transport: Arc<dyn HttpTransport>,
        keys: Arc<dyn CredentialStore>,
    ) -> Self {
        Self {
            config: RwLock::new(config),
            transport,
            keys,
        }
    }

    pub fn config(&self) -> Arc<AppConfig> {
        self.config.read().expect("router config").clone()
    }

    pub fn replace_config(&self, config: Arc<AppConfig>) {
        *self.config.write().expect("router config") = config;
    }

    pub fn resolve(&self, role: &Purpose, session_model: Option<&str>) -> Result<Routed> {
        // One routing decision must see one revision even if Settings reloads concurrently.
        let config = self.config();
        let key = role.as_wire();
        let configured = config.llm_roles.get(&key);

        let mut chain: Vec<String> = match configured {
            Some(r) if !r.models.is_empty() => r.models.clone(),
            _ => builtin_chain(role).iter().map(|s| s.to_string()).collect(),
        };
        if !chain.iter().any(|c| c == SESSION) {
            chain.push(SESSION.to_string());
        }

        let thinking_on = match configured.and_then(|r| r.thinking) {
            Some(t) => t.is_on(),
            None => !thinking_off_by_default(role),
        };

        let mut tried: Vec<String> = Vec::new();
        for candidate in &chain {
            let refs: Vec<String> = match (candidate.as_str(), session_model) {
                (SESSION, Some(pinned)) => {
                    let mut refs = vec![pinned.to_string()];
                    refs.extend(self.expand(&config, SESSION, None));
                    refs
                }
                _ => self.expand(&config, candidate, session_model),
            };
            for model_ref in refs {
                tried.push(model_ref.clone());
                match self.build(&config, &model_ref, candidate, configured, thinking_on) {
                    Ok(routed) => return Ok(routed),
                    Err(e) => {
                        if candidate.as_str() == SESSION
                            && Some(model_ref.as_str()) == session_model
                        {
                            tracing::warn!(
                                target: "zlogic::engine",
                                role = %key,
                                pinned = %model_ref,
                                "session model unavailable ({e}), falling back to default model chain"
                            );
                        }
                        tracing::debug!(
                            target: "zlogic::engine",
                            role = %key, candidate = %model_ref,
                            "candidate unavailable, trying the next one: {e}"
                        );
                    }
                }
            }
        }

        Err(EngineError::NoModel { role: key, tried })
    }

    fn expand(
        &self,
        config: &AppConfig,
        candidate: &str,
        session_model: Option<&str>,
    ) -> Vec<String> {
        if candidate == SESSION {
            return match session_model {
                Some(m) => vec![m.to_string()],
                None => config
                    .default_model
                    .clone()
                    .into_iter()
                    .chain(config.model_refs())
                    .collect(),
            };
        }
        if let Some(tier) = Tier::parse(candidate) {
            return config.models_with_tier(tier);
        }
        vec![candidate.to_string()]
    }

    fn build(
        &self,
        config: &AppConfig,
        model_ref: &str,
        via: &str,
        role: Option<&zlogic_config::RoleSettings>,
        thinking_on: bool,
    ) -> Result<Routed> {
        let (mut model, _warnings) = config.resolve(model_ref)?;

        if !thinking_on {
            merge(
                &mut model.default_params,
                &config.no_think_params(model_ref),
            );
        }
        if let Some(r) = role {
            merge(&mut model.default_params, &r.params);
        }

        let client = self.client_for(&model, model_ref)?;

        Ok(Routed {
            model,
            client,
            thinking: ThinkingIntent {
                mode: if thinking_on {
                    ThinkingMode::Default
                } else {
                    ThinkingMode::Off
                },
                ..Default::default()
            },
            via: via.to_string(),
        })
    }

    fn client_for(&self, model: &ResolvedModel, model_ref: &str) -> Result<Arc<dyn LlmClient>> {
        build_client(model, model_ref, &*self.keys, self.transport.clone())
    }
}

pub(crate) fn build_client(
    model: &ResolvedModel,
    model_ref: &str,
    keys: &dyn CredentialStore,
    transport: Arc<dyn HttpTransport>,
) -> Result<Arc<dyn LlmClient>> {
    let api_key = model.credential_refs.iter().find_map(|r| keys.resolve(r));
    if api_key.is_none() {
        let credential_ref = model.credential_refs.join(" or ");
        tracing::error!(
            target: "zlogic::llm",
            model = model_ref,
            credential_ref = %credential_ref,
            "no credential resolution for model; refusing to send request without a key"
        );
        return Err(EngineError::NoCredential {
            model: model_ref.to_string(),
            credential_ref,
        });
    }

    let bedrock = if matches!(model.client, ClientSpec::Builtin { sdk: Sdk::Bedrock }) {
        let aws = keys.aws().ok_or_else(|| {
            let credential_ref = "AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY".into();
            tracing::error!(
                target: "zlogic::llm",
                model = model_ref,
                "no AWS credential resolution for Bedrock model; refusing to send request"
            );
            EngineError::NoCredential {
                model: model_ref.to_string(),
                credential_ref,
            }
        })?;
        Some(BedrockAuth {
            region: aws.region,
            credentials: zlogic_llm::bedrock::sigv4::Credentials {
                access_key_id: aws.access_key_id,
                secret_access_key: aws.secret_access_key,
                session_token: aws.session_token,
            },
        })
    } else {
        None
    };

    from_resolved(model, api_key, bedrock, transport).map_err(|e| EngineError::Invalid(e.0))
}

fn builtin_chain(role: &Purpose) -> &'static [&'static str] {
    match role {
        Purpose::Main | Purpose::Agent(_) => &[SESSION],
        Purpose::Title | Purpose::Approval | Purpose::Utility => &[SESSION, "light"],
        Purpose::Compaction | Purpose::ApprovalDeep => &["main", SESSION],
    }
}

fn thinking_off_by_default(role: &Purpose) -> bool {
    match role {
        Purpose::Main | Purpose::Agent(_) => false,
        Purpose::Title
        | Purpose::Compaction
        | Purpose::Approval
        | Purpose::ApprovalDeep
        | Purpose::Utility => true,
    }
}

fn merge(base: &mut BTreeMap<String, Value>, over: &BTreeMap<String, Value>) {
    for (k, v) in over {
        base.insert(k.clone(), v.clone());
    }
}
