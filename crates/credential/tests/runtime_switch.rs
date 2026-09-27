//! Flipping the switch at runtime, without a restart.
//!
//! Each store keeps its own entries, so this is not a migration in either direction: turning the
//! keychain off must not carry the keychain's secrets into the plaintext file, and turning it back
//! on must find them exactly where they were.
//!
//! The switch and the vault are process-wide, so this case owns a test binary.

use zlogic_credential::{
    CredentialStore, EncryptedPaths, SystemCredentialStore, init_secret_vault, set_keychain_enabled,
};

#[test]
fn flipping_the_switch_moves_between_stores_without_migrating_either() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("security/.key.json");
    let blob = tmp.path().join("security/.key.enc");

    // Start with the keychain on. The test has no OS credential store to write to, but the vault
    // still owns the encrypted file, so this exercises the encrypted path end to end.
    set_keychain_enabled(true);
    init_secret_vault(
        EncryptedPaths::new(tmp.path().join("security/.master"), blob.clone()),
        plain.clone(),
    );
    SystemCredentialStore
        .set_keyring("shared", "sk-from-keychain")
        .unwrap();
    assert_eq!(
        SystemCredentialStore.resolve("keyring:shared").as_deref(),
        Some("sk-from-keychain")
    );

    // Off: the plaintext store is a different file, so the keychain's entry is not visible here.
    set_keychain_enabled(false);
    assert_eq!(
        SystemCredentialStore.resolve("keyring:shared"),
        None,
        "turning the keychain off must not carry its entries into the plaintext store"
    );
    SystemCredentialStore
        .set_keyring("local-only", "sk-plaintext")
        .unwrap();
    let plain_body = std::fs::read_to_string(&plain).unwrap();
    assert!(plain_body.contains("sk-plaintext"), "{plain_body}");
    assert!(
        !plain_body.contains("sk-from-keychain"),
        "the other store's secret must not have been copied over: {plain_body}"
    );

    // Back on: the keychain's entry is still there, the plaintext one is not.
    set_keychain_enabled(true);
    assert_eq!(
        SystemCredentialStore.resolve("keyring:shared").as_deref(),
        Some("sk-from-keychain"),
        "turning it back on must find the entry exactly where it was left"
    );
    assert_eq!(SystemCredentialStore.resolve("keyring:local-only"), None);

    // And back off again: the plaintext entry survived both flips.
    set_keychain_enabled(false);
    assert_eq!(
        SystemCredentialStore
            .resolve("keyring:local-only")
            .as_deref(),
        Some("sk-plaintext")
    );
}
