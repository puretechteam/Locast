//! P3-T04 client-side download state, chunk planner, chunk verifier,
//! and resumable-download foundation.
//!
//! This module owns:
//!
//! - [`plan`]: a deterministic, fail-loud planner that consumes a
//!   verified [`locast_manifest::MediaManifest`] (a single media
//!   entry, one selected source) and emits a [`plan::DownloadPlan`].
//! - [`verify`]: the per-chunk SHA-256 verifier and the full-file
//!   BLAKE3 final verifier.
//! - [`state`]: the persistent SQLite-backed `downloads` /
//!   `download_chunks` repository that is the source of truth for
//!   resumability.
//!
//! The module deliberately does NOT implement WebRTC transport, the
//! `DOWNLOAD_OFFER` / `DOWNLOAD_CHUNK` wire protocol, the sliding
//! window, or the per-peer token bucket. Those live in later P3
//! tasks (P3-T05 .. P3-T09). P3-T04 establishes the in-memory and
//! on-disk state these later tasks will fill.
//!
//! Quota integration: every public surface goes through
//! [`crate::core::quota::QuotaAccountant`]; nothing in this module
//! duplicates quota accounting.
//!
//! Atomic completion: the final rename uses
//! [`crate::library::fs::complete_download`]. Path construction
//! goes through [`crate::core::paths`]; nothing here builds raw
//! paths into the library.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod plan;
pub mod state;
pub mod verify;

pub use plan::{
    DownloadPlan, PlanError, PlanErrorKind, PlannedChunk, PlannedSource, SelectedSource,
};
pub use state::{
    ChunkState, DownloadRecord, DownloadStore, DownloadSummary, NewDownload, RESUME_MAX_AGE_HOURS,
    SCHEMA_VERSION,
};
pub use verify::{verify_chunk_sha256, verify_full_blake3, ChunkVerifyError};

/// The canonical chunk size used by the planner, verifier, and
/// `download_chunks.length` rows. Mirrors
/// `crate::core::hashing::CHUNK_SIZE` but is duplicated here to
/// avoid a dependency cycle (`transfer::plan` is used by tests
/// that do not import `core::hashing` directly). The constant MUST
/// be kept in sync with `core::hashing::CHUNK_SIZE` and the
/// `downloads.chunk_size_bytes` SQL default (262144).
pub const CHUNK_SIZE_BYTES: usize = 262_144;
