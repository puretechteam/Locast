//! P3-T04 / P3-T06 client-side download state, chunk planner,
//! chunk verifier, wire framing, transport, session
//! orchestration, resumable-download foundation, and
//! final-assembly pipeline.
//!
//! This module owns:
//!
//! - [`plan`]: a deterministic, fail-quiet planner that
//!   consumes a verified [`locast_manifest::MediaManifest`]
//!   (a single media entry, one selected source) and emits a
//!   [`plan::DownloadPlan`].
//! - [`verify`]: the per-chunk SHA-256 verifier and the
//!   full-file BLAKE3 final verifier.
//! - [`state`]: the persistent SQLite-backed `downloads` /
//!   `download_chunks` repository that is the source of
//!   truth for resumability.
//! - [`wire`]: the length-prefixed JSON wire framing used
//!   over the per-peer transport. Defines [`wire::Frame`]
//!   and the closed [`wire::WireError`] set.
//! - [`transport`]: the [`transport::Transport`] trait and
//!   the in-process [`transport::loopback_pair`] used by
//!   tests. The P3-T05 `webrtc` DataChannel will plug into
//!   the same trait in a later iteration; this module does
//!   NOT depend on `webrtc`.
//! - [`session`]: the [`session::SenderSession`] and
//!   [`session::ReceiverSession`] orchestrators that drive a
//!   download end-to-end over a [`transport::Transport`],
//!   persisting every state change through
//!   [`state::DownloadStore`].
//! - [`assemble`]: the streaming concatenation of verified
//!   chunks into the staging partial, streaming BLAKE3 over
//!   it, comparing to the manifest, and on match invoking
//!   [`crate::library::fs::complete_download`] to atomically
//!   rename into the library.
//!
//! Quota integration: every public surface goes through
//! [`crate::core::quota::QuotaAccountant`]; nothing in this
//! module duplicates quota accounting.
//!
//! Atomic completion: the final rename uses
//! [`crate::library::fs::complete_download`]. Path
//! construction goes through [`crate::core::paths`]; nothing
//! here builds raw paths into the library.
//!
//! Re-dacted across the wire and on disk: bearer tokens,
//! private keys, signatures, nonces, SDP bodies, ICE
//! candidate strings, full manifests, and file contents are
//! NEVER logged or persisted. The only fields on the wire
//! are stable, non-sensitive identifiers (`download_id`,
//! `chunk_index`, frame kind, byte counts).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod assemble;
pub mod events;
pub mod plan;
pub mod scheduler;
pub mod session;
pub mod state;
pub mod transport;
pub mod verify;
pub mod wire;

pub use events::{
    DownloadEventEmitter, DownloadEventSink, DownloadProgressEvent, DownloadStateEvent, NoopSink,
    RecordingSink, DOWNLOAD_PROGRESS_EVENT, DOWNLOAD_STATE_EVENT, EMA_ALPHA, PROGRESS_INTERVAL_MS,
    SANITIZE_LONG_TOKEN_MIN, SANITIZE_MAX_BYTES,
};
pub use plan::{
    DownloadPlan, PlanError, PlanErrorKind, PlannedChunk, PlannedSource, SelectedSource,
};
pub use scheduler::{
    backpressure_pair, BackpressureHandle, BackpressureTransport, Scheduler, SchedulerError,
    SchedulerEvent, TokenBucket, BUFFERED_AMOUNT_HIGH, BUFFERED_AMOUNT_LOW,
    PER_PEER_BUCKET_CAPACITY, PER_PEER_REFILL_PER_SEC,
};
pub use session::{
    cancel_session, ReceiverSession, SenderSession, SessionError, MAX_CHUNK_RETRIES, WINDOW_SIZE,
};
pub use state::{
    ChunkState, DownloadRecord, DownloadStore, DownloadSummary, NewDownload, RESUME_MAX_AGE_HOURS,
    SCHEMA_VERSION,
};
pub use transport::{
    loopback_pair as transport_loopback_pair, LoopbackTransport, Transport, TransportError,
};
pub use verify::{verify_chunk_sha256, verify_full_blake3, ChunkVerifyError};
pub use wire::{
    codec as wire_codec, peer_id_from_pubkey, AckFrame, CancelFrame, ChunkFrame, ErrorFrame, Frame,
    FrameKind, HelloFrame, NakFrame, OfferFrame, RequestFrame, WireError, MAX_ERROR_LEN,
    MAX_FRAME_BYTES,
};

/// The canonical chunk size used by the planner, verifier,
/// and `download_chunks.length` rows. Mirrors
/// `crate::core::hashing::CHUNK_SIZE` but is duplicated here
/// to avoid a dependency cycle (`transfer::plan` is used by
/// tests that do not import `core::hashing` directly). The
/// constant MUST be kept in sync with
/// `core::hashing::CHUNK_SIZE` and the
/// `downloads.chunk_size_bytes` SQL default (262144).
pub const CHUNK_SIZE_BYTES: usize = 262_144;
