//! The plaintext store, exercised through the same `CredentialStore` surface the engine uses.
//!
//! The vault and the backend switch are process-wide, so each case lives in its own test binary —
//! within one binary they would overwrite each other's registration.

use zlogic_credential::{
    CredentialStore, EncryptedPaths, SystemCredentialStore, init_secret_vault, set_keychain_enabled,
};

/// The point of the switch: with the keychain off a secret lands in the plaintext file, no master
/// key is generated, and the OS credential store is never consulted.
#[test]
fn with_the_keychain_off_a_secret_round_trips_through_the_plaintext_file() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("security/.key.json");
    let master = tmp.path().join("security/.master");
    let blob = tmp.path().join("security/.key.enc");

    set_keychain_enabled(false);
    init_secret_vault(
        EncryptedPaths::new(master.clone(), blob.clone()),
        plain.clone(),
    );

    SystemCredentialStore
        .set_keyring("plaintext-entry", "sk-plaintext")
        .unwrap();
    assert_eq!(
        SystemCredentialStore
            .resolve("keyring:plaintext-entry")
            .as_deref(),
        Some("sk-plaintext")
    );

    let on_disk = std::fs::read_to_string(&plain).unwrap();
    assert!(
        on_disk.contains("sk-plaintext"),
        "the secret must be readable in the plaintext file, got {on_disk}"
    );
    assert!(
        !master.exists(),
        "no master key is generated when nothing is encrypted"
    );
    assert!(
        !blob.exists(),
        "the encrypted store's file must be left alone entirely"
    );

    SystemCredentialStore
        .delete_keyring("plaintext-entry")
        .unwrap();
    assert_eq!(
        SystemCredentialStore.resolve("keyring:plaintext-entry"),
        None
    );
}
