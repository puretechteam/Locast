//! Disk-quota engine.
//!
//! P1-T05 owns the per-library-root quota: read / write the
//! `library.quota_bytes` setting, compute the current occupied
//! bytes (sum of `media_items.size_bytes` plus on-disk bytes under
//! `tmp/incomplete/` and `tmp/staging/`), and serialize the
//! `import_one` critical section behind a per-library-root mutex so
//! that two truly-concurrent imports against the same library cannot
//! each independently pass the quota check and then together exceed
//! the cap.
//!
//! # Architecture
//!
//! The architecture (section 6) is explicit:
//!
//! - **Settings key.** `library.quota_bytes`, default 50 GiB
//!   (= `50 * 1024 * 1024 * 1024`).
//! - **Counted size.** `SUM(size_bytes)` for all `media_items` rows
//!   (regardless of `status`) plus on-disk bytes of files under
//!   `<library_root>/tmp/incomplete/<download-id>/...` and
//!   `<library_root>/tmp/staging/<download-id>/...`.
//! - **Refusal.** `used + needed > cap` is refused. There is no
//!   over-commit. The refusal is reported as
//!   `QuotaError::Exceeded { used, cap, needed }`; the command layer
//!   converts this to `AppError::QuotaExceeded`.
//! - **Adjustable.** The cap is read at every check; raising the cap
//!   is immediate. We do not preemptively abort in-flight transfers
//!   when the cap is lowered.
//!
//! # Concurrency
//!
//! The per-library-root critical section (quota check -> dedup SELECT
//! -> on-miss copy -> rename -> INSERT) is serialized by a
//! `tokio::sync::Mutex<()>`. The mutex is held only across the
//! `import_one` body (or rather, the section that touches the
//! `media_items` table or the staging layout); the Tauri command's
//! outer loop is not inside the lock. Two `media_import` calls for the
//! same library still serialize correctly because each `import_one`
//! competes for the same per-library-root mutex.
//!
//! The mutex registry is a `std::sync::Mutex<HashMap<PathBuf, Arc<...>>>`
//! that is locked only briefly to look up or insert the per-library
//! `tokio::sync::Mutex<()>`; the actual critical section is held on
//! the per-library `tokio` mutex, not on the registry. The path key
//! in the registry is the **canonicalized** form of the library root,
//! so two callers passing equivalent paths (symlinks, relative vs
//! absolute, `.` segments) lock the same mutex.
//!
//! # What's not here
//!
//! - The 60-second background recompute and the startup recompute are
//!   out of scope for P1-T05. The on-import recompute inside the
//!   `import_one` critical section is the load-bearing one.
//! - The per-room outbound cap (`transfer.per_room_outbound_bytes_per_sec`)
//!   is P2 (download scheduler).
//! - Downloads themselves are P2; the quota check is invoked from
//!   `import_one` for now.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use thiserror::Error;
use tokio::fs as tokio_fs;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::library::fs as library_fs;
use crate::storage::settings::{self, SettingsError};
use crate::storage::Storage;

/// Default disk quota: 50 GiB. Architecture section 6.
pub const DEFAULT_QUOTA_BYTES: i64 = 50 * 1024 * 1024 * 1024;

/// Settings key for the user's cap on total library bytes.
pub const QUOTA_SETTING_KEY: &str = "library.quota_bytes";

/// Result of `QuotaAccountant::check_allow` when the import fits.
///
/// The refusal is communicated by `Err(QuotaError::Exceeded)`, not by
/// a `Deny` variant, so callers do not need to dispatch on the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaCheck {
    /// The import fits. `used` is the current occupied bytes at the
    /// moment of the check; `cap` is the current cap.
    Allow { used: i64, cap: i64 },
}

impl QuotaCheck {
    pub fn used(&self) -> i64 {
        match self {
            QuotaCheck::Allow { used, .. } => *used,
        }
    }

    pub fn cap(&self) -> i64 {
        match self {
            QuotaCheck::Allow { cap, .. } => *cap,
        }
    }
}

/// Errors raised by `QuotaAccountant`.
#[derive(Debug, Error)]
pub enum QuotaError {
    /// A storage / settings read or write failed.
    #[error("settings error: {0}")]
    Storage(#[from] SettingsError),

    /// A SQLite query against `media_items` failed.
    #[error("quota sql error: {0}")]
    Sqlx(#[from] sqlx::Error),

    /// A filesystem walk under `tmp/incomplete/` or `tmp/staging/`
    /// failed. Also used when the library root cannot be canonicalized
    /// at lock-acquisition time.
    #[error("quota io error: {0}")]
    Io(#[from] std::io::Error),

    /// `set_cap_bytes` was called with a non-positive value.
    #[error("invalid cap: {value} bytes (must be > 0)")]
    InvalidCap { value: i64 },

    /// The quota check refused the requested import. Carries the
    /// `used` and `cap` at the moment of the check and the `needed`
    /// amount the caller asked for. Surfaced to the IPC layer as
    /// `AppError::QuotaExceeded`.
    #[error("quota exceeded: used {used} + needed {needed} > cap {cap}")]
    Exceeded { used: i64, cap: i64, needed: i64 },
}

/// Guard returned by `QuotaAccountant::lock_for_library`.
///
/// The guard holds the per-library-root `tokio::sync::Mutex<()>`. The
/// mutex is released when the guard is dropped (or explicitly released
/// via `release`). While the guard is alive, the caller is the sole
/// holder of the per-library critical section.
pub struct QuotaLockGuard {
    inner: Option<OwnedMutexGuard<()>>,
}

impl QuotaLockGuard {
    fn new(inner: OwnedMutexGuard<()>) -> Self {
        Self { inner: Some(inner) }
    }

    /// Explicitly release the lock. The guard can be dropped silently
    /// after this; the destructor will not attempt to release a guard
    /// that was already released.
    pub fn release(mut self) {
        if let Some(g) = self.inner.take() {
            drop(g);
        }
    }
}

impl std::fmt::Debug for QuotaLockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaLockGuard")
            .field("held", &self.inner.is_some())
            .finish()
    }
}

impl Drop for QuotaLockGuard {
    fn drop(&mut self) {
        // `OwnedMutexGuard::Drop` releases the lock. If we already
        // called `release`, the inner is `None` and there is nothing
        // to do.
        drop(self.inner.take());
    }
}

/// Registry of per-library-root mutexes. Locked only briefly to look
/// up or insert the per-library `Arc<AsyncMutex<()>>`.
///
/// This is a process-wide singleton (`OnceLock`) so that two
/// `QuotaAccountant` instances built against the same storage (or
/// against two different storages that point at the same library
/// root) share the same per-library mutex. Without a process-wide
/// registry, each accountant would maintain its own map and the
/// "two accountants on the same root share the lock" guarantee
/// would silently fail.
type Registry = StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>;

fn global_registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// The disk-quota engine. One instance per Tauri app; managed state.
///
/// The accountant is a thin handle over the storage and the
/// process-wide mutex registry. Cloning the accountant is cheap.
#[derive(Clone)]
pub struct QuotaAccountant {
    storage: Storage,
}

impl QuotaAccountant {
    /// Build a new `QuotaAccountant` over the given storage. The
    /// storage handle is cloned (the `Storage` type is itself a
    /// cheap-to-clone handle over a `sqlx::SqlitePool`).
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }

    /// Return the current cap in bytes.
    ///
    /// Reads the `library.quota_bytes` settings row. If the key is
    /// absent or the stored value does not parse as a JSON number,
    /// returns `DEFAULT_QUOTA_BYTES`. A parse failure is not
    /// surfaced as an error: the spec calls for a default, and a
    /// single typo in the settings row should not break the app.
    pub async fn cap_bytes(&self) -> Result<i64, QuotaError> {
        match settings::get_raw(&self.storage, QUOTA_SETTING_KEY).await? {
            None => Ok(DEFAULT_QUOTA_BYTES),
            Some(s) => {
                let parsed: Option<i64> = serde_json::from_str(&s).ok();
                Ok(parsed.unwrap_or(DEFAULT_QUOTA_BYTES))
            }
        }
    }

    /// Set the cap, in bytes. UPSERTs into the `settings` table.
    ///
    /// The cap must be strictly positive. A value of `0` or any
    /// negative value (including `i64::MIN`) is rejected with
    /// `QuotaError::InvalidCap`. The cap is not bounded above
    /// (the architecture mentions a 4 TiB hard cap in the Settings UI,
    /// but that constraint lives at the UI / validation layer, not
    /// here).
    pub async fn set_cap_bytes(&self, new_cap: i64) -> Result<(), QuotaError> {
        if new_cap <= 0 {
            return Err(QuotaError::InvalidCap { value: new_cap });
        }
        let value = serde_json::to_string(&new_cap)
            .map_err(|e| QuotaError::Storage(SettingsError::Json(e)))?;
        settings::set_raw(&self.storage, QUOTA_SETTING_KEY, &value).await?;
        Ok(())
    }

    /// Compute the current occupied bytes under `library_root`.
    ///
    /// The total is `SUM(size_bytes) FROM media_items` (all rows,
    /// regardless of `status`) plus the on-disk size of every regular
    /// file under `<library_root>/tmp/incomplete/<download-id>/...`
    /// and `<library_root>/tmp/staging/<download-id>/...`. The walk is
    /// contained under `library_root`: any entry whose canonical form
    /// does not start with the canonical library root is skipped.
    /// This is the same `canonicalize + starts_with` pattern that
    /// P1-T02's `library::fs::assert_within` uses; for P1-T05, since
    /// these are our own subdirectories, the walk simply skips
    /// entries that fail the containment check rather than failing
    /// the whole call.
    pub async fn compute_used_bytes(&self, library_root: &Path) -> Result<i64, QuotaError> {
        let canonical_root = tokio_fs::canonicalize(library_root).await?;

        let db_total = self.sum_media_items_size().await?;
        let disk_total = walk_tmp_bytes(&canonical_root).await?;
        // `db_total` is `i64` (from `SUM(size_bytes)`); `disk_total` is
        // also `i64` (saturating within `walk_tmp_bytes`). Use
        // `saturating_add` so an adversarial / corrupted `media_items`
        // row with `size_bytes = i64::MAX` cannot overflow the sum
        // and silently wrap to a small (passing) value.
        Ok(db_total.saturating_add(disk_total))
    }

    /// `SELECT COALESCE(SUM(size_bytes), 0) FROM media_items`.
    async fn sum_media_items_size(&self) -> Result<i64, QuotaError> {
        let row: (Option<i64>,) =
            sqlx::query_as("SELECT COALESCE(SUM(size_bytes), 0) FROM media_items")
                .fetch_one(&self.storage.pool())
                .await?;
        Ok(row.0.unwrap_or(0))
    }

    /// Check whether importing `needed_bytes` would fit.
    ///
    /// On success, returns `QuotaCheck::Allow { used, cap }`. On
    /// refusal, returns `Err(QuotaError::Exceeded { used, cap,
    /// needed })` with the values computed at the moment of the
    /// check. The cap is read fresh; the used total is read fresh.
    pub async fn check_allow(
        &self,
        library_root: &Path,
        needed_bytes: i64,
    ) -> Result<QuotaCheck, QuotaError> {
        let cap = self.cap_bytes().await?;
        let used = self.compute_used_bytes(library_root).await?;
        if used.saturating_add(needed_bytes) <= cap {
            Ok(QuotaCheck::Allow { used, cap })
        } else {
            Err(QuotaError::Exceeded {
                used,
                cap,
                needed: needed_bytes,
            })
        }
    }

    /// Acquire the per-library-root critical-section lock.
    ///
    /// Canonicalizes `library_root` (resolving symlinks and `.`/`..`)
    /// and uses the canonical form as the registry key. The returned
    /// tuple is `(guard, canonical_root)`; the caller holds the
    /// `guard` for as long as the critical section needs to be held
    /// and uses `canonical_root` for any further path-keyed
    /// operations (including `compute_used_bytes` and the
    /// `library::fs::complete_download` call). If `library_root`
    /// does not exist or cannot be canonicalized, returns
    /// `QuotaError::Io`.
    ///
    /// The mutex is shared across all `QuotaAccountant` instances
    /// in the process (the registry is process-wide), so two
    /// accountants pointing at the same library root serialize
    /// through the same lock.
    pub async fn lock_for_library(
        &self,
        library_root: &Path,
    ) -> Result<(QuotaLockGuard, PathBuf), QuotaError> {
        let canonical = tokio_fs::canonicalize(library_root).await?;
        let mutex = {
            let mut reg = global_registry()
                .lock()
                .expect("quota registry mutex poisoned");
            reg.entry(canonical.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let guard = mutex.lock_owned().await;
        Ok((QuotaLockGuard::new(guard), canonical))
    }
}

/// Sum the on-disk bytes of every regular file under
/// `<canonical_root>/tmp/incomplete/` and
/// `<canonical_root>/tmp/staging/`. Entries whose canonical form
/// escapes the root are skipped (defense in depth; the dirs are ours
/// so this should not happen in practice).
///
/// The walk is a best-effort snapshot: `entry.metadata().len()` is read
/// at the moment of the `read_dir` step, and a file appended or
/// removed between the `metadata()` call and the size-sum will be
/// undercounted or overcounted by a few bytes. Per the architecture,
/// quota is a policy, not a hard enforcement at the syscall level;
/// small drifts are accepted. The recursive walk uses `tokio::fs`
/// throughout and is bounded by the number of files under `tmp/`,
/// which for a typical library is small (a handful of in-flight
/// downloads at most).
async fn walk_tmp_bytes(canonical_root: &Path) -> Result<i64, QuotaError> {
    let mut total: i64 = 0;
    let mut stack: Vec<PathBuf> = Vec::new();
    for sub in ["incomplete", "staging"] {
        let dir = canonical_root.join("tmp").join(sub);
        if tokio_fs::try_exists(&dir).await.unwrap_or(false) {
            stack.push(dir);
        }
    }
    while let Some(dir) = stack.pop() {
        let mut entries = match tokio_fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(_) => continue,
            };
            let p = entry.path();
            let canonical = match tokio_fs::canonicalize(&p).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            if !library_fs::assert_within(canonical_root, &canonical) {
                continue;
            }
            let meta = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(canonical);
            } else if meta.is_file() {
                let len = meta.len();
                if len > i64::MAX as u64 {
                    total = total.saturating_add(i64::MAX);
                } else {
                    total = total.saturating_add(len as i64);
                }
            }
        }
    }
    Ok(total)
}
