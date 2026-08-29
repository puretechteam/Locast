//! `AppError` - the closed error set surfaced across the Locast desktop
//! client's IPC surface.
//!
//! P1-T04 introduced `AppError` inside `commands::import` because at the
//! time it was the only IPC error type. P2-T01 (`Identity keypair`)
//! is the second IPC consumer: its `KeychainUnavailable` /
//! `KeychainCorrupt` / `IdentityLocked` / `InvalidDisplayName` /
//! `IdentityNotInitialized` variants will join the import-side closed
//! set. P2-T01 also extracts the type out of `commands::import` into
//! this shared `commands::error` module so every IPC command surface
//! can depend on the same `specta::Type`-derivable error enum.
//!
//! The set is closed: each variant is an explicit case; nothing else
//! is returned. The serde shape is `#[serde(tag = "kind")]` so the TS
//! binding becomes a tagged union. `std::io::Error`, `sqlx::Error`,
//! `StorageError`, `PathError`, and `FsError` are flattened to strings
//! because they do not implement `specta::Type`; each carrier variant
//! keeps the underlying message for the logs and the webview's
//! user-friendly rendering.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use specta::Type;
use thiserror::Error;

use crate::core::library::sanitize;
use crate::core::paths::PathError;
use crate::core::quota::QuotaError;
use crate::library::fs::FsError;
use crate::library::scan::ScanError;
use crate::storage::rooms::RecentRoomsError;
use crate::storage::StorageError;

/// Errors raised by every Locast desktop client IPC command and
/// surfaced to the webview.
///
/// The set is closed: each variant is an explicit case; nothing else
/// is returned. The serde shape is `#[serde(tag = "kind")]` so the TS
/// binding becomes a tagged union. `std::io::Error`, `sqlx::Error`,
/// `StorageError`, `PathError`, and `FsError` are flattened to
/// strings because they do not implement `specta::Type`; each
/// carrier variant keeps the underlying message for the logs and the
/// webview's user-friendly rendering.
#[derive(Debug, Error, Serialize, Deserialize, Type)]
#[serde(tag = "kind")]
pub enum AppError {
    /// The source path was empty, did not exist, or was not a regular
    /// file.
    #[error("source file is missing or is not a regular file: {path}")]
    SourceMissing { path: String },

    /// The source path could not be canonicalized for any reason other
    /// than "not found" (e.g. permission denied). The underlying io
    /// error message is included for the logs.
    #[error("invalid source path {path:?}: {message}")]
    InvalidPath { path: String, message: String },

    /// The user-supplied filename failed sanitization. The underlying
    /// `InvalidFilename` from `core::library::sanitize` is unit-only
    /// (it does not carry distinguishing context), so this variant
    /// collapses to a tagged boolean.
    #[error("invalid filename")]
    InvalidFilename,

    /// A filesystem operation on the library root or staging area
    /// failed. The underlying `FsError` (which lives in the locked
    /// `library::fs` module and does not derive `Serialize`) is
    /// flattened to a string so the variant remains a
    /// `specta::Type`-derivable type.
    #[error("filesystem error: {message}")]
    Fs { message: String },

    /// A storage (SQLite) open / migrate / pool error. The
    /// underlying `StorageError` (locked) is flattened to a string.
    #[error("storage error: {message}")]
    Storage { message: String },

    /// A path-construction operation failed. The underlying
    /// `PathError` (locked) is flattened to a string.
    #[error("path error: {message}")]
    Paths { message: String },

    /// A SQLite operation failed. The underlying `sqlx::Error` is
    /// flattened to a string so the variant remains a
    /// `specta::Type`-derivable type.
    #[error("database error: {message}")]
    Database { message: String },

    /// Reading the source file failed mid-stream. The underlying io
    /// error message is flattened into a string so the variant
    /// remains a `specta::Type`-derivable type.
    #[error("failed to read source file: {message}")]
    Read { message: String },

    /// The hash could not be finalized. In practice unreachable
    /// because BLAKE3 and SHA-256 finalizers do not fail, but the
    /// variant exists so a future streaming-hash swap that does fail
    /// can surface it cleanly.
    #[error("hash finalization failed")]
    Hash,

    /// Internal-use variant. The dedup short-circuit catches duplicates
    /// before the row would be inserted, so this variant is never
    /// surfaced to the webview in P1-T04; it is kept so the type
    /// remains a closed set and a future caller can request
    /// "fail instead of dedup" semantics.
    #[allow(dead_code)]
    #[error("duplicate content; existing media_item id = {existing_id}")]
    DuplicateContent { existing_id: String },

    /// The disk-quota check refused the import. Carries the same
    /// three integers the core-layer `QuotaError::Exceeded` carries:
    /// `used` (the current occupied bytes at the moment of the
    /// check), `cap` (the current cap), and `needed` (the size of
    /// the file the caller was trying to import). The TS side can
    /// format these directly. Architecture section 6: the refusal
    /// is `used + needed > cap`, with no over-commit.
    ///
    /// Exposed to TypeScript as `number`. A 50 GiB cap fits in a
    /// JavaScript `number`; even a 1 EiB cap fits in 2^53 - 1.
    #[error("quota exceeded: used {used} + needed {needed} > cap {cap}")]
    QuotaExceeded {
        #[specta(type = specta_typescript::Number)]
        used: i64,
        #[specta(type = specta_typescript::Number)]
        cap: i64,
        #[specta(type = specta_typescript::Number)]
        needed: i64,
    },

    /// A quota-related call (e.g. `quota_set`) was made with an
    /// invalid cap. The cap must be strictly positive; `0` and
    /// negative values are rejected.
    #[error("invalid quota cap: {value} bytes (must be > 0)")]
    InvalidCap {
        #[specta(type = specta_typescript::Number)]
        value: i64,
    },

    // ------------------------------------------------------------------
    // P1-T08 (`locast://` custom protocol) variants.
    // ------------------------------------------------------------------
    /// The URL did not match any `locast://` shape the protocol
    /// recognizes. Returns 400-equivalent.
    #[error("invalid locast:// URL: {message}")]
    BadUrl { message: String },

    /// The `locast://` URL pointed at a `media_id` (or subtitle id
    /// or sha prefix) that does not exist in the local library.
    /// Returns 404-equivalent.
    #[error("not found: {message}")]
    NotFound { message: String },

    /// The HTTP `Range` header could not be parsed (malformed
    /// bytes=, unsatisfiable, or multi-range which v1 does not
    /// support). Returns 416-equivalent.
    #[error("invalid or unsatisfiable Range header: {message}")]
    BadRange { message: String },

    /// The path the protocol resolved to lives outside the
    /// library root. This should be unreachable because the
    /// protocol only resolves URLs whose id is in the DB; it is
    /// here as a defense-in-depth tripwire.
    #[error("path escapes the library root: {message}")]
    OutOfLibrary { message: String },

    /// An I/O error while serving a `locast://` request. The
    /// underlying `std::io::Error` is flattened to a string.
    #[error("locast:// io error: {message}")]
    ProtocolIo { message: String },

    // ------------------------------------------------------------------
    // P2-T01 (identity / keyring) variants.
    // ------------------------------------------------------------------
    /// The OS credential store is unavailable (no Secret Service
    /// on Linux, Credential Manager unreachable on Windows, keychain
    /// inaccessible on macOS). The user must fix the environment
    /// before the identity can be read.
    #[error("OS credential store is unavailable: {message}")]
    KeychainUnavailable { message: String },

    /// A credential exists but cannot be decoded as a base64
    /// Ed25519 private key. The user should rotate.
    #[error("stored identity is corrupt")]
    KeychainCorrupt,

    /// The OS credential store is locked (e.g. macOS login
    /// keychain is locked). The user must unlock it.
    #[error("OS credential store is locked: {message}")]
    IdentityLocked { message: String },

    /// A display name was rejected. Carries the machine-readable
    /// reason (`empty` | `too_long` | `whitespace` | `control`).
    #[error("invalid display name: {reason}")]
    InvalidDisplayName { reason: String },

    /// `identity_get` was called but no keypair has been generated
    /// yet. This should not be reachable from the webview because
    /// the UI always calls `identity_get` (which is a
    /// get-or-create); it is here for the rare `identity_get`
    /// strict mode that some test code uses.
    #[error("identity not initialized")]
    IdentityNotInitialized,

    /// Internal catch-all for unexpected errors. Not reachable
    /// from the production code paths; tests and the
    /// `AppError::other` constructor use it.
    #[error("internal error: {message}")]
    Other { message: String },
}

/// Convert a `sqlx::Error` into an `AppError::Database` so `?` works
/// uniformly on storage operations.
impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        AppError::Database {
            message: err.to_string(),
        }
    }
}

/// Convert the unit `InvalidFilename` error from the sanitizer into
/// the tagged `AppError::InvalidFilename` variant.
impl From<sanitize::InvalidFilename> for AppError {
    fn from(_: sanitize::InvalidFilename) -> Self {
        AppError::InvalidFilename
    }
}

/// Convert the locked `FsError` into the flattened `AppError::Fs`
/// variant. The locked module does not derive `Serialize`; flattening
/// to a string keeps `AppError` specta-derivable.
impl From<FsError> for AppError {
    fn from(err: FsError) -> Self {
        AppError::Fs {
            message: err.to_string(),
        }
    }
}

/// Convert the locked `StorageError` into the flattened
/// `AppError::Storage` variant.
impl From<StorageError> for AppError {
    fn from(err: StorageError) -> Self {
        AppError::Storage {
            message: err.to_string(),
        }
    }
}

/// Convert the locked `PathError` into the flattened
/// `AppError::Paths` variant.
impl From<PathError> for AppError {
    fn from(err: PathError) -> Self {
        AppError::Paths {
            message: err.to_string(),
        }
    }
}

/// Convert the core-layer `QuotaError` into the flattened
/// `AppError` variants. `QuotaError::Exceeded` is the only variant
/// that is NOT routed through `From`; `import_one` handles it
/// directly so the i64 fields are preserved.
impl From<QuotaError> for AppError {
    fn from(err: QuotaError) -> Self {
        match err {
            QuotaError::Storage(s) => AppError::Storage {
                message: s.to_string(),
            },
            QuotaError::Sqlx(s) => AppError::Database {
                message: s.to_string(),
            },
            QuotaError::Io(s) => AppError::Paths {
                message: s.to_string(),
            },
            QuotaError::InvalidCap { value } => AppError::InvalidCap { value },
            QuotaError::Exceeded { used, cap, needed } => {
                AppError::QuotaExceeded { used, cap, needed }
            }
        }
    }
}

/// Map the scanner's internal `ScanError` onto the locked
/// `AppError` variants. The mapping is:
/// - `ScanError::Io` -> `AppError::Read`. A scanner-side read
///   failure uses the same `Read` variant as a media_import
///   read failure; the on-disk scanner does not have a separate
///   "scanner I/O" class.
/// - `ScanError::Storage` -> `AppError::Storage`.
/// - `ScanError::Sqlx` -> `AppError::Database`.
/// - `ScanError::Paths` -> `AppError::Paths`.
impl From<ScanError> for AppError {
    fn from(err: ScanError) -> Self {
        match err {
            ScanError::Io(io) => AppError::Read {
                message: io.to_string(),
            },
            ScanError::Storage(s) => AppError::Storage {
                message: s.to_string(),
            },
            ScanError::Sqlx(s) => AppError::Database {
                message: s.to_string(),
            },
            ScanError::Paths(p) => AppError::Paths {
                message: p.to_string(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// P1-T08 (locast://) error mapping
// ---------------------------------------------------------------------------

/// Map the protocol module's internal `ProtocolError` onto the
/// locked `AppError` variants.
impl From<crate::library::protocol::ProtocolError> for AppError {
    fn from(err: crate::library::protocol::ProtocolError) -> Self {
        use crate::library::protocol::ProtocolError as P;
        match err {
            P::BadUrl(m) => AppError::BadUrl { message: m },
            P::NotFound(m) => AppError::NotFound { message: m },
            P::BadRange(m) => AppError::BadRange { message: m },
            P::OutOfLibrary(m) => AppError::OutOfLibrary { message: m },
            P::Io(io) => AppError::ProtocolIo {
                message: io.to_string(),
            },
            P::Storage(m) => AppError::Storage { message: m },
            P::Paths(m) => AppError::Paths { message: m },
        }
    }
}

// ---------------------------------------------------------------------------
// P2-T01 (identity) error mapping
// ---------------------------------------------------------------------------

/// Map the identity service's internal `IdentityServiceError`
/// onto the locked `AppError` variants.
impl From<crate::identity::keystore::IdentityServiceError> for AppError {
    fn from(err: crate::identity::keystore::IdentityServiceError) -> Self {
        use crate::identity::keystore::IdentityServiceError as I;
        match err {
            I::Unavailable(_) => AppError::KeychainUnavailable {
                message: err.to_string(),
            },
            I::Corrupt => AppError::KeychainCorrupt,
            I::Locked(s) => AppError::IdentityLocked { message: s },
            I::InvalidDisplayName(e) => AppError::InvalidDisplayName {
                reason: e.kind().to_string(),
            },
            I::NotInitialized => AppError::IdentityNotInitialized,
            I::Storage(m) => AppError::Storage { message: m },
            I::Other(m) => AppError::Other { message: m },
        }
    }
}

// ---------------------------------------------------------------------------
// P2-T08 (recent_rooms) error mapping
// ---------------------------------------------------------------------------

/// Map the recents repository's internal `RecentRoomsError` onto
/// the locked `AppError` variants. The only variant today is `Sqlx`,
/// which routes to `AppError::Database` (the same destination the
/// generic `From<sqlx::Error>` impl uses).
impl From<RecentRoomsError> for AppError {
    fn from(err: RecentRoomsError) -> Self {
        match err {
            RecentRoomsError::Sqlx(s) => AppError::Database {
                message: s.to_string(),
            },
        }
    }
}

/// Generic catch-all so internal `String` errors can be raised
/// without ceremony. Used sparingly; the closed set above is
/// preferred.
impl AppError {
    /// Build an `Other` variant with a message.
    pub fn other(message: impl Into<String>) -> Self {
        AppError::Other {
            message: message.into(),
        }
    }
}
