//! With the two stores in separate files, neither can break the other.
//!
//! The encrypted store's file is left exactly as it was when the keychain is off, so an unreadable
//! or encrypted blob in it is not this store's problem — and a name this store has never held is
//! simply "not set", never an error the caller has to handle.

use zlogic_credential::{
    CredentialStore, EncryptedPaths, SystemCredentialStore, init_secret_vault, set_keychain_enabled,
};

#[test]
fn an_unreadable_encrypted_file_does_not_disturb_the_plaintext_store() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("security/.key.json");
    let master = tmp.path().join("security/.master");
    let blob = tmp.path().join("security/.key.enc");

    // Ciphertext from a run that had the keychain on, plus a master file that does not match it.
    std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
    std::fs::write(&blob, b"\x00\x01\x02not json at all\xff").unwrap();
    std::fs::write(&master, b"not a 32-byte key").unwrap();

    set_keychain_enabled(false);
    init_secret_vault(
        EncryptedPaths::new(master.clone(), blob.clone()),
        plain.clone(),
    );

    assert_eq!(
        SystemCredentialStore.resolve("keyring:anything"),
        None,
        "an unknown name is not set, not an error"
    );

    SystemCredentialStore
        .set_keyring("fresh", "sk-fresh")
        .expect("the plaintext store has its own file and must not care what is in the other");
    assert_eq!(
        SystemCredentialStore.resolve("keyring:fresh").as_deref(),
        Some("sk-fresh")
    );

    let untouched = std::fs::read(&blob).unwrap();
    assert_eq!(
        untouched, b"\x00\x01\x02not json at all\xff",
        "the encrypted store's file must not be rewritten by the plaintext store"
    );
    assert_eq!(
        std::fs::read(&master).unwrap(),
        b"not a 32-byte key",
        "and its master file must not be replaced either"
    );
}
