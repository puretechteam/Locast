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
    #[error("quota exceeded: used {used} + needed {needed} > cap {cap}")]
    QuotaExceeded { used: i64, cap: i64, needed: i64 },

    /// A quota-related call (e.g. `quota_set`) was made with an
    /// invalid cap. The cap must be strictly positive; `0` and
    /// negative values are rejected.
    #[error("invalid quota cap: {value} bytes (must be > 0)")]
    InvalidCap { value: i64 },
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
