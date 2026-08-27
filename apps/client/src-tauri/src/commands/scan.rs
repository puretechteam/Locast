//! `commands::scan` - the library scanner IPC surface.
//!
//! P1-T07 introduces the `library_scan` Tauri command. It is a thin
//! wrapper around [`crate::library::scan::scan`]: it resolves the
//! library root from the storage path (the same convention as
//! `commands::import::resolve_library_root` and
//! `commands::quota::resolve_library_root`: the parent of the
//! SQLite file), calls the scanner, and maps [`ScanError`] onto
//! the locked [`AppError`] variants.
//!
//! The library root is the parent of the storage file because
//! `apps/client/src-tauri/src/lib.rs` places `index.sqlite` directly
//! under the user's library root. The Settings UI is the future
//! task that lets the user pick a different root.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::PathBuf;

use tauri::State as TauriState;
use tokio::fs as tokio_fs;

use crate::commands::error::AppError;
use crate::core::quota::QuotaAccountant;
pub use crate::library::scan::ScanResult;
use crate::library::scan::{self, ScanError};
use crate::storage::Storage;

/// Map [`ScanError`] onto the locked [`AppError`] variants.
///
/// - `ScanError::Io` -> `AppError::Read`. A scanner-side read
///   error is structurally a read error; mapping to the existing
///   `Read` variant keeps the closed [`AppError`] set intact and
///   surfaces the underlying message.
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

/// Tauri command: scan the on-disk library and reconcile it
/// against the `media_items` table.
///
/// Resolves the library root from the storage path (the parent of
/// the SQLite file, matching the convention used by
/// `commands::import::resolve_library_root` and
/// `commands::quota::resolve_library_root`) and delegates to
/// [`crate::library::scan::scan`]. The returned
/// [`ScanResult`] carries the per-category counts the React side
/// can render.
///
/// The `QuotaAccountant` is accepted as a managed state for
/// consistency with the other commands; P1-T07 does not
/// recompute quota in the scanner (the architecture's
/// 60-second background recompute is the right place for
/// that).
#[tauri::command]
#[specta::specta]
pub async fn library_scan(
    storage: TauriState<'_, Storage>,
    _accountant: TauriState<'_, QuotaAccountant>,
) -> Result<ScanResult, AppError> {
    let library_root = resolve_library_root(storage.inner()).await?;
    let result = scan::scan(storage.inner(), &library_root).await?;
    Ok(result)
}

/// Resolve the library root for the Tauri command. The library
/// root is the parent of the storage file (the SQLite file lives
/// at `<library_root>/index.sqlite`). This mirrors the convention
/// used in `commands::import::resolve_library_root` and
/// `commands::quota::resolve_library_root`.
async fn resolve_library_root(storage: &Storage) -> Result<PathBuf, AppError> {
    let data_dir = storage
        .path()
        .parent()
        .ok_or_else(|| AppError::InvalidPath {
            path: storage.path().to_string_lossy().into_owned(),
            message: "storage path has no parent".to_string(),
        })?;
    tokio_fs::create_dir_all(data_dir)
        .await
        .map_err(|e| AppError::InvalidPath {
            path: data_dir.to_string_lossy().into_owned(),
            message: e.to_string(),
        })?;
    Ok(data_dir.to_path_buf())
}
