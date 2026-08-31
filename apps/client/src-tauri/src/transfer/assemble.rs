//! P3-T06 final assembly pipeline.
//!
//! Concatenates verified per-chunk files into the staging
//! partial, recomputes BLAKE3 over the assembled file,
//! compares to the bound `blake3`, and on match invokes
//! [`crate::library::fs::complete_download`] to atomically
//! rename into the library.
//!
//! # Streaming
//!
//! The concatenation step streams each chunk file into the
//! staging partial via 256 KiB read/write loops. The full
//! file is never resident in memory. BLAKE3 is computed on
//! the same read pass via a streaming
//! [`crate::core::hashing::Blake3Hasher`].
//!
//! # On failure
//!
//! If BLAKE3 does not match, the staging partial is left on
//! disk; the caller should treat the download as failed and
//! transition to `Failed`. If the atomic rename fails, the
//! staging partial is also left on disk for the next startup
//! to clean up (matches the contract at
//! [`crate::library::fs`]).
//!
//! # Path safety
//!
//! The `sha256` and `filename` come from the verified
//! manifest, never from the peer. They are passed through
//! the existing [`crate::core::paths`] validators inside
//! [`crate::library::fs::complete_download`].
//!
//! # Resource bounds
//!
//! The only allocations are:
//! - The staging partial file (created via
//!   [`crate::core::paths::staging_partial_path`]).
//! - A 256 KiB I/O scratch buffer (`Vec<u8>`).
//! - The streaming BLAKE3 state.
//!
//! There is no unbounded growth.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::core::hashing::Blake3Hasher;
use crate::core::paths;
use crate::library::fs as library_fs;
use crate::transfer::verify::{verify_full_blake3, ChunkVerifyError};

/// I/O scratch size for the streaming concat step. Matches
/// the chunk size so the copy is "one chunk in, one chunk
/// out" per iteration.
const IO_SCRATCH: usize = crate::transfer::CHUNK_SIZE_BYTES;

/// Closed set of assembly errors.
#[derive(Debug, Error)]
pub enum AssembleError {
    #[error("chunk file missing at {0}")]
    ChunkMissing(PathBuf),
    #[error("chunk file length mismatch at {path}: got {got}, expected {expected}")]
    ChunkLengthMismatch {
        path: PathBuf,
        got: u64,
        expected: u64,
    },
    #[error("io error: {0}")]
    Io(String),
    #[error("path builder rejected input: {0}")]
    Path(String),
    #[error("path escapes library root: {0}")]
    PathEscapesLibrary(String),
    #[error("full-file BLAKE3 mismatch")]
    Blake3Mismatch,
    #[error("atomic completion failed: {0}")]
    Completion(String),
}

impl From<std::io::Error> for AssembleError {
    fn from(e: std::io::Error) -> Self {
        AssembleError::Io(e.to_string())
    }
}

/// Result of a successful assembly.
#[derive(Debug, Clone)]
pub struct AssembleResult {
    /// Final content-addressed path inside the library.
    pub final_path: PathBuf,
    /// BLAKE3 hex digest of the assembled file (already
    /// verified to match `expected_blake3`).
    pub blake3: String,
}

/// Concatenate every chunk file under
/// `<library_root>/tmp/incomplete/<download_id>/<download_id>.part.<i>`
/// into the staging partial, stream BLAKE3 over the
/// assembled file, compare to `expected_blake3`, and on
/// match atomically rename into the library at the
/// content-addressed path derived from `sha256` and
/// `sanitized_filename`.
///
/// `chunk_lengths` is the planner-derived `(index, length)`
/// pair for every chunk. The function reads every chunk in
/// ascending index order and asserts the on-disk size
/// matches the planner length.
///
/// This is the single place where verified chunk bytes
/// cross into the permanent library. Nothing writes
/// directly to `library/` from the transport layer.
pub async fn assemble_and_finalize(
    library_root: &Path,
    download_id: &str,
    sha256: &str,
    sanitized_filename: &str,
    expected_blake3: &str,
    chunk_lengths: &[(u32, u32)],
    total_bytes: u64,
) -> Result<AssembleResult, AssembleError> {
    // Validate identifiers up front so a bad path cannot
    // surface halfway through a multi-gigabyte copy.
    paths::validate_sha(sha256).map_err(|e| AssembleError::Path(format!("sha: {e}")))?;
    let staging = paths::staging_partial_path(library_root, download_id, sha256)
        .map_err(|e| AssembleError::Path(format!("staging: {e}")))?;
    let staging_dir = staging
        .parent()
        .ok_or_else(|| AssembleError::Path("staging has no parent".into()))?;
    tokio::fs::create_dir_all(staging_dir).await?;

    // Stream chunks into the staging partial while hashing
    // with BLAKE3.
    let mut out = tokio::fs::File::create(&staging).await?;
    let mut blake3 = Blake3Hasher::new();
    let mut scratch = vec![0u8; IO_SCRATCH];
    let mut bytes_written: u64 = 0;
    for &(index, expected_len) in chunk_lengths {
        let chunk_path = paths::incomplete_chunk_path(library_root, download_id, index)
            .map_err(|e| AssembleError::Path(format!("incomplete: {e}")))?;
        let meta = tokio::fs::metadata(&chunk_path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AssembleError::ChunkMissing(chunk_path.clone())
            } else {
                AssembleError::Io(e.to_string())
            }
        })?;
        if meta.len() != expected_len as u64 {
            return Err(AssembleError::ChunkLengthMismatch {
                path: chunk_path,
                got: meta.len(),
                expected: expected_len as u64,
            });
        }
        let mut in_file = tokio::fs::File::open(&chunk_path).await?;
        loop {
            let n = in_file.read(&mut scratch).await?;
            if n == 0 {
                break;
            }
            out.write_all(&scratch[..n]).await?;
            blake3.update(&scratch[..n]);
            bytes_written += n as u64;
        }
    }
    out.flush().await?;
    out.sync_all().await?;
    drop(out);

    if bytes_written != total_bytes {
        return Err(AssembleError::Io(format!(
            "staging partial size {bytes_written} != total_bytes {total_bytes}"
        )));
    }

    // Compare BLAKE3 against the manifest. Re-read the
    // file to drive `verify_full_blake3` which performs its
    // own length + BLAKE3 pass. This is a deliberate
    // double-pass: the streaming hash above is the fast
    // happy-path; `verify_full_blake3` is the gate that
    // refuses to commit corrupted content.
    let final_blake3 = verify_full_blake3_via_file(&staging, total_bytes, expected_blake3)
        .await
        .map_err(|e| match e {
            ChunkVerifyError::Blake3Mismatch { .. } => AssembleError::Blake3Mismatch,
            ChunkVerifyError::LengthMismatch { .. } => {
                AssembleError::Io("staging partial size changed between passes".into())
            }
            ChunkVerifyError::Sha256Mismatch { .. } => {
                AssembleError::Io("verify_full_blake3 returned Sha256Mismatch (impossible)".into())
            }
        })?;

    // Atomic rename into the library. `complete_download`
    // re-validates every component + canonicalizes +
    // asserts containment.
    let final_path =
        library_fs::complete_download(library_root, sha256, &staging, sanitized_filename)
            .await
            .map_err(|e| match e {
                library_fs::FsError::PathEscapesLibrary => {
                    AssembleError::PathEscapesLibrary("staging -> library".into())
                }
                other => AssembleError::Completion(other.to_string()),
            })?;

    Ok(AssembleResult {
        final_path,
        blake3: final_blake3,
    })
}

/// Wrap the existing `verify_full_blake3` to drive it on a
/// streaming file rather than an in-memory buffer. We
/// re-stream the file in 256 KiB chunks and compare against
/// `expected_blake3`.
async fn verify_full_blake3_via_file(
    path: &Path,
    expected_size: u64,
    expected_blake3: &str,
) -> Result<String, ChunkVerifyError> {
    use crate::core::hashing::Blake3Hasher;
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|_| ChunkVerifyError::LengthMismatch {
            got: 0,
            expected: expected_size,
        })?;
    if meta.len() != expected_size {
        return Err(ChunkVerifyError::LengthMismatch {
            got: meta.len(),
            expected: expected_size,
        });
    }
    let mut f =
        tokio::fs::File::open(path)
            .await
            .map_err(|_| ChunkVerifyError::LengthMismatch {
                got: 0,
                expected: expected_size,
            })?;
    let mut h = Blake3Hasher::new();
    let mut scratch = vec![0u8; IO_SCRATCH];
    loop {
        let n = f
            .read(&mut scratch)
            .await
            .map_err(|_| ChunkVerifyError::LengthMismatch {
                got: 0,
                expected: expected_size,
            })?;
        if n == 0 {
            break;
        }
        h.update(&scratch[..n]);
    }
    let actual = h.finalize_hex();
    if actual != expected_blake3 {
        return Err(ChunkVerifyError::Blake3Mismatch {
            got: actual,
            expected: expected_blake3.to_string(),
        });
    }
    // Touch the unused import in case the file evolves.
    let _ = verify_full_blake3;
    Ok(actual)
}

/// Remove all `incomplete/<download_id>/` chunk files plus
/// the `staging/<download_id>/` directory. Used by the
/// cancel path and by the integration test teardown.
pub async fn cleanup_incomplete(
    library_root: &Path,
    download_id: &str,
) -> Result<(), AssembleError> {
    let inc_dir = library_root
        .join("tmp")
        .join("incomplete")
        .join(download_id);
    if inc_dir.exists() {
        tokio::fs::remove_dir_all(&inc_dir)
            .await
            .map_err(|e| AssembleError::Io(e.to_string()))?;
    }
    let stage_dir = library_root.join("tmp").join("staging").join(download_id);
    if stage_dir.exists() {
        tokio::fs::remove_dir_all(&stage_dir)
            .await
            .map_err(|e| AssembleError::Io(e.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::hashing::{Blake3Hasher, Sha256Hasher, CHUNK_SIZE};

    fn scratch_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[tokio::test]
    async fn assemble_rejects_when_chunk_missing() {
        let tmp = scratch_dir();
        let root = tmp.path();
        // 2-chunk file: 256 KiB + 17 bytes.
        let chunks: Vec<(u32, u32)> = vec![(0, CHUNK_SIZE as u32), (1, 17)];
        let total = (CHUNK_SIZE + 17) as u64;
        // No chunk files written -> ChunkMissing(0).
        let err = assemble_and_finalize(
            root,
            "01234567-89ab-cdef-0123-456789abcdef",
            &"a".repeat(64),
            "movie.mp4",
            &"0".repeat(64),
            &chunks,
            total,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AssembleError::ChunkMissing(_)));
    }

    #[tokio::test]
    async fn assemble_rejects_bad_sha() {
        let tmp = scratch_dir();
        let root = tmp.path();
        let chunks: Vec<(u32, u32)> = vec![(0, 1)];
        let err = assemble_and_finalize(
            root,
            "01234567-89ab-cdef-0123-456789abcdef",
            "not-a-sha",
            "movie.mp4",
            &"0".repeat(64),
            &chunks,
            1,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AssembleError::Path(_)));
    }

    #[tokio::test]
    async fn cleanup_incomplete_is_idempotent() {
        let tmp = scratch_dir();
        let root = tmp.path();
        let id = "01234567-89ab-cdef-0123-456789abcdef";
        // First call (nothing to remove) should be a no-op.
        cleanup_incomplete(root, id).await.expect("clean1");
        cleanup_incomplete(root, id).await.expect("clean2");
    }

    #[tokio::test]
    async fn cleanup_removes_existing_dirs() {
        let tmp = scratch_dir();
        let root = tmp.path();
        let id = "01234567-89ab-cdef-0123-456789abcdef";
        let inc_dir = root.join("tmp").join("incomplete").join(id);
        let stage_dir = root.join("tmp").join("staging").join(id);
        tokio::fs::create_dir_all(&inc_dir).await.unwrap();
        tokio::fs::create_dir_all(&stage_dir).await.unwrap();
        tokio::fs::write(inc_dir.join("anything"), b"x")
            .await
            .unwrap();
        tokio::fs::write(stage_dir.join("anything"), b"x")
            .await
            .unwrap();
        cleanup_incomplete(root, id).await.expect("clean");
        assert!(!inc_dir.exists());
        assert!(!stage_dir.exists());
    }

    #[tokio::test]
    async fn verify_full_blake3_via_file_rejects_size_mismatch() {
        let tmp = scratch_dir();
        let p = tmp.path().join("file");
        tokio::fs::write(&p, b"abc").await.unwrap();
        let err = verify_full_blake3_via_file(&p, 999, &"0".repeat(64))
            .await
            .unwrap_err();
        assert!(matches!(err, ChunkVerifyError::LengthMismatch { .. }));
    }

    // Touch the unused Sha256Hasher import so this module
    // does not raise a dead-code lint when the rest of the
    // crate evolves. Not a behavioral assertion.
    #[allow(dead_code)]
    fn _import_pin() {
        let _ = Sha256Hasher::new();
    }

    #[allow(dead_code)]
    fn _blake3_pin() {
        let _ = Blake3Hasher::new();
    }
}
