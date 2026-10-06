//! Startup purge of leftover scratch bytes under `<library_root>/tmp/`.
//!
//! A crash or a failed completion can leave bytes behind that the quota
//! walk still counts. At startup nothing is in flight, so:
//!
//! - every entry directly under `tmp/staging/` is removed (a staged
//!   `.partial` is never resumed; the chunks under `tmp/incomplete/`
//!   are the resumable state);
//! - a `tmp/incomplete/<download-id>/` directory is removed unless its
//!   `downloads` row is in a resumable state (`pending`, `connecting`,
//!   `transferring`, `verifying`, `paused`). Directories with no row, or
//!   a `complete`, `failed` or `cancelled` row, are orphans.
//!
//! Only direct children of those two directories are touched, and a
//! symlink is unlinked rather than followed.

use std::path::Path;

use tokio::fs as tokio_fs;

use crate::storage::Storage;

/// Counts of removed entries, for logging and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PurgeReport {
    pub staging_removed: usize,
    pub incomplete_removed: usize,
}

const RESUMABLE_STATES: [&str; 5] = [
    "pending",
    "connecting",
    "transferring",
    "verifying",
    "paused",
];

/// Remove one directory entry. Never follows a symlink.
async fn remove_entry(path: &Path) -> std::io::Result<()> {
    let meta = tokio_fs::symlink_metadata(path).await?;
    if meta.is_dir() {
        tokio_fs::remove_dir_all(path).await
    } else {
        tokio_fs::remove_file(path).await
    }
}

/// Purge orphaned scratch entries. Individual removal failures are
/// skipped (and left for the next startup); only a failure to query
/// the database is returned.
pub async fn purge_stale_tmp(
    storage: &Storage,
    library_root: &Path,
) -> Result<PurgeReport, sqlx::Error> {
    let mut report = PurgeReport::default();
    let tmp = library_root.join("tmp");

    if let Ok(mut entries) = tokio_fs::read_dir(tmp.join("staging")).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if remove_entry(&entry.path()).await.is_ok() {
                report.staging_removed += 1;
            }
        }
    }

    let incomplete = tmp.join("incomplete");
    if let Ok(mut entries) = tokio_fs::read_dir(&incomplete).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            let state: Option<(String,)> =
                sqlx::query_as("SELECT state FROM downloads WHERE id = ?1")
                    .bind(&name)
                    .fetch_optional(&storage.pool())
                    .await?;
            let resumable = state.is_some_and(|(s,)| RESUMABLE_STATES.contains(&s.as_str()));
            if !resumable && remove_entry(&entry.path()).await.is_ok() {
                report.incomplete_removed += 1;
            }
        }
    }
    Ok(report)
}
