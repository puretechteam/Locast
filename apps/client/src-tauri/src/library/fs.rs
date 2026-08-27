//! `library::fs` - filesystem operations against the library root.
//!
//! P1-T02 implements the atomic completion of a download: take a
//! verified partial file under `tmp/staging/<download-id>/<sha>.partial`,
//! sanitize the user-supplied destination filename, and rename the
//! partial to its final content-addressed path
//! `library/<sha[0..2]>/<sha[2..4]>/<sha>/<sanitized>`. The rename is
//! `std::fs::rename`.
//!
//! # Atomicity, by platform
//!
//! `std::fs::rename` is atomic at the OS level for renames within the
//! same filesystem:
//!
//! - **POSIX:** `rename(2)` is atomic by definition.
//! - **Windows (Rust 1.65+):** `std::sys::pal::windows::fs::rename`
//!   calls `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`, which is
//!   atomic at the file-system level BUT **silently overwrites** an
//!   existing destination. The Windows rename is not "fail if exists";
//!   the Rust stdlib documents this as "replacing the original file if
//!   `to` already exists".
//!
//! The P1-T02 implementation therefore has TWO layers of concurrency
//! safety:
//!
//! 1. A pre-check (`fs::try_exists`) on the destination. Under sequential
//!    calls this catches every duplicate. The test
//!    `second_call_with_same_args_is_rejected` pins this.
//! 2. On POSIX, the OS-level `rename(2)` rejects the second concurrent
//!    attempt with `EEXIST`/`ENOTEMPTY`, surfaced as `FsError::Rename`.
//!    On Windows, the OS-level `MoveFileExW` SILENTLY OVERWRITES; a
//!    truly-concurrent second call on Windows can therefore return
//!    `Ok` and clobber the first call's destination bytes. The test
//!    `concurrent_complete_download_does_not_panic_and_is_deterministic`
//!    documents this and asserts the deterministic count of successes
//!    on each platform.
//!
//! P1-T05 adds a per-library-root `tokio::sync::Mutex` that serializes
//! completion calls. The mutex is out of scope for P1-T02 (it is the
//! disk-quota task's concern, since the same lock will guard the quota
//! check).
//!
//! # Security
//!
//! The library-root containment check is mandatory: a caller MUST NOT be
//! able to trick the routine into writing outside the user-chosen
//! library directory. We do this in two layers:
//!
//! 1. `core::paths` validates every user-supplied component (sha is 64
//!    lowercase hex; sanitized filenames have no separators; download ids
//!    are uuid-shaped) before any path is constructed. A properly-built
//!    content-addressed path can therefore never contain `..` or `/`.
//! 2. `complete_download` canonicalizes the library root AND the staged
//!    source (resolving any symlinks) and asserts the source's
//!    canonical form is `starts_with` the canonical root. The
//!    destination's parent directory is canonicalized after
//!    `create_dir_all` and also asserted to be under the canonical
//!    root. This is defense in depth: even if the caller passes a
//!    hand-crafted `src` that already exists somewhere on the disk,
//!    the rename will be rejected unless `src` is genuinely inside
//!    the library root.
//!
//! # Failure semantics
//!
//! On any error after the staged source has been identified, the
//! staged source is left in place. The architecture (section 6,
//! "Atomic completion") is explicit: if the rename fails, the partial
//! file is left in staging for the next startup to clean up. P1-T02
//! does NOT delete the staged source on failure.
//!
//! On a SUCCESSFUL rename, the staged source no longer exists at the
//! path the caller passed in (the file has been moved). The caller
//! must use the returned `PathBuf` for any further work.
//!
//! # Concurrency
//!
//! The pre-check for the destination's existence (`try_exists`) is
//! best-effort and racy under true concurrency: two tasks can both
//! see "doesn't exist" and both attempt the rename. On POSIX the OS
//! rename(2) rejects the second attempt with `EEXIST`, surfaced as
//! `FsError::Rename`. On Windows the OS `MoveFileExW` overwrites
//! silently (see "Atomicity, by platform" above). P1-T05 adds a
//! per-library-root mutex to serialize completion; for P1-T02 the
//! integration test uses `tokio::join!` to assert that two truly
//! concurrent calls do not panic and that the outcome (one Ok + one
//! Err on POSIX, two Ok-with-last-write-wins on Windows) is
//! deterministic. See the test's docstring.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::fs;

use crate::core::library::sanitize::{self, InvalidFilename};
use crate::core::paths::{self, PathError};

/// Errors raised by `library::fs` operations.
#[derive(Debug, Error)]
pub enum FsError {
    /// The library root does not exist or is not a directory.
    #[error("library root is not a usable directory: {0}")]
    LibraryRootInvalid(#[source] std::io::Error),

    /// `sha` was not 64 lowercase hex characters. Carries the
    /// underlying `PathError` which carries the offending input.
    #[error(transparent)]
    InvalidSha(#[from] PathError),

    /// The user-supplied destination filename failed sanitization.
    #[error("invalid filename: {0}")]
    InvalidFilename(#[from] InvalidFilename),

    /// The staged source file does not exist or is not a regular file.
    #[error("staging source missing or not a regular file: {0}")]
    StagingSourceMissing(#[source] std::io::Error),

    /// After canonicalization, a path was outside the library root.
    /// This is a containment violation; it can only be triggered by a
    /// caller passing a `src` or `dst` that escapes the root.
    #[error("path escapes the library root")]
    PathEscapesLibrary,

    /// The destination already exists. P1-T02 does not permit
    /// overwriting; a second completion attempt (concurrent or
    /// sequential) is rejected. Note: on Windows the underlying rename
    /// overwrites silently, so under true concurrency this variant is
    /// only returned for the pre-check. See the module docstring.
    #[error("destination already exists")]
    DestinationAlreadyExists,

    /// Creating the destination's parent directory failed.
    #[error("failed to create destination parent directory: {0}")]
    CreateDir(#[source] std::io::Error),

    /// The atomic rename failed. The staged source has been left in
    /// place; the next startup can clean it up.
    #[error("atomic rename failed: {0}")]
    Rename(#[source] std::io::Error),
}

/// Atomically complete a download: take the verified partial file at
/// `src` and rename it to its final content-addressed path under
/// `<library_root>/library/<sha[0..2]>/<sha[2..4]>/<sha>/<sanitized>`.
///
/// # Arguments
///
/// * `library_root` - the user-chosen library directory. Must exist
///   and be a directory.
/// * `sha` - 64 lowercase hex characters. SHA-256 of the file's bytes.
/// * `src` - the staged partial file, conventionally
///   `<library_root>/tmp/staging/<download-id>/<sha>.partial`. Must
///   exist and be a regular file. After canonicalization it must be
///   inside `library_root`; otherwise the call returns
///   `FsError::PathEscapesLibrary`.
/// * `dst_filename_input` - the user-typed filename for the file.
///   This is passed through `core::library::sanitize::sanitize` before
///   being used as the final on-disk filename.
///
/// # Returns
///
/// The destination `PathBuf` on success. On any error the staged
/// source is left in place; the destination has not been created.
pub async fn complete_download(
    library_root: &Path,
    sha: &str,
    src: &Path,
    dst_filename_input: &str,
) -> Result<PathBuf, FsError> {
    // ----- 1. Validate library_root. We canonicalize after the
    //           existence/type check so the user gets a clear
    //           `LibraryRootInvalid` instead of a `NotFound` from
    //           canonicalize for a non-existent path.

    let root_meta = fs::metadata(library_root)
        .await
        .map_err(FsError::LibraryRootInvalid)?;
    if !root_meta.is_dir() {
        return Err(FsError::LibraryRootInvalid(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "library root is not a directory",
        )));
    }

    let canonical_root = fs::canonicalize(library_root)
        .await
        .map_err(FsError::LibraryRootInvalid)?;

    // ----- 2. Validate sha (delegate to core::paths so the rules
    //           stay in one place).
    paths::validate_sha(sha)?;

    // ----- 3. Validate src exists and is a regular file.
    let src_meta = fs::metadata(src)
        .await
        .map_err(FsError::StagingSourceMissing)?;
    if !src_meta.is_file() {
        return Err(FsError::StagingSourceMissing(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "staging source is not a regular file",
        )));
    }

    // ----- 4. Library-root containment for src. Canonicalize
    //           (resolves symlinks) and assert the canonical form
    //           is under the canonical root.
    let canonical_src = fs::canonicalize(src)
        .await
        .map_err(FsError::StagingSourceMissing)?;
    if !assert_within(&canonical_root, &canonical_src) {
        return Err(FsError::PathEscapesLibrary);
    }

    // ----- 5. Sanitize the user-typed destination filename.
    let sanitized = sanitize::sanitize(dst_filename_input)?;

    // ----- 6. Construct the final path. core::paths::content_addressed_path
    //           re-validates sha (already validated above) and the
    //           sanitized filename (already validated by the
    //           sanitizer); it cannot fail given the prior checks, so
    //           the .expect is a programmer-error tripwire rather than
    //           a runtime branch.
    let dst = paths::content_addressed_path(library_root, sha, &sanitized)
        .expect("content_addressed_path: sha and sanitized filename were validated above");

    // ----- 7. Defense-in-depth containment: the destination's parent
    //           is constructed entirely from validated components
    //           (sha hex + literal "library"/sha segments), so the
    //           parent cannot escape the root. We still canonicalize
    //           the parent after create_dir_all and assert it is
    //           under the canonical root, so a symlink planted inside
    //           the library root cannot redirect the write.
    let parent = dst
        .parent()
        .ok_or_else(|| {
            FsError::CreateDir(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "destination has no parent directory",
            ))
        })?
        .to_path_buf();
    fs::create_dir_all(&parent)
        .await
        .map_err(FsError::CreateDir)?;
    let canonical_parent = fs::canonicalize(&parent)
        .await
        .map_err(FsError::CreateDir)?;
    if !assert_within(&canonical_root, &canonical_parent) {
        return Err(FsError::PathEscapesLibrary);
    }

    // ----- 8. Refuse to overwrite an existing valid library file.
    //           The existence pre-check is racy under true concurrency;
    //           the rename itself is atomic and will fail the second
    //           concurrent attempt on POSIX. On Windows the OS
    //           `MoveFileExW` overwrites silently; the serialized
    //           mutex is P1-T05's concern. See module docs.
    if fs::try_exists(&dst).await.unwrap_or(false) {
        return Err(FsError::DestinationAlreadyExists);
    }

    // ----- 9. Atomic completion.
    fs::rename(src, &dst).await.map_err(FsError::Rename)?;

    Ok(dst)
}

/// Return true iff `canonical_path` is inside `canonical_root`.
///
/// Both arguments must already be canonicalized (via `std::fs::canonicalize`
/// or equivalent) — this is a `Path::starts_with` comparison on the
/// canonical forms and does not re-resolve symlinks. The function is
/// used by `complete_download` for its library-root containment checks
/// and is `pub` so integration tests can verify the contract directly.
pub fn assert_within(canonical_root: &Path, canonical_path: &Path) -> bool {
    canonical_path.starts_with(canonical_root)
}
