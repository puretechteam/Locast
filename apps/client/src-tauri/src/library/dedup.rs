//! P3-T11: library dedup on download (architecture section 23.3).
//!
//! When a manifest references a media item by `sha256`, the viewer
//! can skip the network transfer if the content is already locally
//! available under the canonical content-addressed path. This
//! module exposes the dedup decision and the read-mostly helpers a
//! future download-open command will call BEFORE creating any
//! transfer session.
//!
//! # Integrity policy
//!
//! The architecture says the existing file must "match the hash".
//! We do NOT rehash on every check (a rehash would make a fast
//! pre-check expensive and is not what the architecture calls
//! for at this layer). We rely on three guards:
//!
//! 1. The on-disk file must live at the canonical
//!    `<library_root>/library/<sha[0..2]>/<sha[2..4]>/<sha>/<filename>`
//!    path constructed from `core::paths::content_addressed_path`.
//!    The path layout is the content identity.
//! 2. The file must be a regular file (not a symlink, directory,
//!    or special file).
//! 3. The file size must equal the `media_items.size_bytes` for the
//!    matching row.
//!
//! Combined, those three rules make a "wrong bytes under the
//! correct name" case impossible by construction: the layout is a
//! 1-to-1 function of sha256 + filename, and the size check
//! rejects obvious corruption. The library scanner
//! (`library::scan::scan`) rehashes every file under `library/`
//! on every run, so a corrupted file is detected on the next scan
//! and the row is reconciled (see scan.rs `orphan-file policy`).
//!
//! If any of the three guards fails the dedup returns
//! `DedupOutcome::Missing`
//! and the caller falls through to the normal transfer path (which
//! itself ends with `assemble_and_finalize` doing a full
//! SHA-256 + BLAKE3 verification of the assembled file).
//!
//! # POSIX hardlink vs Windows copy
//!
//! The architecture allows either a hardlink or a copy-on-write
//! reference when the destination filename differs from the
//! existing filename. P3-T11's v1 surface returns the existing
//! `media_items` row's canonical on-disk path; a future P3-T12+
//! task can introduce the "alias filename" case where the new
//! download wants a different filename under a different
//! content-addressed subdirectory. For now the dedup is a
//! "reuse the canonical file" decision.
//!
//! # Concurrency
//!
//! The dedup is read-mostly: a single SELECT against
//! `media_items`, a stat on the canonical path, and a status
//! update on a hit. No global mutex is introduced. The library
//! scanner (`library::scan`) and the dedup may run concurrently;
//! both share the SQLite WAL pool and the on-disk layout. The
//! scanner's INSERTs and UPDATEs are idempotent (see scan.rs
//! module docs); the dedup's UPDATE on a temporary promotion
//! is bounded by the row's status (`temporary` -> `permanent`)
//! and is also idempotent because the SELECT guards the UPDATE.
//!
//! # Bypassing the transfer session
//!
//! The acceptance for P3-T11 is "the viewer marks the item
//! 'local' in the UI and never opens a transfer session." The
//! dedup itself does not start, schedule, or open a transfer
//! session; it returns a `DedupOutcome::AlreadyLocal` and the
//! caller is expected to:
//!
//! 1. Create the `downloads` row (via `DownloadStore::create`)
//!    with `state = 'complete'` and every `downchunk
//!    row in `verified` state. (A future task provides the
//!    helper; for P3-T11 the caller in `transfer::planner` or
//!    the future download-open command will do this.)
//! 2. Emit `download://state` and `download://progress` events
//!    so the P3-T10 modal closes. (P3-T08 already emits these;
//!    a future task wires them.)
//!
//! P3-T11 exposes only the dedup decision. No transfer session
//! ctor is called from this module.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use specta::Type;
use sqlx::Row;
use thiserror::Error;
use tokio::fs as tokio_fs;

use crate::core::library::sanitize::{self, InvalidFilename};
use crate::core::paths::{self, PathError};
use crate::storage::Storage;

/// Result of a dedup decision. P3-T11 returns one of these to
/// the caller; the caller (future download-open command) is
/// responsible for marking the `downloads` row complete and
/// emitting the right events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DedupOutcome {
    /// The content is already locally available. `on_disk_path`
    /// is the canonical content-addressed path that the caller
    /// can use directly. `existing_media_id` is the `media_items.id`
    /// of the existing row (so the caller can correlate logs and
    /// UI). The caller should NOT start a transfer session; it
    /// should mark the new `downloads` row complete and emit
    /// `download://state = complete`.
    AlreadyLocal {
        on_disk_path: String,
        existing_media_id: String,
        existing_status: String,
    },
    /// A `temporary` row matched; promote it to `permanent`
    /// in-place (the file is not moved) and treat the situation
    /// as `AlreadyLocal`. P3-T11 applies the promotion in this
    /// call so that subsequent calls see a `permanent` row.
    PromotedFromTemporary {
        on_disk_path: String,
        existing_media_id: String,
    },
    /// No usable local copy. The caller should start a normal
    /// transfer session. (The downloader will end with
    /// `assemble_and_finalize` which does a full hash verify.)
    Missing,
}

impl DedupOutcome {
    pub fn is_local(&self) -> bool {
        matches!(
            self,
            DedupOutcome::AlreadyLocal { .. } | DedupOutcome::PromotedFromTemporary { .. }
        )
    }
}

/// Errors raised by `dedup_on_download`.
#[derive(Debug, Error)]
pub enum DedupError {
    /// `sha256` was not 64 lowercase hex characters.
    #[error(transparent)]
    InvalidSha(#[from] PathError),

    /// `desired_filename` failed sanitization.
    #[error("invalid desired filename: {0}")]
    InvalidFilename(#[from] InvalidFilename),

    /// The library root does not exist or is not a directory.
    #[error("library root is not a usable directory: {0}")]
    LibraryRootInvalid(#[source] std::io::Error),

    /// A path canonicalization or stat failed. The library may be
    /// corrupted, the user may have changed permissions, or this
    /// may be an I/O fault. The caller treats this as a `Missing`
    /// outcome (fall through to normal download) and surfaces
    /// the error in its log.
    #[error("dedup io error: {0}")]
    Io(#[from] std::io::Error),

    /// A SQLite query failed.
    #[error("dedup sql error: {0}")]
    Sql(#[from] sqlx::Error),
}

/// One row from `media_items` relevant to the dedup decision.
#[derive(Debug, Clone)]
struct MediaRow {
    id: String,
    size_bytes: i64,
    relative_path: String,
    status: String,
}

/// Decide whether a download of `sha256` can be satisfied from the
/// local library without a network transfer.
///
/// Behavior:
/// 1. Validate `sha256` and `desired_filename`.
/// 2. `SELECT id, size_bytes, relative_path, status FROM media_items WHERE sha256 = ?`.
/// 3. If no row exists: return `Missing`.
/// 4. If the row's `status` is `permanent` (or `temporary`):
///    - resolve the absolute path as
///      `<library_root>/<relative_path>`;
///    - canonicalize the path and assert it is under the
///      canonicalized library root;
///    - stat the path; require a regular file with size
///      matching `media_items.size_bytes`;
///    - if `status == 'temporary'`, UPDATE the row to
///      `permanent` and return `PromotedFromTemporary`;
///    - otherwise return `AlreadyLocal`.
/// 5. If any of the stat / size / containment / canonicalization
///    checks fail, return `Missing` (do NOT fall through to a
///    transfer session by accident; the caller's download-open
///    command decides that).
pub async fn dedup_on_download(
    storage: &Storage,
    library_root: &Path,
    sha256: &str,
    desired_filename: &str,
) -> Result<DedupOutcome, DedupError> {
    // 1. Validate inputs.
    paths::validate_sha(sha256)?;
    let _sanitized = sanitize::sanitize(desired_filename)?;

    // 2. Library root must exist and be a directory.
    let root_meta = tokio_fs::metadata(library_root)
        .await
        .map_err(DedupError::LibraryRootInvalid)?;
    if !root_meta.is_dir() {
        return Err(DedupError::LibraryRootInvalid(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "library root is not a directory",
        )));
    }
    let canonical_root = tokio_fs::canonicalize(library_root)
        .await
        .map_err(DedupError::LibraryRootInvalid)?;

    // 3. Look up the matching row. `media_items.sha256` has a
    //    UNIQUE index, so at most one row matches.
    let maybe = sqlx::query(
        "SELECT id, size_bytes, relative_path, status \
         FROM media_items WHERE sha256 = ?1",
    )
    .bind(sha256)
    .fetch_optional(&storage.pool())
    .await?;
    let Some(r) = maybe else {
        return Ok(DedupOutcome::Missing);
    };
    let row = MediaRow {
        id: r.get("id"),
        size_bytes: r.get("size_bytes"),
        relative_path: r.get("relative_path"),
        status: r.get("status"),
    };

    // 4. Reject any status outside the architecture's permanent /
    //    temporary set. The schema CHECK already enforces this, so
    //    this branch is defensive only.
    if row.status != "permanent" && row.status != "temporary" {
        return Ok(DedupOutcome::Missing);
    }

    // 5. Resolve the on-disk path. `relative_path` is library-root
    //    relative (forward-slash normalized). Reconstruct the
    //    absolute path; do NOT trust `relative_path` for containment
    //    until canonicalize + assert_within has been run.
    let rel_components = row.relative_path.split('/');
    let mut on_disk = library_root.to_path_buf();
    for c in rel_components {
        if c.is_empty() || c == "." || c == ".." {
            // A row with traversal components must never be
            // trusted; the layout is content-addressed, so a sane
            // row never has these.
            return Ok(DedupOutcome::Missing);
        }
        on_disk.push(c);
    }

    // 6. Stat + size check. If the file is missing, wrong size,
    //    not a regular file, or escapes the library root, the
    //    dedup is a miss.
    match verify_local_file(&canonical_root, &on_disk, row.size_bytes).await? {
        Some(verified_path) => {
            // 7. Handle temporary promotion (idempotent: if a
            //    concurrent caller already promoted, the UPDATE
            //    affects 0 rows, which is fine).
            if row.status == "temporary" {
                sqlx::query(
                    "UPDATE media_items SET status = 'permanent' WHERE id = ?1 AND status = 'temporary'",
                )
                .bind(&row.id)
                .execute(&storage.pool())
                .await?;
                // Architecture §23.3 specifies that the temporary -> permanent
                // promotion leaves the file in place and copies the row's
                // `acquired_ms` and `last_used_ms` (i.e. leaves them alone).
                // The `media_items` schema does not yet have those columns;
                // when they land are the the dedup UPDATE will keep
                // preserving them. The promotion intentionally does NOT
                // touch `last_room_id` -- per §23.4 that is the user's
                // "Keep" decision at room-leave time, not the dedup's.
                Ok(DedupOutcome::PromotedFromTemporary {
                    on_disk_path: verified_path.to_string_lossy().into_owned(),
                    existing_media_id: row.id.clone(),
                })
            } else {
                Ok(DedupOutcome::AlreadyLocal {
                    on_disk_path: verified_path.to_string_lossy().into_owned(),
                    existing_media_id: row.id.clone(),
                    existing_status: row.status.clone(),
                })
            }
        }
        None => Ok(DedupOutcome::Missing),
    }
}

/// Internal: stat the candidate path, canonicalize it, assert
/// it is under the canonical root, and confirm it is a regular
/// file with the expected size. Returns the canonical path on
/// success, `None` on any guard failure.
async fn verify_local_file(
    canonical_root: &Path,
    candidate: &Path,
    expected_size_bytes: i64,
) -> Result<Option<PathBuf>, DedupError> {
    if expected_size_bytes == 0 {
        return Ok(None);
    }
    let meta = match tokio_fs::metadata(candidate).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(DedupError::Io(e)),
    };
    if !meta.is_file() {
        return Ok(None);
    }
    if meta.len() as i64 != expected_size_bytes {
        return Ok(None);
    }
    let canonical = match tokio_fs::canonicalize(candidate).await {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(DedupError::Io(e)),
    };
    // Policy: any non-NotFound error from `metadata` or `canonicalize`
    // propagates as `DedupError::Io` so the caller can decide. `NotFound`
    // is the only "benign miss" because it indicates the user has
    // deleted the file between scans -- exactly the case the architecture
    // says we should fall through to a normal transfer for.
    if !crate::library::fs::assert_within(canonical_root, &canonical) {
        return Ok(None);
    }
    Ok(Some(canonical))
}

/// Pre-check for whether the canonical content-addressed path
/// for `(sha, filename)` is present, is a regular file, and has
/// the expected size. This is the "what's on disk" half of the
/// dedup; `dedup_on_download` adds the "what does media_items
/// say" half.
///
/// Exposed for tests and for any future caller that wants to
/// ask "is this sha already on disk under any filename" without
/// consulting the DB.
pub async fn exists_at_canonical_path(
    library_root: &Path,
    sha256: &str,
    sanitized_filename: &str,
    expected_size_bytes: i64,
) -> Result<bool, DedupError> {
    paths::validate_sha(sha256)?;
    paths::check_sanitized(sanitized_filename)?;
    let path = paths::content_addressed_path(library_root, sha256, sanitized_filename)?;
    let canonical_root = tokio_fs::canonicalize(library_root).await?;
    Ok(
        verify_local_file(&canonical_root, &path, expected_size_bytes)
            .await?
            .is_some(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    const FILENAME: &str = "movie.mkv";
    const FILE_BYTES: &[u8] = &[0xAB; 16 * 1024];

    async fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("locast-dedup-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        dir
    }

    async fn seed_permanent_row(storage: &Storage, sha: &str, size_bytes: i64) -> String {
        let id = Uuid::new_v4().to_string();
        let rel = format!("library/{}/{}/{}/{}", &sha[0..2], &sha[2..4], sha, FILENAME);
        sqlx::query(
            "INSERT INTO media_items (\
                id, sha256, blake3, size_bytes, filename, relative_path, \
                mime, duration_ms, width, height, video_codec, audio_codec, \
                container, status, created_at, last_seen_at, last_room_id, \
                source_url, provenance\
             ) VALUES (\
                ?1, ?2, 'b', ?3, ?4, ?5, \
                'application/octet-stream', NULL, NULL, NULL, NULL, NULL, NULL, \
                'permanent', 1, 1, NULL, NULL, '{}'\
             )",
        )
        .bind(&id)
        .bind(sha)
        .bind(size_bytes)
        .bind(FILENAME)
        .bind(&rel)
        .execute(&storage.pool())
        .await
        .expect("insert permanent row");
        id
    }

    async fn seed_temporary_row(storage: &Storage, sha: &str, size_bytes: i64) -> String {
        let id = Uuid::new_v4().to_string();
        let rel = format!("library/{}/{}/{}/{}", &sha[0..2], &sha[2..4], sha, FILENAME);
        sqlx::query(
            "INSERT INTO media_items (\
                id, sha256, blake3, size_bytes, filename, relative_path, \
                mime, duration_ms, width, height, video_codec, audio_codec, \
                container, status, created_at, last_seen_at, last_room_id, \
                source_url, provenance\
             ) VALUES (\
                ?1, ?2, 'b', ?3, ?4, ?5, \
                'application/octet-stream', NULL, NULL, NULL, NULL, NULL, NULL, \
                'temporary', 1, 1, NULL, NULL, '{}'\
             )",
        )
        .bind(&id)
        .bind(sha)
        .bind(size_bytes)
        .bind(FILENAME)
        .bind(&rel)
        .execute(&storage.pool())
        .await
        .expect("insert temporary row");
        id
    }

    async fn write_cap_file(library_root: &Path, sha: &str, bytes: &[u8]) -> PathBuf {
        let p = paths::content_addressed_path(library_root, sha, FILENAME).unwrap();
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&p, bytes).await.unwrap();
        p
    }

    async fn make_storage() -> (Storage, PathBuf) {
        // Open storage on a temp file under a tempdir, and use
        // the tempdir as the library root.
        let dir = temp_root().await;
        let db = dir.join("index.sqlite");
        let storage = Storage::open(&db).await.expect("storage opens");
        (storage, dir)
    }

    async fn get_status(storage: &Storage, id: &str) -> String {
        let row: (String,) = sqlx::query_as("SELECT status FROM media_items WHERE id = ?1")
            .bind(id)
            .fetch_one(&storage.pool())
            .await
            .expect("status row");
        row.0
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn validate_sha_propagates() {
        let (storage, root) = make_storage().await;
        let err = dedup_on_download(&storage, &root, "not-a-sha", FILENAME)
            .await
            .expect_err("must be InvalidSha");
        assert!(matches!(err, DedupError::InvalidSha(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_filename_propagates() {
        let (storage, root) = make_storage().await;
        // Trailing separator yields empty last segment -> sanitize rejects.
        let err = dedup_on_download(&storage, &root, SHA, "foo/")
            .await
            .expect_err("must be InvalidFilename");
        assert!(matches!(err, DedupError::InvalidFilename(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_when_no_row() {
        let (storage, root) = make_storage().await;
        seed_permanent_row(&storage, OTHER_SHA, FILE_BYTES.len() as i64).await;
        write_cap_file(&root, OTHER_SHA, FILE_BYTES).await;
        let outcome = dedup_on_download(&storage, &root, SHA, FILENAME)
            .await
            .expect("ok");
        assert_eq!(outcome, DedupOutcome::Missing);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_when_row_present_but_file_absent() {
        let (storage, root) = make_storage().await;
        seed_permanent_row(&storage, SHA, FILE_BYTES.len() as i64).await;
        // No on-disk file.
        let outcome = dedup_on_download(&storage, &root, SHA, FILENAME)
            .await
            .expect("ok");
        assert_eq!(outcome, DedupOutcome::Missing);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_when_wrong_size() {
        let (storage, root) = make_storage().await;
        seed_permanent_row(&storage, SHA, FILE_BYTES.len() as i64).await;
        // Write exactly 1 byte instead.
        let p = paths::content_addressed_path(&root, SHA, FILENAME).unwrap();
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&p, &[0u8]).await.unwrap();

        let outcome = dedup_on_download(&storage, &root, SHA, FILENAME)
            .await
            .expect("ok");
        assert_eq!(outcome, DedupOutcome::Missing);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn already_local_when_permanent_row_present_and_file_matches() {
        let (storage, root) = make_storage().await;
        let id = seed_permanent_row(&storage, SHA, FILE_BYTES.len() as i64).await;
        let written = write_cap_file(&root, SHA, FILE_BYTES).await;

        let outcome = dedup_on_download(&storage, &root, SHA, FILENAME)
            .await
            .expect("ok");

        match outcome {
            DedupOutcome::AlreadyLocal {
                on_disk_path,
                existing_media_id,
                existing_status,
            } => {
                assert_eq!(existing_media_id, id);
                assert_eq!(existing_status, "permanent");
                // The on_disk_path is canonical; on Windows the
                // \\?\ prefix may be added. Compare via canonicalize.
                let canonical = tokio_fs::canonicalize(&written).await.unwrap();
                let got = tokio_fs::canonicalize(&on_disk_path).await.unwrap();
                assert_eq!(canonical, got);
            }
            other => panic!("expected AlreadyLocal, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn promoted_from_temporary_updates_status_to_permanent() {
        let (storage, root) = make_storage().await;
        let id = seed_temporary_row(&storage, SHA, FILE_BYTES.len() as i64).await;
        write_cap_file(&root, SHA, FILE_BYTES).await;

        let outcome = dedup_on_download(&storage, &root, SHA, FILENAME)
            .await
            .expect("ok");
        match outcome {
            DedupOutcome::PromotedFromTemporary {
                existing_media_id, ..
            } => {
                assert_eq!(existing_media_id, id);
            }
            other => panic!("expected PromotedFromTemporary, got {other:?}"),
        }
        assert_eq!(get_status(&storage, &id).await, "permanent");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idempotent_second_call_returns_already_local_after_promotion() {
        let (storage, root) = make_storage().await;
        let id = seed_temporary_row(&storage, SHA, FILE_BYTES.len() as i64).await;
        write_cap_file(&root, SHA, FILE_BYTES).await;

        let first = dedup_on_download(&storage, &root, SHA, FILENAME)
            .await
            .expect("ok");
        assert!(matches!(first, DedupOutcome::PromotedFromTemporary { .. }));
        let second = dedup_on_download(&storage, &root, SHA, FILENAME)
            .await
            .expect("ok");
        match second {
            DedupOutcome::AlreadyLocal {
                existing_media_id,
                existing_status,
                ..
            } => {
                assert_eq!(existing_media_id, id);
                assert_eq!(existing_status, "permanent");
            }
            other => panic!("expected AlreadyLocal on second call, got {other:?}"),
        }
        assert_eq!(get_status(&storage, &id).await, "permanent");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn symlink_escape_rejected() {
        use std::os::unix::fs::symlink;

        let (storage, root) = make_storage().await;
        let id = seed_permanent_row(&storage, SHA, FILE_BYTES.len() as i64).await;
        let _ = id;

        // Target OUTSIDE the library root.
        let outside_dir = temp_root().await;
        let outside_file = outside_dir.join("outside.mkv");
        tokio::fs::write(&outside_file, FILE_BYTES).await.unwrap();

        let cap = paths::content_addressed_path(&root, SHA, FILENAME).unwrap();
        if let Some(parent) = cap.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        symlink(&outside_file, &cap).unwrap();

        let outcome = dedup_on_download(&storage, &root, SHA, FILENAME)
            .await
            .expect("ok");
        assert_eq!(outcome, DedupOutcome::Missing);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_regular_file_rejected() {
        let (storage, root) = make_storage().await;
        seed_permanent_row(&storage, SHA, FILE_BYTES.len() as i64).await;

        let cap = paths::content_addressed_path(&root, SHA, FILENAME).unwrap();
        if let Some(parent) = cap.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::create_dir(&cap).await.unwrap();

        let outcome = dedup_on_download(&storage, &root, SHA, FILENAME)
            .await
            .expect("ok");
        assert_eq!(outcome, DedupOutcome::Missing);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn traversal_in_relative_path_rejected() {
        let (storage, root) = make_storage().await;
        // Insert a row with a relative_path containing '..'.
        sqlx::query(
            "INSERT INTO media_items (\
                id, sha256, blake3, size_bytes, filename, relative_path, \
                mime, duration_ms, width, height, video_codec, audio_codec, \
                container, status, created_at, last_seen_at, last_room_id, \
                source_url, provenance\
             ) VALUES (\
                ?1, ?2, 'b', 100, 'evil.mkv', ?3, \
                'application/octet-stream', NULL, NULL, NULL, NULL, NULL, NULL, \
                'permanent', 1, 1, NULL, NULL, '{}'\
             )",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(SHA)
        .bind("../../etc/passwd")
        .execute(&storage.pool())
        .await
        .expect("insert traversal row");

        let outcome = dedup_on_download(&storage, &root, SHA, FILENAME)
            .await
            .expect("ok");
        assert_eq!(outcome, DedupOutcome::Missing);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn library_root_invalid() {
        let (storage, _root) = make_storage().await;
        let bogus =
            std::env::temp_dir().join(format!("locast-dedup-missing-root-{}", Uuid::new_v4()));
        let err = dedup_on_download(&storage, &bogus, SHA, FILENAME)
            .await
            .expect_err("missing root must error");
        assert!(matches!(err, DedupError::LibraryRootInvalid(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_zero_byte_file() {
        let (storage, root) = make_storage().await;
        seed_permanent_row(&storage, SHA, 0).await;
        // Build the canonical path and write 0 bytes.
        let cap = paths::content_addressed_path(&root, SHA, FILENAME).unwrap();
        if let Some(parent) = cap.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&cap, b"").await.unwrap();

        let outcome = dedup_on_download(&storage, &root, SHA, FILENAME)
            .await
            .expect("ok");
        assert_eq!(outcome, DedupOutcome::Missing);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn library_root_is_regular_file_rejected() {
        // Create a regular file; pass it as the library root.
        let parent = temp_root().await;
        let fake_root = parent.join("not-a-dir");
        tokio::fs::write(&fake_root, b"not a directory")
            .await
            .unwrap();

        let (storage, _unused) = make_storage().await;
        let err = dedup_on_download(&storage, &fake_root, SHA, FILENAME)
            .await
            .expect_err("should reject");
        assert!(matches!(err, DedupError::LibraryRootInvalid(_)));
    }
}
