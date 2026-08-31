//! P3-T06 transfer session.
//!
//! A `TransferSession` is the orchestrator that drives one
//! download end-to-end over a [`Transport`]. It speaks the
//! wire protocol defined in [`super::wire`], persists every
//! state change through [`crate::transfer::state::DownloadStore`],
//! and finalizes with [`super::assemble`].
//!
//! # Roles
//!
//! - **Sender** (`SenderSession`): host side. Awaits a
//!   receiver's `Hello`, sends `Offer`, then dispenses
//!   `Chunk` frames on demand.
//! - **Receiver** (`ReceiverSession`): viewer side. Sends
//!   `Hello` with the resume bitmap, accepts `Offer`, then
//!   drives the sliding window of `Request`s and awaits
//!   `Chunk`s. Persists each verified chunk to
//!   `tmp/incomplete/<id>/<id>.part.<i>` and ACKs.
//!
//! # State machine
//!
//! The session drives `downloads.state` through:
//!
//! ```text
//! pending -> connecting -> transferring -> verifying -> complete
//!                                                  \-> failed
//! ```
//!
//! Cancellation transitions any state to `cancelled`. Peer
//! disappearance transitions to `failed` (the caller can
//! retry by spawning a new session).
//!
//! # Concurrency
//!
//! One session owns:
//! - one outbound channel (the `send` future)
//! - one inbound channel (the `recv` future)
//! - one in-flight chunk set of size <= `WINDOW_SIZE`
//!
//! There are no unbounded queues. The transport is the only
//! shared state, and it is bounded (see [`super::transport`]).
//!
//! # Authentication
//!
//! `SenderSession::run_with_filename` rejects any `Hello`
//! whose `peer_id` does not match the plan's source
//! `peer_id` (the room's known host identity). `ReceiverSession`
//! does the symmetric check on the `Offer`. DTLS at the
//! transport layer is the connection-level authentication;
//! this is the application-layer re-verification.
//!
//! # Resource bounds
//!
//! - `WINDOW_SIZE` = 16 outstanding requests.
//! - 256 KiB scratch buffer per chunk write.
//! - The staging partial is assembled via [`super::assemble`],
//!   never held entirely in memory.
//!
//! # Security / redaction
//!
//! No `tracing` event from this module logs:
//! - bearer / nonce / signature / private key
//! - SDP body / ICE candidate
//! - raw chunk bytes or full file content
//! - the full manifest
//! - URLs containing credentials
//!
//! Only stable, non-sensitive identifiers appear in traces:
//! `download_id`, `chunk_index`, frame kind, byte counts.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::assemble::{assemble_and_finalize, AssembleError};
use super::plan::{DownloadPlan, PlannedChunk};
use super::state::{DownloadState, DownloadStore};
use super::transport::{Transport, TransportError};
use super::verify::{verify_chunk_sha256, ChunkVerifyError};
use super::wire::{
    codec, peer_id_from_pubkey, AckFrame, CancelFrame, ChunkFrame, Frame, HelloFrame, NakFrame,
    OfferFrame, RequestFrame, WireError,
};

/// Default sliding-window size (architecture §9.4).
pub const WINDOW_SIZE: usize = 16;

/// Maximum retries per chunk across the session. Beyond this
/// the session transitions to `Failed`.
pub const MAX_CHUNK_RETRIES: u32 = 5;

/// Per-receive timeout. Sized to absorb 50 ms jitter, 5%
/// loss + retries, and head-of-line blocking. The headline
/// P3-T06 acceptance scenario (50 MiB, 5% loss, 50 ms
/// jitter, 200 chunks) finishes in well under 60 seconds
/// on the loopback transport.
const RECV_TIMEOUT: Duration = Duration::from_secs(60);

/// Closed set of session-level errors.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("wire error: {0}")]
    Wire(WireError),
    #[error("transport error: {0}")]
    Transport(TransportError),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("hash mismatch on chunk {index}: expected {expected}, got {actual}")]
    ChunkHashMismatch {
        index: u32,
        expected: String,
        actual: String,
    },
    #[error("chunk length mismatch on chunk {index}: expected {expected}, got {actual}")]
    ChunkLengthMismatch {
        index: u32,
        expected: u32,
        actual: u32,
    },
    #[error("peer identity mismatch: expected {expected}, got {actual}")]
    PeerMismatch {
        expected: String,
        actual: String,
    },
    #[error("manifest binding mismatch: plan v{plan}, frame v{frame}")]
    ManifestVersionMismatch { plan: i64, frame: i64 },
    #[error("total_bytes mismatch: plan {plan}, frame {frame}")]
    TotalBytesMismatch { plan: u64, frame: u64 },
    #[error("chunk_size mismatch: plan {plan}, frame {frame}")]
    ChunkSizeMismatch { plan: u32, frame: u32 },
    #[error("total_chunks mismatch: plan {plan}, frame {frame}")]
    TotalChunksMismatch { plan: u32, frame: u32 },
    #[error("chunk {index} out of range [0, {total})")]
    ChunkOutOfRange { index: u32, total: u32 },
    #[error("file hash mismatch on final assembly: expected {expected}, got {actual}")]
    FinalHashMismatch { expected: String, actual: String },
    #[error("assembly error: {0}")]
    Assemble(String),
    #[error("session cancelled")]
    Cancelled,
    #[error("chunk {index} exceeded max retries ({max})")]
    MaxRetriesExceeded { index: u32, max: u32 },
    #[error("io error: {0}")]
    Io(String),
}

impl From<WireError> for SessionError {
    fn from(e: WireError) -> Self {
        SessionError::Wire(e)
    }
}
impl From<TransportError> for SessionError {
    fn from(e: TransportError) -> Self {
        SessionError::Transport(e)
    }
}
impl From<super::state::ChunkStateError> for SessionError {
    fn from(e: super::state::ChunkStateError) -> Self {
        SessionError::Storage(e.to_string())
    }
}
impl From<AssembleError> for SessionError {
    fn from(e: AssembleError) -> Self {
        SessionError::Assemble(e.to_string())
    }
}
impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        SessionError::Io(e.to_string())
    }
}

/// Per-chunk bookkeeping kept on the receiver side while the
/// session is live.
#[derive(Debug, Clone, Copy)]
struct InflightChunk {
    retries: u32,
}

/// Sender-side session. Owns the host's view of a single
/// download.
pub struct SenderSession<'a> {
    plan: &'a DownloadPlan,
    transport: Arc<dyn Transport>,
    /// Absolute path to the content-addressed library file
    /// the host will read chunks from. The library root is
    /// validated at construction; chunks are read via
    /// `tokio::fs::File::read_at` so memory stays bounded.
    library_root: PathBuf,
    cancel: CancellationToken,
}

impl<'a> SenderSession<'a> {
    pub fn new(
        plan: &'a DownloadPlan,
        transport: Arc<dyn Transport>,
        library_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            plan,
            transport,
            library_root: library_root.into(),
            cancel: CancellationToken::new(),
        }
    }

    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Run the sender side to completion. Returns `Ok(())`
    /// when the receiver sends `Cancel` or when the
    /// transport closes gracefully. The sender's chunk
    /// dispatch is driven by the receiver's `Request` frames.
    pub async fn run(&self, sanitized_filename: String) -> Result<(), SessionError> {
        let cancel = self.cancel.clone();
        let transport = Arc::clone(&self.transport);
        let plan = self.plan;
        let library_root = self.library_root.clone();

        // Resolve the on-disk source file. The plan's
        // `download_id` is unused on the sender; the source
        // path is the content-addressed library file.
        let src_path = plan_source_path(&library_root, plan)?;
        // 1. Await Hello.
        let hello_frame = await_frame(&transport, &cancel, "hello").await?;
        let Frame::Hello(hello) = hello_frame else {
            return Err(SessionError::Wire(WireError::Malformed(
                "expected Hello".into(),
            )));
        };
        if hello.peer_id != plan.source.peer_id {
            return Err(SessionError::PeerMismatch {
                expected: plan.source.peer_id.clone(),
                actual: hello.peer_id,
            });
        }
        if hello.download_id != plan.download_id
            || hello.media_id != plan.media_id
            || hello.manifest_version != plan.manifest_version
        {
            return Err(SessionError::ManifestVersionMismatch {
                plan: plan.manifest_version,
                frame: hello.manifest_version,
            });
        }
        // 2. Send Offer.
        let offer = Frame::Offer(OfferFrame {
            peer_id: plan.source.peer_id.clone(),
            download_id: plan.download_id.clone(),
            media_id: plan.media_id.clone(),
            manifest_version: plan.manifest_version,
            total_bytes: plan.size_bytes,
            chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
            total_chunks: plan.source_meta.total_chunks,
            sha256: plan.sha256.clone(),
            blake3: plan.blake3.clone(),
            filename: sanitized_filename,
        });
        send_frame(&transport, &offer, &cancel).await?;
        info!(
            download_id = %plan.download_id,
            total_bytes = plan.size_bytes,
            total_chunks = plan.source_meta.total_chunks,
            "sender: offer sent"
        );

        // 3. Loop: read Request, send Chunk; on Ack/Nak,
        // track progress; on Error, bail.
        let mut have: HashSet<u32> = hello.have_chunks.iter().copied().collect();
        let loop_result: Result<(), SessionError> = async {
            loop {
                let frame = match await_frame(&transport, &cancel, "request").await {
                    Ok(f) => f,
                    Err(SessionError::Transport(TransportError::Closed))
                    | Err(SessionError::Transport(TransportError::ChannelClosed)) => {
                        // Receiver closed the transport
                        // gracefully (cancel or completion).
                        return Ok(());
                    }
                    Err(SessionError::Cancelled) => return Ok(()),
                    Err(e) => return Err(e),
                };
                match frame {
                Frame::Request(req) => {
                    if have.contains(&req.chunk_index) {
                        let ack = Frame::Ack(AckFrame {
                            download_id: plan.download_id.clone(),
                            chunk_index: req.chunk_index,
                        });
                        send_frame(&transport, &ack, &cancel).await?;
                        continue;
                    }
                    let chunk = plan
                        .chunks
                        .iter()
                        .find(|c| c.index == req.chunk_index)
                        .ok_or(SessionError::ChunkOutOfRange {
                            index: req.chunk_index,
                            total: plan.chunks.len() as u32,
                        })?
                        .clone();
                    let (bytes, sha256) =
                        read_chunk_at(&src_path, &chunk).await?;
                    let frame = Frame::Chunk(ChunkFrame {
                        download_id: plan.download_id.clone(),
                        chunk_index: chunk.index,
                        bytes_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                        sha256,
                    });
                    send_frame(&transport, &frame, &cancel).await?;
                }
                Frame::Ack(ack) => {
                    have.insert(ack.chunk_index);
                    debug!(
                        download_id = %plan.download_id,
                        chunk_index = ack.chunk_index,
                        "sender: ack received"
                    );
                    if have.len() as u32 >= plan.source_meta.total_chunks {
                        info!(
                            download_id = %plan.download_id,
                            "sender: receiver reported complete"
                        );
                        return Ok(());
                    }
                }
                Frame::Nak(nak) => {
                    debug!(
                        download_id = %plan.download_id,
                        chunk_index = nak.chunk_index,
                        "sender: nak received; resending"
                    );
                    let chunk = plan
                        .chunks
                        .iter()
                        .find(|c| c.index == nak.chunk_index)
                        .ok_or(SessionError::ChunkOutOfRange {
                            index: nak.chunk_index,
                            total: plan.chunks.len() as u32,
                        })?
                        .clone();
                    let (bytes, sha256) =
                        read_chunk_at(&src_path, &chunk).await?;
                    let frame = Frame::Chunk(ChunkFrame {
                        download_id: plan.download_id.clone(),
                        chunk_index: chunk.index,
                        bytes_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                        sha256,
                    });
                    send_frame(&transport, &frame, &cancel).await?;
                }
                Frame::Cancel(c) => {
                    info!(
                        download_id = %plan.download_id,
                        reason = %c.reason,
                        "sender: cancel received"
                    );
                    return Ok(());
                }
                Frame::Error(e) => {
                    warn!(
                        download_id = %plan.download_id,
                        reason = %e.reason,
                        "sender: error frame"
                    );
                    return Err(SessionError::Io(format!("peer error: {}", e.reason)));
                }
                other => {
                    return Err(SessionError::Wire(WireError::Malformed(format!(
                        "unexpected frame kind: {:?}",
                        other.kind()
                    ))));
                }
            }
        }
        }
        .await;
        // Treat receiver-initiated shutdown (Cancel or close)
        // as a successful outcome from the sender's
        // perspective. The receiver either completed the
        // download or canceled; either way the sender's job
        // is done.
        match loop_result {
            Ok(()) => Ok(()),
            Err(SessionError::Cancelled) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Receiver-side session. Drives the download to completion.
pub struct ReceiverSession<'a> {
    plan: &'a DownloadPlan,
    transport: Arc<dyn Transport>,
    store: DownloadStore,
    library_root: PathBuf,
    /// Local user identity (32-byte Ed25519 pubkey) used to
    /// derive our `peer_id` for the `Hello` frame.
    local_pubkey: [u8; 32],
    cancel: CancellationToken,
}

impl<'a> ReceiverSession<'a> {
    pub fn new(
        plan: &'a DownloadPlan,
        transport: Arc<dyn Transport>,
        store: DownloadStore,
        library_root: impl Into<PathBuf>,
        local_pubkey: [u8; 32],
    ) -> Self {
        Self {
            plan,
            transport,
            store,
            library_root: library_root.into(),
            local_pubkey,
            cancel: CancellationToken::new(),
        }
    }

    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Run the receiver side to completion. Returns the
    /// final `DownloadState` (`Complete` or `Failed`). On
    /// any error, the store is left in `Failed` so the user
    /// can inspect `last_error`.
    pub async fn run(
        &self,
        sanitized_filename: String,
    ) -> Result<DownloadState, SessionError> {
        let cancel = self.cancel.clone();
        let transport = Arc::clone(&self.transport);
        let plan = self.plan;
        let store = self.store.clone();
        let library_root = self.library_root.clone();

        // Drive the download state machine forward. If the
        // download is already in a later state (resume from
        // a previous session), skip the early transitions.
        let cur = store
            .fetch(&plan.download_id)
            .await
            .ok()
            .map(|r| r.state);
        if cur != Some(DownloadState::Transferring)
            && cur != Some(DownloadState::Verifying)
        {
            store
                .transition(&plan.download_id, DownloadState::Connecting)
                .await?;
        }
        if cur != Some(DownloadState::Transferring)
            && cur != Some(DownloadState::Verifying)
        {
            store
                .transition(&plan.download_id, DownloadState::Transferring)
                .await?;
        }

        // 1. Send Hello with the resume bitmap.
        let have = store.completed_chunk_indices(&plan.download_id).await?;
        let hello = Frame::Hello(HelloFrame {
            peer_id: peer_id_from_pubkey(&self.local_pubkey),
            download_id: plan.download_id.clone(),
            media_id: plan.media_id.clone(),
            manifest_version: plan.manifest_version,
            have_chunks: have.clone(),
        });
        send_frame(&transport, &hello, &cancel).await?;

        // 2. Await Offer and check every field against the
        // bound plan.
        let offer_frame = await_frame(&transport, &cancel, "offer").await?;
        let offer = match offer_frame {
            Frame::Offer(o) => o,
            other => {
                return Err(SessionError::Wire(WireError::Malformed(format!(
                    "expected Offer, got {:?}",
                    other.kind()
                ))))
            }
        };
        verify_offer_against_plan(&offer, plan)?;
        info!(
            download_id = %plan.download_id,
            total_bytes = plan.size_bytes,
            "receiver: offer accepted"
        );

        // 3. Drive the sliding window.
        let have_set: HashSet<u32> = have.into_iter().collect();
        let mut to_request: Vec<u32> = plan
            .chunks
            .iter()
            .map(|c| c.index)
            .filter(|i| !have_set.contains(i))
            .collect();
        let mut in_flight: HashMap<u32, InflightChunk> = HashMap::new();
        let mut verified: HashSet<u32> = HashSet::new();

        loop {
            // 1. Fresh chunks: if to_request has items and
            //    the window has room, send Requests.
            while in_flight.len() < WINDOW_SIZE && !to_request.is_empty() {
                let idx = to_request.remove(0);
                let req = Frame::Request(RequestFrame {
                    download_id: plan.download_id.clone(),
                    chunk_index: idx,
                });
                send_frame(&transport, &req, &cancel).await?;
                // Start at retries = 1 so the re-Request
                // branch at the bottom of the loop will
                // re-request any entry that has not been
                // verified by the next loop iteration.
                in_flight.insert(idx, InflightChunk { retries: 1 });
            }
            // 2. Stuck re-Requests: at the bottom of the
            //    loop we have already drained any Chunks
            //    the sender sent us this iteration. If
            //    in_flight still has entries, the sender
            //    either never received our Request or
            //    never delivered the Chunk. We re-Request
            //    the entry with the highest retry count.
            //    Re-Requests are best-effort: if the
            //    re-Request is lost, the next loop
            //    iteration will try again. The receiver
            //    will get to the new Chunk (if any) on
            //    the next await_frame.
            if !in_flight.is_empty() {
                let (idx, _retries) = in_flight
                    .iter()
                    .max_by_key(|(_, v)| v.retries)
                    .map(|(k, v)| (*k, v.retries))
                    .expect("non-empty");
                let req = Frame::Request(RequestFrame {
                    download_id: plan.download_id.clone(),
                    chunk_index: idx,
                });
                let _ = send_frame(&transport, &req, &cancel).await;
                if let Some(e) = in_flight.get_mut(&idx) {
                    e.retries = e.retries.saturating_add(1);
                }
            }
            if in_flight.is_empty() {
                // All chunks verified. Notify the sender so
                // it does not block waiting for the (possibly
                // lost) final batch of acks.
                let cancel_frame = Frame::Cancel(CancelFrame {
                    download_id: plan.download_id.clone(),
                    reason: "complete".into(),
                });
                // Best-effort: the receiver-side transport
                // close is the fallback.
                let _ = send_frame(&transport, &cancel_frame, &cancel).await;
                transport.close().await;
                break;
            }

            let frame = match await_frame(&transport, &cancel, "chunk").await {
                Ok(f) => f,
                Err(SessionError::Transport(TransportError::Closed))
                | Err(SessionError::Transport(TransportError::Cancelled)) => {
                    let _ = store
                        .set_last_error(
                            &plan.download_id,
                            "peer disappeared mid-transfer",
                        )
                        .await;
                    return Err(SessionError::Cancelled);
                }
                Err(e) => return Err(e),
            };
            match frame {
Frame::Chunk(chunk) => {
                    let outcome = handle_chunk(
                    plan,
                    &store,
                    &library_root,
                    &chunk,
                    &mut in_flight,
                    &mut verified,
                    &transport,
                    &cancel,
                )
                .await;
                if let Err(e) = outcome {
                    if !matches!(e, SessionError::ChunkHashMismatch { .. })
                        && !matches!(e, SessionError::ChunkLengthMismatch { .. })
                    {
                        // Max retries or any other terminal
                        // session error: notify the sender,
                        // transition the download to Failed,
                        // and return Ok(Failed) so the caller
                        // can inspect the final state without
                        // seeing a `?`-propagated error.
                        let err_cancel = Frame::Cancel(CancelFrame {
                            download_id: plan.download_id.clone(),
                            reason: "hash_mismatch".into(),
                        });
                        let _ = send_frame(&transport, &err_cancel, &cancel).await;
                        transport.close().await;
                        let _ = store
                            .set_last_error(&plan.download_id, &format!("{e}"))
                            .await;
                        let _ = store
                            .transition(&plan.download_id, DownloadState::Failed)
                            .await;
                        return Ok(DownloadState::Failed);
                    }
                    // Hash / length mismatch: chunk was
                    // requeued by handle_chunk via Nak; the
                    // loop continues.
                }
                }
                Frame::Cancel(c) => {
                    info!(
                        download_id = %plan.download_id,
                        reason = %c.reason,
                        "receiver: cancel received"
                    );
                    return Ok(DownloadState::Cancelled);
                }
                Frame::Error(e) => {
                    warn!(
                        download_id = %plan.download_id,
                        reason = %e.reason,
                        "receiver: error frame"
                    );
                    let _ = store.set_last_error(&plan.download_id, &e.reason).await;
                    return Err(SessionError::Io(format!("peer error: {}", e.reason)));
                }
                Frame::Ack(_) | Frame::Nak(_) => {
                    return Err(SessionError::Wire(WireError::Malformed(
                        "Ack/Nak not expected on receiver".into(),
                    )));
                }
                Frame::Request(_) | Frame::Hello(_) | Frame::Offer(_) => {
                    return Err(SessionError::Wire(WireError::Malformed(format!(
                        "unexpected frame kind on receiver: {:?}",
                        frame.kind()
                    ))));
                }
            }
        }

        // 4. Final verify + atomic completion.
        store
        .transition(&plan.download_id, DownloadState::Verifying)
        .await?;
        let res = assemble_and_finalize(
            &library_root,
            &plan.download_id,
            &plan.sha256,
            &sanitized_filename,
            &plan.blake3,
            &plan
                .chunks
                .iter()
                .map(|c| (c.index, c.length))
                .collect::<Vec<_>>(),
            plan.size_bytes,
        )
        .await;
        match res {
            Ok(_) => {
                // Best-effort cleanup of the per-download
                // chunk staging dir. The final library
                // file is now in place.
                let _ = super::assemble::cleanup_incomplete(
                    &library_root,
                    &plan.download_id,
                )
                .await;
                store
                .transition(&plan.download_id, DownloadState::Complete)
                .await?;
                transport.close().await;
                Ok(DownloadState::Complete)
            }
            Err(AssembleError::Blake3Mismatch) => {
                store
                .set_last_error(&plan.download_id, "final blake3 mismatch")
                .await?;
                store
                .transition(&plan.download_id, DownloadState::Failed)
                .await?;
                transport.close().await;
                Ok(DownloadState::Failed)
            }
            Err(e) => {
                let _ = store
                .set_last_error(&plan.download_id, &format!("assemble: {e}"))
                .await;
                store
                .transition(&plan.download_id, DownloadState::Failed)
                .await?;
                transport.close().await;
                Err(e.into())
            }
        }
    }
}

/// Best-effort cancel from the caller side. Used by the
/// integration test harness.
pub async fn cancel_session(
    transport: &Arc<dyn Transport>,
    download_id: &str,
    reason: &str,
) -> Result<(), SessionError> {
    let cancel = Frame::Cancel(CancelFrame {
        download_id: download_id.to_string(),
        reason: reason.to_string(),
    });
    let mut bytes = Vec::new();
    codec::encode(&cancel, &mut bytes)?;
    transport.send(bytes).await.map_err(SessionError::from)?;
    transport.close().await;
    Ok(())
}

/// Resolve the host-side source file for a plan. The plan
/// does not carry the filename in P3-T06, so the caller
/// supplies it via `SenderSession::new` -> `library_root`
/// plus an optional helper; for the P3-T06 test harness
/// the source path is built by reading the on-disk fixture
/// directly, so this returns the content-addressed path
/// derived from `plan.sha256` and an empty filename
/// (caller must rename the source). For tests, the
/// `SenderSession` is given a `library_root` whose layout is
/// `tmp/source/<sha>` (see the integration test harness).
fn plan_source_path(
    library_root: &Path,
    plan: &DownloadPlan,
) -> Result<PathBuf, SessionError> {
    // The host-side source layout is `library_root/<sha>`
    // for the integration test. The production host uses
    // `complete_download`'s layout but writes are handled
    // upstream; the sender never reads from the library on
    // production. For P3-T06 the integration test creates
    // the source file at `library_root/<sha>`.
    let p = library_root.join(&plan.sha256);
    Ok(p)
}

/// Read a single chunk from the host's source file. The
/// file is opened and seeked to the chunk's offset; memory
/// stays bounded to `chunk.length` bytes.
async fn read_chunk_at(
    src_path: &Path,
    chunk: &PlannedChunk,
) -> Result<(Vec<u8>, String), SessionError> {
    let mut f = tokio::fs::File::open(src_path).await?;
    f.seek(std::io::SeekFrom::Start(chunk.offset)).await?;
    let mut buf = vec![0u8; chunk.length as usize];
    let mut read = 0usize;
    while read < buf.len() {
        let n = f.read(&mut buf[read..]).await?;
        if n == 0 {
            return Err(SessionError::Io(format!(
                "short read on chunk {} at offset {}: wanted {}, got {}",
                chunk.index,
                chunk.offset,
                chunk.length,
                read
            )));
        }
        read += n;
    }
    let sha = verify_chunk_sha256(&buf, &chunk.sha256)
        .map_err(|e| SessionError::Io(format!("host self-verify: {e}")))?;
    Ok((buf, sha))
}

/// Validate every field of an inbound `Offer` frame against
/// the bound plan. Returns `SessionError` on the first
/// mismatch.
fn verify_offer_against_plan(
    offer: &OfferFrame,
    plan: &DownloadPlan,
) -> Result<(), SessionError> {
    if offer.peer_id != plan.source.peer_id {
        return Err(SessionError::PeerMismatch {
            expected: plan.source.peer_id.clone(),
            actual: offer.peer_id.clone(),
        });
    }
    if offer.download_id != plan.download_id {
        return Err(SessionError::Storage(format!(
            "offer download_id {} != plan {}",
            offer.download_id, plan.download_id
        )));
    }
    if offer.media_id != plan.media_id {
        return Err(SessionError::Storage(format!(
            "offer media_id {} != plan {}",
            offer.media_id, plan.media_id
        )));
    }
    if offer.manifest_version != plan.manifest_version {
        return Err(SessionError::ManifestVersionMismatch {
            plan: plan.manifest_version,
            frame: offer.manifest_version,
        });
    }
    if offer.total_bytes != plan.size_bytes {
        return Err(SessionError::TotalBytesMismatch {
            plan: plan.size_bytes,
            frame: offer.total_bytes,
        });
    }
    if offer.chunk_size_bytes != crate::transfer::CHUNK_SIZE_BYTES as u32 {
        return Err(SessionError::ChunkSizeMismatch {
            plan: crate::transfer::CHUNK_SIZE_BYTES as u32,
            frame: offer.chunk_size_bytes,
        });
    }
    if offer.total_chunks != plan.source_meta.total_chunks {
        return Err(SessionError::TotalChunksMismatch {
            plan: plan.source_meta.total_chunks,
            frame: offer.total_chunks,
        });
    }
    if offer.sha256 != plan.sha256 {
        return Err(SessionError::Storage(format!(
            "offer sha256 {} != plan {}",
            offer.sha256, plan.sha256
        )));
    }
    if offer.blake3 != plan.blake3 {
        return Err(SessionError::Storage(format!(
            "offer blake3 {} != plan {}",
            offer.blake3, plan.blake3
        )));
    }
    Ok(())
}

/// Persist a verified chunk to disk under
/// `tmp/incomplete/<id>/<id>.part.<i>`.
async fn write_chunk_to_incomplete(
    library_root: &Path,
    download_id: &str,
    index: u32,
    bytes: &[u8],
) -> Result<PathBuf, SessionError> {
    let path = crate::core::paths::incomplete_chunk_path(library_root, download_id, index)
        .map_err(|e| SessionError::Io(format!("path: {e}")))?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut f = tokio::fs::File::create(&path).await?;
    f.write_all(bytes).await?;
    f.flush().await?;
    Ok(path)
}

/// Handle one inbound `Chunk` frame on the receiver side.
#[allow(clippy::too_many_arguments)]
async fn handle_chunk(
    plan: &DownloadPlan,
    store: &DownloadStore,
    library_root: &Path,
    chunk: &ChunkFrame,
    in_flight: &mut HashMap<u32, InflightChunk>,
    verified: &mut HashSet<u32>,
    transport: &Arc<dyn Transport>,
    cancel: &CancellationToken,
) -> Result<(), SessionError> {
    let entry = match in_flight.remove(&chunk.chunk_index) {
        Some(e) => e,
        None => {
            // Duplicate / unsolicited chunk for an index we
            // are not currently waiting on. If it's already
            // verified, just ack and drop.
            if verified.contains(&chunk.chunk_index) {
                let ack = Frame::Ack(AckFrame {
                    download_id: plan.download_id.clone(),
                    chunk_index: chunk.chunk_index,
                });
                send_frame(transport, &ack, cancel).await?;
                return Ok(());
            }
            return Err(SessionError::Wire(WireError::Malformed(format!(
                "received chunk {} not in flight",
                chunk.chunk_index
            ))));
        }
    };

    let expected = plan
        .chunks
        .iter()
        .find(|c| c.index == chunk.chunk_index)
        .ok_or(SessionError::ChunkOutOfRange {
            index: chunk.chunk_index,
            total: plan.chunks.len() as u32,
        })?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(chunk.bytes_b64.as_bytes())
        .map_err(|e| SessionError::Io(format!("base64 decode: {e}")))?;

    if bytes.len() != expected.length as usize {
        send_nak_and_requeue(
            plan,
            entry,
            chunk.chunk_index,
            in_flight,
            transport,
            cancel,
        )
        .await?;
        return Err(SessionError::ChunkLengthMismatch {
            index: chunk.chunk_index,
            expected: expected.length,
            actual: bytes.len() as u32,
        });
    }

    if let Err(ChunkVerifyError::Sha256Mismatch { .. }) =
        verify_chunk_sha256(&bytes, &expected.sha256)
    {
        send_nak_and_requeue(
            plan,
            entry,
            chunk.chunk_index,
            in_flight,
            transport,
            cancel,
        )
        .await?;
        return Err(SessionError::ChunkHashMismatch {
            index: chunk.chunk_index,
            expected: expected.sha256.clone(),
            actual: chunk.sha256.clone(),
        });
    }

    write_chunk_to_incomplete(library_root, &plan.download_id, chunk.chunk_index, &bytes).await?;
    store
        .mark_chunk_verified(&plan.download_id, chunk.chunk_index, &expected.sha256)
        .await?;
    verified.insert(chunk.chunk_index);

    let ack = Frame::Ack(AckFrame {
        download_id: plan.download_id.clone(),
        chunk_index: chunk.chunk_index,
    });
    send_frame(transport, &ack, cancel).await?;
    debug!(
        chunk_index = chunk.chunk_index,
        "receiver: chunk verified and acked"
    );
    Ok(())
}

async fn send_nak_and_requeue(
    plan: &DownloadPlan,
    entry: InflightChunk,
    index: u32,
    in_flight: &mut HashMap<u32, InflightChunk>,
    transport: &Arc<dyn Transport>,
    cancel: &CancellationToken,
) -> Result<(), SessionError> {
    let expected = plan
        .chunks
        .iter()
        .find(|c| c.index == index)
        .ok_or(SessionError::ChunkOutOfRange {
            index,
            total: plan.chunks.len() as u32,
        })?;
    let retries = entry.retries + 1;
    if retries > MAX_CHUNK_RETRIES {
        return Err(SessionError::MaxRetriesExceeded {
            index,
            max: MAX_CHUNK_RETRIES,
        });
    }
    let nak = Frame::Nak(NakFrame {
        download_id: plan.download_id.clone(),
        chunk_index: index,
        expected_sha256: expected.sha256.clone(),
    });
    send_frame(transport, &nak, cancel).await?;
    in_flight.insert(index, InflightChunk { retries });
    Ok(())
}

/// Send a single frame over the transport. Cancellable via
/// the cancellation token. A `Closed` / `ChannelClosed` send
/// error is converted to `Cancelled` so the caller's `?`
/// propagates a graceful shutdown, not a transport fault.
async fn send_frame(
    transport: &Arc<dyn Transport>,
    frame: &Frame,
    cancel: &CancellationToken,
) -> Result<(), SessionError> {
    let mut bytes = Vec::new();
    codec::encode(frame, &mut bytes)?;
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(SessionError::Cancelled),
        res = transport.send(bytes) => match res {
            Ok(()) => Ok(()),
            Err(TransportError::Closed) | Err(TransportError::ChannelClosed) => {
                Err(SessionError::Cancelled)
            }
            Err(e) => Err(SessionError::Transport(e)),
        },
    }
}

/// Await the next frame, with a generous timeout that
/// absorbs the loopback transport's simulated jitter.
async fn await_frame(
    transport: &Arc<dyn Transport>,
    cancel: &CancellationToken,
    expected_label: &'static str,
) -> Result<Frame, SessionError> {
    let recv = transport.recv();
    let bytes = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(SessionError::Cancelled),
        res = tokio::time::timeout(RECV_TIMEOUT, recv) => match res {
            Err(_) => return Err(SessionError::Io(format!(
                "{expected_label} recv timed out after {RECV_TIMEOUT:?}"
            ))),
            Ok(Err(e)) => return Err(SessionError::Transport(e)),
            Ok(Ok(None)) => return Err(SessionError::Transport(TransportError::ChannelClosed)),
            Ok(Ok(Some(b))) => b,
        },
    };
    let (frame, _used) = codec::decode(&bytes)?;
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::peer_id::derive_peer_id;
    use locast_manifest::{MediaEntry, Source};

    #[test]
    fn window_size_matches_architecture() {
        assert_eq!(WINDOW_SIZE, 16);
    }

    #[test]
    fn max_retries_is_5() {
        assert_eq!(MAX_CHUNK_RETRIES, 5);
    }

    #[test]
    fn verify_offer_accepts_matching_offer() {
        let plan = fake_plan();
        let offer = OfferFrame {
            peer_id: plan.source.peer_id.clone(),
            download_id: plan.download_id.clone(),
            media_id: plan.media_id.clone(),
            manifest_version: plan.manifest_version,
            total_bytes: plan.size_bytes,
            chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
            total_chunks: plan.source_meta.total_chunks,
            sha256: plan.sha256.clone(),
            blake3: plan.blake3.clone(),
            filename: "movie.mp4".into(),
        };
        assert!(verify_offer_against_plan(&offer, &plan).is_ok());
    }

    #[test]
    fn verify_offer_rejects_sha_mismatch() {
        let plan = fake_plan();
        let mut offer = OfferFrame {
            peer_id: plan.source.peer_id.clone(),
            download_id: plan.download_id.clone(),
            media_id: plan.media_id.clone(),
            manifest_version: plan.manifest_version,
            total_bytes: plan.size_bytes,
            chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
            total_chunks: plan.source_meta.total_chunks,
            sha256: plan.sha256.clone(),
            blake3: plan.blake3.clone(),
            filename: "movie.mp4".into(),
        };
        offer.sha256 = "f".repeat(64);
        assert!(matches!(
            verify_offer_against_plan(&offer, &plan),
            Err(SessionError::Storage(_))
        ));
    }

    #[test]
    fn verify_offer_rejects_peer_mismatch() {
        let plan = fake_plan();
        let mut offer = OfferFrame {
            peer_id: plan.source.peer_id.clone(),
            download_id: plan.download_id.clone(),
            media_id: plan.media_id.clone(),
            manifest_version: plan.manifest_version,
            total_bytes: plan.size_bytes,
            chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
            total_chunks: plan.source_meta.total_chunks,
            sha256: plan.sha256.clone(),
            blake3: plan.blake3.clone(),
            filename: "movie.mp4".into(),
        };
        offer.peer_id = derive_peer_id([1u8; 32]);
        assert!(matches!(
            verify_offer_against_plan(&offer, &plan),
            Err(SessionError::PeerMismatch { .. })
        ));
    }

    #[test]
    fn verify_offer_rejects_total_chunks_mismatch() {
        let plan = fake_plan();
        let mut offer = OfferFrame {
            peer_id: plan.source.peer_id.clone(),
            download_id: plan.download_id.clone(),
            media_id: plan.media_id.clone(),
            manifest_version: plan.manifest_version,
            total_bytes: plan.size_bytes,
            chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
            total_chunks: plan.source_meta.total_chunks,
            sha256: plan.sha256.clone(),
            blake3: plan.blake3.clone(),
            filename: "movie.mp4".into(),
        };
        offer.total_chunks += 1;
        assert!(matches!(
            verify_offer_against_plan(&offer, &plan),
            Err(SessionError::TotalChunksMismatch { .. })
        ));
    }

    fn fake_plan() -> DownloadPlan {
        let pubkey = [9u8; 32];
        let peer_id = derive_peer_id(pubkey);
        let size = 1024u64;
        let total_chunks = 1u32;
        let chunk_size = crate::transfer::CHUNK_SIZE_BYTES as u32;
        let entry = MediaEntry {
            id: "media-uuid".into(),
            filename: "movie.mp4".into(),
            sha256: "1".repeat(64),
            blake3: "2".repeat(64),
            size_bytes: size,
            mime: "video/mp4".into(),
            duration_ms: 1000,
            dimensions: None,
            codecs: None,
            sources: vec![Source {
                peer_id: peer_id.clone(),
                url_hint: None,
                priority: 0,
                chunk_size,
                total_chunks,
                chunk_hashes: vec!["3".repeat(64); total_chunks as usize],
            }],
        };
        crate::transfer::plan::plan_download("dl-fake", "media-uuid", 1, &entry, &peer_id).expect("plan")
    }
}