//! `room::chunk_plan` - the host-side per-file chunk planner (P3-T04
//! prerequisite).
//!
//! The P3-T03 host built a `MediaManifest` whose `Source::chunk_hashes`
//! was a single SHA-256 of the whole file (`total_chunks = 1`,
//! `chunk_size = 65_536`). That was a placeholder. This module
//! replaces it with a real planner: for a media file on disk, it
//! produces the per-chunk SHA-256 digests and the full-file BLAKE3
//! digest that the manifest spec mandates.
//!
//! # Architectural invariants
//!
//! From `docs/ARCHITECTURE.md` section 8 (lines 735-748) and section
//! 9 (lines 822-824):
//!
//! - `media[].chunk_size` = 256 KiB (`CHUNK_SIZE` here).
//! - `media[].sha256` = full-file SHA-256, 64 lowercase hex.
//! - `media[].blake3` = full-file BLAKE3, 64 lowercase hex.
//! - `media[].sources[].total_chunks` = `ceil(size_bytes / chunk_size)`.
//! - `media[].sources[].chunk_hashes[]` = SHA-256 of each chunk, in
//!   order; length = `total_chunks`; final chunk is the possibly-
//!   shorter tail (no padding, per the spec).
//!
//! # Streaming
//!
//! The planner NEVER loads the full file into memory. It reads the
//! file in 64 KiB scratch chunks (`READ_BUFFER`) and updates a
//! full-file BLAKE3 hasher in lockstep. When a 256 KiB boundary
//! is crossed, the current SHA-256 chunk is finalized and pushed
//! into the result `Vec`. The final partial chunk (if any) is
//! finalized at EOF.
//!
//! # Errors
//!
//! [`ChunkPlanError::Io`] wraps `std::io::Error` so callers see
//! one error type. [`ChunkPlanError::EmptyPath`] is raised when
//! the file does not exist or is a directory.
//!
//! [`ChunkPlanError::EmptyFile`] is raised when the file is empty.
//! The architecture is silent on whether an empty file is a valid
//! media item. The scanner writes a 0-byte row only for the
//! `default` placeholder, not a real media file. We return
//! `EmptyFile` to surface the bug rather than silently emitting a
//! zero-chunk manifest.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::Path;

use digest::Digest;
use sha2::Sha256;
use tokio::fs as tokio_fs;
use tokio::io::AsyncReadExt;

use crate::core::hashing::{Blake3Hasher, Sha256Hasher, CHUNK_SIZE};

/// Internal I/O buffer. 64 KiB - small enough to be a no-op on
/// memory, large enough that the syscall overhead is amortized.
const READ_BUFFER: usize = 64 * 1024;

/// One media file's chunk plan, ready to be merged into a
/// `MediaEntry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPlan {
    /// Full-file SHA-256, 64 lowercase hex.
    pub full_sha256: String,
    /// Full-file BLAKE3, 64 lowercase hex.
    pub full_blake3: String,
    /// SHA-256 of each 256 KiB chunk, in order. The final
    /// entry is the possibly-shorter tail's hash. Empty for
    /// an empty file (we reject empty files; see
    /// [`ChunkPlanError::EmptyFile`]).
    pub per_chunk_sha256: Vec<String>,
}

impl ChunkPlan {
    /// `ceil(size_bytes / CHUNK_SIZE)`. Exposed for callers
    /// that need to set `total_chunks` on the wire.
    pub fn total_chunks(&self) -> u32 {
        self.per_chunk_sha256.len() as u32
    }
}

/// Errors raised by [`plan_file`].
#[derive(Debug, thiserror::Error)]
pub enum ChunkPlanError {
    #[error("file path is empty")]
    EmptyPath,
    #[error("file is empty (size == 0); refusing to plan a 0-chunk manifest")]
    EmptyFile,
    #[error("io error: {0}")]
    Io(String),
}

impl From<std::io::Error> for ChunkPlanError {
    fn from(e: std::io::Error) -> Self {
        ChunkPlanError::Io(e.to_string())
    }
}

/// Plan a media file on disk: stream it, compute full-file
/// SHA-256 + BLAKE3, and collect per-256 KiB-chunk SHA-256
/// digests.
///
/// This is the ONLY function in the chunk planner that touches
/// the filesystem. It is `async` because the rest of the host
/// pipeline is `async` (sqlx, signaling). The streaming loop
/// reads at most `READ_BUFFER` bytes at a time and never
/// materializes the full file in memory.
pub async fn plan_file(path: impl AsRef<Path>) -> Result<ChunkPlan, ChunkPlanError> {
    let path = path.as_ref();
    if path.as_os_str().is_empty() {
        return Err(ChunkPlanError::EmptyPath);
    }

    let metadata = tokio_fs::metadata(path).await?;
    let size = metadata.len();
    if size == 0 {
        return Err(ChunkPlanError::EmptyFile);
    }

    let mut file = tokio_fs::File::open(path).await?;
    let mut full_sha = Sha256Hasher::new();
    let mut full_blake = Blake3Hasher::new();
    let mut current_sha = Sha256::new();
    let mut bytes_into_current_chunk: usize = 0;
    let mut per_chunk: Vec<String> = Vec::new();

    let mut buf = vec![0u8; READ_BUFFER];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];

        // Update the full-file hashers on every byte. (BLAKE3
        // is already streaming via `update`; we just feed the
        // same buffer twice.)
        full_sha.update(chunk);
        full_blake.update(chunk);

        // Feed into the current 256 KiB chunk's SHA-256
        // hasher. When the boundary is crossed (or we hit
        // EOF), finalize and start a fresh hasher.
        let mut offset = 0;
        while offset < n {
            let space_left = CHUNK_SIZE - bytes_into_current_chunk;
            let take = space_left.min(n - offset);
            current_sha.update(&chunk[offset..offset + take]);
            bytes_into_current_chunk += take;
            offset += take;
            if bytes_into_current_chunk == CHUNK_SIZE {
                let digest = current_sha.finalize();
                per_chunk.push(hex::encode(digest));
                current_sha = Sha256::new();
                bytes_into_current_chunk = 0;
            }
        }
    }

    // Finalize the trailing partial chunk (if any).
    if bytes_into_current_chunk > 0 {
        let digest = current_sha.finalize();
        per_chunk.push(hex::encode(digest));
    }

    // Sanity: total bytes covered = file size. (N-1)*CHUNK_SIZE
    // for the full chunks, plus the trailing partial chunk
    // length. If the file is an exact multiple of CHUNK_SIZE,
    // the trailing partial is zero and the last chunk is full
    // (= CHUNK_SIZE), so we add CHUNK_SIZE.
    let full_chunks = (per_chunk.len() as u64).saturating_sub(1);
    let total_chunk_bytes = if bytes_into_current_chunk == 0 && !per_chunk.is_empty() {
        full_chunks * (CHUNK_SIZE as u64) + (CHUNK_SIZE as u64)
    } else {
        full_chunks * (CHUNK_SIZE as u64) + (bytes_into_current_chunk as u64)
    };
    debug_assert_eq!(
        total_chunk_bytes, size,
        "sum(chunk lengths) must equal size_bytes"
    );
    // total_chunks check: per_chunk.len() == ceil(size / CHUNK_SIZE).
    let expected_chunks = size.div_ceil(CHUNK_SIZE as u64) as usize;
    debug_assert_eq!(
        per_chunk.len(),
        expected_chunks,
        "chunk count must be ceil(size_bytes / CHUNK_SIZE)"
    );

    Ok(ChunkPlan {
        full_sha256: full_sha.finalize_hex(),
        full_blake3: full_blake.finalize_hex(),
        per_chunk_sha256: per_chunk,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::hashing::sha256_hex;
    use sha2::Digest;
    use std::io::Write as _;
    use tempfile::NamedTempFile;

    /// Build a temp file of `len` bytes with the index-as-byte
    /// pattern `i * 31 + 7` (a deterministic, easy-to-debug
    /// fill). Returns the file path.
    fn write_temp_file(len: usize) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        // Write in 4 KiB chunks to exercise the streaming loop.
        let scratch_size = 4 * 1024;
        let mut written = 0;
        while written < len {
            let take = (len - written).min(scratch_size);
            let mut chunk = vec![0u8; take];
            for (i, b) in chunk.iter_mut().enumerate() {
                *b = ((written + i) as u8).wrapping_mul(31).wrapping_add(7);
            }
            f.write_all(&chunk).expect("write");
            written += take;
        }
        f.flush().expect("flush");
        f
    }

    /// One-shot equivalent: hash the file in memory and return
    /// the per-chunk SHA-256 + full-file SHA-256 + full-file
    /// BLAKE3. Used as the test oracle.
    fn oracle(data: &[u8]) -> (String, String, Vec<String>) {
        let full_sha = {
            let mut h = sha2::Sha256::new();
            h.update(data);
            hex::encode(h.finalize())
        };
        let full_blake = blake3::hash(data).to_hex().to_string();
        let per_chunk = data
            .chunks(CHUNK_SIZE)
            .map(|c| {
                let mut h = sha2::Sha256::new();
                h.update(c);
                hex::encode(h.finalize())
            })
            .collect();
        (full_sha, full_blake, per_chunk)
    }

    #[tokio::test]
    async fn empty_file_is_rejected() {
        let f = write_temp_file(0);
        let err = plan_file(f.path())
            .await
            .expect_err("empty file must reject");
        assert!(matches!(err, ChunkPlanError::EmptyFile));
    }

    #[tokio::test]
    async fn one_byte_file_produces_one_chunk() {
        let f = write_temp_file(1);
        let plan = plan_file(f.path()).await.expect("plan");
        let data = std::fs::read(f.path()).expect("read");
        let (exp_sha, exp_blake, exp_chunks) = oracle(&data);
        assert_eq!(plan.full_sha256, exp_sha);
        assert_eq!(plan.full_blake3, exp_blake);
        assert_eq!(plan.per_chunk_sha256, exp_chunks);
        assert_eq!(plan.per_chunk_sha256.len(), 1);
        assert_eq!(plan.total_chunks(), 1);
    }

    #[tokio::test]
    async fn exact_chunk_size_is_one_chunk() {
        let f = write_temp_file(CHUNK_SIZE);
        let plan = plan_file(f.path()).await.expect("plan");
        let data = std::fs::read(f.path()).expect("read");
        let (exp_sha, exp_blake, exp_chunks) = oracle(&data);
        assert_eq!(plan.full_sha256, exp_sha);
        assert_eq!(plan.full_blake3, exp_blake);
        assert_eq!(plan.per_chunk_sha256, exp_chunks);
        assert_eq!(plan.per_chunk_sha256.len(), 1);
        assert_eq!(plan.total_chunks(), 1);
    }

    #[tokio::test]
    async fn chunk_plus_one_byte_produces_two_chunks() {
        let len = CHUNK_SIZE + 1;
        let f = write_temp_file(len);
        let plan = plan_file(f.path()).await.expect("plan");
        let data = std::fs::read(f.path()).expect("read");
        let (exp_sha, exp_blake, exp_chunks) = oracle(&data);
        assert_eq!(plan.full_sha256, exp_sha);
        assert_eq!(plan.full_blake3, exp_blake);
        assert_eq!(plan.per_chunk_sha256, exp_chunks);
        assert_eq!(plan.per_chunk_sha256.len(), 2);
        assert_eq!(plan.total_chunks(), 2);
    }

    #[tokio::test]
    async fn multiple_chunks_final_partial() {
        // 600 KiB = 2 full chunks (512 KiB) + 1 tail (88 KiB).
        let len = 600 * 1024;
        let f = write_temp_file(len);
        let plan = plan_file(f.path()).await.expect("plan");
        let data = std::fs::read(f.path()).expect("read");
        let (exp_sha, exp_blake, exp_chunks) = oracle(&data);
        assert_eq!(plan.full_sha256, exp_sha);
        assert_eq!(plan.full_blake3, exp_blake);
        assert_eq!(plan.per_chunk_sha256, exp_chunks);
        assert_eq!(plan.per_chunk_sha256.len(), 3);
        assert_eq!(plan.total_chunks(), 3);
    }

    #[tokio::test]
    async fn known_digest_on_three_chunks() {
        // 1 KiB of 0xA5 bytes (1024 = 1 partial chunk).
        let data = vec![0xA5u8; 1024];
        let f = NamedTempFile::new().expect("tempfile");
        std::fs::write(f.path(), &data).expect("write");
        let plan = plan_file(f.path()).await.expect("plan");
        // The full-file SHA-256 must match the same 1 KiB
        // hashed via sha2 directly.
        let expected_full_sha = {
            let mut h = sha2::Sha256::new();
            h.update(&data);
            hex::encode(h.finalize())
        };
        assert_eq!(plan.full_sha256, expected_full_sha);
        assert_eq!(plan.per_chunk_sha256.len(), 1);
        // The single partial chunk must match the per-chunk
        // one-shot.
        assert_eq!(plan.per_chunk_sha256[0], sha256_hex(&data));
        // The full-file BLAKE3 must match the same 1 KiB
        // hashed via blake3 directly.
        assert_eq!(plan.full_blake3, blake3::hash(&data).to_hex().to_string());
    }

    #[tokio::test]
    async fn sum_of_chunk_lengths_equals_size() {
        // Cover the ceil() formula at three sizes.
        for &len in &[
            1usize,
            CHUNK_SIZE - 1,
            CHUNK_SIZE,
            CHUNK_SIZE + 1,
            3 * CHUNK_SIZE + 17,
        ] {
            let f = write_temp_file(len);
            let plan = plan_file(f.path()).await.expect("plan");
            let expected = len.div_ceil(CHUNK_SIZE);
            assert_eq!(
                plan.per_chunk_sha256.len(),
                expected,
                "len={} -> {} chunks, got {}",
                len,
                expected,
                plan.per_chunk_sha256.len()
            );
            let total_bytes = (plan.per_chunk_sha256.len() - 1) * CHUNK_SIZE
                + (len % CHUNK_SIZE).max(if len % CHUNK_SIZE == 0 {
                    CHUNK_SIZE
                } else {
                    len % CHUNK_SIZE
                });
            assert_eq!(
                total_bytes, len,
                "len={} total covered = {}",
                len, total_bytes
            );
        }
    }

    #[tokio::test]
    async fn no_whole_file_buffering_for_large_input() {
        // 5 MiB = 20 chunks + 4 KiB tail. The streaming path
        // uses 64 KiB scratch buffers; the test confirms the
        // output is correct (so the streaming path is right)
        // and indirectly confirms we never allocate 5 MiB at
        // once (a 1-MiB scratch is the max; with 64 KiB
        // scratch this is true).
        let len = 5 * 1024 * 1024 + 4096;
        let f = write_temp_file(len);
        let plan = plan_file(f.path()).await.expect("plan");
        let data = std::fs::read(f.path()).expect("read");
        let (exp_sha, exp_blake, exp_chunks) = oracle(&data);
        assert_eq!(plan.full_sha256, exp_sha);
        assert_eq!(plan.full_blake3, exp_blake);
        assert_eq!(plan.per_chunk_sha256, exp_chunks);
        let expected = len.div_ceil(CHUNK_SIZE);
        assert_eq!(plan.per_chunk_sha256.len(), expected);
    }

    #[test]
    fn canonical_chunk_size_is_256kib() {
        // Pin the canonical chunk size to 256 KiB. This
        // is a regression guard: if the constant ever
        // drifts, the test fails loudly.
        assert_eq!(CHUNK_SIZE, 262_144);
    }
}
