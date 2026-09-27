use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use zlogic_credential::{CredentialRef, CredentialStore, credential_candidates, keyring_entry};
use zlogic_protocol::config::ProviderAuth;
use zlogic_protocol::error::ErrorCategory;
use zlogic_protocol::llm::{
    CacheSpec, LlmError, LlmEvent, LlmRequest, RequestMeta, ThinkingIntent, ThinkingMode,
};
use zlogic_protocol::message::{ContentPart, Message, TextPart};
use zlogic_protocol::query::{
    ApiError, ApiResult, CredentialDeleteReq, CredentialSetReq, CredentialSource, CredentialState,
    CredentialVerifyReq, CredentialVerifyResult, ProviderModels, ProviderModelsReq,
    ProviderSignInBegin, ProviderSignInBeginReq, ProviderSignInCancelReq, ProviderSignInStatus,
    ProviderSignInStatusReq,
};
use zlogic_protocol::usage::Purpose;

use crate::config::Config;
use crate::provider_auth::{ProviderSignIn, auth_of};
use crate::service::{ConfigService, CredentialService};
use crate::{EngineError, Result};

pub struct Credentials {
    config: Arc<Config>,
    store: Arc<dyn CredentialStore>,
    transport: Arc<dyn zlogic_llm::transport::HttpTransport>,
    sign_in: Arc<ProviderSignIn>,
}

impl Credentials {
    pub fn new(
        config: Arc<Config>,
        store: Arc<dyn CredentialStore>,
        transport: Arc<dyn zlogic_llm::transport::HttpTransport>,
    ) -> Self {
        Self::with_codex(config, store, transport, zlogic_codex::shared())
    }

    /// With a client the caller owns, so a sign-in can be pointed somewhere other than the real
    /// issuer.
    pub fn with_codex(
        config: Arc<Config>,
        store: Arc<dyn CredentialStore>,
        transport: Arc<dyn zlogic_llm::transport::HttpTransport>,
        codex: Arc<zlogic_codex::Codex>,
    ) -> Self {
        let sign_in = Arc::new(ProviderSignIn::with_codex(
            Arc::clone(&config),
            Arc::clone(&store),
            codex,
        ));
        Self {
            config,
            store,
            transport,
            sign_in,
        }
    }

    const SEARCH_BACKENDS: [&'static str; 2] = ["exa", "parallel"];

    async fn provider_ids(&self) -> Result<(Vec<String>, bool)> {
        let config = self.config.snapshot().await;
        let mut ids: BTreeSet<String> = config.providers.keys().cloned().collect();
        let catalog = zlogic_config::builtin_catalog_with_local().map_err(EngineError::from)?;
        ids.extend(catalog.providers.keys().cloned());
        ids.extend(Self::SEARCH_BACKENDS.map(str::to_string));
        Ok((ids.into_iter().collect(), config.auto_detect_env))
    }

    async fn state_for(&self, provider_id: &str, auto_detect_env: bool) -> CredentialState {
        let auth = self.auth_of(provider_id).await;
        let keyring_source = if self.store.keyring_is_local_file() {
            CredentialSource::File
        } else {
            CredentialSource::Keyring
        };
        if auth == ProviderAuth::Chatgpt {
            let tokens = zlogic_codex::store::load(&*self.store, provider_id);
            return CredentialState {
                provider_id: provider_id.to_string(),
                present: tokens.is_some(),
                source: if tokens.is_some() {
                    keyring_source
                } else {
                    CredentialSource::Missing
                },
                hint: tokens.as_ref().and_then(|t| t.label()),
                candidates: vec![
                    CredentialRef::keyring(crate::provider_auth::oauth_entry(provider_id))
                        .to_string(),
                ],
            };
        }

        let candidates = credential_candidates(provider_id, auto_detect_env);
        let found = candidates.iter().find_map(|reference| {
            self.store
                .resolve(&reference.to_string())
                .map(|value| (reference, value))
        });
        CredentialState {
            provider_id: provider_id.to_string(),
            present: found.is_some(),
            source: match found.as_ref().map(|(reference, _)| *reference) {
                Some(CredentialRef::Env(_)) => CredentialSource::Env,
                Some(CredentialRef::Keyring(_)) => keyring_source,
                None => CredentialSource::Missing,
            },
            hint: found.map(|(_, value)| mask(&value)),
            candidates: candidates
                .into_iter()
                .map(|item| item.to_string())
                .collect(),
        }
    }

    async fn auth_of(&self, provider_id: &str) -> ProviderAuth {
        let cfg = self.config.snapshot().await;
        auth_of(&cfg, provider_id).unwrap_or_default()
    }

    async fn list_inner(&self) -> Result<Vec<CredentialState>> {
        let (ids, auto_detect_env) = self.provider_ids().await?;
        let mut states = Vec::with_capacity(ids.len());
        for provider_id in ids {
            states.push(self.state_for(&provider_id, auto_detect_env).await);
        }
        Ok(states)
    }

    async fn set_inner(&self, req: CredentialSetReq) -> Result<CredentialState> {
        let provider_id = normalized_provider_id(&req.provider_id)?;
        if self.auth_of(&provider_id).await == ProviderAuth::Chatgpt {
            return Err(EngineError::Invalid(format!(
                "provider {provider_id} signs in with a ChatGPT subscription rather than an API key; \
                 run `zlogic auth login {provider_id}`"
            )));
        }
        let secret = req.value.trim();
        if secret.is_empty() {
            return Err(EngineError::Invalid(
                "credential_set does not accept an empty key; use credential_delete to remove one"
                    .into(),
            ));
        }
        self.store
            .set_keyring(&keyring_entry(&provider_id), secret)
            .map_err(|error| EngineError::Invalid(error.to_string()))?;
        self.config.reload().await.map_err(api_to_engine)?;
        let auto_detect_env = self.config.snapshot().await.auto_detect_env;
        Ok(self.state_for(&provider_id, auto_detect_env).await)
    }

    async fn delete_inner(&self, req: CredentialDeleteReq) -> Result<CredentialState> {
        let provider_id = normalized_provider_id(&req.provider_id)?;
        if self.auth_of(&provider_id).await == ProviderAuth::Chatgpt {
            // Signing out of a client is not revoking the plan, so only the token is dropped.
            self.sign_in.logout(&provider_id).await?;
            self.sign_in.forget_models(&provider_id).await?;
        } else {
            self.store
                .delete_keyring(&keyring_entry(&provider_id))
                .map_err(|error| EngineError::Invalid(error.to_string()))?;
        }
        self.config.reload().await.map_err(api_to_engine)?;
        let auto_detect_env = self.config.snapshot().await.auto_detect_env;
        Ok(self.state_for(&provider_id, auto_detect_env).await)
    }

    async fn verify_inner(&self, req: CredentialVerifyReq) -> ApiResult<CredentialVerifyResult> {
        let provider_id = normalized_provider_id(&req.provider_id)?;
        let model_id = req.model_id.trim();
        if model_id.is_empty() {
            return Err(ApiError::invalid_code(
                "engine_invalid_argument",
                "model id must not be empty",
            ));
        }
        let model_ref = format!("{provider_id}:{model_id}");
        let (model, _warnings) = self
            .config
            .snapshot()
            .await
            .resolve(&model_ref)
            .map_err(EngineError::from)?;
        let client = crate::router::build_client(
            &model,
            &model_ref,
            Arc::clone(&self.store),
            Arc::clone(&self.transport),
        )?;

        tracing::info!(
            target: "zlogic::engine",
            model = %model_ref,
            "credential verify: sending a minimal test request"
        );

        let mut params = model.default_params.clone();
        params.insert("max_tokens".into(), serde_json::json!(32));
        let request = LlmRequest {
            model: model.wire_model.clone(),
            system: Vec::new(),
            messages: vec![Message::user(vec![ContentPart::Text(TextPart {
                text: "Reply with exactly: OK".into(),
                raw: None,
                truncated: false,
            })])],
            tools: Vec::new(),
            thinking: ThinkingIntent {
                mode: ThinkingMode::Off,
                ..Default::default()
            },
            params,
            response_format: None,
            cache: CacheSpec::off(),
            meta: RequestMeta {
                session_id: "credential-verify".into(),
                turn_id: "credential-verify".into(),
                round_id: "credential-verify".into(),
                purpose: Purpose::Utility,
            },
        };

        let started = Instant::now();
        let reply = tokio::time::timeout(Duration::from_secs(30), async {
            let mut stream = client.stream(request).await.map_err(|error| {
                verify_llm_error(&model_ref, "connection verification failed", error)
            })?;
            let mut reply = String::new();
            let mut completed = false;
            while let Some(event) = stream.next().await {
                match event.map_err(|error| {
                    verify_llm_error(&model_ref, "response stream verification failed", error)
                })? {
                    LlmEvent::PartEnd {
                        part: ContentPart::Text(part),
                        ..
                    } => reply.push_str(&part.text),
                    LlmEvent::ResponseEnd { .. } => completed = true,
                    _ => {}
                }
            }
            if !completed {
                return Err(ApiError::unavailable(
                    "credential_verify_incomplete",
                    "The model response ended before a completion marker",
                )
                .with_detail(
                    "diagnostic",
                    format!("the response from {model_ref} ended before a completion marker"),
                ));
            }
            Ok::<String, ApiError>(reply.chars().take(240).collect())
        })
        .await
        .map_err(|_| {
            tracing::error!(
                target: "zlogic::engine",
                model = %model_ref,
                "credential verify: timed out after 30s"
            );
            ApiError::unavailable(
                "credential_verify_timeout",
                "The model connection check timed out (30 s)",
            )
            .with_detail(
                "diagnostic",
                format!("connection verification for {model_ref} timed out (30 s)"),
            )
        })??;

        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        tracing::info!(
            target: "zlogic::engine",
            model = %model_ref,
            duration_ms,
            reply_chars = reply.chars().count(),
            "credential verify: succeeded"
        );

        Ok(CredentialVerifyResult {
            provider_id,
            model_id: model_id.to_string(),
            reply: reply.trim().to_string(),
            duration_ms,
        })
    }
}

#[async_trait]
impl CredentialService for Credentials {
    async fn list(&self) -> ApiResult<Vec<CredentialState>> {
        self.list_inner().await.map_err(ApiError::from)
    }

    async fn set(&self, req: CredentialSetReq) -> ApiResult<CredentialState> {
        self.set_inner(req).await.map_err(ApiError::from)
    }

    async fn delete(&self, req: CredentialDeleteReq) -> ApiResult<CredentialState> {
        self.delete_inner(req).await.map_err(ApiError::from)
    }

    async fn verify(&self, req: CredentialVerifyReq) -> ApiResult<CredentialVerifyResult> {
        self.verify_inner(req).await
    }

    async fn sign_in_begin(&self, req: ProviderSignInBeginReq) -> ApiResult<ProviderSignInBegin> {
        self.sign_in.begin(req).await
    }

    async fn sign_in_status(
        &self,
        req: ProviderSignInStatusReq,
    ) -> ApiResult<ProviderSignInStatus> {
        self.sign_in.status(req).await
    }

    async fn sign_in_cancel(&self, req: ProviderSignInCancelReq) -> ApiResult<()> {
        self.sign_in.cancel(req)
    }

    async fn models(&self, req: ProviderModelsReq) -> ApiResult<ProviderModels> {
        self.sign_in.models(req).await
    }

    async fn forget_models(&self, req: ProviderModelsReq) -> ApiResult<()> {
        self.sign_in
            .forget_models(req.provider_id.trim())
            .await
            .map_err(ApiError::from)
    }
}

fn normalized_provider_id(raw: &str) -> Result<String> {
    let provider_id = raw.trim().to_ascii_lowercase();
    if provider_id.is_empty() {
        Err(EngineError::Invalid("provider id must not be empty".into()))
    } else {
        Ok(provider_id)
    }
}

fn mask(value: &str) -> String {
    let tail: String = value
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

fn verify_llm_error(model_ref: &str, phase: &str, error: LlmError) -> ApiError {
    tracing::error!(
        target: "zlogic::engine",
        model = model_ref,
        phase,
        kind = ?error.kind,
        status = error.status,
        request_id = ?error.request_id,
        "credential verify failed: {error}"
    );
    zlogic_llm::to_api_error(&error, format!("{model_ref} {phase}: {error}"))
}

fn api_to_engine(error: ApiError) -> EngineError {
    match error.category {
        ErrorCategory::InvalidArgument => EngineError::Invalid(error.message.fallback),
        ErrorCategory::Conflict => EngineError::Conflict(error.message.fallback),
        _ => EngineError::Invalid(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use zlogic_config::{AppConfig, Dirs};
    use zlogic_core::SharedStore;
    use zlogic_credential::CredentialError;

    use super::*;
    use crate::service::WorkspaceService;

    #[derive(Default)]
    struct MemoryCredentialStore(Mutex<HashMap<String, String>>);

    impl MemoryCredentialStore {
        fn with(values: &[(&str, &str)]) -> Self {
            Self(Mutex::new(
                values
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
            ))
        }
    }

    impl CredentialStore for MemoryCredentialStore {
        fn resolve(&self, credential_ref: &str) -> Option<String> {
            self.0.lock().unwrap().get(credential_ref).cloned()
        }

        fn set_keyring(
            &self,
            provider_id: &str,
            secret: &str,
        ) -> std::result::Result<(), CredentialError> {
            self.0
                .lock()
                .unwrap()
                .insert(format!("keyring:{provider_id}"), secret.to_string());
            Ok(())
        }

        fn delete_keyring(&self, provider_id: &str) -> std::result::Result<(), CredentialError> {
            self.0
                .lock()
                .unwrap()
                .remove(&format!("keyring:{provider_id}"));
            Ok(())
        }
    }

    struct Rig {
        credentials: Credentials,
        _home: tempfile::TempDir,
    }

    impl Rig {
        fn new(
            yaml: &str,
            values: &[(&str, &str)],
            transport: Arc<dyn zlogic_llm::transport::HttpTransport>,
        ) -> Self {
            let home = tempfile::tempdir().unwrap();
            let dirs = Dirs {
                config: home.path().join("config"),
                data: home.path().join("data"),
                state: home.path().join("state"),
                cache: home.path().join("cache"),
            };
            std::fs::create_dir_all(&dirs.config).unwrap();
            std::fs::write(dirs.config_file(), yaml).unwrap();
            let credential_store = Arc::new(MemoryCredentialStore::with(values));
            let config = Arc::new(
                AppConfig::load(&dirs, |reference| credential_store.is_available(reference))
                    .unwrap(),
            );
            let store = SharedStore::new(zlogic_store::Db::open_in_memory().unwrap());
            let workspaces: Arc<dyn WorkspaceService> =
                Arc::new(crate::Workspaces::new(store.clone()));
            let config = Arc::new(Config::new(
                config,
                dirs,
                credential_store.clone(),
                store,
                workspaces,
            ));
            Self {
                credentials: Credentials::new(config, credential_store, transport),
                _home: home,
            }
        }
    }

    const YAML: &str = r#"
providers:
  test:
    sdk: openai_chat
    base_url: https://example.test/v1
    models:
      tiny:
        context_window: 4096
"#;

    fn empty_transport() -> Arc<dyn zlogic_llm::transport::HttpTransport> {
        Arc::new(zlogic_llm::transport::ReplayTransport::whole(""))
    }

    #[tokio::test]
    async fn the_search_backends_are_listed_so_their_keys_have_a_home() {
        let rig = Rig::new(YAML, &[], empty_transport());
        let states = rig.credentials.list().await.unwrap();

        let exa = states
            .iter()
            .find(|state| state.provider_id == "exa")
            .expect("exa must be in this list");
        assert!(!exa.present);
        assert_eq!(
            exa.candidates.last().map(String::as_str),
            Some("keyring:web_search:exa")
        );

        let saved = rig
            .credentials
            .set(CredentialSetReq {
                provider_id: "exa".into(),
                value: "sk-search-abcd".into(),
            })
            .await
            .unwrap();
        assert!(saved.present);
        assert_eq!(saved.source, CredentialSource::Keyring);
        assert_eq!(saved.hint.as_deref(), Some("…abcd"));
    }

    #[tokio::test]
    async fn set_and_delete_are_separate_operations() {
        let rig = Rig::new(YAML, &[], empty_transport());
        let saved = rig
            .credentials
            .set(CredentialSetReq {
                provider_id: "test".into(),
                value: "secret-abcd".into(),
            })
            .await
            .unwrap();
        assert!(saved.present);
        assert_eq!(saved.source, CredentialSource::Keyring);
        assert_eq!(saved.hint.as_deref(), Some("…abcd"));

        let deleted = rig
            .credentials
            .delete(CredentialDeleteReq {
                provider_id: "test".into(),
            })
            .await
            .unwrap();
        assert!(!deleted.present);
        assert_eq!(deleted.source, CredentialSource::Missing);
    }

    #[tokio::test]
    async fn set_rejects_the_old_empty_value_delete_convention() {
        let rig = Rig::new(YAML, &[], empty_transport());
        let error = rig
            .credentials
            .set(CredentialSetReq {
                provider_id: "test".into(),
                value: " ".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::InvalidArgument);
    }

    #[tokio::test]
    async fn verification_uses_the_real_llm_transport() {
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"OK\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let rig = Rig::new(
            YAML,
            &[("keyring:test", "secret-that-must-not-be-returned")],
            Arc::new(zlogic_llm::transport::ReplayTransport::whole(body)),
        );
        let result = rig
            .credentials
            .verify(CredentialVerifyReq {
                provider_id: "test".into(),
                model_id: "tiny".into(),
            })
            .await
            .unwrap();
        assert_eq!(result.reply, "OK");
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("secret-that-must-not-be-returned")
        );
    }
}
