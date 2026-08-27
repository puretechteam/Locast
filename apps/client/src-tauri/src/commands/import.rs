//! `media_import` - the user-driven "import a file into the library" IPC.
//!
//! P1-T04 adds the first non-trivial command. The roadmap's acceptance:
//!
//! > an integration test (or a manual Tauri dev session) imports two
//! > files with identical bytes; the second one dedupes via
//! > hardlink/copy and the `media_items` table contains two rows
//! > pointing to the same on-disk file; the TS binding types match.
//!
//! P0-T05's `media_items.relative_path` is `UNIQUE COLLATE NOCASE`, so
//! two rows that point at the same on-disk file (and therefore share a
//! `relative_path`) would collide on the unique index. The P1-T04
//! contract resolves this: the dedup check runs BEFORE the copy, and a
//! hit returns the existing row's data. The database therefore contains
//! exactly one `media_items` row per unique content, but the TS layer
//! receives one `ImportedMedia` per input path - the duplicate simply
//! gets the same `id` and `relative_path` back as the first import.
//! That is what the tests assert and what the integration test's
//! "two rows pointing to the same on-disk file" wording is interpreted
//! to mean: two `ImportedMedia` returns, one row, one on-disk file.
//!
//! # I/O shape
//!
//! One read pass over the source for the dual SHA-256 + BLAKE3 hash.
//! On a dedup miss, a second read pass streams the source into
//! `tmp/staging/<import-id>/<sha>.partial`. The I/O shape is therefore:
//!
//! - Dedup hit: 1 read, 0 writes, 0 renames, 0 inserts.
//! - Dedup miss: 2 reads (hash + copy), 1 write (staging), 1 rename,
//!   1 insert.
//!
//! No file is read or written more than twice. Consolidating the hash
//! and copy into a single read pass is a future optimization (open
//! the source, tee each chunk into both the hashers and a `BufWriter`
//! over the staging file); P1-T04 keeps the two passes separate for
//! code clarity.
//!
//! # Order of operations (the dedup short-circuit matters)
//!
//! 1. Validate source path and filename.
//! 2. Hash (one read pass).
//! 3. Dedup check: `SELECT ... FROM media_items WHERE sha256 = ?1`.
//!    If a row exists, return its data. NO COPY, NO INSERT.
//! 4. Otherwise: copy to staging, call `library::fs::complete_download`
//!    to atomically rename into the content-addressed path, then
//!    `INSERT` the `media_items` row.
//!
//! # `AppError` location
//!
//! P1-T04 originally declared `AppError` in this module. P2-T01
//! (identity) is the second IPC consumer, so the type has been
//! extracted to `commands::error` and re-exported here so every
//! existing `use crate::commands::import::AppError` site continues
//! to compile unchanged. The import-side error mapping (`From` impls
//! for `FsError`, `StorageError`, `PathError`, `QuotaError`,
//! `sqlx::Error`, `sanitize::InvalidFilename`) lives alongside the
//! type in `commands::error`.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State as TauriState;
use tokio::fs as tokio_fs;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use crate::core::hashing::{Blake3Hasher, Sha256Hasher};
use crate::core::library::sanitize;
use crate::core::paths;
use crate::core::quota::QuotaAccountant;
use crate::library::fs as library_fs;
use crate::probe::ffprobe::{self, ProbeResult};
use crate::storage::Storage;

// Re-export the closed `AppError` set from its new home. The old
// `commands::import::AppError` import path stays valid for every
// existing caller; new code should prefer `crate::commands::error::AppError`.
pub use crate::commands::error::AppError;

/// Scratch buffer size for streaming copy + hash. 64 KiB is a balance:
/// large enough to amortize syscall overhead on large files, small
/// enough not to dominate memory for many parallel imports.
const COPY_CHUNK: usize = 64 * 1024;

/// One successfully-imported media file, returned to the webview.
///
/// `relative_path` is the library-root-relative path of the on-disk
/// file (the content-addressed path). Two imports of identical bytes
/// share the same `relative_path`; the dedup short-circuit in
/// `import_one` returns the existing row's `id` and `relative_path`
/// rather than inserting a second `media_items` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ImportedMedia {
    pub id: String,
    pub sha256: String,
    pub blake3: String,
    /// The byte count of the on-disk file. Stored as i64 in
    /// SQLite (matches `media_items.size_bytes`) but exposed to
    /// TypeScript as `number`. JavaScript numbers can represent
    /// values up to 2^53 - 1; files larger than that (~9 PiB)
    /// will be truncated on the wire. The architecture's
    /// `media_items.size_bytes` is a `CHECK (size_bytes >= 0)`
    /// INTEGER; for a desktop media library, 2^53 bytes is
    /// vastly more than any plausible file size.
    #[specta(type = specta_typescript::Number)]
    pub size_bytes: i64,
    pub filename: String,
    pub relative_path: String,
}

/// Tauri command: import one or more files into the local library.
///
/// The command is a thin wrapper around [`import_one`]. It is kept
/// separate so the test suite can call `import_one` directly without
/// going through the Tauri runtime.
#[tauri::command]
#[specta::specta]
pub async fn media_import(
    storage: TauriState<'_, Storage>,
    accountant: TauriState<'_, QuotaAccountant>,
    paths: Vec<String>,
) -> Result<Vec<ImportedMedia>, AppError> {
    let library_root = resolve_library_root(&storage).await?;
    let mut out = Vec::with_capacity(paths.len());
    for raw in paths {
        let source = PathBuf::from(&raw);
        let display_filename = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let imported = import_one(
            accountant.inner(),
            &library_root,
            storage.inner(),
            &source,
            &display_filename,
        )
        .await?;
        out.push(imported);
    }
    Ok(out)
}

/// Import a single file into the library.
///
/// Public so the integration test in `tests/media_import.rs` can call
/// it without spinning up a Tauri runtime. The Tauri command
/// `media_import` is a thin per-path loop over this function.
///
/// # Steps (P1-T05: per-library-root mutex + quota check; P1-T06: optional probe)
///
/// 1. Validate the source path (must exist and be a regular file).
/// 2. Sanitize the destination filename.
/// 3. **Acquire the per-library-root mutex.** Two concurrent
///    `import_one` calls against the same library serialize here.
///    The mutex is held until the end of the function and released
///    by the guard's `Drop` on every return path.
/// 4. Hash the source in 64 KiB chunks (SHA-256 + BLAKE3 in lockstep).
/// 5. Quota check: `used + size_bytes <= cap`. Refusal returns
///    `AppError::QuotaExceeded { used, cap, needed }`. The
///    `used` and `cap` are computed inside the critical section so
///    they reflect the current state.
/// 6. Dedup check against `media_items`; on hit, return the existing
///    row's data without writing or inserting. The quota is NOT
///    charged again for the duplicate (the existing row already
///    contributes its `size_bytes` to the `SUM(size_bytes)`).
/// 7. On miss: copy the source to `<root>/tmp/staging/<id>/<sha>.partial`,
///    call `library::fs::complete_download` to atomically rename to
///    the content-addressed path.
/// 8. **Optional ffmpeg probe (P1-T06).** Best-effort; on any
///    failure (missing executable, timeout, malformed JSON) the
///    probe returns `None` and the six optional columns stay `NULL`.
/// 9. Insert the `media_items` row.
pub async fn import_one(
    accountant: &QuotaAccountant,
    library_root: &Path,
    storage: &Storage,
    source: &Path,
    display_filename: &str,
) -> Result<ImportedMedia, AppError> {
    // ----- 1. Source validation. Existence and type only; we do NOT
    //           canonicalize the source against the library root, because
    //           the source is allowed to live anywhere on disk - that is
    //           the whole point of an import. The library-root
    //           containment check is enforced on the STAGING side via
    //           `library::fs::complete_download`.
    let meta = tokio_fs::metadata(source).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::SourceMissing {
                path: source.to_string_lossy().into_owned(),
            }
        } else {
            AppError::InvalidPath {
                path: source.to_string_lossy().into_owned(),
                message: e.to_string(),
            }
        }
    })?;
    if !meta.is_file() {
        return Err(AppError::SourceMissing {
            path: source.to_string_lossy().into_owned(),
        });
    }

    // ----- 2. Sanitize the destination filename.
    let sanitized = sanitize::sanitize(display_filename)?;

    // ----- 3. Acquire the per-library-root critical-section lock.
    //           P1-T05 closes the P1-T04 race: the dedup SELECT -> copy
    //           -> rename -> INSERT critical section is now serialized
    //           by a per-library-root `tokio::sync::Mutex<()>`. The
    //           guard is dropped (releasing the lock) on every return
    //           path below, including the quota refusal and the
    //           dedup-hit short-circuit.
    // Hold the per-library-root mutex across the entire critical
    // section: hash -> dedup SELECT -> quota check -> (on miss) copy
    // -> rename -> INSERT. The guard's Drop releases the lock when
    // this function returns (Ok or Err). The guard binding must not
    // be dropped early; the leading underscore suppresses the
    // unused-variable lint while keeping the binding alive.
    let (_guard, canonical_root) = accountant.lock_for_library(library_root).await?;

    // ----- 4. Hash in 64 KiB chunks. One read pass; same bytes go
    //           into both hashers. Holding the mutex through the
    //           hash is intentional: it prevents two concurrent
    //           imports of the same source from racing past the
    //           dedup check.
    let (size_bytes, sha256, blake3) = hash_file(source).await?;

    // ----- 5. Dedup short-circuit. Runs BEFORE the quota check so
    //           that a duplicate import does NOT consume additional
    //           quota. The existing row already contributes its
    //           `size_bytes` to the SUM, so `used` is unchanged by
    //           the dedup hit. The P0-T05 schema has a UNIQUE index
    //           on `relative_path` (NOCASE), so a second import of
    //           identical bytes must NOT attempt a second INSERT.
    if let Some(existing) = lookup_by_sha(storage, &sha256).await? {
        return Ok(existing);
    }

    // ----- 6. Quota check. Runs only on a dedup MISS so that a
    //           duplicate import is not refused for quota. The
    //           `used` and `cap` are computed inside the critical
    //           section so they reflect the current state.
    let used = accountant.compute_used_bytes(&canonical_root).await?;
    let cap = accountant.cap_bytes().await?;
    if used.saturating_add(size_bytes) > cap {
        return Err(AppError::QuotaExceeded {
            used,
            cap,
            needed: size_bytes,
        });
    }

    // ----- 7. On miss: stage, complete, insert. The mutex is still
    //           held; no other `import_one` against this library can
    //           race past the dedup SELECT or the rename.
    //
    // Failure-cleanup contract (P1-T04, documented for future P1-T05):
    //
    // - If `stage_source` fails, the staging directory may or may not
    //   exist; we have not yet written the partial file. No orphan is
    //   left on disk.
    // - If `complete_download` fails after `stage_source` succeeded,
    //   the partial file is left at
    //   `<library_root>/tmp/staging/<import-id>/<sha>.partial`. P1-T04
    //   does NOT clean it up. P1-T05's staging-purge task will remove
    //   orphans older than 30 days on startup (per architecture
    //   section 6 / 22). The architecture's "leave the partial in
    //   staging for the next startup to clean up" guarantee applies
    //   here.
    // - If `insert_media_item` fails after `complete_download`
    //   succeeded, the content-addressed on-disk file is present but
    //   no `media_items` row exists for it. P1-T04 does NOT roll back
    //   the on-disk file. P1-T07's library scanner detects this on
    //   startup (a file under `library/<sha[0..2]>/<sha[2..4]>/<sha>/`
    //   with no DB row) and inserts the missing row, so the orphan is
    //   eventually self-healed. The architecture's "the sidecar is
    //   never trusted over the DB; the DB is authoritative when
    //   present" implies that the scanner can rebuild rows from the
    //   filesystem state.
    let import_id = Uuid::new_v4();
    let staged = stage_source(library_root, &import_id, &sha256, source).await?;

    let final_path =
        library_fs::complete_download(library_root, &sha256, &staged, &sanitized).await?;
    let relative_path = relative_path_from(library_root, &final_path);

    // ----- 8. P1-T06: optional ffprobe sidecar. The probe is
    //           best-effort: any failure (no ffmpeg on PATH, subprocess
    //           timeout, nonzero exit, malformed JSON) is swallowed and
    //           yields an all-None `ProbeResult`. The six optional
    //           columns then stay NULL. The probe runs on the final
    //           on-disk file so the content-addressed path is what
    //           the React side will later serve via `locast://`.
    let probe_result = ffprobe::run(&final_path).await;
    let ProbeResult {
        duration_ms,
        width,
        height,
        video_codec,
        audio_codec,
        container,
    } = probe_result.unwrap_or_default();

    let now_ms = unix_millis_now();
    let id = Uuid::new_v4().to_string();
    let provenance = r#"{"source":"user-import"}"#;

    insert_media_item(
        storage,
        &id,
        &sha256,
        &blake3,
        size_bytes,
        &sanitized,
        &relative_path,
        "permanent",
        now_ms,
        provenance,
        duration_ms,
        width,
        height,
        video_codec.as_deref(),
        audio_codec.as_deref(),
        container.as_deref(),
    )
    .await?;

    Ok(ImportedMedia {
        id,
        sha256,
        blake3,
        size_bytes,
        filename: sanitized,
        relative_path,
    })
}

/// Hash a file with both SHA-256 and BLAKE3, streaming in 64 KiB
/// chunks. Returns `(size_bytes, sha256_hex, blake3_hex)`.
async fn hash_file(source: &Path) -> Result<(i64, String, String), AppError> {
    let mut file = tokio_fs::File::open(source)
        .await
        .map_err(|e| AppError::Read {
            message: e.to_string(),
        })?;
    let mut sha = Sha256Hasher::new();
    let mut blake = Blake3Hasher::new();
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut total: i64 = 0;
    loop {
        let n = file.read(&mut buf).await.map_err(|e| AppError::Read {
            message: e.to_string(),
        })?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
        blake.update(&buf[..n]);
        total += n as i64;
    }
    let sha_hex = sha.finalize_hex();
    let blake_hex = blake.finalize_hex();
    Ok((total, sha_hex, blake_hex))
}

/// Copy `source` into `<library_root>/tmp/staging/<import_id>/<sha>.partial`.
/// Returns the path to the staged file. The staged file is then passed
/// to `library::fs::complete_download`, which performs the atomic
/// rename to the content-addressed path.
///
/// `sha` is used in the filename purely for human-readability; the
/// importer UUID is the uniqueness key.
async fn stage_source(
    library_root: &Path,
    import_id: &Uuid,
    sha: &str,
    source: &Path,
) -> Result<PathBuf, AppError> {
    let staged = paths::staging_partial_path(library_root, &import_id.to_string(), sha)?;
    if let Some(parent) = staged.parent() {
        tokio_fs::create_dir_all(parent)
            .await
            .map_err(|e| AppError::Read {
                message: e.to_string(),
            })?;
    }
    let mut src = tokio_fs::File::open(source)
        .await
        .map_err(|e| AppError::Read {
            message: e.to_string(),
        })?;
    let mut dst = tokio_fs::File::create(&staged)
        .await
        .map_err(|e| AppError::Read {
            message: e.to_string(),
        })?;
    tokio::io::copy(&mut src, &mut dst)
        .await
        .map_err(|e| AppError::Read {
            message: e.to_string(),
        })?;
    dst.sync_all().await.map_err(|e| AppError::Read {
        message: e.to_string(),
    })?;
    Ok(staged)
}

/// Look up an existing `media_items` row by SHA-256. Returns `Some`
/// when a row exists, with the projection needed to fill in
/// `ImportedMedia`. Returns `None` when no row exists. The `blake3`
/// and `size_bytes` returned here are the values stored in the
/// original row; the duplicate import returns them as-is.
async fn lookup_by_sha(storage: &Storage, sha: &str) -> Result<Option<ImportedMedia>, AppError> {
    let row = sqlx::query_as::<_, (String, String, String, i64, String, String)>(
        "SELECT id, sha256, blake3, size_bytes, filename, relative_path \
         FROM media_items WHERE sha256 = ?1",
    )
    .bind(sha)
    .fetch_optional(&storage.pool())
    .await?;
    Ok(row.map(
        |(id, sha256, blake3, size_bytes, filename, relative_path)| ImportedMedia {
            id,
            sha256,
            blake3,
            size_bytes,
            filename,
            relative_path,
        },
    ))
}

/// Insert a `media_items` row. All fields are bound individually so a
/// future schema change is a one-line diff here. The six optional
/// probe-derived fields (`duration_ms`, `width`, `height`,
/// `video_codec`, `audio_codec`, `container`) are bound separately so
/// the call site can pass `None` cleanly when the probe is unavailable.
#[allow(clippy::too_many_arguments)]
async fn insert_media_item(
    storage: &Storage,
    id: &str,
    sha256: &str,
    blake3: &str,
    size_bytes: i64,
    filename: &str,
    relative_path: &str,
    status: &str,
    now_ms: i64,
    provenance: &str,
    duration_ms: Option<i64>,
    width: Option<i32>,
    height: Option<i32>,
    video_codec: Option<&str>,
    audio_codec: Option<&str>,
    container: Option<&str>,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO media_items (\
            id, sha256, blake3, size_bytes, filename, relative_path, \
            mime, duration_ms, width, height, video_codec, audio_codec, \
            container, status, created_at, last_seen_at, last_room_id, \
            source_url, provenance\
         ) VALUES (\
            ?1, ?2, ?3, ?4, ?5, ?6, \
            'application/octet-stream', ?7, ?8, ?9, ?10, ?11, ?12, \
            ?13, ?14, ?14, NULL, NULL, ?15\
         )",
    )
    .bind(id)
    .bind(sha256)
    .bind(blake3)
    .bind(size_bytes)
    .bind(filename)
    .bind(relative_path)
    .bind(duration_ms)
    .bind(width)
    .bind(height)
    .bind(video_codec)
    .bind(audio_codec)
    .bind(container)
    .bind(status)
    .bind(now_ms)
    .bind(provenance)
    .execute(&storage.pool())
    .await?;
    Ok(())
}

/// Resolve the library root for the Tauri command. The library root
/// is the parent of the storage file: the SQLite file lives at
/// `<library_root>/index.sqlite` per the architecture (section 7), so
/// the library root IS the parent of the SQLite path. This matches
/// `commands::quota::resolve_library_root` so that `media_import`,
/// `quota_get`, and `quota_set` all agree on the same root. P1-T04's
/// earlier `<app_data_dir>/library` convention was a transient mistake
/// corrected by P1-T05: the on-disk content-addressed path is already
/// `<library_root>/library/<sha>/<file>` (per the architecture), so
/// adding an extra `/library` segment above would nest the content
/// two levels deep. The Settings UI is the future task that lets the
/// user pick a different root.
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

/// Compute the library-root-relative path of a final on-disk file.
/// Both arguments are already-canonical-form paths.
fn relative_path_from(library_root: &Path, final_path: &Path) -> String {
    match final_path.strip_prefix(library_root) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => final_path.to_string_lossy().into_owned(),
    }
}

/// Current unix time in milliseconds. Wrapped in a helper so the
/// tests can substitute a clock later if needed; for now it is
/// straightforward `SystemTime`.
fn unix_millis_now() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(now.as_millis()).unwrap_or(i64::MAX)
}
