//! # zlogic-llm
//! ```text
//! LlmRequest ──> client.serialize ──> HttpTransport ──> SSE ──> client.parse
//!                                                                   │
//!                                                            PartEmitter
//!                                                                   │
//!                                                              LlmEvent…
//! ```

pub mod anthropic;
pub mod bedrock;
pub mod cache;
pub mod chat;
pub mod detail;
pub mod emitter;
pub mod error;
pub mod factory;
pub mod gemini;
pub mod media;
#[cfg(feature = "test-support")]
pub mod mock;
pub mod ratelimit;
pub mod responses;
pub mod retry;
pub mod sse;
pub mod think_tags;
pub mod tool_schema;
pub mod transport;
pub mod usage_map;

use std::sync::Arc;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use zlogic_protocol::config::ModelCapabilities;
use zlogic_protocol::llm::{LlmError, LlmEvent, LlmRequest, RequestMeta};

pub use emitter::{PartEmitter, ReasoningRawSpec};
pub use error::to_api_error;
pub use factory::{Auth, ClientConfig, create_client, from_resolved};
pub use transport::{HttpRequest, HttpTransport};

pub type EventStream = BoxStream<'static, Result<LlmEvent, LlmError>>;

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn stream(&self, req: LlmRequest) -> Result<EventStream, LlmError>;
}

/// A bearer token and the headers that must travel with it.
#[derive(Debug, Clone, Default)]
pub struct Token {
    pub access: String,
    pub headers: Vec<(String, String)>,
}

/// Where a request's credential comes from when it is not a string kept under a key.
///
/// A subscription is the reason this exists: its bearer token expires, so it cannot be read once
/// when a client is built. The provider is asked per request, and only it decides whether that
/// means handing over a cached token or refreshing the stored one first.
#[async_trait]
pub trait TokenProvider: Send + Sync + std::fmt::Debug {
    async fn token(&self, meta: &RequestMeta) -> Result<Token, LlmError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AuthStyle {
    #[default]
    Native,
    Header(String),
}

#[derive(Debug, Clone, Default)]
pub struct Endpoint {
    pub base_url: String,
    pub api_key: Option<String>,
    pub auth: AuthStyle,
    pub extra_headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
    pub capabilities: ModelCapabilities,
    pub network: zlogic_protocol::config::NetworkConfig,
    /// The request path, when the provider does not live at the codec's usual one.
    pub path: Option<String>,
    /// Set when the credential is a token that has to be fetched (and refreshed) per request.
    pub token: Option<Arc<dyn TokenProvider>>,
}

impl Endpoint {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            ..Default::default()
        }
    }

    pub fn with_key(mut self, key: Option<String>) -> Self {
        self.api_key = key;
        self
    }

    pub fn with_capabilities(mut self, caps: ModelCapabilities) -> Self {
        self.capabilities = caps;
        self
    }

    pub fn url(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        let mut url = format!("{base}/{path}");
        for (i, (k, v)) in self.query.iter().enumerate() {
            let sep = if i == 0 && !url.contains('?') {
                '?'
            } else {
                '&'
            };
            url.push(sep);
            url.push_str(&encode(k));
            url.push('=');
            url.push_str(&encode(v));
        }
        url
    }

    /// The path this codec would use, unless the provider names another one.
    pub fn path_or<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.path.as_deref().unwrap_or(fallback)
    }

    /// Attach the credential to a request.
    ///
    /// A subscription asks its [`TokenProvider`] here, per request: that is the only point where a
    /// token that has since expired can be replaced without rebuilding the client.
    pub async fn authorize(
        &self,
        req: crate::transport::HttpRequest,
        native: AuthHeader,
        meta: &RequestMeta,
    ) -> Result<crate::transport::HttpRequest, LlmError> {
        if let Some(provider) = &self.token {
            let token = provider.token(meta).await?;
            let mut req = req.header("authorization", format!("Bearer {}", token.access));
            for (name, value) in token.headers {
                req = req.header(name, value);
            }
            return Ok(req);
        }

        let Some(key) = &self.api_key else {
            return Ok(req);
        };
        Ok(match &self.auth {
            AuthStyle::Native => match native {
                AuthHeader::Bearer => req.header("authorization", format!("Bearer {key}")),
                AuthHeader::Raw(name) => req.header(name, key.clone()),
            },
            AuthStyle::Header(name) => req.header(name.clone(), key.clone()),
        })
    }

    pub fn request(&self, path: &str, body: Vec<u8>) -> crate::transport::HttpRequest {
        crate::transport::HttpRequest {
            url: self.url(path),
            headers: vec![("content-type".into(), "application/json".into())],
            body,
            network: self.network.clone(),
        }
    }
}

pub enum AuthHeader {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// `<name>: <key>`
    Raw(&'static str),
}

fn encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            other => other
                .to_string()
                .as_bytes()
                .iter()
                .map(|b| format!("%{b:02X}"))
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> RequestMeta {
        RequestMeta {
            session_id: "s".into(),
            turn_id: "t".into(),
            round_id: "r".into(),
            purpose: zlogic_protocol::usage::Purpose::Main,
        }
    }

    #[tokio::test]
    async fn wiring_replaces_the_auth_header_and_appends_query_params() {
        let mut e = Endpoint::new("https://my-res.openai.azure.com/openai/v1")
            .with_key(Some("secret".into()));
        e.auth = AuthStyle::Header("api-key".into());
        e.query = vec![("api-version".into(), "v1".into())];

        assert_eq!(
            e.url("responses"),
            "https://my-res.openai.azure.com/openai/v1/responses?api-version=v1"
        );

        let req = e
            .authorize(
                crate::transport::HttpRequest::json(e.url("responses"), Vec::new()),
                AuthHeader::Bearer,
                &meta(),
            )
            .await
            .unwrap();
        let names: Vec<&str> = req.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"api-key"), "{names:?}");
        assert!(
            !names.contains(&"authorization"),
            "Bearer must be replaced, not added — sending both is a 401: {names:?}"
        );
    }

    #[tokio::test]
    async fn without_wiring_each_codec_keeps_its_native_header() {
        let e = Endpoint::new("https://api.anthropic.com").with_key(Some("k".into()));
        let bearer = e
            .authorize(
                crate::transport::HttpRequest::json(e.url("v1/messages"), Vec::new()),
                AuthHeader::Bearer,
                &meta(),
            )
            .await
            .unwrap();
        assert!(
            bearer
                .headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer k")
        );

        let raw = e
            .authorize(
                crate::transport::HttpRequest::json(e.url("v1/messages"), Vec::new()),
                AuthHeader::Raw("x-api-key"),
                &meta(),
            )
            .await
            .unwrap();
        assert!(
            raw.headers
                .iter()
                .any(|(k, v)| k == "x-api-key" && v == "k")
        );
    }

    #[tokio::test]
    async fn no_key_means_no_auth_header() {
        let e = Endpoint::new("https://x.test");
        let req = e
            .authorize(
                crate::transport::HttpRequest::json(e.url("a"), Vec::new()),
                AuthHeader::Bearer,
                &meta(),
            )
            .await
            .unwrap();
        assert!(req.headers.iter().all(|(k, _)| k != "authorization"));
    }

    #[tokio::test]
    async fn a_token_provider_supplies_the_bearer_and_its_headers() {
        #[derive(Debug)]
        struct Fixed;

        #[async_trait]
        impl TokenProvider for Fixed {
            async fn token(&self, meta: &RequestMeta) -> Result<Token, LlmError> {
                Ok(Token {
                    access: "subscription-token".into(),
                    headers: vec![("session_id".into(), meta.session_id.clone())],
                })
            }
        }

        let mut e = Endpoint::new("https://chatgpt.test/backend-api/codex");
        e.token = Some(Arc::new(Fixed));
        let req = e
            .authorize(
                crate::transport::HttpRequest::json(e.url("responses"), Vec::new()),
                AuthHeader::Bearer,
                &meta(),
            )
            .await
            .unwrap();
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer subscription-token")
        );
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "session_id" && v == "s")
        );
    }

    #[tokio::test]
    async fn a_provider_that_cannot_produce_a_token_fails_the_request() {
        #[derive(Debug)]
        struct Broken;

        #[async_trait]
        impl TokenProvider for Broken {
            async fn token(&self, _meta: &RequestMeta) -> Result<Token, LlmError> {
                Err(LlmError {
                    kind: zlogic_protocol::llm::LlmErrorKind::Auth,
                    retryable: false,
                    message: "no subscription".into(),
                    status: None,
                    request_id: None,
                })
            }
        }

        let mut e = Endpoint::new("https://chatgpt.test/backend-api/codex");
        e.token = Some(Arc::new(Broken));
        let error = e
            .authorize(
                crate::transport::HttpRequest::json(e.url("responses"), Vec::new()),
                AuthHeader::Bearer,
                &meta(),
            )
            .await;
        assert!(
            error.is_err(),
            "an unauthenticated request must not leave the process"
        );
    }

    #[test]
    fn a_path_override_replaces_the_codec_default() {
        let mut e = Endpoint::new("https://chatgpt.test/backend-api/codex");
        assert_eq!(e.path_or("v1/responses"), "v1/responses");
        e.path = Some("responses".into());
        assert_eq!(e.path_or("v1/responses"), "responses");
    }

    #[test]
    fn query_params_append_to_a_path_that_already_has_some() {
        let mut e = Endpoint::new("https://gw.test");
        e.query = vec![("api-version".into(), "2024-10-01".into())];
        assert_eq!(
            e.url("v1beta/models/x:streamGenerateContent?alt=sse"),
            "https://gw.test/v1beta/models/x:streamGenerateContent?alt=sse&api-version=2024-10-01"
        );
    }

    #[test]
    fn query_values_are_percent_encoded() {
        let mut e = Endpoint::new("https://x.test");
        e.query = vec![("k".into(), "a b&c=d".into())];
        assert_eq!(e.url("p"), "https://x.test/p?k=a%20b%26c%3Dd");
    }

    #[test]
    fn request_stamps_the_endpoints_network_timeouts() {
        let mut e = Endpoint::new("https://x.test");
        e.network = zlogic_protocol::config::NetworkConfig {
            connect_timeout_ms: Some(12_345),
            read_timeout_ms: Some(67_890),
        };
        let req = e.request("v1/messages", vec![1, 2, 3]);
        assert_eq!(
            req.connect_timeout(),
            Some(std::time::Duration::from_millis(12_345))
        );
        assert_eq!(
            req.read_timeout(),
            Some(std::time::Duration::from_millis(67_890))
        );

        let plain = Endpoint::new("https://x.test").request("p", Vec::new());
        assert!(plain.connect_timeout().is_none());
        assert!(plain.read_timeout().is_none());
    }

    #[test]
    fn url_join_tolerates_slashes() {
        assert_eq!(
            Endpoint::new("https://api.test/v1").url("chat/completions"),
            "https://api.test/v1/chat/completions"
        );
        assert_eq!(
            Endpoint::new("https://api.test/v1/").url("/chat/completions"),
            "https://api.test/v1/chat/completions"
        );
    }
}
