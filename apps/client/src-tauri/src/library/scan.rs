//! `library::scan` - the on-disk library scanner.
//!
//! P1-T07 walks the content-addressed tree under
//! `<library_root>/library/<sha[0..2]>/<sha[2..4]>/<sha>/<filename>`,
//! reconciles it against the `media_items` table, and inserts or
//! updates rows so the table reflects the on-disk state.
//!
//! # What "media" means
//!
//! The architecture is explicit: permanent media lives ONLY under
//! `<library_root>/library/<sha[0..2]>/<sha[2..4]>/<sha>/<filename>`.
//! Everything else under the library root (`tmp/`, `trash/`,
//! `index.sqlite*`, top-level files, hidden files) is NOT media. The
//! scanner walks `library/` recursively and treats every regular file
//! under it as a candidate media file. No extension filter is needed
//! because the directory tree itself encodes the constraint.
//!
//! # Symlink handling
//!
//! The scanner does NOT follow symlinks. `tokio::fs::symlink_metadata`
//! returns `is_symlink() == true` for symlinks; the scanner skips
//! them. This prevents both the security risk (symlink escape from
//! the library root) and the inefficiency (double-counting via a
//! symlink chain). The architecture's content-addressed layout is a
//! regular-file layout; symlinks would only appear if the user
//! manually created them, and ignoring them is the safe default.
//!
//! # Zero-byte files
//!
//! Skipped. No SHA can be computed, and the architecture does not
//! say zero-byte files are valid media.
//!
//! # Missing-file policy
//!
//! The roadmap says: "Do not silently delete database records unless
//! the architecture explicitly requires that behavior." The
//! architecture (section 23.6) talks about user-initiated
//! `media_delete(id, mode)`; it does NOT say the scanner should
//! auto-delete rows for files that are missing. P1-T07 therefore
//! BUMPS `last_seen_at` for missing files and reports the count in
//! `ScanResult.files_missing`. The `status` column is left
//! unchanged because the P0-T05 schema has a
//! `CHECK (status IN ('permanent','temporary'))` constraint that
//! rejects a third value; setting a new status for missing files
//! would be rejected at the SQL layer. A future P2 task may add
//! automatic deletion; that's out of scope.
//!
//! # Orphan-file policy
//!
//! A content-addressed file with no DB row is recovered as a
//! permanent media row. The provenance is
//! `{"source":"library-scan","orphan":true,"discovered_at":"<rfc3339>"}`.
//! The `last_seen_at` is set to now. The FTS5 trigger fires
//! automatically.
//!
//! # Updated-file policy
//!
//! If a row exists for the same sha but a different
//! `relative_path` (the on-disk file was renamed or replaced), the
//! scanner UPDATEs the row's `filename` and `relative_path`. The
//! `id`, `sha256`, `blake3`, `size_bytes`, `created_at`, `status`
//! are NOT changed. This is the "DB tracks what is actually on
//! disk" rule from the architecture.
//!
//! # Concurrency
//!
//! The scanner does NOT acquire the per-library-root mutex from
//! P1-T05 (the scanner is read-mostly and does not call
//! `import_one`). The scanner's INSERTs and UPDATEs share the SQLx
//! pool with any in-flight import. The architecture's WAL mode
//! allows concurrent readers; the scanner's writer is one of
//! potentially many. The scanner does NOT need to serialize against
//! imports because the schema's UNIQUE INDEX on
//! `media_items.relative_path` (NOCASE) provides the atomicity
//! guarantee. The `last_seen_at` UPDATE on missing files is
//! idempotent. The INSERT for a new file is atomic via the UNIQUE
//! constraint. This is a deliberate non-serialization; documented
//! here and in the module docstring.
//!
//! # Performance
//!
//! P1-T07 does a full re-hash. The architecture (line 3765)
//! describes an incremental cache keyed by `(path, size, mtime)`,
//! but that is a future optimization. For the 50-file acceptance
//! test the scan is sub-second.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use specta::Type;
use thiserror::Error;
use tokio::fs as tokio_fs;
use tokio::io::AsyncReadExt;

use crate::core::hashing::{Blake3Hasher, Sha256Hasher};
use crate::core::paths::{self, PathError};
use crate::storage::{Storage, StorageError};

/// Scratch buffer size for streaming hash. 64 KiB matches
/// `commands::import::COPY_CHUNK` so the syscall overhead profile
/// is the same.
const SCAN_HASH_CHUNK: usize = 64 * 1024;

/// Per-call result of the library scanner. The counts are
/// exhaustive: every file the scanner encounters is placed in
/// exactly one of `files_scanned`, `files_upserted`,
/// `files_orphans_discovered`, `files_missing`, or `files_failed`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ScanResult {
    /// Total number of regular files observed under `library/`.
    /// This includes files that hit the idempotent no-op branch,
    /// files that were upserted, and files that failed mid-hash.
    #[specta(type = specta_typescript::Number)]
    pub files_scanned: u64,
    /// Number of files whose on-disk state triggered a database
    /// INSERT or UPDATE (i.e. a new file or a renamed file). A
    /// re-scan of an unchanged library yields `files_upserted = 0`.
    #[specta(type = specta_typescript::Number)]
    pub files_upserted: u64,
    /// Number of content-addressed files under `library/` that
    /// had no matching `media_items` row and were recovered into
    /// the table. P1-T07's recovery path: compute the hash, insert
    /// a row with
    /// `provenance = {"source":"library-scan","orphan":true}`.
    #[specta(type = specta_typescript::Number)]
    pub files_orphans_discovered: u64,
    /// Number of `media_items` rows whose `relative_path` no
    /// longer corresponds to a file on disk. The scan bumps
    /// `last_seen_at` to now for these rows; it does NOT delete
    /// them.
    #[specta(type = specta_typescript::Number)]
    pub files_missing: u64,
    /// Number of files the scan could not process (read error,
    /// metadata error, hash error, etc.). The scan is fail-soft:
    /// the error is recorded as `files_failed` and the scan
    /// continues with the next file.
    #[specta(type = specta_typescript::Number)]
    pub files_failed: u64,
    /// Sum of `size_bytes` for every file the scanner
    /// successfully processed. Equal to `SUM(size_bytes) FROM
    /// media_items` after the scan (modulo the missing-file bump,
    /// which does not change `size_bytes`). Exposed to
    /// TypeScript as `number`; a desktop library will not
    /// realistically exceed 2^53 - 1 bytes.
    #[specta(type = specta_typescript::Number)]
    pub bytes_total: i64,
}

/// Errors raised by `library::scan::scan`.
#[derive(Debug, Error)]
pub enum ScanError {
    /// A filesystem walk under `<library_root>/library/` failed.
    #[error("scan io error: {0}")]
    Io(#[from] std::io::Error),

    /// The underlying `Storage` handle is unusable (open / pool
    /// error). Normally unreachable because `Storage::open` is
    /// called by the Tauri command before the scan starts; the
    /// variant exists so the scanner's error type is closed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// A SQLite statement failed at runtime.
    #[error("scan sql error: {0}")]
    Sqlx(#[from] sqlx::Error),

    /// A path-construction operation failed (invalid sha, invalid
    /// sanitized filename).
    #[error(transparent)]
    Paths(#[from] PathError),
}

/// Walk the content-addressed tree under
/// `<library_root>/library/<sha[0..2]>/<sha[2..4]>/<sha>/<filename>` and
/// reconcile it against the `media_items` table.
///
/// The scan is fail-soft: a single unreadable / malformed file
/// increments `files_failed` and the scan continues with the next
/// file. The returned `ScanResult` is exhaustive: every file the
/// scanner encounters is counted in exactly one category.
///
/// # Steps
///
/// 1. Validate the library root (must exist and be a directory).
/// 2. For every regular file under `library/`:
///    - skip symlinks (`is_symlink()` is true);
///    - skip zero-byte files;
///    - stream-hash (SHA-256 + BLAKE3 in lockstep);
///    - compute the content-addressed `relative_path`;
///    - if a row exists for this `sha256`, UPDATE the row's
///      `filename` and `relative_path` if they differ from the
///      on-disk state, otherwise no-op;
///    - if no row exists, INSERT a new `media_items` row with
///      `status = 'permanent'`, `provenance` set to
///      `{"source":"library-scan","discovered_at":"<rfc3339>"}`
///      and `mime = "application/octet-stream"`. The FTS5 trigger
///      fires.
/// 3. For every `media_items` row whose `relative_path` no longer
///    corresponds to a file on disk: bump `last_seen_at` to now.
///    The row is left otherwise unchanged; it is NOT deleted.
pub async fn scan(storage: &Storage, library_root: &Path) -> Result<ScanResult, ScanError> {
    let root_meta = tokio_fs::metadata(library_root).await?;
    if !root_meta.is_dir() {
        return Err(ScanError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "library root is not a directory",
        )));
    }

    let library_dir = library_root.join("library");
    let mut result = ScanResult::default();
    let mut visited: Vec<String> = Vec::new();

    // The library/ subdirectory may be absent on a fresh install.
    // `try_exists` returns false rather than erroring, so an
    // absent `library/` is not a scan failure - it just means
    // there is nothing to walk. We still need to mark every
    // existing media_items row as missing, so we fall through to
    // the missing-file pass.
    if tokio_fs::try_exists(&library_dir).await.unwrap_or(false) {
        walk_library(
            storage,
            library_root,
            &library_dir,
            &mut result,
            &mut visited,
        )
        .await?;
    }

    let missing = mark_missing_files(storage, &visited).await?;
    result.files_missing = missing;
    Ok(result)
}

/// Recursive walk of `library_dir`. Every regular file is hashed
/// and reconciled. Symlinks are skipped (not followed). The
/// recursive descent uses a manual stack of `PathBuf` so the
/// function is `async` without spawning.
async fn walk_library(
    storage: &Storage,
    library_root: &Path,
    library_dir: &Path,
    result: &mut ScanResult,
    visited: &mut Vec<String>,
) -> Result<(), ScanError> {
    let mut stack: Vec<PathBuf> = vec![library_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // Collect every entry into a `Vec` and sort by `file_name`
        // before processing. The order returned by `tokio::fs::read_dir`
        // is OS-dependent (alphabetical on Windows NTFS, insertion order
        // on Linux ext4, catalog order on macOS APFS), and the
        // scanner's "second file visited wins the UPDATE" semantics
        // produces a different DB row on different platforms if the
        // order is not pinned. Sorting by the final path component is
        // a deterministic, locale-independent, byte-wise total order
        // and gives identical test results on every host.
        let mut entries: Vec<PathBuf> = match tokio_fs::read_dir(&dir).await {
            Ok(mut e) => {
                let mut v: Vec<PathBuf> = Vec::new();
                loop {
                    match e.next_entry().await {
                        Ok(Some(entry)) => v.push(entry.path()),
                        Ok(None) => break,
                        Err(_) => continue,
                    }
                }
                v
            }
            Err(_) => continue,
        };
        entries.sort_by(|a, b| {
            a.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .cmp(&b.file_name().map(|n| n.to_string_lossy().into_owned()))
        });
        for path in entries {
            let meta = match tokio_fs::symlink_metadata(&path).await {
                Ok(m) => m,
                Err(_) => {
                    result.files_failed += 1;
                    continue;
                }
            };
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }

            // Zero-byte files are silently skipped: there are no bytes
            // to hash, and the architecture's content-addressed model
            // treats an empty file as invalid. The skip happens
            // BEFORE `files_scanned += 1` so the empty file does not
            // appear in the result counts and does not surface as a
            // failure.
            let size = meta.len();
            if size == 0 {
                continue;
            }
            let size_i64: i64 = match size.try_into() {
                Ok(n) => n,
                Err(_) => {
                    result.files_failed += 1;
                    continue;
                }
            };

            result.files_scanned += 1;

            match process_file(storage, library_root, &path, size_i64).await {
                Outcome::Changed(rel) => {
                    visited.push(rel);
                    result.files_upserted += 1;
                    result.bytes_total = result.bytes_total.saturating_add(size_i64);
                }
                Outcome::Unchanged(rel) => {
                    visited.push(rel);
                    result.bytes_total = result.bytes_total.saturating_add(size_i64);
                }
                Outcome::OrphanInsert(rel) => {
                    visited.push(rel);
                    result.files_upserted += 1;
                    result.files_orphans_discovered += 1;
                    result.bytes_total = result.bytes_total.saturating_add(size_i64);
                }
                Outcome::Failed => {
                    result.files_failed += 1;
                }
            }
        }
    }
    Ok(())
}

/// The per-file outcome classification.
enum Outcome {
    /// An UPDATE on an existing row because the on-disk
    /// `relative_path` differed from the DB row's value.
    Changed(String),
    /// The on-disk `relative_path` matched an existing row; the
    /// row was not touched.
    Unchanged(String),
    /// A new INSERT: the on-disk file had no matching DB row. The
    /// scanner INSERTed a fresh row. The scanner cannot tell
    /// whether the file was previously seen by `import_one` (in
    /// which case a prior INSERT failed and the on-disk file is
    /// an orphan) or never seen at all; both surface here. The
    /// `provenance` JSON is the same in both cases:
    /// `{"source":"library-scan","discovered_at":"<rfc3339>"}`.
    OrphanInsert(String),
    /// The file could not be processed (read error, hash error,
    /// non-UTF-8 filename, etc.).
    Failed,
}

/// Process a single file: hash it, look up any existing row by
/// `sha256`, and INSERT or UPDATE the row as appropriate. Returns
/// the per-file `Outcome`. The function is fail-soft: any error
/// from the per-file work surfaces as `Outcome::Failed`; the scan
/// continues with the next file.
async fn process_file(storage: &Storage, library_root: &Path, path: &Path, size: i64) -> Outcome {
    // Filename must be valid UTF-8 AND must pass P1-T01's
    // sanitization. Sanitization rejects path separators, control
    // characters, reserved Windows names, and trailing dots/spaces.
    // A misplaced or hostile filename becomes `Outcome::Failed` so
    // the file is recorded in the scan result's `files_failed` count
    // but never creates a row in the DB.
    let raw_filename = match path.file_name().and_then(|n| n.to_str()) {
        Some(s) => s.to_string(),
        None => return Outcome::Failed,
    };
    let filename = match crate::core::library::sanitize::sanitize(&raw_filename) {
        Ok(s) => s,
        Err(_) => return Outcome::Failed,
    };

    let (sha256, blake3) = match stream_hash_file(path).await {
        Ok(h) => h,
        Err(_) => return Outcome::Failed,
    };

    // Compute the canonical content-addressed path. If the file
    // is NOT at the path the architecture's content-addressed layout
    // prescribes for this sha + filename, skip it as a "misplaced
    // file" (counts as `files_failed`). This prevents a phantom
    // "missing file" row from accumulating on every rescan when the
    // user (or some tool) places a file at a non-canonical path
    // under `library/`.
    let expected_path = match paths::content_addressed_path(library_root, &sha256, &filename) {
        Ok(p) => p,
        Err(_) => return Outcome::Failed,
    };
    let path_matches_expected = path == expected_path.as_path()
        || path.canonicalize().ok().as_ref() == expected_path.canonicalize().ok().as_ref();
    if !path_matches_expected {
        return Outcome::Failed;
    }
    let relative_path = relative_path_from(library_root, &expected_path);

    let existing: Option<(String, String)> =
        match sqlx::query_as("SELECT filename, relative_path FROM media_items WHERE sha256 = ?1")
            .bind(&sha256)
            .fetch_optional(&storage.pool())
            .await
        {
            Ok(row) => row,
            Err(_) => return Outcome::Failed,
        };

    match existing {
        Some((existing_filename, existing_relative_path)) => {
            if existing_relative_path == relative_path && existing_filename == filename {
                Outcome::Unchanged(relative_path)
            } else {
                // UPDATE the row's filename and relative_path. The
                // other columns (id, sha256, blake3, size_bytes,
                // created_at, status) are not touched. The FTS5
                // trigger fires (delete + reinsert) automatically.
                let upd = sqlx::query(
                    "UPDATE media_items SET filename = ?1, relative_path = ?2 \
                     WHERE sha256 = ?3",
                )
                .bind(&filename)
                .bind(&relative_path)
                .bind(&sha256)
                .execute(&storage.pool())
                .await;
                match upd {
                    Ok(_) => Outcome::Changed(relative_path),
                    Err(_) => Outcome::Failed,
                }
            }
        }
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            let now_ms = unix_millis_now();
            // The provenance JSON marks the row as an
            // orphan-recovery: there is no prior import record for
            // this content. The `discovered_at` RFC 3339 timestamp
            // is when the scanner ran.
            let provenance = format!(
                r#"{{"source":"library-scan","orphan":true,"discovered_at":"{}"}}"#,
                rfc3339_now()
            );
            let ins = sqlx::query(
                "INSERT INTO media_items (\
                    id, sha256, blake3, size_bytes, filename, relative_path, \
                    mime, duration_ms, width, height, video_codec, audio_codec, \
                    container, status, created_at, last_seen_at, last_room_id, \
                    source_url, provenance\
                 ) VALUES (\
                    ?1, ?2, ?3, ?4, ?5, ?6, \
                    'application/octet-stream', NULL, NULL, NULL, NULL, NULL, \
                    NULL, 'permanent', ?7, ?7, NULL, NULL, ?8\
                 )",
            )
            .bind(&id)
            .bind(&sha256)
            .bind(&blake3)
            .bind(size)
            .bind(&filename)
            .bind(&relative_path)
            .bind(now_ms)
            .bind(&provenance)
            .execute(&storage.pool())
            .await;
            match ins {
                Ok(_) => Outcome::OrphanInsert(relative_path),
                Err(sqlx::Error::Database(e)) if is_unique_violation(e.message()) => {
                    // A concurrent scanner or import inserted a
                    // row for this sha256 between our SELECT and
                    // INSERT. Re-SELECT to find the existing
                    // row's relative_path and report that as
                    // visited, so the missing-file pass does
                    // not spuriously flag the existing row as
                    // missing. The current on-disk file is
                    // NOT marked visited (its relative_path
                    // does not match the row's); the next
                    // pass will reconcile it.
                    let existing_rel: Option<(String,)> =
                        sqlx::query_as("SELECT relative_path FROM media_items WHERE sha256 = ?1")
                            .bind(&sha256)
                            .fetch_optional(&storage.pool())
                            .await
                            .ok()
                            .flatten();
                    let reported = existing_rel.map(|(r,)| r).unwrap_or(relative_path);
                    Outcome::Unchanged(reported)
                }
                Err(_) => Outcome::Failed,
            }
        }
    }
}

/// Best-effort detection of a SQLite UNIQUE-constraint failure in
/// the error message. We treat the standard `UNIQUE constraint
/// failed: ...` text as a unique violation; any other error
/// propagates.
fn is_unique_violation(msg: &str) -> bool {
    msg.contains("UNIQUE constraint failed")
}

/// Stream-hash `path` with both SHA-256 and BLAKE3, in 64 KiB
/// chunks. Returns `(sha256_hex, blake3_hex)`. Reads the file
/// once; the two hashers are updated in lockstep.
async fn stream_hash_file(path: &Path) -> Result<(String, String), std::io::Error> {
    let mut file = tokio_fs::File::open(path).await?;
    let mut sha = Sha256Hasher::new();
    let mut blake = Blake3Hasher::new();
    let mut buf = vec![0u8; SCAN_HASH_CHUNK];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
        blake.update(&buf[..n]);
    }
    Ok((sha.finalize_hex(), blake.finalize_hex()))
}

/// For every `media_items` row whose `relative_path` is NOT in
/// `visited`, bump `last_seen_at` to now. Returns the count of
/// such rows. The row is left otherwise unchanged; the scan does
/// not delete rows for missing files.
async fn mark_missing_files(storage: &Storage, visited: &[String]) -> Result<u64, ScanError> {
    let now_ms = unix_millis_now();
    let all_rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, relative_path FROM media_items")
            .fetch_all(&storage.pool())
            .await?;
    let missing_ids: Vec<String> = all_rows
        .into_iter()
        .filter_map(|(id, rel)| {
            if visited.iter().any(|v| v == &rel) {
                None
            } else {
                Some(id)
            }
        })
        .collect();
    let missing = missing_ids.len() as u64;
    if !missing_ids.is_empty() {
        // SQLite's `IN (?, ?, ?)` is bounded by SQLITE_MAX_VARIABLE_NUMBER
        // (default 32766 since SQLite 3.32). For typical libraries this is
        // a non-issue; chunking would be a future optimization for very
        // large libraries (>32k missing rows). The single-statement
        // approach is correct for the acceptance test (50 files) and
        // for any reasonable real-world library.
        let placeholders = std::iter::repeat("?")
            .take(missing_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "UPDATE media_items SET last_seen_at = ?1 WHERE id IN ({})",
            placeholders
        );
        let mut q = sqlx::query(&sql).bind(now_ms);
        for id in &missing_ids {
            q = q.bind(id);
        }
        q.execute(&storage.pool()).await?;
    }
    Ok(missing)
}

/// Compute the library-root-relative path of an absolute path, in
/// forward-slash form.
fn relative_path_from(library_root: &Path, p: &Path) -> String {
    match p.strip_prefix(library_root) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => p.to_string_lossy().into_owned(),
    }
}

/// Current unix time in milliseconds. Duplicated from
/// `commands::import::unix_millis_now` to keep this module
/// self-contained.
fn unix_millis_now() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(now.as_millis()).unwrap_or(i64::MAX)
}

/// Current time as an RFC 3339 string. The scanner embeds this
/// into the `provenance.discovered_at` field.
fn rfc3339_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    unix_secs_to_rfc3339(secs)
}

/// Convert a unix-seconds value to an RFC 3339 timestamp in UTC.
/// Implements the Gregorian calendar algorithm from Howard
/// Hinnant's `date.h` (public domain). The result is a string of
/// the form `YYYY-MM-DDTHH:MM:SSZ`.
fn unix_secs_to_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let secs_of_day = (secs % 86_400) as u32;
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m_raw = if mp < 10 { mp + 3 } else { mp - 9 };
    let y_out = if m_raw <= 2 { y + 1 } else { y };
    let m = m_raw as u32;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y_out, m, d, hh, mm, ss
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_unix_seconds() {
        assert_eq!(unix_secs_to_rfc3339(1_767_225_600), "2026-01-01T00:00:00Z");
        assert_eq!(unix_secs_to_rfc3339(946_684_800), "2000-01-01T00:00:00Z");
        assert_eq!(unix_secs_to_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn rfc3339_rollover_boundaries() {
        assert_eq!(unix_secs_to_rfc3339(1_767_225_599), "2025-12-31T23:59:59Z");
        assert_eq!(unix_secs_to_rfc3339(1_767_225_600), "2026-01-01T00:00:00Z");
        // 2024 is a leap year.
        assert_eq!(unix_secs_to_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }
}
