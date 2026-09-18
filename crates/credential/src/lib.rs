mod vault;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

pub use vault::{
    MASTER_ACCOUNT, MASTER_ENV_VAR, MASTER_KEY_LEN, SecretVault, init_secret_vault,
    secret_vault_initialized,
};

pub const KEYRING_SERVICE: &str = "zlogic";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum CredentialRef {
    Env(String),
    Keyring(String),
}

impl CredentialRef {
    pub fn env(name: impl Into<String>) -> Self {
        Self::Env(name.into())
    }

    pub fn keyring(name: impl Into<String>) -> Self {
        Self::Keyring(name.into())
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Env(name) | Self::Keyring(name) => name,
        }
    }

    pub fn resolve(&self) -> Result<String, CredentialError> {
        SystemCredentialStore::resolve_ref(self)
    }

    pub fn resolve_with(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<String, CredentialError> {
        match self {
            Self::Env(name) => env(name)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| CredentialError::Missing(self.clone())),
            Self::Keyring(_) => Self::resolve(self),
        }
    }

    pub fn store(&self, secret: &str) -> Result<(), CredentialError> {
        match self {
            Self::Keyring(name) => SystemCredentialStore.set_keyring(name, secret),
            Self::Env(name) => Err(CredentialError::NotWritable(name.clone())),
        }
    }

    pub fn delete(&self) -> Result<(), CredentialError> {
        match self {
            Self::Keyring(name) => SystemCredentialStore.delete_keyring(name),
            Self::Env(name) => Err(CredentialError::NotWritable(name.clone())),
        }
    }

    pub fn is_available(&self) -> bool {
        SystemCredentialStore.is_available(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    #[error("credential {0} is not set")]
    Missing(CredentialRef),
    #[error("keyring entry {name:?} failed: {reason}")]
    Keyring { name: String, reason: String },
    #[error("key storage file operation failed at {path}: {reason}")]
    Io { path: PathBuf, reason: String },
    #[error("secret encryption/decryption failed: {0}")]
    Crypto(String),
    #[error("master key unavailable: {reason}")]
    MasterKey { reason: String },
    #[error("{0} cannot be written from here; set the environment variable in your shell")]
    NotWritable(String),
    #[error(
        "credential must name its source: env:YOUR_VAR or keyring:<name>; secrets are never read from configuration files"
    )]
    InvalidReference,
    #[error("unknown credential scheme {0:?}; use env:VAR or keyring:NAME")]
    UnknownScheme(String),
    #[error("this credential store does not support {operation} for {entry}")]
    Unsupported {
        operation: &'static str,
        entry: String,
    },
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(name) => write!(f, "env:{name}"),
            Self::Keyring(name) => write!(f, "keyring:{name}"),
        }
    }
}

impl From<CredentialRef> for String {
    fn from(value: CredentialRef) -> Self {
        value.to_string()
    }
}

impl TryFrom<String> for CredentialRef {
    type Error = CredentialError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl FromStr for CredentialRef {
    type Err = CredentialError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().split_once(':') {
            Some(("env", name)) if !name.is_empty() => Ok(Self::Env(name.to_string())),
            Some(("keyring", name)) if !name.is_empty() => Ok(Self::Keyring(name.to_string())),
            Some((scheme, _)) => Err(CredentialError::UnknownScheme(scheme.to_string())),
            None => Err(CredentialError::InvalidReference),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub region: String,
}

pub trait CredentialStore: Send + Sync {
    fn resolve(&self, credential_ref: &str) -> Option<String>;

    fn is_available(&self, credential_ref: &CredentialRef) -> bool {
        self.resolve(&credential_ref.to_string()).is_some()
    }

    fn set_keyring(&self, entry: &str, _secret: &str) -> Result<(), CredentialError> {
        Err(CredentialError::Unsupported {
            operation: "write",
            entry: entry.to_string(),
        })
    }

    fn delete_keyring(&self, entry: &str) -> Result<(), CredentialError> {
        Err(CredentialError::Unsupported {
            operation: "delete",
            entry: entry.to_string(),
        })
    }

    fn aws(&self) -> Option<AwsCredentials> {
        None
    }
}

pub struct SystemCredentialStore;

static ENV_OVERLAY: OnceLock<RwLock<BTreeMap<String, String>>> = OnceLock::new();

pub fn set_env_overlay(map: BTreeMap<String, String>) {
    let store = ENV_OVERLAY.get_or_init(|| RwLock::new(BTreeMap::new()));
    if let Ok(mut guard) = store.write() {
        *guard = map;
    }
}

fn env_overlay_get(name: &str) -> Option<String> {
    ENV_OVERLAY
        .get()
        .and_then(|store| store.read().ok())
        .and_then(|guard| guard.get(name).cloned())
}

impl SystemCredentialStore {
    fn resolve_ref(reference: &CredentialRef) -> Result<String, CredentialError> {
        match reference {
            CredentialRef::Env(name) => std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| env_overlay_get(name))
                .ok_or_else(|| CredentialError::Missing(reference.clone())),
            CredentialRef::Keyring(name) => {
                if let Some(guard) = vault::global() {
                    return guard
                        .as_ref()
                        .and_then(|vault| vault.get(name))
                        .ok_or_else(|| CredentialError::Missing(reference.clone()));
                }
                keyring::Entry::new(KEYRING_SERVICE, name)
                    .and_then(|entry| entry.get_password())
                    .map_err(|error| match error {
                        keyring::Error::NoEntry => CredentialError::Missing(reference.clone()),
                        other => CredentialError::Keyring {
                            name: name.clone(),
                            reason: other.to_string(),
                        },
                    })
            }
        }
    }
}

impl CredentialStore for SystemCredentialStore {
    fn resolve(&self, credential_ref: &str) -> Option<String> {
        credential_ref
            .parse::<CredentialRef>()
            .ok()
            .and_then(|reference| Self::resolve_ref(&reference).ok())
    }

    fn set_keyring(&self, entry: &str, secret: &str) -> Result<(), CredentialError> {
        if let Some(guard) = vault::global() {
            match guard.as_ref() {
                Some(vault) => return vault.set(entry, secret),
                None => {}
            }
        }
        set_keyring_secret(entry, secret)
    }

    fn delete_keyring(&self, entry: &str) -> Result<(), CredentialError> {
        if let Some(guard) = vault::global() {
            match guard.as_ref() {
                Some(vault) => return vault.delete(entry),
                None => {}
            }
        }
        keyring::Entry::new(KEYRING_SERVICE, entry)
            .and_then(|entry| entry.delete_credential())
            .or_else(|error| {
                if matches!(error, keyring::Error::NoEntry) {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|error| CredentialError::Keyring {
                name: entry.to_string(),
                reason: error.to_string(),
            })
    }

    fn aws(&self) -> Option<AwsCredentials> {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        Some(AwsCredentials {
            access_key_id: var("AWS_ACCESS_KEY_ID")?,
            secret_access_key: var("AWS_SECRET_ACCESS_KEY")?,
            session_token: var("AWS_SESSION_TOKEN"),
            region: var("AWS_REGION").unwrap_or_else(|| "us-east-1".into()),
        })
    }
}

/// `keyring`'s macOS backend uses the legacy Keychain API. Updating an existing
/// generic password through that API first calls `find_generic_password`, which
/// decrypts and returns the old value before replacing it. Besides doing work we
/// do not need, that read can display an authorization dialog.
/// Security Framework's item API instead tries `SecItemAdd` and, on a duplicate,
/// calls `SecItemUpdate` with only the new value. The old secret is never read.
#[cfg(target_os = "macos")]
fn set_keyring_secret(entry: &str, secret: &str) -> Result<(), CredentialError> {
    security_framework::passwords::set_generic_password(KEYRING_SERVICE, entry, secret.as_bytes())
        .map_err(|error| CredentialError::Keyring {
            name: entry.to_string(),
            reason: error.to_string(),
        })
}

#[cfg(not(target_os = "macos"))]
fn set_keyring_secret(entry: &str, secret: &str) -> Result<(), CredentialError> {
    keyring::Entry::new(KEYRING_SERVICE, entry)
        .and_then(|entry| entry.set_password(secret))
        .map_err(|error| CredentialError::Keyring {
            name: entry.to_string(),
            reason: error.to_string(),
        })
}

pub fn keyring_entry(id: &str) -> String {
    match id {
        "exa" | "parallel" => format!("web_search:{id}"),
        other => other.to_string(),
    }
}

pub fn credential_candidates(provider_id: &str, auto_detect_env: bool) -> Vec<CredentialRef> {
    let mut candidates = Vec::new();
    if auto_detect_env {
        let mut push_env = |name: String| {
            if !candidates
                .iter()
                .any(|item: &CredentialRef| item.name() == name)
            {
                candidates.push(CredentialRef::env(name));
            }
        };
        push_env(env_key_name(provider_id, "ZLOGIC_"));
        for name in known_env_keys(provider_id) {
            push_env((*name).to_string());
        }
        push_env(env_key_name(provider_id, ""));
    }
    candidates.push(CredentialRef::keyring(keyring_entry(provider_id)));
    candidates
}

fn env_key_name(provider_id: &str, prefix: &str) -> String {
    let body: String = provider_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{prefix}{body}_API_KEY")
}

fn known_env_keys(provider_id: &str) -> &'static [&'static str] {
    match provider_id {
        "openai" => &["OPENAI_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "gemini" => &[
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GOOGLE_GENERATIVE_AI_API_KEY",
        ],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "dashscope" => &["DASHSCOPE_API_KEY"],
        "glm" => &["ZHIPUAI_API_KEY", "GLM_API_KEY"],
        "fireworks" => &["FIREWORKS_API_KEY"],
        "bedrock" => &["AWS_SECRET_ACCESS_KEY"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overlay_backs_env_references() {
        let key = "ZLOGIC_TEST_ENV_YAML_OVERLAY";
        set_env_overlay(BTreeMap::from([(key.to_string(), "from-overlay".into())]));
        assert_eq!(
            CredentialRef::env(key).resolve().unwrap(),
            "from-overlay",
            "a key in env.yaml must resolve through an env: reference"
        );

        set_env_overlay(BTreeMap::new());
        assert!(matches!(
            CredentialRef::env(key).resolve(),
            Err(CredentialError::Missing(_))
        ));
    }

    #[test]
    fn unregistered_vault_never_panics_on_keyring_write() {
        assert!(
            vault::global().is_some(),
            "touching the global lock must yield a guard"
        );
        let entry = "test-unregistered-vault-fallback";
        let result = SystemCredentialStore.set_keyring(entry, "secret");
        assert!(
            result.is_ok(),
            "unregistered vault must fall back to the keyring, not panic: {result:?}"
        );
        let _ = SystemCredentialStore.delete_keyring(entry);
    }

    #[test]
    fn references_require_an_explicit_scheme() {
        assert!("OPENAI_API_KEY".parse::<CredentialRef>().is_err());
        assert!("sk-secret".parse::<CredentialRef>().is_err());
        assert_eq!(
            "keyring:openai".parse::<CredentialRef>().unwrap(),
            CredentialRef::keyring("openai")
        );
    }

    #[test]
    fn candidates_put_environment_before_the_keyring() {
        let candidates = credential_candidates("openai", true);
        assert_eq!(candidates[0], CredentialRef::env("ZLOGIC_OPENAI_API_KEY"));
        assert!(candidates.contains(&CredentialRef::env("OPENAI_API_KEY")));
        assert_eq!(candidates.last(), Some(&CredentialRef::keyring("openai")));
    }

    #[test]
    fn disabling_environment_leaves_only_the_keyring() {
        assert_eq!(
            credential_candidates("anthropic", false),
            [CredentialRef::keyring("anthropic")]
        );
    }

    #[test]
    fn the_search_backends_have_their_own_keyring_entries() {
        assert_eq!(
            credential_candidates("exa", true),
            [
                CredentialRef::env("ZLOGIC_EXA_API_KEY"),
                CredentialRef::env("EXA_API_KEY"),
                CredentialRef::keyring("web_search:exa"),
            ]
        );
        assert_eq!(
            credential_candidates("parallel", true),
            [
                CredentialRef::env("ZLOGIC_PARALLEL_API_KEY"),
                CredentialRef::env("PARALLEL_API_KEY"),
                CredentialRef::keyring("web_search:parallel"),
            ]
        );
        assert_eq!(
            credential_candidates("exa", false),
            [CredentialRef::keyring("web_search:exa")]
        );
        assert_eq!(keyring_entry("my-gw.2"), "my-gw.2");
    }
}
