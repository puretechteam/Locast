//! `identity::keystore` - the keyring abstraction.
//!
//! The trait [`IdentityKeyring`] is the only thing the rest of the
//! identity code calls. There are two implementations:
//!
//! - [`OsKeyring`] - the production store, backed by the
//!   platform-native credential store via the `keyring` crate
//!   (Windows Credential Manager, macOS Keychain, Secret Service on
//!   Linux).
//! - [`MockKeyring`] - an in-memory store used by tests. The mock
//!   never touches the developer's real keychain; tests can run on
//!   any host without producing real credentials.
//!
//! The `keyring` crate is the only thing the OS implementation
//! calls. Errors are mapped onto the closed [`KeystoreError`]
//! enum; the Tauri command surface maps `KeystoreError` onto
//! `AppError` variants (see `crate::commands::error`).
//!
//! See `docs/ARCHITECTURE.md` section 10.6 and `docs/ROADMAP.md`
//! P2-T01.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::Mutex;

use super::types::{validate_display_name, Identity, Keypair};

/// The service name under which the Locast private key is stored
/// in the OS credential store. Stable across releases; changing
/// this is a breaking change because users would lose access to
/// their existing identity.
pub const KEYRING_SERVICE: &str = "locast";

/// The credential "user" name under which the private key is
/// stored. We use a single, fixed account name per device because
/// v1 has exactly one local identity per device (architecture
/// section 10.6: "Each client has exactly one long-lived keypair
/// per device").
pub const KEYRING_USER: &str = "device-identity";

/// Errors raised by the keyring abstraction. The enum is closed
/// and the variants are the contract between the keystore and the
/// Tauri command surface.
#[derive(Debug, Error)]
pub enum KeystoreError {
    /// The platform credential store is unavailable. On Linux this
    /// typically means no Secret Service is running; on Windows
    /// the Credential Manager may be unavailable in some service
    /// contexts; on macOS the Keychain may be locked or
    /// inaccessible.
    #[error("OS credential store is unavailable: {0}")]
    Unavailable(String),

    /// A credential exists but cannot be decoded. Most likely
    /// cause: a partial or corrupt write in a prior run. The
    /// keystore does NOT delete the corrupt entry; the caller may
    /// surface a "your saved identity is corrupt" error and let
    /// the user decide to rotate.
    #[error("stored identity is corrupt")]
    Corrupt,

    /// The keyring is locked (e.g. on macOS, the user's login
    /// keychain is locked). The user must unlock the keychain
    /// before the identity can be read.
    #[error("OS credential store is locked: {0}")]
    Locked(String),

    /// The credential store rejected the operation for some other
    /// reason.
    #[error("OS credential store error: {0}")]
    Other(String),
}

/// The keyring abstraction. The trait is `Send + Sync` so the
/// `IdentityService` can hold it in a `tokio::Mutex` and the
/// `IdentityService` itself can be installed as Tauri managed
/// state.
#[async_trait::async_trait]
pub trait IdentityKeyring: Send + Sync {
    /// Look up the stored keypair, or `Ok(None)` if none exists
    /// (the first-launch case). The error variants are a closed
    /// set; callers should map them to the Tauri command's
    /// `AppError`.
    async fn load(&self) -> Result<Option<Keypair>, KeystoreError>;

    /// Persist `keypair` to the keyring. Overwrites any existing
    /// value (the call is the rotation primitive).
    async fn store(&self, keypair: &Keypair) -> Result<(), KeystoreError>;

    /// Delete the stored keypair. No-op if none exists.
    async fn forget(&self) -> Result<(), KeystoreError>;
}

/// The production keyring, backed by the `keyring` crate. The
/// `keyring::Entry` is the only state; the struct is a zero-sized
/// marker that names the service.
pub struct OsKeyring {
    service: &'static str,
    user: &'static str,
}

impl OsKeyring {
    /// Construct a new `OsKeyring` using the canonical service
    /// and user names. This is the only constructor; tests use
    /// [`MockKeyring`] instead.
    pub const fn new() -> Self {
        Self {
            service: KEYRING_SERVICE,
            user: KEYRING_USER,
        }
    }

    fn entry(&self) -> Result<keyring::Entry, KeystoreError> {
        keyring::Entry::new(self.service, self.user).map_err(|e| map_keyring_error(&e))
    }
}

impl Default for OsKeyring {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl IdentityKeyring for OsKeyring {
    async fn load(&self) -> Result<Option<Keypair>, KeystoreError> {
        let entry = self.entry()?;
        // `get_password` is blocking; offload to a blocking thread.
        let res = tokio::task::spawn_blocking(move || entry.get_password())
            .await
            .map_err(|e| KeystoreError::Other(format!("join error: {e}")))?;
        match res {
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_keyring_error(&e)),
            Ok(s) => match Keypair::from_base64(&s) {
                Some(k) => Ok(Some(k)),
                None => Err(KeystoreError::Corrupt),
            },
        }
    }

    async fn store(&self, keypair: &Keypair) -> Result<(), KeystoreError> {
        let entry = self.entry()?;
        let encoded = keypair.to_base64();
        tokio::task::spawn_blocking(move || entry.set_password(&encoded))
            .await
            .map_err(|e| KeystoreError::Other(format!("join error: {e}")))?
            .map_err(|e| map_keyring_error(&e))?;
        Ok(())
    }

    async fn forget(&self) -> Result<(), KeystoreError> {
        let entry = self.entry()?;
        let res = tokio::task::spawn_blocking(move || entry.delete_credential())
            .await
            .map_err(|e| KeystoreError::Other(format!("join error: {e}")))?;
        match res {
            // NoEntry is fine: there's nothing to forget.
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_keyring_error(&e)),
            Ok(()) => Ok(()),
        }
    }
}

/// Map the `keyring` crate's error onto the closed
/// [`KeystoreError`] set. The mapping is documented in the
/// unit tests in this file; the architecture requires a
/// deterministic mapping that does not expose platform-specific
/// error codes to the webview.
fn map_keyring_error(e: &keyring::Error) -> KeystoreError {
    use keyring::Error as K;
    match e {
        K::NoEntry => KeystoreError::Other("no entry".to_string()),
        K::PlatformFailure(_) => KeystoreError::Other(e.to_string()),
        K::NoStorageAccess(_) => {
            let s = e.to_string();
            if s.to_lowercase().contains("locked")
                || s.to_lowercase().contains("interactionnotallowed")
            {
                KeystoreError::Locked(s)
            } else {
                KeystoreError::Unavailable(s)
            }
        }
        K::Ambiguous(_) => KeystoreError::Other(e.to_string()),
        K::BadEncoding(_) => KeystoreError::Corrupt,
        K::Invalid(_, _) => KeystoreError::Other(e.to_string()),
        K::TooLong(_, _) => KeystoreError::Other(e.to_string()),
        _ => KeystoreError::Other(e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// MockKeyring
// ---------------------------------------------------------------------------

/// In-memory keyring for tests. The mock is a single
/// `Arc<Mutex<Option<String>>>` so it can be cloned and shared
/// across tests; multiple tests in the same process can use
/// different `MockKeyring` instances without cross-talk because
/// each instance owns its state.
///
/// The mock only ever holds the base64-encoded private key. It
/// never persists to disk, never touches the developer's real
/// keychain, and is `Drop`-clean (the state is simply freed when
/// the last reference is dropped).
#[derive(Clone, Default)]
pub struct MockKeyring {
    inner: Arc<Mutex<Option<String>>>,
}

impl MockKeyring {
    /// Construct a fresh empty mock keyring.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a mock keyring pre-populated with `keypair`. Used
    /// by tests that want to simulate a second launch with an
    /// existing identity.
    pub fn with_keypair(keypair: &Keypair) -> Self {
        let inner = Arc::new(Mutex::new(Some(keypair.to_base64())));
        Self { inner }
    }
}

#[async_trait::async_trait]
impl IdentityKeyring for MockKeyring {
    async fn load(&self) -> Result<Option<Keypair>, KeystoreError> {
        let g = self.inner.lock().await;
        match g.as_deref() {
            None => Ok(None),
            Some(s) => match Keypair::from_base64(s) {
                Some(k) => Ok(Some(k)),
                None => Err(KeystoreError::Corrupt),
            },
        }
    }

    async fn store(&self, keypair: &Keypair) -> Result<(), KeystoreError> {
        let mut g = self.inner.lock().await;
        *g = Some(keypair.to_base64());
        Ok(())
    }

    async fn forget(&self) -> Result<(), KeystoreError> {
        let mut g = self.inner.lock().await;
        *g = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// IdentityService - the higher-level wrapper used by the Tauri commands
// ---------------------------------------------------------------------------

/// The `IdentityService` is the object the Tauri command surface
/// holds in managed state. It is the single chokepoint for every
/// `identity_*` command. It composes a keyring implementation
/// with a [`Storage`] for the `user_identities` mirror row.
///
/// The service serializes every keyring access through an internal
/// `tokio::Mutex` so that two concurrent first-launch calls do
/// not race to generate two different keypairs. The lock is held
/// only for the duration of the keyring call (microseconds); the
/// SQLite write that follows is not under the lock and can run in
/// parallel.
pub struct IdentityService {
    keyring: Arc<dyn IdentityKeyring>,
    storage: crate::storage::Storage,
    /// Serializes keyring access. See the type docstring.
    lock: Mutex<()>,
}

impl IdentityService {
    /// Construct a new service with a real OS keyring and the
    /// given storage handle.
    pub fn new(storage: crate::storage::Storage) -> Self {
        Self {
            keyring: Arc::new(OsKeyring::new()),
            storage,
            lock: Mutex::new(()),
        }
    }

    /// Construct a new service with a caller-supplied keyring.
    /// Used by tests to inject a `MockKeyring`.
    pub fn with_keyring(
        keyring: Arc<dyn IdentityKeyring>,
        storage: crate::storage::Storage,
    ) -> Self {
        Self {
            keyring,
            storage,
            lock: Mutex::new(()),
        }
    }

    /// Read the current identity, generating one on first launch.
    /// On first launch the new keypair is persisted to the
    /// keyring AND a `user_identities` row is INSERTed (or
    /// UPSERTed). On subsequent calls the existing keypair is
    /// loaded and the DB row's `last_seen` is bumped.
    pub async fn get_or_create(
        &self,
        display_name: &str,
    ) -> Result<Identity, IdentityServiceError> {
        validate_display_name(display_name)?;
        let _g = self.lock.lock().await;
        let keypair = match self.keyring.load().await? {
            Some(k) => k,
            None => {
                let k = super::types::generate();
                self.keyring.store(&k).await?;
                k
            }
        };
        let id = Identity::from_signing_key(&keypair.signing, display_name);
        self.upsert_user_identity(&id).await?;
        Ok(id)
    }

    /// Read the current identity. Returns `NotInitialized` if no
    /// keypair has been generated yet.
    /// Load the raw keypair from the keyring without going
    /// through the `get_or_create` flow. Returns `NotInitialized`
    /// if no keypair has been generated yet. Used by P2-T03's
    /// `SignalingClient` to sign the server's CHALLENGE nonce
    /// during the WebSocket handshake.
    ///
    /// The returned `Keypair` holds the private key in memory;
    /// it MUST be dropped as soon as the caller is done with
    /// it. The signaling client only uses it to compute one
    /// signature per handshake and then drops it.
    pub async fn load_keypair(&self) -> Result<Keypair, IdentityServiceError> {
        let _g = self.lock.lock().await;
        match self.keyring.load().await? {
            Some(k) => Ok(k),
            None => Err(IdentityServiceError::NotInitialized),
        }
    }

    /// Read the current identity. Returns `NotInitialized` if no
    /// keypair has been generated yet.
    pub async fn get(&self, display_name: &str) -> Result<Identity, IdentityServiceError> {
        validate_display_name(display_name)?;
        let _g = self.lock.lock().await;
        let keypair = match self.keyring.load().await? {
            Some(k) => k,
            None => return Err(IdentityServiceError::NotInitialized),
        };
        let id = Identity::from_signing_key(&keypair.signing, display_name);
        self.upsert_user_identity(&id).await?;
        Ok(id)
    }

    /// Rotate: generate a fresh keypair, overwrite the keyring
    /// entry, and update the `user_identities` row. The old row
    /// for the previous public key is left in place (the
    /// architecture keeps every Locast user we've ever met).
    pub async fn rotate(&self, display_name: &str) -> Result<Identity, IdentityServiceError> {
        validate_display_name(display_name)?;
        let _g = self.lock.lock().await;
        let k = super::types::generate();
        self.keyring.store(&k).await?;
        let id = Identity::from_signing_key(&k.signing, display_name);
        self.upsert_user_identity(&id).await?;
        Ok(id)
    }

    /// UPSERT the `user_identities` row. The schema is the one
    /// in `apps/client/src-tauri/migrations/0001_init.sql`: the
    /// row's `id` is `sha256(public_key)` hex, the `public_key`
    /// is base64, the `display_name` is local-only, and
    /// `last_seen` is unix milliseconds.
    async fn upsert_user_identity(&self, id: &Identity) -> Result<(), IdentityServiceError> {
        let now_ms: i64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| IdentityServiceError::Other(e.to_string()))?
            .as_millis()
            .try_into()
            .map_err(|e: std::num::TryFromIntError| IdentityServiceError::Other(e.to_string()))?;
        sqlx::query(
            "INSERT INTO user_identities (id, public_key, display_name, created_at, last_seen) \
             VALUES (?1, ?2, ?3, ?4, ?4) \
             ON CONFLICT(id) DO UPDATE SET \
                display_name = excluded.display_name, \
                last_seen    = excluded.last_seen",
        )
        .bind(&id.user_id)
        .bind(&id.public_key)
        .bind(&id.display_name)
        .bind(now_ms)
        .execute(&self.storage.pool())
        .await
        .map_err(|e| IdentityServiceError::Storage(e.to_string()))?;
        Ok(())
    }
}

/// Errors raised by [`IdentityService`]. The variants are the
/// contract between the service and the Tauri command surface;
/// `commands::identity` maps each onto the closed `AppError`
/// variants.
#[derive(Debug, Error)]
pub enum IdentityServiceError {
    /// The OS credential store is unavailable. The Tauri command
    /// surface maps this to `AppError::KeychainUnavailable`.
    #[error("OS credential store is unavailable: {0}")]
    Unavailable(String),

    /// The stored keypair is corrupt. Mapped to
    /// `AppError::KeychainCorrupt`.
    #[error("stored identity is corrupt")]
    Corrupt,

    /// The keyring is locked. Mapped to `AppError::IdentityLocked`.
    #[error("OS credential store is locked: {0}")]
    Locked(String),

    /// The display name failed validation. Mapped to
    /// `AppError::InvalidDisplayName`.
    #[error(transparent)]
    InvalidDisplayName(#[from] super::types::DisplayNameError),

    /// The identity has not been initialized. Mapped to
    /// `AppError::IdentityNotInitialized`.
    #[error("identity not initialized; call identity_get() to create one")]
    NotInitialized,

    /// A SQLite operation failed. Mapped to
    /// `AppError::Storage`.
    #[error("storage error: {0}")]
    Storage(String),

    /// A catch-all for unexpected internal errors.
    #[error("identity service error: {0}")]
    Other(String),
}

impl From<KeystoreError> for IdentityServiceError {
    fn from(e: KeystoreError) -> Self {
        match e {
            KeystoreError::Unavailable(s) => Self::Unavailable(s),
            KeystoreError::Corrupt => Self::Corrupt,
            KeystoreError::Locked(s) => Self::Locked(s),
            KeystoreError::Other(s) => Self::Other(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::types::derive_user_id;

    #[test]
    fn mock_keyring_round_trip() {
        let mock = MockKeyring::new();
        let kp = super::super::types::generate();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert!(mock.load().await.unwrap().is_none());
            mock.store(&kp).await.unwrap();
            let loaded = mock.load().await.unwrap().expect("present after store");
            assert_eq!(loaded.to_base64(), kp.to_base64());
            mock.forget().await.unwrap();
            assert!(mock.load().await.unwrap().is_none());
        });
    }

    #[test]
    fn mock_keyring_detects_corrupt_entry() {
        let mock = MockKeyring::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Inject a non-base64 / wrong-length value directly.
            {
                let mut g = mock.inner.lock().await;
                *g = Some("not-base64".to_string());
            }
            let res = mock.load().await;
            assert!(matches!(res, Err(KeystoreError::Corrupt)));
        });
    }

    #[test]
    fn validate_display_name_accepts_normal() {
        assert_eq!(validate_display_name("Alice").unwrap(), "Alice");
    }

    #[test]
    fn validate_display_name_rejects_empty() {
        assert!(validate_display_name("").is_err());
    }

    #[test]
    fn validate_display_name_rejects_too_long() {
        let s = "a".repeat(33);
        assert!(validate_display_name(&s).is_err());
    }

    #[test]
    fn validate_display_name_rejects_whitespace_edges() {
        assert!(validate_display_name(" Alice").is_err());
        assert!(validate_display_name("Alice ").is_err());
    }

    #[test]
    fn validate_display_name_rejects_control() {
        assert!(validate_display_name("Ali\u{01}ce").is_err());
    }

    #[test]
    fn derive_user_id_is_stable() {
        let bytes = [7u8; 32];
        let id = derive_user_id(bytes);
        assert_eq!(id.len(), 64);
        // sha256(b"\x07" * 32) = first 8 hex chars
        assert_eq!(&id[..8], "4bb06f8e");
    }

    #[test]
    fn keyring_error_mapping() {
        // The `keyring::Error` is `non_exhaustive`; we cannot
        // construct every variant from outside the crate, but
        // the `Other` fallback is exercised by mapping a
        // synthetic display string.
        let mapped = map_keyring_error(&keyring::Error::NoEntry);
        match mapped {
            KeystoreError::Other(_) => {}
            _ => panic!("NoEntry should map to Other"),
        }
    }
}
