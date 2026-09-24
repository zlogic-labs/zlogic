//! ChatGPT subscription sign-in for zlogic.
//!
//! A ChatGPT plan does not answer on `api.openai.com`; the only host it reaches is the one the
//! Codex CLI itself uses (`chatgpt.com/backend-api/codex`), guarded by an OAuth token minted by
//! `auth.openai.com`. This crate owns that whole story and nothing else:
//!
//! * [`oauth`] — the three token operations (authorize, exchange, refresh) and the model catalog.
//! * [`flow`] — the two interactive sign-ins (loopback browser callback, device code) and their
//!   in-process state, so a host can show a URL and poll for the result.
//! * [`provider`] — the token custodian an outbound request asks for a bearer token; it refreshes
//!   when the stored one has expired, so no caller ever holds a stale token.
//!
//! Tokens live in the OS keychain, under `<provider id>_oauth`, one JSON document per provider
//! ([`zlogic_credential::provider_oauth_entry`]).

pub mod claims;
pub mod flow;
pub mod models;
pub mod oauth;
pub mod pkce;
pub mod provider;
pub mod store;
pub mod tokens;

pub use flow::{Begin, FlowOutcome as FlowStatus, Flows, SignInMethod};
pub use models::ModelInfo;
pub use oauth::{Codex, DeviceCode, DevicePoll};
pub use provider::Subscription;
pub use tokens::Tokens;

/// The issuer that mints ChatGPT subscription tokens.
pub const ISSUER: &str = "https://auth.openai.com";
/// The ChatGPT host the Codex CLI talks to. It does not answer under `/v1`.
pub const BACKEND: &str = "https://chatgpt.com/backend-api/codex";
/// Codex CLI's public OAuth client, the only one this redirect URI is registered for.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Marks the requesting client in outbound headers.
pub const ORIGINATOR: &str = "zlogic";
/// The loopback port the registered redirect URI names.
pub const CALLBACK_PORT: u16 = 1455;

/// Where sign-ins and model calls go. Tests point it at a local server.
#[derive(Debug, Clone)]
pub struct Config {
    pub issuer: String,
    pub backend: String,
    pub client_id: String,
    pub originator: String,
    pub client_version: String,
}

impl Default for Config {
    fn default() -> Self {
        Self::openai()
    }
}

impl Config {
    pub fn openai() -> Self {
        Self {
            issuer: ISSUER.to_string(),
            backend: BACKEND.to_string(),
            client_id: CLIENT_ID.to_string(),
            originator: ORIGINATOR.to_string(),
            client_version: format!("zlogic/{}", env!("CARGO_PKG_VERSION")),
        }
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{CALLBACK_PORT}/auth/callback")
    }

    pub fn device_redirect_uri(&self) -> String {
        format!("{}/deviceauth/callback", self.issuer)
    }

    pub fn device_url(&self) -> String {
        format!("{}/codex/device", self.issuer)
    }

    pub fn user_agent(&self) -> String {
        format!(
            "{} ({} {}; {})",
            self.client_version,
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::consts::FAMILY
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ChatGPT sign-in is not reachable at {url}: {reason}")]
    Network { url: String, reason: String },
    #[error("{endpoint} answered {status}: {body}")]
    Status {
        endpoint: String,
        status: u16,
        body: String,
    },
    #[error("{0}")]
    Protocol(String),
    #[error("no ChatGPT subscription is signed in for provider {0}")]
    NotSignedIn(String),
    #[error("the stored ChatGPT subscription for provider {0} is unreadable: {1}")]
    Unreadable(String, String),
    #[error("the ChatGPT session expired; sign in again")]
    SessionExpired,
    #[error(
        "port {0} is already in use, so the browser sign-in cannot listen there; use the device code flow instead"
    )]
    CallbackPortInUse(u16),
    #[error("{0}")]
    Store(String),
    #[error("that sign-in was cancelled")]
    Cancelled,
    #[error("that sign-in is not running")]
    UnknownFlow,
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The process-wide client.
///
/// Sign-in and the request path must share one HTTP client: it owns a connection pool, and a
/// per-request client would rebuild that pool on every turn.
pub fn shared() -> std::sync::Arc<Codex> {
    static SHARED: std::sync::OnceLock<std::sync::Arc<Codex>> = std::sync::OnceLock::new();
    std::sync::Arc::clone(SHARED.get_or_init(|| std::sync::Arc::new(Codex::new(Config::openai()))))
}
