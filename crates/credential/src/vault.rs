use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

use crate::{CredentialError, KEYRING_SERVICE};

pub const MASTER_KEY_LEN: usize = 32;
pub const MASTER_ENV_VAR: &str = "ZLOGIC_MASTER_KEY";
pub const MASTER_ACCOUNT: &str = "master";

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

static VAULT: OnceLock<RwLock<Option<SecretVault>>> = OnceLock::new();

fn vault() -> &'static RwLock<Option<SecretVault>> {
    VAULT.get_or_init(|| RwLock::new(None))
}

pub fn init_secret_vault(master_path: PathBuf, blob_path: PathBuf) {
    *vault().write().expect("secret vault lock poisoned") =
        Some(SecretVault::new(master_path, blob_path));
}

pub fn secret_vault_initialized() -> bool {
    vault()
        .read()
        .expect("secret vault lock poisoned")
        .is_some()
}

pub(crate) fn global() -> Option<std::sync::RwLockReadGuard<'static, Option<SecretVault>>> {
    vault().read().ok()
}

// ─────────────────────────── AES-256-GCM ───────────────────────────

fn encrypt(key: &[u8; MASTER_KEY_LEN], plaintext: &[u8]) -> Result<Vec<u8>, CredentialError> {
    let key = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, key)
            .map_err(|e| CredentialError::Crypto(format!("invalid AES-256 key: {e}")))?,
    );

    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|e| CredentialError::Crypto(format!("failed to generate nonce: {e}")))?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);

    let mut in_out = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|e| CredentialError::Crypto(format!("encryption failed: {e}")))?;
    let mut out = Vec::with_capacity(NONCE_LEN + in_out.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&in_out);
    Ok(out)
}

fn decrypt(key: &[u8; MASTER_KEY_LEN], blob: &[u8]) -> Result<Vec<u8>, CredentialError> {
    if blob.len() < NONCE_LEN + TAG_LEN {
        return Err(CredentialError::Crypto(
            "blob too short to be valid ciphertext".into(),
        ));
    }
    let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
    let mut nonce_arr = [0u8; NONCE_LEN];
    nonce_arr.copy_from_slice(nonce_bytes);
    let nonce = Nonce::try_assume_unique_for_key(&nonce_arr)
        .map_err(|e| CredentialError::Crypto(format!("invalid nonce: {e}")))?;
    let key = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, key)
            .map_err(|e| CredentialError::Crypto(format!("invalid AES-256 key: {e}")))?,
    );

    let mut in_out = ct.to_vec();
    let plain = key
        .open_in_place(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| {
            CredentialError::Crypto(
                "decryption failed (master key mismatch or corrupted blob)".into(),
            )
        })?;
    Ok(plain.to_vec())
}

fn generate_master_key() -> [u8; MASTER_KEY_LEN] {
    let mut key = [0u8; MASTER_KEY_LEN];
    SystemRandom::new()
        .fill(&mut key)
        .expect("system CSPRNG unavailable; cannot generate master key");
    key
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn parse_master_hex(hex: &str) -> Option<[u8; MASTER_KEY_LEN]> {
    let s = hex.trim();
    if s.len() != MASTER_KEY_LEN * 2 {
        return None;
    }
    let mut out = [0u8; MASTER_KEY_LEN];
    for (i, pair) in s.as_bytes().chunks_exact(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

// ─────────────────────────── master key ───────────────────────────

fn load_or_create_master(master_path: &Path) -> Result<[u8; MASTER_KEY_LEN], CredentialError> {
    if let Some(hex) = std::env::var(MASTER_ENV_VAR)
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return parse_master_hex(&hex).ok_or_else(|| CredentialError::MasterKey {
            reason: format!(
                "{MASTER_ENV_VAR} is not a valid 64-char hex string (length {})",
                hex.trim().len()
            ),
        });
    }

    if let Some(key) = keychain_master() {
        return Ok(key);
    }

    if let Ok(bytes) = std::fs::read(master_path) {
        if let Ok(key) = <[u8; MASTER_KEY_LEN]>::try_from(bytes.as_slice()) {
            return Ok(key);
        }
    }

    let key = generate_master_key();
    persist_master(&key, master_path)?;
    Ok(key)
}

fn keychain_master() -> Option<[u8; MASTER_KEY_LEN]> {
    keyring::Entry::new(KEYRING_SERVICE, MASTER_ACCOUNT)
        .and_then(|entry| entry.get_password())
        .ok()
        .and_then(|hex| parse_master_hex(&hex))
}

fn persist_master(key: &[u8; MASTER_KEY_LEN], master_path: &Path) -> Result<(), CredentialError> {
    let hex = to_hex(key);
    match keyring::Entry::new(KEYRING_SERVICE, MASTER_ACCOUNT)
        .and_then(|entry| entry.set_password(&hex))
    {
        Ok(()) => Ok(()),
        Err(_) => write_master_file(master_path, key),
    }
}

fn write_master_file(
    master_path: &Path,
    key: &[u8; MASTER_KEY_LEN],
) -> Result<(), CredentialError> {
    if let Some(parent) = master_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CredentialError::Io {
            path: parent.to_path_buf(),
            reason: e.to_string(),
        })?;
    }
    write_private(master_path, key).map_err(|e| CredentialError::Io {
        path: master_path.to_path_buf(),
        reason: e.to_string(),
    })
}

// ─────────────────────────── blob ───────────────────────────

fn load_blob(
    master: &[u8; MASTER_KEY_LEN],
    blob_path: &Path,
) -> Result<BTreeMap<String, String>, CredentialError> {
    let enc = match std::fs::read(blob_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => {
            return Err(CredentialError::Io {
                path: blob_path.to_path_buf(),
                reason: e.to_string(),
            });
        }
    };
    let plain = decrypt(master, &enc)?;
    serde_json::from_slice(&plain).map_err(|e| {
        CredentialError::Crypto(format!("blob content is not a valid secret table: {e}"))
    })
}

fn save_blob(
    master: &[u8; MASTER_KEY_LEN],
    blob_path: &Path,
    keys: &BTreeMap<String, String>,
) -> Result<(), CredentialError> {
    let plain = serde_json::to_vec(keys)
        .map_err(|e| CredentialError::Crypto(format!("failed to serialize secret table: {e}")))?;
    let enc = encrypt(master, &plain)?;

    if let Some(parent) = blob_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CredentialError::Io {
            path: parent.to_path_buf(),
            reason: e.to_string(),
        })?;
    }
    let tmp = blob_path.with_extension("enc.tmp");
    write_private(&tmp, &enc).map_err(|e| CredentialError::Io {
        path: tmp.clone(),
        reason: e.to_string(),
    })?;
    if blob_path.exists() {
        std::fs::remove_file(blob_path).map_err(|e| CredentialError::Io {
            path: blob_path.to_path_buf(),
            reason: e.to_string(),
        })?;
    }
    std::fs::rename(&tmp, blob_path).map_err(|e| CredentialError::Io {
        path: blob_path.to_path_buf(),
        reason: e.to_string(),
    })?;
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

pub struct SecretVault {
    master_path: PathBuf,
    blob_path: PathBuf,
    state: RwLock<VaultState>,
}

#[derive(Default)]
struct VaultState {
    loaded: bool,
    error: Option<String>,
    keys: BTreeMap<String, String>,
}

impl SecretVault {
    pub fn new(master_path: PathBuf, blob_path: PathBuf) -> Self {
        Self {
            master_path,
            blob_path,
            state: RwLock::new(VaultState::default()),
        }
    }

    fn ensure_loaded_locked(
        state: &mut VaultState,
        master_path: &Path,
        blob_path: &Path,
    ) -> Result<(), CredentialError> {
        if state.loaded {
            return match &state.error {
                Some(reason) => Err(CredentialError::MasterKey {
                    reason: reason.clone(),
                }),
                None => Ok(()),
            };
        }
        state.loaded = true;
        match load_or_create_master(master_path).and_then(|master| load_blob(&master, blob_path)) {
            Ok(keys) => {
                state.keys = keys;
                Ok(())
            }
            Err(e) => {
                state.error = Some(e.to_string());
                Err(e)
            }
        }
    }

    fn ensure_loaded(&self) -> Result<(), CredentialError> {
        let mut state = self.state.write().expect("secret vault state poisoned");
        Self::ensure_loaded_locked(&mut state, &self.master_path, &self.blob_path)
    }

    pub fn get(&self, name: &str) -> Option<String> {
        self.ensure_loaded().ok()?;
        self.state
            .read()
            .expect("secret vault state poisoned")
            .keys
            .get(name)
            .cloned()
    }

    pub fn set(&self, name: &str, secret: &str) -> Result<(), CredentialError> {
        let mut state = self.state.write().expect("secret vault state poisoned");
        Self::ensure_loaded_locked(&mut state, &self.master_path, &self.blob_path)?;

        let mut keys = std::mem::take(&mut state.keys);
        keys.insert(name.to_string(), secret.to_string());
        let master = load_or_create_master(&self.master_path)?;
        if let Err(e) = save_blob(&master, &self.blob_path, &keys) {
            state.keys = keys;
            return Err(e);
        }
        state.keys = keys;
        state.error = None;
        Ok(())
    }

    pub fn delete(&self, name: &str) -> Result<(), CredentialError> {
        let mut state = self.state.write().expect("secret vault state poisoned");
        Self::ensure_loaded_locked(&mut state, &self.master_path, &self.blob_path)?;

        if !state.keys.contains_key(name) {
            return Ok(());
        }
        let mut keys = std::mem::take(&mut state.keys);
        keys.remove(name);
        let master = load_or_create_master(&self.master_path)?;
        if let Err(e) = save_blob(&master, &self.blob_path, &keys) {
            state.keys = keys;
            return Err(e);
        }
        state.keys = keys;
        state.error = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CredentialStore;

    fn test_key() -> [u8; MASTER_KEY_LEN] {
        let mut key = [0u8; MASTER_KEY_LEN];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        key
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = test_key();
        let plain = b"{\"openai\":\"sk-123\"}";
        let enc = encrypt(&key, plain).unwrap();
        assert!(enc.len() > plain.len());
        assert_eq!(decrypt(&key, &enc).unwrap(), plain);
    }

    #[test]
    fn tampered_blob_fails_decryption() {
        let key = test_key();
        let mut enc = encrypt(&key, b"secret").unwrap();
        *enc.last_mut().unwrap() ^= 0x01; // flip one tag byte
        assert!(decrypt(&key, &enc).is_err());
    }

    #[test]
    fn wrong_key_fails_decryption() {
        let enc = encrypt(&test_key(), b"secret").unwrap();
        let mut other = test_key();
        other[0] ^= 0xff;
        assert!(decrypt(&other, &enc).is_err());
    }

    #[test]
    fn master_hex_roundtrip() {
        let key = test_key();
        let hex = to_hex(&key);
        assert_eq!(hex.len(), 64);
        assert_eq!(parse_master_hex(&hex), Some(key));
        assert_eq!(parse_master_hex(&hex.to_uppercase()), Some(key));
        assert_eq!(parse_master_hex("zz"), None);
        assert_eq!(parse_master_hex(&hex[..62]), None); // wrong length
    }

    #[test]
    fn master_file_fallback_is_written_private() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("security/.master");
        write_master_file(&path, &test_key()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), test_key());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, ".master must be 0o600");
        }
    }

    #[test]
    fn vault_set_get_delete_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = SecretVault::new(
            tmp.path().join("security/.master"),
            tmp.path().join("security/.key.enc"),
        );

        assert_eq!(vault.get("openai"), None);

        vault.set("openai", "sk-123").unwrap();
        assert_eq!(vault.get("openai").as_deref(), Some("sk-123"));
        vault.set("openai", "sk-456").unwrap();
        assert_eq!(vault.get("openai").as_deref(), Some("sk-456"));
        vault.set("anthropic", "sk-ant").unwrap();
        assert_eq!(vault.get("anthropic").as_deref(), Some("sk-ant"));

        vault.delete("openai").unwrap();
        assert_eq!(vault.get("openai"), None);
        assert_eq!(vault.get("anthropic").as_deref(), Some("sk-ant"));
        vault.delete("openai").unwrap();
    }

    #[test]
    fn blob_is_encrypted_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let blob_path = tmp.path().join("security/.key.enc");
        let vault = SecretVault::new(tmp.path().join("security/.master"), blob_path.clone());

        vault.set("openai", "sk-super-secret").unwrap();
        let on_disk = std::fs::read(&blob_path).unwrap();
        assert!(
            !on_disk
                .windows(b"sk-super-secret".len())
                .any(|w| w == b"sk-super-secret"),
            "plaintext must never appear in the blob"
        );
        let master = load_or_create_master(&tmp.path().join("security/.master")).unwrap();
        let map = load_blob(&master, &blob_path).unwrap();
        assert_eq!(
            map.get("openai").map(String::as_str),
            Some("sk-super-secret")
        );
    }

    #[test]
    fn blob_persists_across_vault_instances() {
        let tmp = tempfile::tempdir().unwrap();
        let master_path = tmp.path().join("security/.master");
        let blob_path = tmp.path().join("security/.key.enc");

        SecretVault::new(master_path.clone(), blob_path.clone())
            .set("openai", "sk-123")
            .unwrap();
        let reopened = SecretVault::new(master_path, blob_path);
        assert_eq!(reopened.get("openai").as_deref(), Some("sk-123"));
    }

    #[test]
    fn missing_blob_reads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = SecretVault::new(
            tmp.path().join("security/.master"),
            tmp.path().join("security/.key.enc"),
        );
        assert_eq!(vault.get("anything"), None);
    }

    #[test]
    fn global_registration_routes_keyring_ops() {
        let tmp = tempfile::tempdir().unwrap();
        init_secret_vault(
            tmp.path().join("security/.master"),
            tmp.path().join("security/.key.enc"),
        );
        assert!(secret_vault_initialized());

        crate::SystemCredentialStore
            .set_keyring("test-global-vault", "v")
            .unwrap();
        assert_eq!(
            crate::SystemCredentialStore
                .resolve("keyring:test-global-vault")
                .as_deref(),
            Some("v")
        );
        crate::SystemCredentialStore
            .delete_keyring("test-global-vault")
            .unwrap();
        assert_eq!(
            crate::SystemCredentialStore.resolve("keyring:test-global-vault"),
            None
        );
    }
}
