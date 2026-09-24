//! The token custodian an outbound request asks for.
//!
//! Callers never hold a stale token: they ask [`Subscription`] for one, and it refreshes — once,
//! even under concurrent requests — when the stored token is about to die.

use std::sync::Arc;

use async_trait::async_trait;
use zlogic_credential::CredentialStore;
use zlogic_llm::{Token, TokenProvider};
use zlogic_protocol::llm::{LlmError, LlmErrorKind, RequestMeta};

use crate::oauth::Codex;
use crate::{Error, Tokens, store};

pub struct Subscription {
    provider_id: String,
    codex: Arc<Codex>,
    store: Arc<dyn CredentialStore>,
    cached: tokio::sync::Mutex<Option<Tokens>>,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("provider_id", &self.provider_id)
            .finish_non_exhaustive()
    }
}

impl Subscription {
    pub fn new(
        provider_id: impl Into<String>,
        codex: Arc<Codex>,
        store: Arc<dyn CredentialStore>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            codex,
            store,
            cached: tokio::sync::Mutex::new(None),
        }
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// The stored credential as it is, without touching the network.
    pub fn stored(&self) -> Option<Tokens> {
        store::load(&*self.store, &self.provider_id)
    }

    /// A usable credential: the cached one, the stored one, or a refresh of it.
    pub async fn access(&self) -> Result<Tokens, Error> {
        let mut cached = self.cached.lock().await;
        if let Some(tokens) = cached.as_ref().filter(|t| !t.needs_refresh()) {
            return Ok(tokens.clone());
        }

        let stored = store::read(&*self.store, &self.provider_id)?;
        if !stored.needs_refresh() {
            *cached = Some(stored.clone());
            return Ok(stored);
        }
        if stored.refresh_token.is_empty() {
            return Err(Error::SessionExpired);
        }

        tracing::debug!(
            target: "zlogic::codex",
            provider = %self.provider_id,
            "the ChatGPT subscription access token expired; refreshing it"
        );
        let refreshed = self
            .codex
            .refresh(&stored.refresh_token, Some(&stored))
            .await?;
        store::save(&*self.store, &self.provider_id, &refreshed)?;
        *cached = Some(refreshed.clone());
        Ok(refreshed)
    }
}

#[async_trait]
impl TokenProvider for Subscription {
    async fn token(&self, meta: &RequestMeta) -> Result<Token, LlmError> {
        let tokens = self.access().await.map_err(|error| LlmError {
            kind: LlmErrorKind::Auth,
            retryable: false,
            message: format!(
                "the {} model is served by the ChatGPT subscription in provider {}: {error}",
                meta.purpose.as_wire(),
                self.provider_id
            ),
            status: None,
            request_id: None,
        })?;

        let headers = self
            .codex
            .auth_headers(&tokens, Some(&meta.session_id))
            .into_iter()
            .filter_map(|(name, value)| {
                let name = name?;
                let name = name.as_str();
                // The bearer token travels in `Token::access`; sending it twice is a header
                // duplicate, not redundancy.
                if name.eq_ignore_ascii_case("authorization") {
                    return None;
                }
                Some((name.to_string(), value.to_str().ok()?.to_string()))
            })
            .collect();

        Ok(Token {
            access: tokens.access_token.clone(),
            headers,
        })
    }
}
