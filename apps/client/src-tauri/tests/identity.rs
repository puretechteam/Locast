//! Integration tests for the P2-T01 identity / keyring surface.
//!
//! The tests use a `MockKeyring` so the developer's real
//! keychain is never touched. The flow covered is:
//!
//! 1. First launch: `get_or_create` mints a fresh keypair,
//!    stores it in the keyring, and UPSERTs a `user_identities`
//!    row.
//! 2. Second launch with the same mock: `get_or_create`
//!    returns the same public key (persistence).
//! 3. `rotate` produces a different public key (and a new
//!    `user_identities` row).
//! 4. Display-name validation rejects empty / too long /
//!    whitespace / control characters.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;

use base64::Engine as _;
use tempfile::TempDir;

use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::storage::Storage;

async fn open_storage(lib_root: &std::path::Path) -> Storage {
    let db_path = lib_root.join("index.sqlite");
    Storage::open(&db_path).await.expect("open storage")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_first_launch_creates_keypair() {
    let tmp = TempDir::new().expect("tempdir");
    let lib_root = tmp.path().to_path_buf();
    let storage = open_storage(&lib_root).await;
    let keyring = Arc::new(MockKeyring::new());
    let service = IdentityService::with_keyring(keyring.clone(), storage.clone());

    let id1 = service.get_or_create("Alice").await.expect("first launch");
    assert_eq!(id1.display_name, "Alice");
    assert_eq!(id1.public_key.len(), 44, "base64 32 bytes = 44 chars");
    assert_eq!(id1.user_id.len(), 64, "sha256 hex = 64 chars");
    // user_id must equal sha256(public_key bytes).
    let pk_bytes = base64_decode(&id1.public_key);
    let expected = sha256_hex(pk_bytes);
    assert_eq!(id1.user_id, expected);

    // The keyring now holds the keypair.
    let loaded = keyring.load().await.expect("load").expect("present");
    {
        // The base64 we have is the public key. Recompute
        // the private key by re-loading from the keyring and
        // comparing public keys.
        let kp = loaded;
        let pk_from_kp = kp.signing.verifying_key().to_bytes();
        let pk_b64 = base64::engine::general_purpose::STANDARD.encode(pk_from_kp);
        assert_eq!(pk_b64, id1.public_key);
        let _ = kp.to_base64();
    }

    // A second call returns the same public key (persistence).
    let id2 = service.get_or_create("Alice").await.expect("second launch");
    assert_eq!(id2.public_key, id1.public_key);
    assert_eq!(id2.user_id, id1.user_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_rotate_changes_public_key() {
    let tmp = TempDir::new().expect("tempdir");
    let lib_root = tmp.path().to_path_buf();
    let storage = open_storage(&lib_root).await;
    let keyring = Arc::new(MockKeyring::new());
    let service = IdentityService::with_keyring(keyring.clone(), storage.clone());

    let id1 = service.get_or_create("Alice").await.expect("first");
    let id2 = service.rotate("Alice").await.expect("rotate");
    assert_ne!(
        id1.public_key, id2.public_key,
        "rotate must produce a different public key"
    );
    assert_ne!(id1.user_id, id2.user_id);
    // The keyring now holds the new keypair.
    let loaded = keyring.load().await.expect("load").expect("present");
    {
        let pk_b64 = base64::engine::general_purpose::STANDARD
            .encode(loaded.signing.verifying_key().to_bytes());
        assert_eq!(pk_b64, id2.public_key);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_rejects_invalid_display_names() {
    let tmp = TempDir::new().expect("tempdir");
    let lib_root = tmp.path().to_path_buf();
    let storage = open_storage(&lib_root).await;
    let keyring = Arc::new(MockKeyring::new());
    let service = IdentityService::with_keyring(keyring, storage);

    for bad in ["", " Alice", "Alice ", "Ali\u{01}ce", &"a".repeat(33)] {
        let res = service.get_or_create(bad).await;
        assert!(res.is_err(), "expected error for {bad:?}");
        let err = res.err().unwrap();
        assert!(matches!(
            err,
            locast_client_lib::identity::keystore::IdentityServiceError::InvalidDisplayName(_)
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_get_returns_not_initialized_when_empty() {
    let tmp = TempDir::new().expect("tempdir");
    let lib_root = tmp.path().to_path_buf();
    let storage = open_storage(&lib_root).await;
    let keyring = Arc::new(MockKeyring::new());
    let service = IdentityService::with_keyring(keyring, storage);

    let res = service.get("Alice").await;
    assert!(matches!(
        res,
        Err(locast_client_lib::identity::keystore::IdentityServiceError::NotInitialized)
    ));
}

/// Regression: a fresh install had no keypair until some page called
/// `identity_get`, so the first signaling handshake failed with
/// "identity not initialized". `ensure_keypair` is what app setup calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_keypair_creates_the_keypair_on_first_launch() {
    let tmp = TempDir::new().expect("tempdir");
    let storage = open_storage(tmp.path()).await;
    let keyring = Arc::new(MockKeyring::new());
    let service = IdentityService::with_keyring(keyring.clone(), storage);

    assert!(
        matches!(
            service.load_keypair().await,
            Err(locast_client_lib::identity::keystore::IdentityServiceError::NotInitialized)
        ),
        "precondition: nothing exists before first launch"
    );

    let created = service.ensure_keypair("guest").await.expect("ensure");
    let loaded = service.load_keypair().await.expect("load after ensure");
    assert_eq!(
        created.signing.verifying_key().to_bytes(),
        loaded.signing.verifying_key().to_bytes(),
        "the keypair handed back is the one stored in the keyring"
    );
    let stored = keyring.load().await.expect("load").expect("present");
    assert_eq!(
        stored.signing.verifying_key().to_bytes(),
        loaded.signing.verifying_key().to_bytes()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_keypair_is_idempotent_and_keeps_the_display_name() {
    let tmp = TempDir::new().expect("tempdir");
    let storage = open_storage(tmp.path()).await;
    let keyring = Arc::new(MockKeyring::new());
    let service = IdentityService::with_keyring(keyring, storage.clone());

    let first = service.ensure_keypair("guest").await.expect("first");
    let named = service
        .get_or_create("Alice")
        .await
        .expect("user sets a name");
    let second = service.ensure_keypair("guest").await.expect("second start");

    assert_eq!(
        first.signing.verifying_key().to_bytes(),
        second.signing.verifying_key().to_bytes(),
        "restarting must never mint a new identity"
    );
    let stored_name: String =
        sqlx::query_scalar("SELECT display_name FROM user_identities WHERE id = ?1")
            .bind(&named.user_id)
            .fetch_one(&storage.pool())
            .await
            .expect("identity row");
    assert_eq!(
        stored_name, "Alice",
        "ensure_keypair must not overwrite a name the user chose"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_keypair_rejects_an_invalid_default_name() {
    let tmp = TempDir::new().expect("tempdir");
    let storage = open_storage(tmp.path()).await;
    let service = IdentityService::with_keyring(Arc::new(MockKeyring::new()), storage);
    assert!(service.ensure_keypair("").await.is_err());
    assert!(
        matches!(
            service.load_keypair().await,
            Err(locast_client_lib::identity::keystore::IdentityServiceError::NotInitialized)
        ),
        "a rejected name must not create a keypair"
    );
}

fn sha256_hex(bytes: Vec<u8>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(&bytes);
    hex::encode(h.finalize())
}

fn base64_decode(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}
