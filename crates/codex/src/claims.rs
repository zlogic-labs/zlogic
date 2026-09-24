//! The claims an id/access token carries.
//!
//! Everything a request needs beyond the bearer token itself — the account the subscription
//! belongs to, the compute residency it is pinned to — travels inside the token. Reading it back
//! out costs no extra call and is the only place the account id can come from.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Claims {
    #[serde(default)]
    pub chatgpt_account_id: Option<String>,
    #[serde(default)]
    pub chatgpt_plan_type: Option<String>,
    #[serde(default)]
    pub chatgpt_compute_residency: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub organizations: Vec<Organization>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    pub auth: Option<AuthClaims>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthClaims {
    #[serde(default)]
    pub chatgpt_account_id: Option<String>,
    #[serde(default)]
    pub chatgpt_plan_type: Option<String>,
    #[serde(default)]
    pub chatgpt_compute_residency: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Organization {
    pub id: String,
}

pub fn parse(token: &str) -> Option<Claims> {
    let mut parts = token.split('.');
    let (Some(_header), Some(payload), Some(_signature)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let payload = payload.trim_end_matches('=');
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

impl Claims {
    pub fn account_id(&self) -> Option<String> {
        self.chatgpt_account_id
            .clone()
            .or_else(|| {
                self.auth
                    .as_ref()
                    .and_then(|a| a.chatgpt_account_id.clone())
            })
            .or_else(|| self.organizations.first().map(|o| o.id.clone()))
    }

    pub fn plan(&self) -> Option<String> {
        self.chatgpt_plan_type
            .clone()
            .or_else(|| self.auth.as_ref().and_then(|a| a.chatgpt_plan_type.clone()))
    }

    /// The residency a token pins its account to, when it pins one.
    pub fn residency(&self) -> Option<String> {
        let residency = self.chatgpt_compute_residency.clone().or_else(|| {
            self.auth
                .as_ref()
                .and_then(|a| a.chatgpt_compute_residency.clone())
        })?;
        if residency.is_empty() || residency == "no_constraint" {
            return None;
        }
        Some(residency)
    }
}

/// The account id a token names, preferring the id token (it is the one carrying organizations).
pub fn account_id(id_token: Option<&str>, access_token: &str) -> Option<String> {
    id_token
        .and_then(parse)
        .and_then(|c| c.account_id())
        .or_else(|| parse(access_token).and_then(|c| c.account_id()))
}

pub fn residency(access_token: &str) -> Option<String> {
    parse(access_token).and_then(|c| c.residency())
}

/// A short, non-secret label for an account: an email when the token carries one, else its tail.
pub fn label(email: Option<&str>, account_id: Option<&str>) -> Option<String> {
    if let Some(email) = email.filter(|e| !e.trim().is_empty()) {
        return Some(email.to_string());
    }
    let account_id = account_id?;
    let tail: String = account_id
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Some(format!("…{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(payload: &str) -> String {
        let encoded = URL_SAFE_NO_PAD.encode(payload.as_bytes());
        format!("header.{encoded}.signature")
    }

    #[test]
    fn the_account_id_comes_from_the_top_level_claim() {
        let claims = parse(&token(r#"{"chatgpt_account_id":"acct-1"}"#)).unwrap();
        assert_eq!(claims.account_id().as_deref(), Some("acct-1"));
    }

    #[test]
    fn the_scoped_auth_claim_is_read_too() {
        let claims = parse(&token(
            r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-2","chatgpt_plan_type":"plus"}}"#,
        ))
        .unwrap();
        assert_eq!(claims.account_id().as_deref(), Some("acct-2"));
        assert_eq!(claims.plan().as_deref(), Some("plus"));
    }

    #[test]
    fn an_organization_is_the_last_resort_for_the_account_id() {
        let claims = parse(&token(r#"{"organizations":[{"id":"org-1"}]}"#)).unwrap();
        assert_eq!(claims.account_id().as_deref(), Some("org-1"));
    }

    #[test]
    fn no_constraint_is_not_a_residency() {
        let claims = parse(&token(r#"{"chatgpt_compute_residency":"no_constraint"}"#)).unwrap();
        assert!(claims.residency().is_none());
        let claims = parse(&token(r#"{"chatgpt_compute_residency":"eu"}"#)).unwrap();
        assert_eq!(claims.residency().as_deref(), Some("eu"));
    }

    #[test]
    fn a_non_jwt_never_panics() {
        assert!(parse("not-a-token").is_none());
        assert!(parse("a.b").is_none());
        assert!(parse("a.!!!.c").is_none());
    }

    #[test]
    fn the_label_prefers_the_email_and_never_shows_the_whole_account() {
        assert_eq!(
            label(Some("me@example.com"), Some("acct-123456")).as_deref(),
            Some("me@example.com")
        );
        assert_eq!(label(None, Some("acct-123456")).as_deref(), Some("…123456"));
        assert!(label(None, None).is_none());
    }
}
