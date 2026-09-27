//! Signing a provider in to a subscription, and asking the backend what it may call.
//!
//! The protocol work lives in `zlogic-codex`; this is the engine's half: which providers may sign
//! in at all, what a flow is allowed to do to the configuration, and where the fetched model list
//! is kept. A host only ever sees a URL (or a code) and a state to poll.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use zlogic_codex::flow::{FlowOutcome, SignInMethod};
use zlogic_codex::{Codex, Flows, ModelInfo, Subscription, Tokens};
use zlogic_credential::{CredentialStore, provider_oauth_entry};
use zlogic_protocol::config::{ProviderAuth, ThinkingCapability};
use zlogic_protocol::llm::Effort;
use zlogic_protocol::query::{
    ApiError, ApiResult, ProviderModels, ProviderModelsReq, ProviderSignInBegin,
    ProviderSignInBeginReq, ProviderSignInCancelReq, ProviderSignInMethod, ProviderSignInState,
    ProviderSignInStatus, ProviderSignInStatusReq,
};

use crate::EngineError;
use crate::config::Config;
use crate::service::ConfigService;

const FALLBACK_CONTEXT_WINDOW: u64 = 272_000;

pub struct ProviderSignIn {
    config: Arc<Config>,
    store: Arc<dyn CredentialStore>,
    codex: Arc<Codex>,
    flows: Flows,
}

impl ProviderSignIn {
    pub fn new(config: Arc<Config>, store: Arc<dyn CredentialStore>) -> Self {
        Self::with_codex(config, store, zlogic_codex::shared())
    }

    pub fn with_codex(
        config: Arc<Config>,
        store: Arc<dyn CredentialStore>,
        codex: Arc<Codex>,
    ) -> Self {
        Self {
            config,
            store: Arc::clone(&store),
            flows: Flows::new(Arc::clone(&codex), store),
            codex,
        }
    }

    /// A request-time credential holder for one provider, when it signs in.
    pub fn subscription(
        &self,
        provider_id: &str,
        store: Arc<dyn CredentialStore>,
    ) -> Arc<Subscription> {
        Arc::new(Subscription::new(
            provider_id,
            Arc::clone(&self.codex),
            store,
        ))
    }

    pub async fn tokens(&self, provider_id: &str) -> Result<Tokens, ApiError> {
        self.require_subscription(provider_id).await?;
        zlogic_codex::store::read(&*self.store, provider_id).map_err(|e| {
            ApiError::invalid_code("provider_not_signed_in", e.to_string())
                .with_detail("provider_id", provider_id.to_string())
        })
    }

    async fn require_subscription(&self, provider_id: &str) -> Result<(), ApiError> {
        let provider_id = provider_id.trim();
        if provider_id.is_empty() {
            return Err(ApiError::invalid_code(
                "engine_invalid_argument",
                "provider id must not be empty",
            ));
        }
        let cfg = self.config.snapshot().await;
        if auth_of(&cfg, provider_id) == Some(ProviderAuth::Chatgpt) {
            return Ok(());
        }
        Err(ApiError::invalid_code(
            "provider_not_subscription",
            format!("provider {provider_id} does not sign in with a ChatGPT subscription"),
        ))
    }

    pub async fn begin(&self, req: ProviderSignInBeginReq) -> ApiResult<ProviderSignInBegin> {
        let provider_id = req.provider_id.trim().to_string();
        self.require_subscription(&provider_id).await?;
        let method = match req.method {
            ProviderSignInMethod::Browser => SignInMethod::Browser,
            ProviderSignInMethod::Device => SignInMethod::Device,
        };
        let begin = self
            .flows
            .begin(&provider_id, method)
            .await
            .map_err(sign_in_error)?;

        Ok(ProviderSignInBegin {
            flow_id: begin.flow_id,
            provider_id,
            method: req.method,
            authorization_url: begin.authorization_url,
            user_code: begin.user_code,
            instructions: begin.instructions,
            expires_at: begin.expires_at,
        })
    }

    pub async fn status(&self, req: ProviderSignInStatusReq) -> ApiResult<ProviderSignInStatus> {
        let flow_id = req.flow_id.trim().to_string();
        let snapshot = self
            .flows
            .view(&flow_id)
            .ok_or_else(|| ApiError::not_found("sign_in_unknown", "That sign-in is not running"))?;
        let state = state_of(snapshot.outcome);
        if matches!(state, ProviderSignInState::Succeeded { .. }) {
            self.adopt(&snapshot.provider_id).await;
        }
        Ok(ProviderSignInStatus {
            flow_id,
            provider_id: snapshot.provider_id,
            state,
            expires_at: snapshot.expires_at,
        })
    }

    /// Make a provider that has just signed in visible to routing and the pickers.
    ///
    /// Whether a subscription provider is part of the configuration at all is decided by whether
    /// its credential is there, and the running snapshot was loaded before the sign-in wrote one.
    /// Without this reload the account would be signed in and still invisible until a restart.
    async fn adopt(&self, provider_id: &str) {
        if self
            .config
            .snapshot()
            .await
            .providers
            .contains_key(provider_id)
        {
            return;
        }
        if let Err(error) = self.config.reload().await {
            tracing::warn!(
                target: "zlogic::engine",
                provider = provider_id,
                "signed in, but the configuration could not be reloaded: {error}"
            );
        }
    }

    pub fn cancel(&self, req: ProviderSignInCancelReq) -> ApiResult<()> {
        self.flows.cancel(req.flow_id.trim()).map_err(sign_in_error)
    }

    /// Forget the stored subscription. The account itself is not touched: signing out of a client
    /// is not revoking a plan.
    pub async fn logout(&self, provider_id: &str) -> Result<(), EngineError> {
        zlogic_codex::store::clear(&*self.store, provider_id)
            .map_err(|e| EngineError::Invalid(e.to_string()))
    }

    /// Ask the backend which models this account may call, and keep the answer.
    pub async fn models(&self, req: ProviderModelsReq) -> ApiResult<ProviderModels> {
        let provider_id = req.provider_id.trim().to_string();
        let tokens = self.tokens(&provider_id).await?;
        let info = self.codex.models(&tokens).await.map_err(sign_in_error)?;

        let cfg = self.config.snapshot().await;
        let floor = cfg
            .providers
            .get(&provider_id)
            .map(|p| p.models.clone())
            .unwrap_or_default();
        let settings: BTreeMap<String, zlogic_config::ModelSettings> = info
            .iter()
            .map(|model| {
                (
                    model.slug.clone(),
                    settings_of(model, floor.get(&model.slug)),
                )
            })
            .collect();
        if settings.is_empty() {
            return Err(ApiError::unavailable(
                "provider_models_empty",
                "The subscription backend listed no usable models",
            ));
        }

        self.config
            .write_provider_models(&provider_id, settings)
            .await
            .map_err(ApiError::from)?;

        let cfg = self.config.snapshot().await;
        let provider = crate::config::providers_of(&cfg, &BTreeSet::new())
            .into_iter()
            .find(|p| p.provider_id == provider_id)
            .ok_or_else(|| {
                ApiError::not_found(
                    "provider_unknown",
                    format!("provider {provider_id} is not in the configuration"),
                )
            })?;

        Ok(ProviderModels {
            provider_id,
            fetched_at: Some(chrono::Utc::now()),
            models: provider.models,
        })
    }

    /// Forget a fetched list, so the built-in floor applies again.
    pub async fn forget_models(&self, provider_id: &str) -> Result<(), EngineError> {
        self.config
            .remove_provider_models(provider_id)
            .await
            .map_err(|e| EngineError::Invalid(e.to_string()))
    }
}

/// The auth a provider is configured with, if it is configured at all.
pub fn auth_of(cfg: &zlogic_config::AppConfig, provider_id: &str) -> Option<ProviderAuth> {
    cfg.providers.get(provider_id).map(|p| p.auth).or_else(|| {
        zlogic_config::builtin_catalog_with_local()
            .ok()?
            .providers
            .get(provider_id)
            .map(|p| p.auth)
    })
}

fn state_of(outcome: FlowOutcome) -> ProviderSignInState {
    match outcome {
        FlowOutcome::Pending => ProviderSignInState::Pending,
        FlowOutcome::Succeeded {
            account,
            label,
            plan,
        } => ProviderSignInState::Succeeded {
            account,
            label,
            plan,
        },
        FlowOutcome::Failed { message } => ProviderSignInState::Failed { message },
        FlowOutcome::Expired => ProviderSignInState::Expired,
        FlowOutcome::Cancelled => ProviderSignInState::Cancelled,
    }
}

fn sign_in_error(error: zlogic_codex::Error) -> ApiError {
    match error {
        zlogic_codex::Error::NotSignedIn(_) => {
            ApiError::invalid_code("provider_not_signed_in", error.to_string())
        }
        zlogic_codex::Error::SessionExpired => {
            ApiError::invalid_code("provider_session_expired", error.to_string())
        }
        zlogic_codex::Error::Network { .. } | zlogic_codex::Error::Status { .. } => {
            ApiError::unavailable("provider_sign_in_failed", error.to_string())
        }
        other => ApiError::invalid_code("provider_sign_in_failed", other.to_string()),
    }
}

fn settings_of(
    model: &ModelInfo,
    floor: Option<&zlogic_config::ModelSettings>,
) -> zlogic_config::ModelSettings {
    let efforts: Vec<Effort> = model
        .efforts()
        .iter()
        .filter_map(|e| Effort::from_wire(e))
        .collect();
    zlogic_config::ModelSettings {
        display_name: Some(if model.display_name.trim().is_empty() {
            model.slug.clone()
        } else {
            model.display_name.clone()
        }),
        context_window: Some(
            model
                .context_window
                .or_else(|| floor.and_then(|f| f.context_window))
                .unwrap_or(FALLBACK_CONTEXT_WINDOW),
        ),
        tier: floor.and_then(|f| f.tier),
        vision: model
            .supports_image_detail_original
            .or_else(|| floor.and_then(|f| f.vision)),
        thinking: Some(ThinkingCapability {
            supported: !efforts.is_empty(),
            can_disable: false,
            efforts,
            budget: false,
        }),
        no_think_params: floor.map(|f| f.no_think_params.clone()).unwrap_or_default(),
        ..Default::default()
    }
}

/// The keychain entry a provider's subscription lives in.
pub fn oauth_entry(provider_id: &str) -> String {
    provider_oauth_entry(provider_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ModelInfo {
        ModelInfo {
            slug: "gpt-5.5".into(),
            display_name: "GPT-5.5".into(),
            context_window: Some(400_000),
            supports_image_detail_original: Some(true),
            supported_reasoning_levels: vec![
                zlogic_codex::models::ReasoningLevel {
                    effort: "high".into(),
                    description: None,
                },
                zlogic_codex::models::ReasoningLevel {
                    effort: "low".into(),
                    description: None,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn a_fetched_model_becomes_config_with_its_efforts() {
        let settings = settings_of(&info(), None);
        assert_eq!(settings.context_window, Some(400_000));
        assert_eq!(settings.vision, Some(true));
        let thinking = settings.thinking.unwrap();
        assert!(thinking.supported);
        assert_eq!(thinking.efforts, vec![Effort::High, Effort::Low]);
    }

    #[test]
    fn a_fetched_model_keeps_the_floor_where_the_backend_is_silent() {
        let floor = zlogic_config::ModelSettings {
            context_window: Some(123_456),
            tier: Some(zlogic_protocol::config::Tier::Thinking),
            ..Default::default()
        };
        let mut model = info();
        model.context_window = None;
        model.supported_reasoning_levels.clear();
        let settings = settings_of(&model, Some(&floor));
        assert_eq!(settings.context_window, Some(123_456));
        assert_eq!(
            settings.tier,
            Some(zlogic_protocol::config::Tier::Thinking),
            "tiers are ours, not the backend's"
        );
        assert!(!settings.thinking.unwrap().supported);
    }

    #[test]
    fn an_unknown_effort_is_dropped_rather_than_guessed() {
        let mut model = info();
        model.supported_reasoning_levels = vec![zlogic_codex::models::ReasoningLevel {
            effort: "ludicrous".into(),
            description: None,
        }];
        assert!(!settings_of(&model, None).thinking.unwrap().supported);
    }
}
