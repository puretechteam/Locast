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
use crate::library::scan;
pub use crate::library::scan::ScanResult;
use crate::storage::Storage;

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
    // One scan at a time: two overlapping runs hash the whole library twice
    // and race their upserts. A second request is refused, not queued.
    let Some(_slot) = try_begin_scan() else {
        return Err(AppError::other("a library scan is already running"));
    };
    let library_root = resolve_library_root(storage.inner()).await?;
    let result = scan::scan(storage.inner(), &library_root).await?;
    Ok(result)
}

static SCAN_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Held for the duration of a scan; dropping it lets the next one start.
#[derive(Debug)]
pub struct ScanSlot(());

impl Drop for ScanSlot {
    fn drop(&mut self) {
        SCAN_RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Claim the single scan slot, or `None` while another scan holds it.
pub fn try_begin_scan() -> Option<ScanSlot> {
    SCAN_RUNNING
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .ok()
        .map(|_| ScanSlot(()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_scan_can_run_at_a_time() {
        let first = try_begin_scan().expect("the slot is free");
        assert!(try_begin_scan().is_none(), "a second scan must be refused");
        drop(first);
        assert!(
            try_begin_scan().is_some(),
            "the slot is free again after the scan ends"
        );
    }
}
