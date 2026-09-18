//! MCP OAuth persistence.
//! `rmcp` owns protocol behaviour (RFC 9728/8414 discovery, PKCE, dynamic registration, refresh,
//! scope upgrades, and issuer validation). Zlogic supplies the persistence boundary: the complete
//! credential record is serialized into the same OS keychain service used by model-provider keys.
//! Authorization state remains in memory because it is short-lived and one-time-use.

use async_trait::async_trait;
use rmcp::transport::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    StoredCredentials,
};
use zlogic_credential::{
    CredentialError, CredentialRef, CredentialStore as ZlogicCredentialStore, SystemCredentialStore,
};

/// Canonical OS-keychain entry for one MCP server's OAuth client and tokens.
pub fn oauth_key(server_id: &str) -> String {
    format!(
        "{}_oauth",
        crate::def::token_key(server_id).trim_end_matches("_token")
    )
}

#[derive(Clone)]
pub struct OAuthKeyringStore {
    credential: CredentialRef,
}

impl OAuthKeyringStore {
    pub fn new(server_id: &str) -> Self {
        Self {
            credential: CredentialRef::keyring(oauth_key(server_id)),
        }
    }
}

impl std::fmt::Debug for OAuthKeyringStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthKeyringStore")
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl CredentialStore for OAuthKeyringStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let encoded = match SystemCredentialStore.resolve(&self.credential.to_string()) {
            Some(value) => value,
            None => return Ok(None),
        };
        serde_json::from_str(&encoded).map(Some).map_err(|_| {
            AuthError::InternalError("stored MCP OAuth credentials are invalid".into())
        })
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let encoded = serde_json::to_string(&credentials).map_err(|_| {
            AuthError::InternalError("could not encode MCP OAuth credentials".into())
        })?;
        SystemCredentialStore
            .set_keyring(self.credential.name(), &encoded)
            .map_err(|error| storage_error("write", error))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        SystemCredentialStore
            .delete_keyring(self.credential.name())
            .map_err(|error| storage_error("delete", error))
    }
}

fn storage_error(operation: &str, error: CredentialError) -> AuthError {
    // Credential errors contain only an entry name/backend reason, never the stored value.
    AuthError::InternalError(format!(
        "could not {operation} MCP OAuth credentials: {error}"
    ))
}

/// Build the SDK authorization manager with Zlogic's durable keychain store attached.
/// The caller then uses `AuthorizationRequest`/`AuthorizationSession` to expose the authorization
/// URL and pass the callback URL back. Keeping browser/UI ownership outside this crate lets the CLI
/// use a loopback listener while desktop/mobile use their platform deep-link handler.
pub async fn authorization_manager(
    server_id: &str,
    endpoint: &str,
) -> Result<AuthorizationManager, AuthError> {
    let mut manager = AuthorizationManager::new(endpoint).await?;
    manager.set_credential_store(OAuthKeyringStore::new(server_id));
    Ok(manager)
}

/// A PKCE authorization flow waiting for the browser/deep-link callback.
pub struct PendingAuthorization {
    session: AuthorizationSession,
}

impl PendingAuthorization {
    /// URL the host should open in the user's browser.
    pub fn authorization_url(&self) -> &str {
        self.session.get_authorization_url()
    }

    pub fn redirect_uri(&self) -> &str {
        &self.session.redirect_uri
    }

    /// Validate state/issuer, exchange the code, and persist the complete token record in keychain.
    pub async fn complete(self, callback_url: &str) -> Result<AuthorizationManager, AuthError> {
        self.session.handle_callback_url(callback_url).await?;
        Ok(self.session.auth_manager)
    }
}

/// Start discovery, PKCE, and client registration for an interactive OAuth flow.
/// With no pre-registered client ID the SDK follows MCP's preferred registration order and uses
/// dynamic client registration when advertised. A client secret, when needed, must already have
/// been resolved from a [`CredentialRef`] by the caller; it is never accepted from server config as
/// plaintext.
pub async fn begin_authorization(
    server_id: &str,
    endpoint: &str,
    redirect_uri: &str,
    scopes: impl IntoIterator<Item = String>,
    preregistered_client: Option<(&str, Option<&CredentialRef>)>,
) -> Result<PendingAuthorization, AuthError> {
    let mut manager = authorization_manager(server_id, endpoint).await?;
    let resolution = manager.resolve_metadata().await?;
    manager.set_metadata(resolution.metadata);

    let mut request = AuthorizationRequest::new(redirect_uri)
        .with_client_name("Zlogic")
        .with_scopes(scopes);
    if let Some((client_id, secret)) = preregistered_client {
        request = request.with_preregistered_client(client_id);
        if let Some(secret) = secret {
            let value = SystemCredentialStore
                .resolve(&secret.to_string())
                .ok_or_else(|| {
                    AuthError::InternalError("could not resolve MCP OAuth client credential".into())
                })?;
            request = request.with_client_secret(value);
        }
    }

    AuthorizationSession::new(manager, request)
        .await
        .map(|session| PendingAuthorization { session })
        .map_err(|(_, error)| error)
}

/// Restore a stored OAuth session. `Ok(None)` means this server has not been authorized yet.
pub async fn restore_authorization(
    server_id: &str,
    endpoint: &str,
) -> Result<Option<AuthorizationManager>, AuthError> {
    let mut manager = authorization_manager(server_id, endpoint).await?;
    if manager.initialize_from_store().await? {
        Ok(Some(manager))
    } else {
        Ok(None)
    }
}

/// Forget local OAuth client/tokens. Remote revocation is a separate, best-effort network action
/// and must be initiated explicitly by the management UI.
pub async fn clear_authorization(server_id: &str) -> Result<(), AuthError> {
    OAuthKeyringStore::new(server_id).clear().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_entries_are_scoped_and_collision_resistant() {
        assert_eq!(oauth_key("github"), "mcp_github_oauth");
        assert_ne!(oauth_key("a.b"), oauth_key("a_b"));
    }

    #[test]
    fn debug_never_contains_a_token_field() {
        let shown = format!("{:?}", OAuthKeyringStore::new("github"));
        assert!(shown.contains("mcp_github_oauth"));
        assert!(!shown.contains("access_token"));
        assert!(!shown.contains("refresh_token"));
    }
}
