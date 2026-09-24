//! The stored subscription credential: one JSON document holding everything a request needs.

use serde::{Deserialize, Serialize};

use crate::claims;
use crate::now_ms;

/// Refresh this long before the access token actually dies, so a request that is already in
/// flight never crosses the line.
pub const REFRESH_MARGIN_MS: i64 = 60_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix milliseconds.
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residency: Option<String>,
}

/// What `auth.openai.com/oauth/token` answers, for all three grants.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    #[serde(default)]
    pub id_token: Option<String>,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}

impl Tokens {
    pub fn from_response(response: TokenResponse, previous: Option<&Tokens>) -> Self {
        let parsed = claims::parse(&response.access_token);
        let id_claims = response.id_token.as_deref().and_then(claims::parse);
        let account_id = claims::account_id(response.id_token.as_deref(), &response.access_token)
            .or_else(|| previous.and_then(|p| p.account_id.clone()));
        let residency = parsed
            .as_ref()
            .and_then(|c| c.residency())
            .or_else(|| previous.and_then(|p| p.residency.clone()))
            .or_else(|| claims::residency(&response.access_token));

        Self {
            access_token: response.access_token.clone(),
            refresh_token: response
                .refresh_token
                .clone()
                .or_else(|| previous.map(|p| p.refresh_token.clone()))
                .unwrap_or_default(),
            expires_at: now_ms() + response.expires_in.unwrap_or(3600).max(0) * 1000,
            account_id,
            email: id_claims
                .as_ref()
                .and_then(|c| c.email.clone())
                .or_else(|| parsed.as_ref().and_then(|c| c.email.clone()))
                .or_else(|| previous.and_then(|p| p.email.clone())),
            plan: id_claims
                .as_ref()
                .and_then(|c| c.plan())
                .or_else(|| parsed.as_ref().and_then(|c| c.plan()))
                .or_else(|| previous.and_then(|p| p.plan.clone())),
            residency,
        }
    }

    /// True when the access token is missing or about to die and a refresh is due.
    pub fn needs_refresh(&self) -> bool {
        self.access_token.is_empty() || self.expires_at - now_ms() <= REFRESH_MARGIN_MS
    }

    pub fn is_usable(&self) -> bool {
        !self.refresh_token.is_empty() || !self.access_token.is_empty()
    }

    pub fn label(&self) -> Option<String> {
        claims::label(self.email.as_deref(), self.account_id.as_deref())
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Not a `TryFrom`: a value that cannot be read is a stale entry, not a caller error.
    pub fn from_json(body: &str) -> Option<Self> {
        serde_json::from_str(body).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(access: &str, refresh: Option<&str>, expires_in: Option<i64>) -> TokenResponse {
        TokenResponse {
            id_token: None,
            access_token: access.to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_in,
        }
    }

    #[test]
    fn a_response_becomes_a_credential() {
        let tokens = Tokens::from_response(response("a", Some("r"), Some(3600)), None);
        assert_eq!(tokens.access_token, "a");
        assert_eq!(tokens.refresh_token, "r");
        assert!(!tokens.needs_refresh());
        assert!(tokens.expires_at > now_ms());
    }

    #[test]
    fn a_refresh_that_omits_the_refresh_token_keeps_the_stored_one() {
        let previous = Tokens::from_response(response("a", Some("r"), Some(60)), None);
        let refreshed = Tokens::from_response(response("b", None, Some(60)), Some(&previous));
        assert_eq!(refreshed.refresh_token, "r");
        assert_eq!(refreshed.access_token, "b");
    }

    #[test]
    fn a_missing_expiry_is_an_hour_not_forever() {
        let tokens = Tokens::from_response(response("a", Some("r"), None), None);
        assert!(tokens.expires_at - now_ms() > 3_500_000);
        assert!(tokens.expires_at - now_ms() <= 3_600_000);
    }

    #[test]
    fn the_margin_forces_a_refresh_before_the_token_dies() {
        let mut tokens = Tokens::from_response(response("a", Some("r"), Some(30)), None);
        assert!(tokens.needs_refresh(), "30s < the 60s margin");
        tokens.access_token.clear();
        assert!(
            tokens.needs_refresh(),
            "an empty access token is never usable"
        );
    }

    #[test]
    fn a_stored_document_round_trips_and_junk_is_just_absent() {
        let tokens = Tokens::from_response(response("a", Some("r"), Some(60)), None);
        let back = Tokens::from_json(&tokens.to_json().unwrap()).unwrap();
        assert_eq!(back, tokens);
        assert!(Tokens::from_json("not json").is_none());
    }
}
