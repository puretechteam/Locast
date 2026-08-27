//! `commands::quota` - the disk-quota IPC surface.
//!
//! P1-T05 introduces two thin Tauri commands over the
//! [`crate::core::quota::QuotaAccountant`]:
//!
//! - `quota_get` returns the current `{ used_bytes, cap_bytes }`.
//! - `quota_set` updates the cap, refusing non-positive values.
//!
//! Both commands need the library root. P1-T05 derives the library
//! root from the storage path: the storage file is at
//! `<app_data_dir>/index.sqlite`, so the library root is the parent
//! of the storage file (which is `app_data_dir` itself). This is
//! implicit and matches the P1-T04 convention used by
//! `commands::import::resolve_library_root`; a future task adds a
//! settings-driven library root.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State as TauriState;
use tokio::fs as tokio_fs;

use crate::commands::error::AppError;
use crate::core::quota::QuotaAccountant;
use crate::storage::Storage;

/// Information returned to the webview by `quota_get`.
///
/// Both fields are signed 64-bit integers (matching the i64-typed
/// `media_items.size_bytes` and the cap in `settings.value`). The TS
/// surface is `{ used_bytes: number, cap_bytes: number }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct QuotaInfo {
    /// Used bytes in the library. The architecture's cap is 50
    /// GiB by default; even a maliciously-set 1 EiB cap fits in
    /// a JavaScript `number` (2^53 - 1 ≈ 9 PiB). The TS surface
    /// is `number`; the precision trade-off is documented and
    /// acceptable.
    #[specta(type = specta_typescript::Number)]
    pub used_bytes: i64,
    /// Storage cap in bytes. See the `used_bytes` comment.
    #[specta(type = specta_typescript::Number)]
    pub cap_bytes: i64,
}

/// Resolve the library root for the Tauri command. The library root
/// is the parent of the storage file (the SQLite file lives at
/// `<library_root>/index.sqlite`). This mirrors the convention used
/// in `commands::import::resolve_library_root`.
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

/// Tauri command: return the current `{ used_bytes, cap_bytes }`.
///
/// Computes `used` via `compute_used_bytes` against the library root
/// resolved from the storage path. The cap is read from the
/// `library.quota_bytes` settings row (default 50 GiB if absent or
/// unparseable).
#[tauri::command]
#[specta::specta]
pub async fn quota_get(
    storage: TauriState<'_, Storage>,
    accountant: TauriState<'_, QuotaAccountant>,
) -> Result<QuotaInfo, AppError> {
    let library_root = resolve_library_root(storage.inner()).await?;
    let cap = accountant.cap_bytes().await?;
    let used = accountant.compute_used_bytes(&library_root).await?;
    Ok(QuotaInfo {
        used_bytes: used,
        cap_bytes: cap,
    })
}

/// Tauri command: set the disk-quota cap, in bytes.
///
/// Rejects non-positive values with `AppError::InvalidCap`. The
/// settings row `library.quota_bytes` is UPSERTed.
#[tauri::command]
#[specta::specta]
pub async fn quota_set(
    new_cap_bytes: i64,
    accountant: TauriState<'_, QuotaAccountant>,
) -> Result<(), AppError> {
    accountant.set_cap_bytes(new_cap_bytes).await?;
    Ok(())
}

// Re-export the core-layer constants so a future task that wants
// them has a single import path.
pub use crate::core::quota::{
    DEFAULT_QUOTA_BYTES as DEFAULT_QUOTA, QUOTA_SETTING_KEY as SETTING_KEY,
};
