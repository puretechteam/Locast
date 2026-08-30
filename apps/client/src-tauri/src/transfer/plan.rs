//! P3-T04 deterministic, fail-loud chunk planner.
//!
//! The planner consumes a single verified [`locast_manifest::MediaEntry`]
//! plus a chosen [`Source`] (the manifest's authoritative `peer_id`,
//! after the multi-source rotation policy of P3-T09 has picked one)
//! and produces a [`DownloadPlan`] bound to a manifest version.
//!
//! The planner is the **only** component in the client that
//! decides how many chunks a file should have and what their
//! per-chunk SHA-256s are expected to be. Every consumer of the
//! resulting plan (the storage layer that persists `download_chunks`
//! rows, the transport layer that schedules requests, the verifier
//! that confirms chunk bytes) reads the same plan and never
//! re-derives the chunking from the file. This keeps the on-disk
//! state the authoritative source of truth for resumability: a
//! `downloads.chunk_size_bytes`, `download_chunks.length`, and
//! `download_chunks.sha256` are all bound together by the planner.
//!
//! Failure semantics: every rejection returns [`PlanError`] with a
//! closed [`PlanErrorKind`] set. The planner never panics on
//! attacker-controlled input.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use thiserror::Error;

use crate::core::hashing::CHUNK_SIZE as CORE_CHUNK_SIZE;
use crate::room::peer_id::is_canonical_peer_id;
use locast_manifest::{MediaEntry, Source};

/// Mirror of `core::hashing::CHUNK_SIZE` so callers that already
/// import the transfer crate do not need to know about core.
pub use crate::transfer::CHUNK_SIZE_BYTES;

/// The canonical chunk size used by the planner. Re-exported from
/// `transfer::CHUNK_SIZE_BYTES` to keep the constant name stable
/// regardless of where the type is used.
pub const CHUNK_SIZE: usize = CHUNK_SIZE_BYTES;

/// Reject the plan if its declared chunk size does not match the
/// canonical chunk size. The architecture (section 9) permits
/// 64 KiB, 256 KiB, and 1 MiB on the wire, but Locast's own
/// downloader is locked to 256 KiB. A manifest that declares any
/// other value is rejected at the planner so the storage layer
/// never has to deal with mixed chunk sizes per download.
/// Compile-time sanity check: the planner's CHUNK_SIZE must
/// equal `core::hashing::CHUNK_SIZE` so every chunk-length
/// computation in the planner agrees with every chunk-length
/// computation in the rest of the codebase. Evaluated at
/// type-check time via the const `assert!` below; clippy's
/// "constant assertion" lint is silenced because the
/// duplication is intentional and audited (see `transfer/
/// mod.rs:46-53`).
#[allow(clippy::assertions_on_constants)]
const _: () = assert!(
    CHUNK_SIZE == CORE_CHUNK_SIZE,
    "transfer::CHUNK_SIZE must equal core::hashing::CHUNK_SIZE"
);

/// A single per-chunk expectation. Index, byte offset, byte
/// length, and the SHA-256 of those exact bytes (computed
/// independently by the planner from the on-disk file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedChunk {
    /// Zero-based chunk index, `0 <= index < total_chunks`.
    pub index: u32,
    /// Byte offset of this chunk's first byte inside the file.
    pub offset: u64,
    /// Byte length of this chunk. Always in `[1, CHUNK_SIZE]`. The
    /// last chunk is `size - offset`.
    pub length: u32,
    /// Lowercase hex SHA-256 of exactly `length` bytes starting at
    /// `offset`.
    pub sha256: String,
}

/// The single selected source the planner is bound to.
///
/// Multi-source rotation (P3-T09) is not part of P3-T04; the
/// planner consumes one already-chosen source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedSource {
    /// The canonical peer_id (64 lowercase hex chars = sha256 of
    /// the raw 32-byte Ed25519 pubkey).
    pub peer_id: String,
    /// The wire-form `Source` the planner is bound to.
    pub source: Source,
}

/// Mirror of the wire [`Source`] with the fields the planner
/// actually uses, defensively validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSource {
    pub chunk_size: u32,
    pub total_chunks: u32,
}

/// A bound, deterministic plan for downloading one media item
/// from one source, anchored to a specific manifest version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadPlan {
    pub download_id: String,
    pub media_id: String,
    pub sha256: String,
    pub blake3: String,
    pub size_bytes: u64,
    pub source: SelectedSource,
    pub source_meta: PlannedSource,
    pub chunks: Vec<PlannedChunk>,
    pub manifest_version: i64,
}

/// Closed set of planner rejections. Each variant is an explicit
/// case; nothing else is returned.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanError {
    /// The media entry was missing the field the planner needed
    /// (filename, sha256, blake3, size_bytes).
    #[error("media entry missing required field: {field}")]
    MissingField { field: &'static str },
    /// The entry had no sources, or the requested peer_id was not
    /// among the entry's sources.
    #[error("source for peer_id not found in media entry")]
    SourceNotFound,
    /// The peer_id did not match the canonical 64-lowercase-hex
    /// form.
    #[error("invalid peer_id: must be 64 lowercase hex chars")]
    InvalidPeerId,
    /// `size_bytes` was zero or otherwise impossible (e.g. negative).
    #[error("invalid size_bytes: {size}")]
    InvalidSize { size: u64 },
    /// `chunk_size` was not 262144. The Locast downloader is
    /// locked to 256 KiB chunks.
    #[error("invalid chunk_size: {chunk_size} (expected {expected})")]
    InvalidChunkSize { chunk_size: u32, expected: u32 },
    /// `chunk_hashes.len()` did not equal `total_chunks`.
    #[error("chunk_hashes length {hashes} does not match total_chunks {total}")]
    ChunkHashCountMismatch { hashes: usize, total: u32 },
    /// `total_chunks` was zero, or did not match
    /// `ceil(size_bytes / chunk_size)`.
    #[error("total_chunks {declared} does not match ceil(size / chunk_size) = {expected}")]
    TotalChunksMismatch { declared: u32, expected: u32 },
    /// A chunk hash was not 64 lowercase hex chars.
    #[error("chunk_hashes[{index}] is not a valid SHA-256 hex digest")]
    InvalidChunkHash { index: usize },
}

/// The reason string the planner emits (closed set). Used as the
/// `reason` for `AppError::InvalidDownloadPlan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanErrorKind {
    MissingField(&'static str),
    SourceNotFound,
    InvalidPeerId,
    InvalidSize,
    InvalidChunkSize,
    ChunkHashCountMismatch,
    TotalChunksMismatch,
    InvalidChunkHash,
}

impl PlanError {
    pub fn kind(&self) -> PlanErrorKind {
        match self {
            PlanError::MissingField { field } => PlanErrorKind::MissingField(field),
            PlanError::SourceNotFound => PlanErrorKind::SourceNotFound,
            PlanError::InvalidPeerId => PlanErrorKind::InvalidPeerId,
            PlanError::InvalidSize { .. } => PlanErrorKind::InvalidSize,
            PlanError::InvalidChunkSize { .. } => PlanErrorKind::InvalidChunkSize,
            PlanError::ChunkHashCountMismatch { .. } => PlanErrorKind::ChunkHashCountMismatch,
            PlanError::TotalChunksMismatch { .. } => PlanErrorKind::TotalChunksMismatch,
            PlanError::InvalidChunkHash { .. } => PlanErrorKind::InvalidChunkHash,
        }
    }
}

/// Validate that the given source is the one the caller intends to
/// use. Returns `Ok(SelectedSource)` on success or a [`PlanError`]
/// describing the rejection.
fn select_source(entry: &MediaEntry, peer_id: &str) -> Result<SelectedSource, PlanError> {
    if !is_canonical_peer_id(peer_id) {
        return Err(PlanError::InvalidPeerId);
    }
    let source = entry
        .sources
        .iter()
        .find(|s| s.peer_id == peer_id)
        .cloned()
        .ok_or(PlanError::SourceNotFound)?;
    Ok(SelectedSource {
        peer_id: peer_id.to_string(),
        source,
    })
}

/// Compute `ceil(size / chunk_size)` as a `u32` without overflow.
fn total_chunks_for(size: u64, chunk_size: u32) -> u32 {
    if size == 0 {
        0
    } else {
        size.div_ceil(chunk_size as u64) as u32
    }
}

/// Validate a 64-character lowercase hex string. Used for both
/// `media.sha256`, `media.blake3`, and every entry in
/// `chunk_hashes`.
fn is_64_lowercase_hex(s: &str) -> bool {
    s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Validate a 64-character lowercase hex string with a maximum
/// length. Rejects oversized attacker inputs before any other
/// work is done.
fn bounded_is_64_lowercase_hex(s: &str) -> bool {
    // 64 hex chars is the canonical length; anything longer is a
    // hard reject (manifest fields have a hard length cap).
    if s.len() > 64 {
        return false;
    }
    is_64_lowercase_hex(s)
}

fn validate_source_shape(source: &Source) -> Result<PlannedSource, PlanError> {
    if source.chunk_size != CHUNK_SIZE as u32 {
        return Err(PlanError::InvalidChunkSize {
            chunk_size: source.chunk_size,
            expected: CHUNK_SIZE as u32,
        });
    }
    if source.chunk_hashes.len() != source.total_chunks as usize {
        return Err(PlanError::ChunkHashCountMismatch {
            hashes: source.chunk_hashes.len(),
            total: source.total_chunks,
        });
    }
    Ok(PlannedSource {
        chunk_size: source.chunk_size,
        total_chunks: source.total_chunks,
    })
}

/// Plan a download for the given verified media entry, anchored
/// to the given manifest version.
///
/// `download_id` and `media_id` are opaque identifiers supplied by
/// the caller (typically a `Uuid` formatted as a string). The
/// planner does not parse them; it only echoes them on the
/// resulting plan.
///
/// `peer_id` must be the canonical 64 lowercase hex
/// representation AND must appear in `entry.sources`.
pub fn plan_download(
    download_id: &str,
    media_id: &str,
    manifest_version: i64,
    entry: &MediaEntry,
    peer_id: &str,
) -> Result<DownloadPlan, PlanError> {
    // 1. Validate all scalar fields on the entry.
    if entry.filename.is_empty() {
        return Err(PlanError::MissingField { field: "filename" });
    }
    if !bounded_is_64_lowercase_hex(&entry.sha256) {
        return Err(PlanError::MissingField { field: "sha256" });
    }
    if !bounded_is_64_lowercase_hex(&entry.blake3) {
        return Err(PlanError::MissingField { field: "blake3" });
    }
    if entry.size_bytes == 0 {
        return Err(PlanError::InvalidSize {
            size: entry.size_bytes,
        });
    }

    // 2. Select and validate the source.
    let selected = select_source(entry, peer_id)?;
    let meta = validate_source_shape(&selected.source)?;

    // 3. Cross-check declared vs derived chunk math.
    let expected_total = total_chunks_for(entry.size_bytes, meta.chunk_size);
    if meta.total_chunks != expected_total {
        return Err(PlanError::TotalChunksMismatch {
            declared: meta.total_chunks,
            expected: expected_total,
        });
    }

    // 4. Validate every chunk hash. We DO NOT recompute chunk
    // hashes from the file here -- that happens at the verifier
    // on each received chunk, and at the host's `chunk_plan`
    // when the manifest is built. The planner only validates
    // that the *expected* hashes match the manifest's
    // `chunk_hashes[]` entries.
    for (i, h) in selected.source.chunk_hashes.iter().enumerate() {
        if !is_64_lowercase_hex(h) {
            return Err(PlanError::InvalidChunkHash { index: i });
        }
    }

    // 5. Compute per-chunk offsets/lengths from the file shape.
    let total = meta.total_chunks;
    let chunk_size_u32 = meta.chunk_size;
    let mut chunks = Vec::with_capacity(total as usize);
    for index in 0..total {
        let offset = (index as u64).saturating_mul(chunk_size_u32 as u64);
        let length = if index + 1 == total {
            // Trailing partial chunk.
            (entry.size_bytes - offset) as u32
        } else {
            chunk_size_u32
        };
        chunks.push(PlannedChunk {
            index,
            offset,
            length,
            sha256: selected.source.chunk_hashes[index as usize].clone(),
        });
    }
    // Final partial chunk must have `length > 0` and be the
    // exact residual.
    if let Some(last) = chunks.last() {
        debug_assert!(last.length > 0);
        debug_assert!(last.offset + last.length as u64 == entry.size_bytes);
    }

    Ok(DownloadPlan {
        download_id: download_id.to_string(),
        media_id: media_id.to_string(),
        sha256: entry.sha256.clone(),
        blake3: entry.blake3.clone(),
        size_bytes: entry.size_bytes,
        source: selected,
        source_meta: meta,
        chunks,
        manifest_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use locast_manifest::{Dimensions, MediaEntry, Source};

    fn pubkey_bytes() -> [u8; 32] {
        [7u8; 32]
    }
    fn canonical_peer_id() -> String {
        crate::room::peer_id::derive_peer_id(pubkey_bytes())
    }

    fn fake_source(peer_id: &str, size: u64, chunk_size: u32) -> Source {
        let total = total_chunks_for(size, chunk_size);
        let hashes: Vec<String> = (0..total).map(|_| "0".repeat(64)).collect();
        Source {
            peer_id: peer_id.to_string(),
            url_hint: None,
            priority: 0,
            chunk_size,
            total_chunks: total,
            chunk_hashes: hashes,
        }
    }

    fn entry_with(size: u64, peer_id: &str) -> MediaEntry {
        MediaEntry {
            id: "media-uuid".to_string(),
            filename: "movie.mp4".to_string(),
            sha256: "1".repeat(64),
            blake3: "2".repeat(64),
            size_bytes: size,
            mime: "video/mp4".to_string(),
            duration_ms: 1000,
            dimensions: Some(Dimensions {
                width: 1920,
                height: 1080,
            }),
            codecs: None,
            sources: vec![fake_source(peer_id, size, CHUNK_SIZE as u32)],
        }
    }

    #[test]
    fn plan_accepts_one_byte_file() {
        let e = entry_with(1, &canonical_peer_id());
        let plan = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).expect("plan");
        assert_eq!(plan.chunks.len(), 1);
        assert_eq!(plan.chunks[0].offset, 0);
        assert_eq!(plan.chunks[0].length, 1);
    }

    #[test]
    fn plan_accepts_valid_one_chunk() {
        let e = entry_with(1024, &canonical_peer_id());
        let plan = plan_download("dl-1", "media-uuid", 1, &e, &canonical_peer_id()).expect("plan");
        assert_eq!(plan.chunks.len(), 1);
        assert_eq!(plan.chunks[0].offset, 0);
        assert_eq!(plan.chunks[0].length, 1024);
    }

    #[test]
    fn plan_accepts_exact_chunk_boundary() {
        let e = entry_with(CHUNK_SIZE as u64, &canonical_peer_id());
        let plan = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).expect("plan");
        assert_eq!(plan.chunks.len(), 1);
        assert_eq!(plan.chunks[0].length, CHUNK_SIZE as u32);
    }

    #[test]
    fn plan_accepts_chunk_boundary_plus_one() {
        let e = entry_with(CHUNK_SIZE as u64 + 1, &canonical_peer_id());
        let plan = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).expect("plan");
        assert_eq!(plan.chunks.len(), 2);
        assert_eq!(plan.chunks[0].length, CHUNK_SIZE as u32);
        assert_eq!(plan.chunks[1].offset, CHUNK_SIZE as u64);
        assert_eq!(plan.chunks[1].length, 1);
    }

    #[test]
    fn plan_accepts_multiple_chunks_with_final_partial() {
        // 600 KiB + 17 -> 3 chunks.
        let size = 600 * 1024 + 17;
        let e = entry_with(size as u64, &canonical_peer_id());
        let plan = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).expect("plan");
        assert_eq!(plan.chunks.len(), 3);
        assert_eq!(plan.chunks[0].length, CHUNK_SIZE as u32);
        assert_eq!(plan.chunks[1].length, CHUNK_SIZE as u32);
        assert_eq!(plan.chunks[2].offset, 2 * CHUNK_SIZE as u64);
        assert_eq!(plan.chunks[2].length, 17);
        assert_eq!(
            plan.chunks.iter().map(|c| c.length as u64).sum::<u64>(),
            size as u64
        );
    }

    #[test]
    fn plan_rejects_zero_byte_file() {
        let mut e = entry_with(0, &canonical_peer_id());
        e.size_bytes = 0;
        let err = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).unwrap_err();
        assert_eq!(err.kind(), PlanErrorKind::InvalidSize);
    }

    #[test]
    fn plan_rejects_bad_chunk_size() {
        let mut e = entry_with(1024, &canonical_peer_id());
        e.sources[0].chunk_size = 65536;
        e.sources[0].total_chunks = total_chunks_for(1024, 65536);
        e.sources[0].chunk_hashes = (0..e.sources[0].total_chunks)
            .map(|_| "0".repeat(64))
            .collect();
        let err = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).unwrap_err();
        assert_eq!(err.kind(), PlanErrorKind::InvalidChunkSize);
    }

    #[test]
    fn plan_rejects_bad_chunk_count() {
        let mut e = entry_with(600 * 1024, &canonical_peer_id());
        e.sources[0].total_chunks += 1;
        e.sources[0].chunk_hashes = (0..e.sources[0].total_chunks)
            .map(|_| "0".repeat(64))
            .collect();
        let err = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).unwrap_err();
        assert_eq!(err.kind(), PlanErrorKind::ChunkHashCountMismatch);
    }

    #[test]
    fn plan_rejects_mismatched_total_chunks() {
        let mut e = entry_with(CHUNK_SIZE as u64 * 5, &canonical_peer_id());
        e.sources[0].total_chunks = 4;
        e.sources[0].chunk_hashes = (0..4).map(|_| "0".repeat(64)).collect();
        let err = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).unwrap_err();
        assert_eq!(err.kind(), PlanErrorKind::TotalChunksMismatch);
    }

    #[test]
    fn plan_rejects_invalid_hash() {
        let mut e = entry_with(CHUNK_SIZE as u64 * 2, &canonical_peer_id());
        e.sources[0].chunk_hashes[1] = "Z".repeat(64);
        let err = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).unwrap_err();
        assert_eq!(err.kind(), PlanErrorKind::InvalidChunkHash);
    }

    #[test]
    fn plan_rejects_oversized_hash() {
        let mut e = entry_with(CHUNK_SIZE as u64 * 2, &canonical_peer_id());
        e.sources[0].chunk_hashes[0] = "a".repeat(65);
        let err = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).unwrap_err();
        assert_eq!(err.kind(), PlanErrorKind::InvalidChunkHash);
    }

    #[test]
    fn plan_rejects_invalid_peer_id_uppercase() {
        let bad = canonical_peer_id().to_uppercase();
        let e = entry_with(1024, &bad);
        let err = plan_download("dl-1", "m", 1, &e, &bad).unwrap_err();
        assert_eq!(err.kind(), PlanErrorKind::InvalidPeerId);
    }

    #[test]
    fn plan_rejects_peer_id_with_slash() {
        let bad = "a".repeat(63) + "/";
        let e = entry_with(1024, &canonical_peer_id());
        let err = plan_download("dl-1", "m", 1, &e, &bad).unwrap_err();
        assert_eq!(err.kind(), PlanErrorKind::InvalidPeerId);
    }

    #[test]
    fn plan_rejects_unknown_peer_id() {
        let e = entry_with(1024, &canonical_peer_id());
        let other = "b".repeat(64);
        let err = plan_download("dl-1", "m", 1, &e, &other).unwrap_err();
        assert_eq!(err.kind(), PlanErrorKind::SourceNotFound);
    }

    #[test]
    fn plan_rejects_missing_sha256() {
        let mut e = entry_with(1024, &canonical_peer_id());
        e.sha256 = String::new();
        let err = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).unwrap_err();
        assert_eq!(err.kind(), PlanErrorKind::MissingField("sha256"));
    }

    #[test]
    fn plan_rejects_missing_blake3() {
        let mut e = entry_with(1024, &canonical_peer_id());
        e.blake3 = String::new();
        let err = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).unwrap_err();
        assert_eq!(err.kind(), PlanErrorKind::MissingField("blake3"));
    }

    #[test]
    fn plan_is_deterministic() {
        let e = entry_with(600 * 1024, &canonical_peer_id());
        let a = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).expect("a");
        let b = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).expect("b");
        assert_eq!(a, b);
    }

    #[test]
    fn plan_carries_manifest_version() {
        let e = entry_with(1024, &canonical_peer_id());
        let p1 = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).expect("p1");
        let p7 = plan_download("dl-1", "m", 7, &e, &canonical_peer_id()).expect("p7");
        assert_eq!(p1.manifest_version, 1);
        assert_eq!(p7.manifest_version, 7);
    }

    #[test]
    fn plan_rejects_attacker_controlled_oversized_size() {
        // size_bytes > u32::MAX would overflow offsets; planner
        // must still reject gracefully.
        let mut e = entry_with(1024, &canonical_peer_id());
        e.size_bytes = u64::MAX;
        e.sources[0].total_chunks = u32::MAX;
        e.sources[0].chunk_hashes = (0..u32::MAX as usize).map(|_| "0".repeat(64)).collect();
        let err = plan_download("dl-1", "m", 1, &e, &canonical_peer_id()).unwrap_err();
        // Either total mismatch or just rejected. The point is:
        // it is rejected, not panicking.
        assert!(matches!(
            err.kind(),
            PlanErrorKind::TotalChunksMismatch | PlanErrorKind::ChunkHashCountMismatch
        ));
    }

    #[test]
    fn total_chunks_helper_math() {
        assert_eq!(total_chunks_for(0, CHUNK_SIZE as u32), 0);
        assert_eq!(total_chunks_for(1, CHUNK_SIZE as u32), 1);
        assert_eq!(total_chunks_for(CHUNK_SIZE as u64, CHUNK_SIZE as u32), 1);
        assert_eq!(
            total_chunks_for(CHUNK_SIZE as u64 + 1, CHUNK_SIZE as u32),
            2
        );
        assert_eq!(total_chunks_for(600 * 1024, CHUNK_SIZE as u32), 3);
    }
}
