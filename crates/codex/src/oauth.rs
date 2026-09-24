//! The token endpoints, and the headers a subscription request must carry.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::tokens::{TokenResponse, Tokens};
use crate::{Config, Error, pkce::Pkce};

/// Everything the loopback authorization URL needs.
pub fn authorize_url(config: &Config, pkce: &Pkce, state: &str) -> String {
    let query = [
        ("response_type", "code"),
        ("client_id", config.client_id.as_str()),
        ("redirect_uri", &config.redirect_uri()),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", config.originator.as_str()),
    ]
    .iter()
    .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
    .collect::<Vec<_>>()
    .join("&");
    format!("{}/oauth/authorize?{query}", config.issuer)
}

/// A device authorization waiting for the user to type its code.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval_secs: u64,
}

/// One poll of a device authorization.
#[derive(Debug, Clone)]
pub enum DevicePoll {
    Pending,
    Done(Box<Tokens>),
}

pub struct Codex {
    config: Config,
    http: reqwest::Client,
}

impl std::fmt::Debug for Codex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Codex")
            .field("issuer", &self.config.issuer)
            .field("backend", &self.config.backend)
            .finish_non_exhaustive()
    }
}

impl Codex {
    pub fn new(config: Config) -> Self {
        Self::with_client(
            config,
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
        )
    }

    /// A client the caller owns, rather than the default one.
    ///
    /// The HTTP policy — proxy, timeouts, TLS roots — is a host decision: the CLI wants the system
    /// proxy settings, while a test talking to its own loopback issuer does not.
    pub fn with_client(config: Config, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Exchange the authorization code the callback delivered.
    pub async fn exchange_code(&self, code: &str, verifier: &str) -> Result<Tokens, Error> {
        let response = self
            .http
            .post(format!("{}/oauth/token", self.config.issuer))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &self.config.redirect_uri()),
                ("client_id", &self.config.client_id),
                ("code_verifier", verifier),
            ])
            .send()
            .await
            .map_err(|e| self.network("/oauth/token", e))?;
        let body = self.read(response, "/oauth/token").await?;
        Ok(Tokens::from_response(body, None))
    }

    pub async fn refresh(
        &self,
        refresh_token: &str,
        previous: Option<&Tokens>,
    ) -> Result<Tokens, Error> {
        let response = self
            .http
            .post(format!("{}/oauth/token", self.config.issuer))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", &self.config.client_id),
            ])
            .send()
            .await
            .map_err(|e| self.network("/oauth/token", e))?;

        if matches!(response.status().as_u16(), 400 | 401 | 403) {
            return Err(Error::SessionExpired);
        }
        let body = self.read(response, "/oauth/token").await?;
        Ok(Tokens::from_response(body, previous))
    }

    pub async fn device_start(&self) -> Result<DeviceCode, Error> {
        let endpoint = "/api/accounts/deviceauth/usercode";
        let response = self
            .http
            .post(format!("{}{endpoint}", self.config.issuer))
            .header("User-Agent", self.config.user_agent())
            .json(&serde_json::json!({ "client_id": self.config.client_id }))
            .send()
            .await
            .map_err(|e| self.network(endpoint, e))?;
        let body = self.read::<serde_json::Value>(response, endpoint).await?;

        let device_auth_id = body
            .get("device_auth_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Protocol("the device authorization has no id".into()))?
            .to_string();
        let user_code = body
            .get("user_code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Protocol("the device authorization has no user code".into()))?
            .to_string();
        let interval_secs = body
            .get("interval")
            .and_then(number_or_string)
            .unwrap_or(5)
            .max(1);

        Ok(DeviceCode {
            device_auth_id,
            user_code,
            interval_secs,
        })
    }

    /// One poll. `403`/`404` mean "not yet", anything else is an answer.
    pub async fn device_poll(&self, device: &DeviceCode) -> Result<DevicePoll, Error> {
        let endpoint = "/api/accounts/deviceauth/token";
        let response = self
            .http
            .post(format!("{}{endpoint}", self.config.issuer))
            .header("User-Agent", self.config.user_agent())
            .json(&serde_json::json!({
                "device_auth_id": device.device_auth_id,
                "user_code": device.user_code,
            }))
            .send()
            .await
            .map_err(|e| self.network(endpoint, e))?;

        let status = response.status().as_u16();
        if status == 403 || status == 404 {
            return Ok(DevicePoll::Pending);
        }
        let body = self.read::<serde_json::Value>(response, endpoint).await?;
        let code = body
            .get("authorization_code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Protocol("the device authorization returned no code".into()))?;
        let verifier = body
            .get("code_verifier")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                Error::Protocol("the device authorization returned no verifier".into())
            })?;

        let token_response = self
            .http
            .post(format!("{}/oauth/token", self.config.issuer))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &self.config.device_redirect_uri()),
                ("client_id", &self.config.client_id),
                ("code_verifier", verifier),
            ])
            .send()
            .await
            .map_err(|e| self.network("/oauth/token", e))?;
        let body = self
            .read::<TokenResponse>(token_response, "/oauth/token")
            .await?;
        Ok(DevicePoll::Done(Box::new(Tokens::from_response(
            body, None,
        ))))
    }

    /// A GET on the subscription backend, already authorized.
    pub(crate) fn http_get(&self, url: &str, tokens: &Tokens) -> reqwest::RequestBuilder {
        self.http.get(url).headers(self.auth_headers(tokens, None))
    }

    /// The headers every subscription request carries. `session_id` groups a conversation.
    pub(crate) fn auth_headers(&self, tokens: &Tokens, session_id: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_str(&format!("Bearer {}", tokens.access_token))
                .unwrap_or_else(|_| HeaderValue::from_static("")),
        );
        let mut put = |name: &'static str, value: Option<&str>| {
            if let Some(value) = value.filter(|v| !v.trim().is_empty())
                && let Ok(value) = HeaderValue::from_str(value)
            {
                headers.insert(HeaderName::from_static(name), value);
            }
        };
        put("chatgpt-account-id", tokens.account_id.as_deref());
        put("originator", Some(self.config.originator.as_str()));
        put("user-agent", Some(&self.config.user_agent()));
        put("session_id", session_id);
        put(
            "x-openai-internal-codex-residency",
            tokens.residency.as_deref(),
        );
        headers
    }

    fn network(&self, endpoint: &str, error: reqwest::Error) -> Error {
        Error::Network {
            url: format!("{}{endpoint}", self.config.issuer),
            reason: error.to_string(),
        }
    }

    async fn read<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
        endpoint: &str,
    ) -> Result<T, Error> {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(Error::Status {
                endpoint: format!("{}{endpoint}", self.config.issuer),
                status,
                body: body.chars().take(400).collect(),
            });
        }
        serde_json::from_str(&body).map_err(|e| {
            Error::Protocol(format!(
                "{endpoint} did not answer with the expected JSON: {e}; body: {}",
                body.chars().take(200).collect::<String>()
            ))
        })
    }
}

fn number_or_string(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn encode(value: &str) -> String {
    value
        .chars()
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

    fn config() -> Config {
        Config {
            issuer: "https://issuer.test".into(),
            backend: "https://backend.test/api/codex".into(),
            client_id: "client-1".into(),
            originator: "zlogic".into(),
            client_version: "zlogic/0.0.0".into(),
        }
    }

    #[test]
    fn the_authorization_url_carries_pkce_and_the_registered_redirect() {
        let pkce = Pkce::generate().unwrap();
        let url = authorize_url(&config(), &pkce, "state-1");
        assert!(url.starts_with("https://issuer.test/oauth/authorize?"));
        for want in [
            "response_type=code",
            "client_id=client-1",
            "code_challenge_method=S256",
            "id_token_add_organizations=true",
            "codex_cli_simplified_flow=true",
            "state=state-1",
            "originator=zlogic",
        ] {
            assert!(url.contains(want), "{url} is missing {want}");
        }
        assert!(
            url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"),
            "the redirect URI must be escaped and must be the registered one: {url}"
        );
        assert!(url.contains(&format!("code_challenge={}", pkce.challenge)));
        assert!(
            !url.contains(&pkce.verifier),
            "the verifier stays local: {url}"
        );
    }

    #[test]
    fn the_headers_name_the_account_the_client_and_the_session() {
        let codex = Codex::new(config());
        let tokens = Tokens {
            access_token: "access-1".into(),
            account_id: Some("acct-1".into()),
            residency: Some("eu".into()),
            ..Default::default()
        };

        let headers = codex.auth_headers(&tokens, Some("session-1"));
        assert_eq!(headers["authorization"], "Bearer access-1");
        assert_eq!(headers["chatgpt-account-id"], "acct-1");
        assert_eq!(headers["originator"], "zlogic");
        assert_eq!(headers["session_id"], "session-1");
        assert_eq!(headers["x-openai-internal-codex-residency"], "eu");
        assert!(
            headers["user-agent"]
                .to_str()
                .unwrap()
                .starts_with("zlogic/")
        );

        let plain = codex.auth_headers(&Tokens::default(), None);
        assert!(
            !plain.contains_key("chatgpt-account-id"),
            "an absent account must not become an empty header"
        );
        assert!(!plain.contains_key("x-openai-internal-codex-residency"));
    }

    #[test]
    fn a_numeric_interval_arrives_as_a_string_or_a_number() {
        assert_eq!(number_or_string(&serde_json::json!(5)), Some(5));
        assert_eq!(number_or_string(&serde_json::json!("7")), Some(7));
        assert_eq!(number_or_string(&serde_json::json!(null)), None);
    }
}
