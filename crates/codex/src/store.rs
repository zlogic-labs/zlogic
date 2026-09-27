//! Where the subscription credential is kept: one keychain entry per provider.

use zlogic_credential::{CredentialError, CredentialRef, CredentialStore, provider_oauth_entry};

use crate::Error;
use crate::tokens::Tokens;

pub fn reference(provider_id: &str) -> CredentialRef {
    CredentialRef::keyring(provider_oauth_entry(provider_id))
}

pub fn load(store: &dyn CredentialStore, provider_id: &str) -> Option<Tokens> {
    let encoded = store.resolve(&reference(provider_id).to_string())?;
    Tokens::from_json(&encoded)
}

pub fn read(store: &dyn CredentialStore, provider_id: &str) -> Result<Tokens, Error> {
    match store.resolve(&reference(provider_id).to_string()) {
        None => Err(Error::NotSignedIn(provider_id.to_string())),
        Some(encoded) => Tokens::from_json(&encoded).ok_or_else(|| {
            Error::Unreadable(
                provider_id.to_string(),
                format!(
                    "the stored value is not a subscription document; remove {} to sign in again",
                    reference(provider_id)
                ),
            )
        }),
    }
}

pub fn save(store: &dyn CredentialStore, provider_id: &str, tokens: &Tokens) -> Result<(), Error> {
    let encoded = tokens
        .to_json()
        .map_err(|e| Error::Store(format!("could not encode the subscription: {e}")))?;
    store
        .set_keyring(&provider_oauth_entry(provider_id), &encoded)
        .map_err(store_error)
}

pub fn clear(store: &dyn CredentialStore, provider_id: &str) -> Result<(), Error> {
    store
        .delete_keyring(&provider_oauth_entry(provider_id))
        .map_err(store_error)
}

fn store_error(error: CredentialError) -> Error {
    Error::Store(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::tokens::TokenResponse;

    #[derive(Default)]
    struct Memory(Mutex<HashMap<String, String>>);

    impl CredentialStore for Memory {
        fn resolve(&self, credential_ref: &str) -> Option<String> {
            self.0.lock().unwrap().get(credential_ref).cloned()
        }

        fn set_keyring(&self, entry: &str, secret: &str) -> Result<(), CredentialError> {
            self.0
                .lock()
                .unwrap()
                .insert(format!("keyring:{entry}"), secret.to_string());
            Ok(())
        }

        fn delete_keyring(&self, entry: &str) -> Result<(), CredentialError> {
            self.0.lock().unwrap().remove(&format!("keyring:{entry}"));
            Ok(())
        }
    }

    fn tokens() -> Tokens {
        Tokens::from_response(
            TokenResponse {
                id_token: None,
                access_token: "access".into(),
                refresh_token: Some("refresh".into()),
                expires_in: Some(3600),
            },
            None,
        )
    }

    #[test]
    fn the_entry_is_scoped_to_the_provider() {
        assert_eq!(reference("codex").to_string(), "keyring:codex_oauth");
        assert_eq!(
            reference("my-codex").to_string(),
            "keyring:my-codex_oauth",
            "a second subscription must not overwrite the first"
        );
    }

    #[test]
    fn a_credential_round_trips_through_the_store() {
        let store = Memory::default();
        let tokens = tokens();
        assert!(matches!(read(&store, "codex"), Err(Error::NotSignedIn(_))));
        save(&store, "codex", &tokens).unwrap();
        assert_eq!(load(&store, "codex").unwrap(), tokens);
        clear(&store, "codex").unwrap();
        assert!(load(&store, "codex").is_none());
    }

    #[test]
    fn an_api_key_in_that_slot_is_reported_rather_than_parsed() {
        let store = Memory::default();
        store
            .set_keyring("codex_oauth", "sk-not-a-subscription")
            .unwrap();
        assert!(matches!(
            read(&store, "codex"),
            Err(Error::Unreadable(_, _))
        ));
    }
}
