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
pub mod responses;
pub mod retry;
pub mod sse;
pub mod think_tags;
pub mod tool_schema;
pub mod transport;
pub mod usage_map;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use zlogic_protocol::config::ModelCapabilities;
use zlogic_protocol::llm::{LlmError, LlmEvent, LlmRequest};

pub use emitter::{PartEmitter, ReasoningRawSpec};
pub use error::to_api_error;
pub use factory::{ClientConfig, create_client, from_resolved};
pub use transport::{HttpRequest, HttpTransport};

pub type EventStream = BoxStream<'static, Result<LlmEvent, LlmError>>;

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn stream(&self, req: LlmRequest) -> Result<EventStream, LlmError>;
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

    pub fn authorize(
        &self,
        req: crate::transport::HttpRequest,
        native: AuthHeader,
    ) -> crate::transport::HttpRequest {
        let Some(key) = &self.api_key else {
            return req;
        };
        match &self.auth {
            AuthStyle::Native => match native {
                AuthHeader::Bearer => req.header("authorization", format!("Bearer {key}")),
                AuthHeader::Raw(name) => req.header(name, key.clone()),
            },
            AuthStyle::Header(name) => req.header(name.clone(), key.clone()),
        }
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

    #[test]
    fn wiring_replaces_the_auth_header_and_appends_query_params() {
        let mut e = Endpoint::new("https://my-res.openai.azure.com/openai/v1")
            .with_key(Some("secret".into()));
        e.auth = AuthStyle::Header("api-key".into());
        e.query = vec![("api-version".into(), "v1".into())];

        assert_eq!(
            e.url("responses"),
            "https://my-res.openai.azure.com/openai/v1/responses?api-version=v1"
        );

        let req = e.authorize(
            crate::transport::HttpRequest::json(e.url("responses"), Vec::new()),
            AuthHeader::Bearer,
        );
        let names: Vec<&str> = req.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"api-key"), "{names:?}");
        assert!(
            !names.contains(&"authorization"),
            "Bearer must be replaced, not added — sending both is a 401: {names:?}"
        );
    }

    #[test]
    fn without_wiring_each_codec_keeps_its_native_header() {
        let e = Endpoint::new("https://api.anthropic.com").with_key(Some("k".into()));
        let bearer = e.authorize(
            crate::transport::HttpRequest::json(e.url("v1/messages"), Vec::new()),
            AuthHeader::Bearer,
        );
        assert!(
            bearer
                .headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer k")
        );

        let raw = e.authorize(
            crate::transport::HttpRequest::json(e.url("v1/messages"), Vec::new()),
            AuthHeader::Raw("x-api-key"),
        );
        assert!(
            raw.headers
                .iter()
                .any(|(k, v)| k == "x-api-key" && v == "k")
        );
    }

    #[test]
    fn no_key_means_no_auth_header() {
        let e = Endpoint::new("https://x.test");
        let req = e.authorize(
            crate::transport::HttpRequest::json(e.url("a"), Vec::new()),
            AuthHeader::Bearer,
        );
        assert!(req.headers.iter().all(|(k, _)| k != "authorization"));
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
