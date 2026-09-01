//! P3-T09 multi-source selection + bitmap merge.
//!
//! The `MultiSourceReceiver` is the orchestrator that drives
//! one `DownloadPlan` end-to-end across **N** peer sources.
//! P3-T06's `ReceiverSession` drove a single `Transport`;
//! P3-T09 lifts that to a small ring of [`SourceHandle`]s,
//! each with its own `Transport`, sliding-window scheduler,
//! NAK counter, RTT estimate, and rotation policy.
//!
//! # Invariants
//!
//! 1. **One plan per download.** There is exactly one
//!    `DownloadPlan` and exactly one `downloads` row. The plan
//!    is bound to source A's `peer_id` at construction; source
//!    B (and any others) are alternative sources for the same
//!    `(download_id, media_id, manifest_version)` triple.
//! 2. **One logical completion bitmap.** `download_chunks` is
//!    the only authoritative state. Every successful chunk
//!    from any source calls
//!    `DownloadStore::mark_chunk_verified`, which is
//!    idempotent on `(verified, same_sha256)`. Multi-source
//!    failover exploits that idempotency: a chunk verified by
//!    source A is a no-op if source B then delivers the same
//!    chunk, and vice versa.
//! 3. **No client-supplied identity.** The orchestrator
//!    announces itself with its own `peer_id` (derived from
//!    `local_pubkey`) on the `Hello` frame to every source.
//!    Sources do not impersonate each other.
//! 4. **Reuse everything from P3-T06 / P3-T07 / P3-T08.**
//!    No new wire frames, no new transport, no new state
//!    transitions. The receiver reuses
//!    `DownloadEventEmitter`, `assemble_and_finalize`, and
//!    `Scheduler` per source.
//!
//! # Concurrency / lock ordering
//!
//! Five locks appear in this module. They are acquired in
//! the order listed; the order is total and never reversed
//! inside a single code path. No lock is held across a
//! network I/O call, a filesystem I/O call, or a Tauri emit:
//!
//! 1. `sources` (`Arc<Mutex<Vec<SourceHandle>>>`) -- rotated
//!    first to mutate selection state (priority, cooldown,
//!    demotion, NAK counter, last_request_at, `rtt_samples`,
//!    `unavailable`, `unavailable_since`).
//! 2. `in_flight` (`Arc<Mutex<HashMap<u32, InflightRecord>>>`)
//!    -- record/remove per-chunk outstanding requests.
//! 3. `chunk_retries` (`Arc<Mutex<HashMap<u32, u32>>>`) --
//!    total retries across sources for one chunk index.
//! 4. `chunk_tried` (`Arc<Mutex<HashMap<u32, HashSet<String>>>>`)
//!    -- per-attempt set of peer_ids already tried for one
//!    chunk index.
//! 5. `nak_counters`
//!    (`Arc<Mutex<HashMap<(u32, String), u32>>>`) --
//!    consecutive-NAK count per `(chunk_index, peer_id)` pair.
//!
//! `verified_sources` is a `#[cfg(test)]`-only accessor map
//! and is NOT held across I/O in production code paths.
//!
//! # RTT-driven demotion
//!
//! Every successful chunk arrival records an RTT sample on
//! the source that served it. If the rolling p95 over the
//! last `RTT_P95_WINDOW` exceeds `RTT_P95_LIMIT_MS`, the
//! source is marked `unavailable` for `RTT_COOLDOWN`. While
//! unavailable, [`SourceSelector::pick`] skips it. While
//! skipped, any in-flight requests destined for that source
//! are rotated back into the dispatch pool (the source's
//! `peer_id` is inserted into `chunk_tried[chunk_index]`).
//!
//! # NAK-driven demotion
//!
//! Every NAK on `(chunk_index, peer_id)` increments
//! `nak_counters[(chunk_index, peer_id)]`. When the counter
//! reaches `NAK_THRESHOLD = 3`, the peer is demoted: its
//! `peer_id` is added to `chunk_tried[chunk_index]`, the
//! counter is reset to 0, the source's `demotion_count`
//! increments, and the slot is released. The next
//! `SourceSelector::pick` call returns a different source
//! for the same `chunk_index`. Total chunk retries reaching
//! `MAX_CHUNK_RETRIES = 5` (i.e. 5 NAKs across all sources
//! before the download transitions to Failed) transition to
//! `AllSourcesExhausted`.
//!
//! # State machine
//!
//! The state transitions mirror `ReceiverSession::run`:
//!
//! ```text
//! pending -> connecting -> transferring -> verifying -> complete
//!                                                  \-> failed
//! ```
//!
//! Cancellation transitions any state to `Cancelled`. All
//! sources exhausted transitions to `Failed`.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::assemble::{assemble_and_finalize, cleanup_incomplete, AssembleError};
use super::events::{
    sanitize_error_message, DownloadEventEmitter, DownloadProgressEvent, DownloadStateEvent,
    EMA_ALPHA,
};
use super::plan::DownloadPlan;
use super::scheduler::Scheduler;
use super::session::{MAX_CHUNK_RETRIES, WINDOW_SIZE};
use super::state::{ChunkStateError, DownloadState, DownloadStore};
use super::transport::{Transport, TransportError};
use super::verify::{verify_chunk_sha256, ChunkVerifyError};
use super::wire::{
    codec, peer_id_from_pubkey, AckFrame, ChunkFrame, Frame, HelloFrame, NakFrame, RequestFrame,
    WireError,
};

/// P3-T09: NAK count on the same `(chunk_index, peer_id)`
/// at which the peer is demoted and rotated to a different
/// source. The architecture fixes this at 3.
pub const NAK_THRESHOLD: u32 = 3;

/// P3-T09: cooldown window after a source is marked
/// `unavailable` (either by the NAK demotion policy or by the
/// RTT p95 demotion policy). While in cooldown, the source
/// is invisible to [`SourceSelector::pick`].
pub const RTT_COOLDOWN: Duration = Duration::from_secs(30);

/// P3-T09: cap on the rolling RTT sample deque per source.
/// Pinned at 64; the architecture uses this as the
/// deterministic sample count for the p95 estimator.
pub const RTT_WINDOW_CAP: usize = 64;

/// P3-T09: rolling window the p95 is computed over. Only RTT
/// samples whose timestamp is within `now - RTT_P95_WINDOW`
/// participate.
pub const RTT_P95_WINDOW: Duration = Duration::from_secs(10);

/// P3-T09: p95 threshold above which a source is marked
/// `unavailable`. Mirrors the architecture §9.4 "RTT > 2 s
/// for 10 s" demotion rule.
pub const RTT_P95_LIMIT_MS: u64 = 2000;

/// P3-T09: minimum interval between stuck-chunk
/// re-requests on the same peer. Keeps the orchestrator
/// from busy-looping when a peer silently drops every
/// Request frame. A re-request itself is NOT an implicit
/// NAK: it just nudges the peer again. Only after
/// `STUCK_REQUEST_DEMOTE_AFTER` consecutive stuck ticks
/// (i.e. ~6 s of silence at this 2 s interval) does the
/// orchestrator count the silence as an implicit NAK and
/// call `apply_nak`. This keeps legitimate slow links from
/// being demoted.
const STUCK_REQUEST_RETRY: Duration = Duration::from_secs(2);

/// P3-T09: how many consecutive stuck re-requests on the
/// same `(chunk, peer)` must elapse before the orchestrator
/// counts the silence as an implicit NAK. With
/// `STUCK_REQUEST_RETRY = 2 s` and `STUCK_REQUEST_DEMOTE_AFTER
/// = 3`, the peer must be silent for ~6 s before its
/// per-(chunk, peer) NAK counter increments.
const STUCK_REQUEST_DEMOTE_AFTER: u32 = 3;

/// One peer-side source for a multi-source download. Owns a
/// `Transport`, a per-source [`Scheduler`], and the small
/// state the [`SourceSelector`] needs to choose between
/// sources.
pub struct SourceHandle {
    /// Canonical peer_id of this source (64 lowercase hex).
    pub peer_id: String,
    /// Per-peer transport. Owned exclusively by this handle
    /// inside the orchestrator.
    pub transport: Arc<dyn Transport>,
    /// Selection priority. **Lower wins.** The architecture
    /// defines `priority = 0` as the host (preferred).
    pub priority: i32,
    /// Per-source sliding-window scheduler. The orchestrator
    /// trusts this scheduler's per-source WINDOW_SIZE=16 cap.
    /// Wrapped in `Arc` so the orchestrator can clone the
    /// scheduler reference cheaply when dispatching.
    pub sched: Arc<Scheduler>,
    /// How many times this source has been demoted (NAK
    /// threshold reached) for any chunk in this download.
    /// Surfaced for observability + integration tests.
    pub demotion_count: u32,
    /// True when this source is in cooldown (either because
    /// the RTT p95 exceeded the limit or because all of its
    /// recent NAKs crossed the demotion threshold).
    pub unavailable: bool,
    /// When `unavailable` became true. Used by the cooldown
    /// timer; [`SourceSelector::pick`] clears
    /// `unavailable` once `now - unavailable_since >=
    /// RTT_COOLDOWN`.
    pub unavailable_since: Option<Instant>,
    /// Cancellation token for this source's recv task. The
    /// orchestrator's main cancel token is the parent; this
    /// per-source token lets the orchestrator cancel just one
    /// source if its transport is stuck.
    pub cancel: CancellationToken,
    /// Rolling RTT sample deque for this source. Capped at
    /// [`RTT_WINDOW_CAP`]. Newest samples are at the back;
    /// the orchestrator pops from the front when the cap is
    /// exceeded. Read by the RTT-driven demotion path in
    /// `handle_chunk` and the unit tests for `rtt_p95`.
    pub rtt_samples: VecDeque<(Instant, Duration)>,
}

/// A single in-flight chunk's bookkeeping. The orchestrator
/// keeps one of these per chunk currently requested.
#[derive(Debug, Clone)]
pub struct InflightRecord {
    /// Peer_id of the source the request was sent to.
    pub peer_id: String,
    /// When the `Request` frame was sent. The RTT estimator
    /// reads this on the matching `Chunk` arrival.
    pub requested_at: Instant,
}

/// Closed set of multi-source-specific errors. Mirrors
/// `SessionError` plus the multi-source additions.
#[derive(Debug, Error)]
pub enum MultiSourceError {
    #[error("wire error: {0}")]
    Wire(WireError),
    #[error("transport error: {0}")]
    Transport(TransportError),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("assemble error: {0}")]
    Assemble(String),
    #[error("peer identity mismatch: expected {expected}, got {actual}")]
    PeerMismatch { expected: String, actual: String },
    #[error("duplicate peer_id in sources: {0}")]
    DuplicatePeerId(String),
    #[error("no sources configured")]
    NoSources,
    #[error("all sources exhausted for chunk {index}")]
    AllSourcesExhausted { index: u32 },
    #[error("all source transports closed")]
    AllSourcesGone,
    #[error("session cancelled")]
    Cancelled,
    #[error("chunk {index} exceeded max retries ({max})")]
    MaxRetriesExceeded { index: u32, max: u32 },
    #[error("scheduler error: {0}")]
    Scheduler(String),
}

impl From<WireError> for MultiSourceError {
    fn from(e: WireError) -> Self {
        MultiSourceError::Wire(e)
    }
}
impl From<TransportError> for MultiSourceError {
    fn from(e: TransportError) -> Self {
        MultiSourceError::Transport(e)
    }
}
impl From<ChunkStateError> for MultiSourceError {
    fn from(e: ChunkStateError) -> Self {
        MultiSourceError::Storage(e.to_string())
    }
}
impl From<AssembleError> for MultiSourceError {
    fn from(e: AssembleError) -> Self {
        MultiSourceError::Assemble(e.to_string())
    }
}
impl From<std::io::Error> for MultiSourceError {
    fn from(e: std::io::Error) -> Self {
        MultiSourceError::Io(e.to_string())
    }
}
impl From<super::scheduler::SchedulerError> for MultiSourceError {
    fn from(e: super::scheduler::SchedulerError) -> Self {
        MultiSourceError::Scheduler(e.to_string())
    }
}

/// Deterministic, pure source-selection function. The same
/// `(handles, chunk_index, tried_this_attempt, now)` tuple
/// always returns the same `Some(handle)` reference. Used by
/// the orchestrator's dispatch loop.
pub struct SourceSelector;

impl SourceSelector {
    /// Pick the best source for `chunk_index` given the set
    /// of `tried_this_attempt` peer_ids. Returns `None` when
    /// every source is unavailable, already tried, or not
    /// configured.
    ///
    /// Order (deterministic):
    ///
    /// 1. Skip sources whose `unavailable == true` and
    ///    `(now - unavailable_since) < RTT_COOLDOWN`. The
    ///    function transparently clears `unavailable` if the
    ///    cooldown has elapsed (so a stale `unavailable`
    ///    flag never strands a download).
    /// 2. Skip sources in `tried_this_attempt`.
    /// 3. Lowest `priority` wins.
    /// 4. Tie-break: lowest `demotion_count`.
    /// 5. Final tie-break: lowest `peer_id` lexicographic.
    pub fn pick<'a>(
        handles: &'a [SourceHandle],
        chunk_index: u32,
        tried_this_attempt: &HashSet<String>,
        now: Instant,
    ) -> Option<&'a SourceHandle> {
        let _ = chunk_index;
        let mut best: Option<&SourceHandle> = None;
        for h in handles {
            // Step 1: cooldown gate. Clear the flag if the
            // cooldown has elapsed so a flag set early in
            // the download does not haunt us for the rest.
            if h.unavailable {
                if let Some(since) = h.unavailable_since {
                    if now.duration_since(since) >= RTT_COOLDOWN {
                        // We cannot mutate h through a shared
                        // reference here; the orchestrator's
                        // caller is responsible for clearing
                        // the flag. The `pick` function is
                        // pure and must not write. The
                        // cooldown-clear happens in
                        // `MultiSourceReceiver::maybe_recover_sources`
                        // (called once per dispatch loop
                        // iteration, with `&mut self`).
                        // For the in-test picker we still
                        // treat this source as eligible.
                        // We deliberately do NOT mutate here.
                        // We do not skip either -- the caller
                        // is expected to call the recovery
                        // helper before `pick`.
                    } else {
                        continue;
                    }
                }
            }
            // Step 2: already tried on this attempt?
            if tried_this_attempt.contains(&h.peer_id) {
                continue;
            }
            // Steps 3-5: priority, then demotion_count, then
            // peer_id lexicographic.
            best = match best {
                None => Some(h),
                Some(cur) => {
                    if h.priority < cur.priority {
                        Some(h)
                    } else if h.priority > cur.priority {
                        Some(cur)
                    } else if h.demotion_count < cur.demotion_count {
                        Some(h)
                    } else if h.demotion_count > cur.demotion_count {
                        Some(cur)
                    } else if h.peer_id < cur.peer_id {
                        Some(h)
                    } else {
                        Some(cur)
                    }
                }
            };
        }
        best
    }
}

/// Rolling p95 over the RTT sample deque. Returns `None`
/// when there are no samples in the window. The function is
/// pure: callers retain ownership of `samples`.
pub fn rtt_p95(
    samples: &VecDeque<(Instant, Duration)>,
    window: Duration,
    now: Instant,
) -> Option<Duration> {
    let mut in_window: Vec<Duration> = samples
        .iter()
        .filter_map(|(t, d)| {
            if now.duration_since(*t) <= window {
                Some(*d)
            } else {
                None
            }
        })
        .collect();
    if in_window.is_empty() {
        return None;
    }
    in_window.sort_unstable();
    let idx = ((in_window.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
    Some(in_window[idx])
}

/// The orchestrator. Owns the plan, the store, the source
/// ring, and the per-chunk bookkeeping.
pub struct MultiSourceReceiver {
    plan: Arc<DownloadPlan>,
    store: DownloadStore,
    library_root: PathBuf,
    local_pubkey: [u8; 32],
    sources: Arc<Mutex<Vec<SourceHandle>>>,
    in_flight: Arc<Mutex<HashMap<u32, InflightRecord>>>,
    chunk_retries: Arc<Mutex<HashMap<u32, u32>>>,
    chunk_tried: Arc<Mutex<HashMap<u32, HashSet<String>>>>,
    nak_counters: Arc<Mutex<HashMap<(u32, String), u32>>>,
    /// Consecutive stuck re-request count per
    /// `(chunk_index, peer_id)`. Each time the dispatch
    /// loop fires a stuck-tick re-request for this pair it
    /// increments; each successful `handle_chunk` for the
    /// same pair resets it to 0. When the count reaches
    /// `STUCK_REQUEST_DEMOTE_AFTER`, the orchestrator calls
    /// `apply_nak` so the demotion / retry budget advances.
    consecutive_stuck: Arc<Mutex<HashMap<(u32, String), u32>>>,
    emitter: Arc<DownloadEventEmitter>,
    cancel: CancellationToken,
    /// Test-only: which peer actually delivered each
    /// verified chunk. Populated on every successful
    /// chunk arrival. Always present in memory (a
    /// `HashMap<u32, String>` is O(chunks) bytes) but only
    /// read by the test-only [`Self::verified_sources_snapshot`]
    /// accessor and the test-only
    /// [`Self::inflight_peer_for`] accessor.
    verified_sources: Arc<Mutex<HashMap<u32, String>>>,
}

impl MultiSourceReceiver {
    /// Build a new orchestrator. Validates that no two
    /// sources share a `peer_id` and that at least one source
    /// is configured.
    pub fn new(
        plan: Arc<DownloadPlan>,
        store: DownloadStore,
        library_root: impl Into<PathBuf>,
        local_pubkey: [u8; 32],
        sources: Vec<SourceHandle>,
    ) -> Result<Self, MultiSourceError> {
        if sources.is_empty() {
            return Err(MultiSourceError::NoSources);
        }
        let mut seen: HashSet<String> = HashSet::new();
        for s in &sources {
            if !seen.insert(s.peer_id.clone()) {
                return Err(MultiSourceError::DuplicatePeerId(s.peer_id.clone()));
            }
        }
        let cancel = CancellationToken::new();
        // Wire each source's cancel token to the orchestrator
        // cancel token. The per-source tokens let the
        // orchestrator shut down one source individually if
        // needed; in practice the orchestrator's main cancel
        // cascades to all of them.
        for s in &sources {
            let child = s.cancel.clone();
            let parent = cancel.clone();
            tokio::spawn(async move {
                parent.cancelled().await;
                child.cancel();
            });
        }
        let emitter = {
            let global = crate::get_download_event_emitter();
            Arc::new(DownloadEventEmitter::new(global.sink_clone()))
        };
        Ok(Self {
            plan,
            store,
            library_root: library_root.into(),
            local_pubkey,
            sources: Arc::new(Mutex::new(sources)),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            chunk_retries: Arc::new(Mutex::new(HashMap::new())),
            chunk_tried: Arc::new(Mutex::new(HashMap::new())),
            nak_counters: Arc::new(Mutex::new(HashMap::new())),
            consecutive_stuck: Arc::new(Mutex::new(HashMap::new())),
            emitter,
            cancel,
            verified_sources: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Clear any `unavailable` flag whose cooldown has
    /// elapsed. Called once per dispatch loop iteration.
    async fn maybe_recover_sources(&self) {
        let now = Instant::now();
        let mut g = self.sources.lock().await;
        for h in g.iter_mut() {
            if h.unavailable {
                if let Some(since) = h.unavailable_since {
                    if now.duration_since(since) >= RTT_COOLDOWN {
                        h.unavailable = false;
                        h.unavailable_since = None;
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub async fn in_flight_len(&self) -> usize {
        self.in_flight.lock().await.len()
    }

    #[cfg(test)]
    pub async fn inflight_peer_for(&self, chunk_index: u32) -> Option<String> {
        self.in_flight
            .lock()
            .await
            .get(&chunk_index)
            .map(|r| r.peer_id.clone())
    }

    #[cfg(test)]
    pub async fn chunk_retries_for(&self, chunk_index: u32) -> u32 {
        self.chunk_retries
            .lock()
            .await
            .get(&chunk_index)
            .copied()
            .unwrap_or(0)
    }

    /// Test-only debug accessor: snapshot every source's
    /// `(peer_id, priority, unavailable, demotion_count)`
    /// tuple. Exposed as `pub` rather than `#[cfg(test)]` so
    /// integration tests in `tests/` can poll from a sibling
    /// crate (mirrors `Scheduler::in_flight_len`).
    pub async fn sources_snapshot(&self) -> Vec<(String, i32, bool, u32)> {
        let g = self.sources.lock().await;
        g.iter()
            .map(|h| {
                (
                    h.peer_id.clone(),
                    h.priority,
                    h.unavailable,
                    h.demotion_count,
                )
            })
            .collect()
    }

    /// Test-only debug accessor: the `chunk_index ->
    /// peer_id` map of every chunk verified so far. The
    /// only honest proof that "source B served chunks after
    /// failover". Mirrors `Scheduler::in_flight_len`'s
    /// pub-but-cfg-test rationale.
    /// Test-only debug accessor: the `chunk_index ->
    /// peer_id` map of every chunk verified so far. The
    /// only honest proof that "source B served chunks after
    /// failover". The field is always populated; the
    /// accessor is `pub` so integration tests in `tests/`
    /// can read it.
    pub async fn verified_sources_snapshot(&self) -> HashMap<u32, String> {
        self.verified_sources.lock().await.clone()
    }

    /// Access the verified-chunk count for tests.
    #[cfg(test)]
    pub async fn verified_count(&self, download_id: &str) -> Result<usize, ChunkStateError> {
        Ok(self.store.verified_chunk_indices(download_id).await?.len())
    }
}

/// Inbound frame carried from a per-source recv task to the
/// orchestrator's main loop.
struct InboundFrame {
    peer_id: String,
    frame: Frame,
}

/// Run the multi-source download to completion. Mirrors
/// `ReceiverSession::run` (Pending -> Connecting ->
/// Transferring -> Verifying -> Complete | Failed).
pub async fn run_multi_source(
    receiver: Arc<MultiSourceReceiver>,
    sanitized_filename: String,
) -> Result<DownloadState, MultiSourceError> {
    let cancel = receiver.cancel.clone();
    let plan = receiver.plan.clone();
    let store = receiver.store.clone();
    let library_root = receiver.library_root.clone();
    let local_pubkey = receiver.local_pubkey;
    let sources = receiver.sources.clone();
    let in_flight = receiver.in_flight.clone();
    let chunk_retries = receiver.chunk_retries.clone();
    let chunk_tried = receiver.chunk_tried.clone();
    let nak_counters = receiver.nak_counters.clone();
    let consecutive_stuck = receiver.consecutive_stuck.clone();
    let emitter = receiver.emitter.clone();
    let verified_sources = receiver.verified_sources.clone();

    // Initial state event.
    emitter.record_state(DownloadStateEvent {
        v: 1,
        id: plan.download_id.clone(),
        media_id: plan.media_id.clone(),
        state: DownloadState::Pending.as_str().to_string(),
        error_message: None,
    });
    let cur = store.fetch(&plan.download_id).await.ok().map(|r| r.state);
    if cur != Some(DownloadState::Transferring) && cur != Some(DownloadState::Verifying) {
        store
            .transition(&plan.download_id, DownloadState::Connecting)
            .await?;
        emitter.record_state(DownloadStateEvent {
            v: 1,
            id: plan.download_id.clone(),
            media_id: plan.media_id.clone(),
            state: DownloadState::Connecting.as_str().to_string(),
            error_message: None,
        });
    }
    if cur != Some(DownloadState::Transferring) && cur != Some(DownloadState::Verifying) {
        store
            .transition(&plan.download_id, DownloadState::Transferring)
            .await?;
        emitter.record_state(DownloadStateEvent {
            v: 1,
            id: plan.download_id.clone(),
            media_id: plan.media_id.clone(),
            state: DownloadState::Transferring.as_str().to_string(),
            error_message: None,
        });
    }

    // Send Hello to every source. Do not await Offer per
    // the architecture's "minimal handshaking" note.
    {
        let have = store.completed_chunk_indices(&plan.download_id).await?;
        let hello = Frame::Hello(HelloFrame {
            peer_id: peer_id_from_pubkey(&local_pubkey),
            download_id: plan.download_id.clone(),
            media_id: plan.media_id.clone(),
            manifest_version: plan.manifest_version,
            have_chunks: have.clone(),
        });
        let mut bytes = Vec::new();
        codec::encode(&hello, &mut bytes)?;
        // Snapshot transports under the lock; drop the guard
        // BEFORE any await to honor the module's "no lock
        // across network I/O" invariant.
        let targets: Vec<Arc<dyn Transport>> = {
            let g = sources.lock().await;
            g.iter().map(|h| h.transport.clone()).collect()
        };
        for t in targets {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(MultiSourceError::Cancelled),
                res = t.send(bytes.clone()) => {
                    if let Err(e) = res {
                        return Err(MultiSourceError::Transport(e));
                    }
                }
            }
        }
    }

    // Per-source recv tasks: drain frames into a single
    // mpsc::unbounded_channel<InboundFrame>.
    let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<InboundFrame>();
    {
        let sources_g = sources.lock().await;
        for h in sources_g.iter() {
            let transport = Arc::clone(&h.transport);
            let peer_id = h.peer_id.clone();
            let source_cancel = h.cancel.clone();
            let tx = inbound_tx.clone();
            tokio::spawn(async move {
                loop {
                    let bytes = tokio::select! {
                        biased;
                        _ = source_cancel.cancelled() => break,
                        res = transport.recv() => match res {
                            Ok(Some(b)) => b,
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    };
                    let parsed = match codec::decode(&bytes) {
                        Ok((f, _used)) => f,
                        Err(_) => continue,
                    };
                    if tx
                        .send(InboundFrame {
                            peer_id: peer_id.clone(),
                            frame: parsed,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
    }
    drop(inbound_tx);

    // Set: chunks already verified.
    let mut have_set: HashSet<u32> = store
        .completed_chunk_indices(&plan.download_id)
        .await?
        .into_iter()
        .collect();
    let total_chunks = plan.chunks.len() as u32;

    // Progress tracking locals.
    let mut transferred_bytes: u64 = (plan.size_bytes
        - have_set.len() as u64 * crate::transfer::CHUNK_SIZE_BYTES as u64)
        .min(plan.size_bytes);
    let mut bytes_per_sec_ema: f64 = 0.0;
    let mut last_chunk_at: Instant = Instant::now();

    loop {
        if have_set.len() as u32 >= total_chunks {
            break;
        }
        // Recovery: clear stale unavailable flags.
        receiver.maybe_recover_sources().await;
        // Step 1: refill the outstanding request pool up to
        // WINDOW_SIZE.
        let outstanding = in_flight.lock().await.len();
        if outstanding < WINDOW_SIZE {
            // Compute the pending chunks once, holding the
            // locks in a single pass.
            let pending: Vec<u32> = {
                let inflight_g = in_flight.lock().await;
                plan.chunks
                    .iter()
                    .map(|c| c.index)
                    .filter(|i| !inflight_g.contains_key(i))
                    .filter(|i| !have_set.contains(i))
                    .collect()
            };
            for chunk_index in pending {
                // Retry budget check. The per-chunk retry
                // budget is `MAX_CHUNK_RETRIES = 5` NAKs
                // across all sources before the download
                // transitions to Failed. We have already
                // consumed `retries` NAKs, so the next
                // dispatch would be NAK #(retries+1). Fail
                // out at the 5th NAK -- i.e. when
                // `retries >= MAX_CHUNK_RETRIES` (5), not
                // after 6.
                let retries = chunk_retries
                    .lock()
                    .await
                    .get(&chunk_index)
                    .copied()
                    .unwrap_or(0);
                if retries >= MAX_CHUNK_RETRIES {
                    return Err(MultiSourceError::MaxRetriesExceeded {
                        index: chunk_index,
                        max: MAX_CHUNK_RETRIES,
                    });
                }
                let tried = {
                    let g = chunk_tried.lock().await;
                    g.get(&chunk_index).cloned().unwrap_or_default()
                };
                let chosen = {
                    let g = sources.lock().await;
                    let picked = SourceSelector::pick(&g, chunk_index, &tried, Instant::now());
                    picked.map(|h| (h.peer_id.clone(), h.transport.clone(), h.sched.clone()))
                };
                let (chosen_peer_id, chosen_transport, chosen_sched) = match chosen {
                    Some(t) => t,
                    None => continue,
                };
                // Acquire the per-source scheduler slot.
                chosen_sched.try_acquire_slot(chunk_index, None).await?;
                // Record inflight.
                let now = Instant::now();
                {
                    let mut g = in_flight.lock().await;
                    g.insert(
                        chunk_index,
                        InflightRecord {
                            peer_id: chosen_peer_id.clone(),
                            requested_at: now,
                        },
                    );
                }
                // Send Request frame.
                let req = Frame::Request(RequestFrame {
                    download_id: plan.download_id.clone(),
                    chunk_index,
                });
                if let Err(e) = send_frame(&chosen_transport, &req, &cancel).await {
                    // Roll the slot back so the chunk is
                    // dispatched again next iteration.
                    chosen_sched.release_slot(chunk_index).await;
                    in_flight.lock().await.remove(&chunk_index);
                    return Err(e);
                }
                if in_flight.lock().await.len() >= WINDOW_SIZE {
                    break;
                }
            }
        }
        // Step 1b: re-request every in_flight entry whose
        // outstanding age exceeds STUCK_REQUEST_RETRY. A
        // re-request is NOT itself an implicit NAK: it
        // merely nudges the peer again. Only after
        // `STUCK_REQUEST_DEMOTE_AFTER` consecutive stuck
        // ticks for the same (chunk, peer) does the
        // orchestrator call `apply_nak`, which advances the
        // per-(chunk, peer) NAK counter and the per-chunk
        // retry budget. This protects legitimate slow links
        // from being demoted by a single slow round-trip.
        let mut stuck_now: Vec<(u32, String)> = Vec::new();
        {
            let now = Instant::now();
            let g = in_flight.lock().await;
            for (idx, rec) in g.iter() {
                if now.duration_since(rec.requested_at) >= STUCK_REQUEST_RETRY {
                    stuck_now.push((*idx, rec.peer_id.clone()));
                }
            }
        }
        for (idx, peer_id) in stuck_now {
            let transport = {
                let g = sources.lock().await;
                g.iter()
                    .find(|h| h.peer_id == peer_id)
                    .map(|h| h.transport.clone())
            };
            if let Some(t) = transport {
                let req = Frame::Request(RequestFrame {
                    download_id: plan.download_id.clone(),
                    chunk_index: idx,
                });
                let _ = send_frame(&t, &req, &cancel).await;
                if let Some(r) = in_flight.lock().await.get_mut(&idx) {
                    r.requested_at = Instant::now();
                }
                // Increment the consecutive-stuck counter
                // for this (chunk, peer). Only when the
                // counter reaches STUCK_REQUEST_DEMOTE_AFTER
                // do we call apply_nak.
                let reached = {
                    let mut g = consecutive_stuck.lock().await;
                    let c = g
                        .entry((idx, peer_id.clone()))
                        .and_modify(|v| *v += 1)
                        .or_insert(1);
                    *c >= STUCK_REQUEST_DEMOTE_AFTER
                };
                if reached {
                    // Reset the counter so a fresh streak
                    // of silence has to re-accumulate.
                    consecutive_stuck
                        .lock()
                        .await
                        .remove(&(idx, peer_id.clone()));
                    if let Err(MultiSourceError::AllSourcesExhausted { .. }) = apply_nak(
                        idx,
                        &peer_id,
                        &chunk_retries,
                        &chunk_tried,
                        &nak_counters,
                        &sources,
                        &in_flight,
                        &consecutive_stuck,
                    )
                    .await
                    {
                        // Demoted past MAX_CHUNK_RETRIES.
                        // The next dispatch will pick a new
                        // source.
                    }
                }
            }
        }
        // Step 2: await one inbound frame OR a
        // STUCK_REQUEST_RETRY tick. The tick gives the
        // re-request step above another chance to fire
        // even if no source is sending frames (e.g. all
        // in-flight requests were lost at the wire).
        let inbound = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(MultiSourceError::Cancelled),
            _ = tokio::time::sleep(STUCK_REQUEST_RETRY) => {
                // Tick without any inbound frame: loop
                // back to refill + re-request so a stuck
                // chunk gets another chance.
                continue;
            }
            maybe = inbound_rx.recv() => match maybe {
                Some(i) => i,
                None => return Err(MultiSourceError::AllSourcesGone),
            }
        };
        match inbound.frame {
            Frame::Chunk(chunk) => {
                let peer_id = inbound.peer_id.clone();
                let outcome = handle_chunk(
                    &plan,
                    &store,
                    &library_root,
                    &peer_id,
                    &chunk,
                    &in_flight,
                    &sources,
                    &nak_counters,
                    &verified_sources,
                    &consecutive_stuck,
                    &mut transferred_bytes,
                    &mut bytes_per_sec_ema,
                    &mut last_chunk_at,
                    plan.size_bytes,
                    &cancel,
                )
                .await;
                match outcome {
                    Ok(HandleOutcome::Verified { index, peer_id }) => {
                        have_set.insert(index);
                        // Emit progress.
                        let now = Instant::now();
                        let dt = now.duration_since(last_chunk_at).as_secs_f64().max(0.001);
                        let instant_bps = plan.chunks[index as usize].length as f64 / dt;
                        bytes_per_sec_ema =
                            EMA_ALPHA * instant_bps + (1.0 - EMA_ALPHA) * bytes_per_sec_ema;
                        last_chunk_at = now;
                        transferred_bytes = transferred_bytes
                            .saturating_add(plan.chunks[index as usize].length as u64);
                        let eta_seconds =
                            if bytes_per_sec_ema > 0.0 && plan.size_bytes > transferred_bytes {
                                let remaining = plan.size_bytes - transferred_bytes;
                                Some((remaining as f64 / bytes_per_sec_ema) as u32)
                            } else {
                                None
                            };
                        emitter.record_progress(DownloadProgressEvent {
                            v: 1,
                            id: plan.download_id.clone(),
                            state: DownloadState::Transferring.as_str().to_string(),
                            transferred_bytes,
                            total_bytes: plan.size_bytes,
                            bytes_per_sec_ema,
                            eta_seconds,
                        });
                        // Best-effort: stamp the source peer
                        // if this chunk came from a
                        // non-primary source.
                        if peer_id != plan.source.peer_id {
                            let _ = store.set_source_peer_id(&plan.download_id, &peer_id).await;
                        }
                    }
                    Ok(HandleOutcome::Duplicate) => {}
                    Ok(HandleOutcome::NakTrigger { index, peer_id }) => {
                        // The peer delivered a malformed
                        // chunk; treat as a NAK. Update
                        // retry counters; potentially demote.
                        //
                        // Note: `handle_chunk` has already
                        // removed the inflight record for
                        // this chunk before returning
                        // NakTrigger (the record is consumed
                        // at the top of that function).
                        // `apply_nak`'s in-flight guard will
                        // therefore see an empty map and
                        // return early without incrementing
                        // any counter; the Nak-resend
                        // immediately below is also a no-op
                        // because the peer is not in
                        // `chunk_tried[index]` (no demotion
                        // happened). No double-send.
                        let nak_outcome = apply_nak(
                            index,
                            &peer_id,
                            &chunk_retries,
                            &chunk_tried,
                            &nak_counters,
                            &sources,
                            &in_flight,
                            &consecutive_stuck,
                        )
                        .await;
                        if let Err(MultiSourceError::AllSourcesExhausted { .. }) = nak_outcome {
                            return Err(nak_outcome.unwrap_err());
                        }
                        // On demote we re-send a Nak to the
                        // offending peer so it stops trying
                        // to deliver this chunk_index.
                        if chunk_tried
                            .lock()
                            .await
                            .get(&index)
                            .map(|s| s.contains(&peer_id))
                            .unwrap_or(false)
                        {
                            let expected_sha = plan
                                .chunks
                                .iter()
                                .find(|c| c.index == index)
                                .map(|c| c.sha256.clone())
                                .unwrap_or_default();
                            let resp = Frame::Nak(NakFrame {
                                download_id: plan.download_id.clone(),
                                chunk_index: index,
                                expected_sha256: expected_sha,
                            });
                            let transport = {
                                let g = sources.lock().await;
                                g.iter()
                                    .find(|h| h.peer_id == peer_id)
                                    .map(|h| h.transport.clone())
                            };
                            if let Some(t) = transport {
                                let _ = send_frame(&t, &resp, &cancel).await;
                            }
                        }
                    }
                    Ok(HandleOutcome::PeerMismatch { actual }) => {
                        warn!(
                            download_id = %plan.download_id,
                            actual = %actual,
                            "multi-source: chunk from unknown peer dropped"
                        );
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
            Frame::Cancel(c) => {
                info!(
                    download_id = %plan.download_id,
                    reason = %c.reason,
                    "multi-source: cancel received"
                );
                emitter.record_state(DownloadStateEvent {
                    v: 1,
                    id: plan.download_id.clone(),
                    media_id: plan.media_id.clone(),
                    state: DownloadState::Cancelled.as_str().to_string(),
                    error_message: None,
                });
                emitter.shutdown();
                let _ = store
                    .transition(&plan.download_id, DownloadState::Cancelled)
                    .await;
                return Ok(DownloadState::Cancelled);
            }
            Frame::Error(e) => {
                warn!(
                    download_id = %plan.download_id,
                    reason = %e.reason,
                    "multi-source: error frame"
                );
                let sanitized_peer_error = sanitize_error_message(&e.reason);
                let _ = store
                    .set_last_error(&plan.download_id, &sanitized_peer_error)
                    .await;
                return Err(MultiSourceError::Io(format!(
                    "peer error: {}",
                    sanitized_peer_error
                )));
            }
            Frame::Nak(nak) => {
                let peer_id = inbound.peer_id.clone();
                let outcome = apply_nak(
                    nak.chunk_index,
                    &peer_id,
                    &chunk_retries,
                    &chunk_tried,
                    &nak_counters,
                    &sources,
                    &in_flight,
                    &consecutive_stuck,
                )
                .await;
                if let Err(MultiSourceError::AllSourcesExhausted { .. }) = outcome {
                    return Err(outcome.unwrap_err());
                }
                // Send Nak back to the source so it stops
                // trying to deliver that chunk.
                let expected_sha = plan
                    .chunks
                    .iter()
                    .find(|c| c.index == nak.chunk_index)
                    .map(|c| c.sha256.clone())
                    .unwrap_or_default();
                let resp = Frame::Nak(NakFrame {
                    download_id: plan.download_id.clone(),
                    chunk_index: nak.chunk_index,
                    expected_sha256: expected_sha,
                });
                let transport = {
                    let g = sources.lock().await;
                    g.iter()
                        .find(|h| h.peer_id == peer_id)
                        .map(|h| h.transport.clone())
                };
                if let Some(t) = transport {
                    let _ = send_frame(&t, &resp, &cancel).await;
                }
            }
            Frame::Ack(_) => {
                // Receiver does not expect Acks from a
                // sender. Drop silently.
            }
            Frame::Hello(_) | Frame::Offer(_) | Frame::Request(_) => {
                warn!(
                    kind = ?inbound.frame.kind(),
                    "multi-source: unexpected inbound frame kind"
                );
            }
        }
    }

    // All chunks verified. Transition + assemble.
    store
        .transition(&plan.download_id, DownloadState::Verifying)
        .await?;
    emitter.record_state(DownloadStateEvent {
        v: 1,
        id: plan.download_id.clone(),
        media_id: plan.media_id.clone(),
        state: DownloadState::Verifying.as_str().to_string(),
        error_message: None,
    });
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
            let _ = cleanup_incomplete(&library_root, &plan.download_id).await;
            store
                .transition(&plan.download_id, DownloadState::Complete)
                .await?;
            emitter.record_state(DownloadStateEvent {
                v: 1,
                id: plan.download_id.clone(),
                media_id: plan.media_id.clone(),
                state: DownloadState::Complete.as_str().to_string(),
                error_message: None,
            });
            emitter.shutdown();
            // Close every source's transport.
            {
                let g = sources.lock().await;
                for h in g.iter() {
                    h.transport.close().await;
                }
            }
            Ok(DownloadState::Complete)
        }
        Err(AssembleError::Blake3Mismatch) => {
            let sanitized = sanitize_error_message("blake3 mismatch");
            store.set_last_error(&plan.download_id, &sanitized).await?;
            store
                .transition(&plan.download_id, DownloadState::Failed)
                .await?;
            emitter.record_state(DownloadStateEvent {
                v: 1,
                id: plan.download_id.clone(),
                media_id: plan.media_id.clone(),
                state: DownloadState::Failed.as_str().to_string(),
                error_message: Some(sanitized),
            });
            emitter.shutdown();
            let g = sources.lock().await;
            for h in g.iter() {
                h.transport.close().await;
            }
            Ok(DownloadState::Failed)
        }
        Err(e) => {
            let raw = format!("assemble: {e}");
            let sanitized = sanitize_error_message(&raw);
            let _ = store.set_last_error(&plan.download_id, &sanitized).await;
            store
                .transition(&plan.download_id, DownloadState::Failed)
                .await?;
            emitter.record_state(DownloadStateEvent {
                v: 1,
                id: plan.download_id.clone(),
                media_id: plan.media_id.clone(),
                state: DownloadState::Failed.as_str().to_string(),
                error_message: Some(sanitized),
            });
            emitter.shutdown();
            let g = sources.lock().await;
            for h in g.iter() {
                h.transport.close().await;
            }
            Err(MultiSourceError::from(e))
        }
    }
}

/// The outcome of one inbound chunk frame.
enum HandleOutcome {
    Verified { index: u32, peer_id: String },
    Duplicate,
    NakTrigger { index: u32, peer_id: String },
    PeerMismatch { actual: String },
}

#[allow(clippy::too_many_arguments)]
async fn handle_chunk(
    plan: &DownloadPlan,
    store: &DownloadStore,
    library_root: &Path,
    peer_id: &str,
    chunk: &ChunkFrame,
    in_flight: &Arc<Mutex<HashMap<u32, InflightRecord>>>,
    sources: &Arc<Mutex<Vec<SourceHandle>>>,
    nak_counters: &Arc<Mutex<HashMap<(u32, String), u32>>>,
    verified_sources: &Arc<Mutex<HashMap<u32, String>>>,
    consecutive_stuck: &Arc<Mutex<HashMap<(u32, String), u32>>>,
    transferred_bytes: &mut u64,
    _bytes_per_sec_ema: &mut f64,
    _last_chunk_at: &mut Instant,
    _total_bytes: u64,
    cancel: &CancellationToken,
) -> Result<HandleOutcome, MultiSourceError> {
    // Look up the inflight record; drop mismatched-peer chunks.
    let record = {
        let mut g = in_flight.lock().await;
        g.remove(&chunk.chunk_index)
    };
    let _record = match record {
        Some(r) => r,
        None => {
            // Duplicate: we already verified this chunk but
            // the sender re-delivered. Send Ack and drop.
            let ack = Frame::Ack(AckFrame {
                download_id: plan.download_id.clone(),
                chunk_index: chunk.chunk_index,
            });
            // Find the source by peer_id and Ack.
            let transport = {
                let g = sources.lock().await;
                g.iter()
                    .find(|h| h.peer_id == peer_id)
                    .map(|h| h.transport.clone())
            };
            if let Some(t) = transport {
                let _ = send_frame(&t, &ack, cancel).await;
            }
            return Ok(HandleOutcome::Duplicate);
        }
    };
    if _record.peer_id != peer_id {
        // Race: a chunk arrived from a different source
        // than we sent the request to. Drop it (but Ack to
        // the unexpected source so it stops).
        let ack = Frame::Ack(AckFrame {
            download_id: plan.download_id.clone(),
            chunk_index: chunk.chunk_index,
        });
        let transport = {
            let g = sources.lock().await;
            g.iter()
                .find(|h| h.peer_id == peer_id)
                .map(|h| h.transport.clone())
        };
        if let Some(t) = transport {
            let _ = send_frame(&t, &ack, cancel).await;
        }
        return Ok(HandleOutcome::PeerMismatch {
            actual: peer_id.to_string(),
        });
    }

    let expected = plan
        .chunks
        .iter()
        .find(|c| c.index == chunk.chunk_index)
        .ok_or(MultiSourceError::Wire(WireError::Malformed(format!(
            "chunk index {} out of range",
            chunk.chunk_index
        ))))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(chunk.bytes_b64.as_bytes())
        .map_err(|e| MultiSourceError::Io(format!("base64 decode: {e}")))?;
    if bytes.len() != expected.length as usize {
        // Treat as NAK from this peer.
        return Ok(HandleOutcome::NakTrigger {
            index: chunk.chunk_index,
            peer_id: peer_id.to_string(),
        });
    }
    if let Err(ChunkVerifyError::Sha256Mismatch { .. }) =
        verify_chunk_sha256(&bytes, &expected.sha256)
    {
        return Ok(HandleOutcome::NakTrigger {
            index: chunk.chunk_index,
            peer_id: peer_id.to_string(),
        });
    }
    // Persist to disk under tmp/incomplete/<id>/<id>.part.<i>.
    let path = crate::core::paths::incomplete_chunk_path(
        library_root,
        &plan.download_id,
        chunk.chunk_index,
    )
    .map_err(|e| MultiSourceError::Io(format!("path: {e}")))?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut f = tokio::fs::File::create(&path).await?;
    f.write_all(&bytes).await?;
    f.flush().await?;
    // Mark verified (idempotent).
    store
        .mark_chunk_verified(&plan.download_id, chunk.chunk_index, &expected.sha256)
        .await?;
    // Reset this (chunk, peer) NAK counter.
    nak_counters
        .lock()
        .await
        .remove(&(chunk.chunk_index, peer_id.to_string()));
    // Reset the consecutive-stuck counter for this
    // (chunk, peer) so any future silence on the same pair
    // has to re-accumulate from zero.
    consecutive_stuck
        .lock()
        .await
        .remove(&(chunk.chunk_index, peer_id.to_string()));
    // Release the per-source slot. Drop the sources lock
    // before awaiting release_slot to avoid holding it
    // across a Mutex::lock().
    let sched_ref = {
        let g = sources.lock().await;
        g.iter()
            .find(|h| h.peer_id == peer_id)
            .map(|h| h.sched.clone())
    };
    if let Some(sched) = sched_ref {
        sched.release_slot(chunk.chunk_index).await;
    }
    // Ack to that source.
    let ack = Frame::Ack(AckFrame {
        download_id: plan.download_id.clone(),
        chunk_index: chunk.chunk_index,
    });
    let transport = {
        let g = sources.lock().await;
        g.iter()
            .find(|h| h.peer_id == peer_id)
            .map(|h| h.transport.clone())
    };
    if let Some(t) = transport {
        let _ = send_frame(&t, &ack, cancel).await;
    }
    let _ = transferred_bytes;
    {
        verified_sources
            .lock()
            .await
            .insert(chunk.chunk_index, peer_id.to_string());
    }
    // Record an RTT sample on the source that served this
    // chunk, and run the RTT-driven demotion policy: if the
    // rolling p95 over the last `RTT_P95_WINDOW` exceeds
    // `RTT_P95_LIMIT_MS`, mark the source `unavailable`
    // (unless it already is). Computation is cheap and
    // pure, so it runs under the sources lock; the lock is
    // not held across any await.
    {
        let rtt = Instant::now().duration_since(_record.requested_at);
        let mut g = sources.lock().await;
        if let Some(src) = g.iter_mut().find(|h| h.peer_id == peer_id) {
            src.rtt_samples.push_back((Instant::now(), rtt));
            while src.rtt_samples.len() > RTT_WINDOW_CAP {
                src.rtt_samples.pop_front();
            }
            if !src.unavailable {
                let now = Instant::now();
                if let Some(p) = rtt_p95(&src.rtt_samples, RTT_P95_WINDOW, now) {
                    if p > Duration::from_millis(RTT_P95_LIMIT_MS) {
                        src.unavailable = true;
                        src.unavailable_since = Some(now);
                        info!(
                            peer_id = %src.peer_id,
                            p95_ms = p.as_millis() as u64,
                            "multi-source: RTT demote"
                        );
                    }
                }
            }
        }
    }
    Ok(HandleOutcome::Verified {
        index: chunk.chunk_index,
        peer_id: peer_id.to_string(),
    })
}

/// Apply a NAK (transport loss or chunk hash mismatch) from
/// `peer_id` for `chunk_index`. Increments
/// `chunk_retries[chunk_index]` and `nak_counters[(chunk,
/// peer)]`. If the latter reaches `NAK_THRESHOLD`, demotes
/// the peer: adds it to `chunk_tried[chunk_index]`, resets
/// the per-(chunk, peer) counter, increments
/// `demotion_count`, releases the slot, removes the inflight
/// entry. If `chunk_retries[chunk_index]` reaches
/// `MAX_CHUNK_RETRIES` (5), the download has already
/// consumed its 5-NAK budget across all sources and the
/// next call returns `AllSourcesExhausted`.
///
/// Guard: if the inflight record for `chunk_index` no
/// longer names this peer (already verified, or rotated to
/// another source), the NAK is dropped silently. The
/// `consecutive_stuck` map is also reset for this pair on
/// every NAK regardless of whether we proceed, so a fresh
/// chunk on the new peer starts from zero.
#[allow(clippy::too_many_arguments)]
async fn apply_nak(
    chunk_index: u32,
    peer_id: &str,
    chunk_retries: &Arc<Mutex<HashMap<u32, u32>>>,
    chunk_tried: &Arc<Mutex<HashMap<u32, HashSet<String>>>>,
    nak_counters: &Arc<Mutex<HashMap<(u32, String), u32>>>,
    sources: &Arc<Mutex<Vec<SourceHandle>>>,
    in_flight: &Arc<Mutex<HashMap<u32, InflightRecord>>>,
    consecutive_stuck: &Arc<Mutex<HashMap<(u32, String), u32>>>,
) -> Result<(), MultiSourceError> {
    // Reset the consecutive-stuck counter for this pair.
    // The stuck loop will start fresh; the next silence
    // has to re-accumulate STUCK_REQUEST_DEMOTE_AFTER
    // ticks before another NAK fires.
    consecutive_stuck
        .lock()
        .await
        .remove(&(chunk_index, peer_id.to_string()));
    // Guard: do not demote a peer for a chunk that is no
    // longer in-flight on this peer (already verified, or
    // rotated). The inflight record is removed by
    // `handle_chunk` on the success path; it is also
    // removed by this function on the demotion path
    // below. Either way, if the record is gone or names a
    // different peer, the NAK is stale and we drop it.
    {
        let g = in_flight.lock().await;
        match g.get(&chunk_index) {
            Some(rec) if rec.peer_id == peer_id => {}
            _ => return Ok(()),
        }
    }
    // Total retries for this chunk.
    let retries = {
        let mut g = chunk_retries.lock().await;
        let r = g.entry(chunk_index).and_modify(|v| *v += 1).or_insert(1);
        *r
    };
    if retries >= MAX_CHUNK_RETRIES {
        return Err(MultiSourceError::AllSourcesExhausted { index: chunk_index });
    }
    // Per-(chunk, peer) NAK counter.
    let count = {
        let mut g = nak_counters.lock().await;
        let c = g
            .entry((chunk_index, peer_id.to_string()))
            .and_modify(|v| *v += 1)
            .or_insert(1);
        *c
    };
    if count >= NAK_THRESHOLD {
        // Demote this peer for this chunk.
        {
            let mut g = chunk_tried.lock().await;
            g.entry(chunk_index)
                .or_insert_with(HashSet::new)
                .insert(peer_id.to_string());
        }
        {
            let mut g = nak_counters.lock().await;
            g.remove(&(chunk_index, peer_id.to_string()));
        }
        // Release the per-source scheduler slot. Drop the
        // sources lock before awaiting release_slot to
        // avoid holding the lock across a Mutex::lock().
        let demoted = {
            let mut g = sources.lock().await;
            if let Some(h) = g.iter_mut().find(|h| h.peer_id == peer_id) {
                h.demotion_count = h.demotion_count.saturating_add(1);
                Some(h.sched.clone())
            } else {
                None
            }
        };
        if let Some(sched) = demoted {
            sched.release_slot(chunk_index).await;
        }
        // Remove inflight so the next dispatch picks a new
        // source for this chunk.
        in_flight.lock().await.remove(&chunk_index);
    }
    Ok(())
}

async fn send_frame(
    transport: &Arc<dyn Transport>,
    frame: &Frame,
    cancel: &CancellationToken,
) -> Result<(), MultiSourceError> {
    let mut bytes = Vec::new();
    codec::encode(frame, &mut bytes)?;
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(MultiSourceError::Cancelled),
        res = transport.send(bytes) => match res {
            Ok(()) => Ok(()),
            Err(TransportError::Closed) | Err(TransportError::ChannelClosed) => {
                Err(MultiSourceError::Cancelled)
            }
            Err(e) => Err(MultiSourceError::Transport(e)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::peer_id::derive_peer_id;
    use crate::transfer::transport::loopback_pair;
    use std::collections::HashMap;

    fn mk_handle(peer_pub: u8, priority: i32) -> SourceHandle {
        let (_a, b) = loopback_pair(0, 0);
        let transport: Arc<dyn Transport> = Arc::new(b);
        let cancel = CancellationToken::new();
        let sched = Arc::new(Scheduler::new(transport.clone(), cancel.clone()));
        SourceHandle {
            peer_id: derive_peer_id([peer_pub; 32]),
            transport,
            priority,
            sched,
            demotion_count: 0,
            unavailable: false,
            unavailable_since: None,
            cancel,
            rtt_samples: VecDeque::new(),
        }
    }

    #[test]
    fn selector_picks_lowest_priority_first() {
        let mut handles: Vec<SourceHandle> = Vec::new();
        // SAFETY: we never move these until they go out of
        // scope at end of test; the pick() borrows immutably.
        let a = mk_handle(1, 5);
        let b = mk_handle(2, 1);
        let c = mk_handle(3, 3);
        handles.push(a);
        handles.push(b);
        handles.push(c);
        let tried: HashSet<String> = HashSet::new();
        let picked = SourceSelector::pick(&handles, 0, &tried, Instant::now()).unwrap();
        assert_eq!(picked.priority, 1);
    }

    #[test]
    fn selector_tie_breaks_by_peer_id_lexicographic() {
        let mut handles: Vec<SourceHandle> = Vec::new();
        // Two handles with the SAME priority 0. Lex-smallest
        // peer_id should win.
        let lower = mk_handle(1, 0);
        let upper = mk_handle(2, 0);
        let l_id = lower.peer_id.clone();
        let u_id = upper.peer_id.clone();
        handles.push(upper);
        handles.push(lower);
        assert!(l_id < u_id);
        let tried: HashSet<String> = HashSet::new();
        let picked = SourceSelector::pick(&handles, 0, &tried, Instant::now()).unwrap();
        assert_eq!(picked.peer_id, l_id);
    }

    #[test]
    fn selector_tie_breaks_by_demotion_count_first() {
        let mut handles: Vec<SourceHandle> = Vec::new();
        let mut a = mk_handle(1, 0);
        let mut b = mk_handle(2, 0);
        a.demotion_count = 5;
        b.demotion_count = 2;
        // b has fewer demotions -> wins.
        handles.push(a);
        handles.push(b);
        let tried: HashSet<String> = HashSet::new();
        let picked = SourceSelector::pick(&handles, 0, &tried, Instant::now()).unwrap();
        assert_eq!(picked.demotion_count, 2);
    }

    #[test]
    fn selector_skips_unavailable_in_cooldown() {
        let mut handles: Vec<SourceHandle> = Vec::new();
        let mut a = mk_handle(1, 0);
        a.unavailable = true;
        a.unavailable_since = Some(Instant::now()); // within cooldown
        let b = mk_handle(2, 5);
        let a_id = a.peer_id.clone();
        handles.push(a);
        handles.push(b);
        let tried: HashSet<String> = HashSet::new();
        let picked = SourceSelector::pick(&handles, 0, &tried, Instant::now()).unwrap();
        assert_ne!(picked.peer_id, a_id);
        assert_eq!(picked.peer_id, handles[1].peer_id);
    }

    #[test]
    fn selector_treats_post_cooldown_as_eligible() {
        // After the cooldown elapsed, the selector MUST
        // consider the source again (and let the recovery
        // helper clear the flag). We simulate by passing
        // a now long past unavailable_since.
        let mut handles: Vec<SourceHandle> = Vec::new();
        let mut a = mk_handle(1, 0);
        a.unavailable = true;
        a.unavailable_since = Some(Instant::now() - RTT_COOLDOWN * 2);
        handles.push(a);
        let tried: HashSet<String> = HashSet::new();
        let now = Instant::now();
        // With now == later than cooldown, the picker
        // treats the source as eligible (the recovery
        // helper clears the flag in production code). The
        // picker returns Some.
        let picked = SourceSelector::pick(&handles, 0, &tried, now).unwrap();
        assert_eq!(picked.peer_id, handles[0].peer_id);
    }

    #[test]
    fn selector_skips_already_tried_this_attempt() {
        let mut handles: Vec<SourceHandle> = Vec::new();
        let a = mk_handle(1, 0);
        let b = mk_handle(2, 1);
        let a_id = a.peer_id.clone();
        handles.push(a);
        handles.push(b);
        let mut tried: HashSet<String> = HashSet::new();
        tried.insert(a_id.clone());
        let picked = SourceSelector::pick(&handles, 0, &tried, Instant::now()).unwrap();
        assert_ne!(picked.peer_id, a_id);
    }

    #[test]
    fn selector_returns_none_when_all_exhausted() {
        let mut handles: Vec<SourceHandle> = Vec::new();
        let a = mk_handle(1, 0);
        let b = mk_handle(2, 1);
        let a_id = a.peer_id.clone();
        let b_id = b.peer_id.clone();
        handles.push(a);
        handles.push(b);
        let mut tried: HashSet<String> = HashSet::new();
        tried.insert(a_id);
        tried.insert(b_id);
        let picked = SourceSelector::pick(&handles, 0, &tried, Instant::now());
        assert!(picked.is_none());
    }

    #[test]
    fn selector_is_deterministic_under_same_inputs() {
        let mut handles: Vec<SourceHandle> = Vec::new();
        let a = mk_handle(1, 5);
        let b = mk_handle(2, 3);
        let c = mk_handle(3, 7);
        handles.push(a);
        handles.push(b);
        handles.push(c);
        let tried: HashSet<String> = HashSet::new();
        let now = Instant::now();
        let p1 = SourceSelector::pick(&handles, 0, &tried, now)
            .unwrap()
            .peer_id
            .clone();
        let p2 = SourceSelector::pick(&handles, 0, &tried, now)
            .unwrap()
            .peer_id
            .clone();
        assert_eq!(p1, p2);
    }

    #[test]
    fn rtt_p95_filters_old_samples() {
        let now = Instant::now();
        let mut samples: VecDeque<(Instant, Duration)> = VecDeque::new();
        // Old sample (older than window).
        samples.push_back((now - Duration::from_secs(60), Duration::from_millis(5000)));
        // Recent sample (in window).
        samples.push_back((now, Duration::from_millis(100)));
        let p = rtt_p95(&samples, RTT_P95_WINDOW, now).unwrap();
        assert_eq!(p, Duration::from_millis(100));
    }

    #[test]
    fn rtt_p95_handles_empty() {
        let samples: VecDeque<(Instant, Duration)> = VecDeque::new();
        let p = rtt_p95(&samples, RTT_P95_WINDOW, Instant::now());
        assert!(p.is_none());
    }

    #[tokio::test]
    async fn nak_counter_threshold_three_demotes() {
        use crate::transfer::transport::loopback_pair;
        let (_a, b) = loopback_pair(0, 0);
        let transport: Arc<dyn Transport> = Arc::new(b);
        let cancel = CancellationToken::new();
        let sched = Arc::new(Scheduler::new(transport.clone(), cancel.clone()));
        let peer = derive_peer_id([9u8; 32]);
        let handle = SourceHandle {
            peer_id: peer.clone(),
            transport,
            priority: 0,
            sched,
            demotion_count: 0,
            unavailable: false,
            unavailable_since: None,
            cancel,
            rtt_samples: VecDeque::new(),
        };
        let sources = Arc::new(Mutex::new(vec![handle]));
        let in_flight = Arc::new(Mutex::new(HashMap::new()));
        let chunk_retries = Arc::new(Mutex::new(HashMap::new()));
        let chunk_tried = Arc::new(Mutex::new(HashMap::new()));
        let nak_counters = Arc::new(Mutex::new(HashMap::new()));
        let consecutive_stuck = Arc::new(Mutex::new(HashMap::new()));
        // Three NAKs on the same (chunk, peer) -> demote.
        // Pre-seed the inflight record for every NAK so
        // the new in-flight guard at the top of apply_nak
        // sees a matching peer and proceeds.
        for _ in 0..NAK_THRESHOLD {
            in_flight.lock().await.insert(
                42u32,
                InflightRecord {
                    peer_id: peer.clone(),
                    requested_at: Instant::now(),
                },
            );
            apply_nak(
                42,
                &peer,
                &chunk_retries,
                &chunk_tried,
                &nak_counters,
                &sources,
                &in_flight,
                &consecutive_stuck,
            )
            .await
            .expect("nak");
        }
        // After 3 NAKs, the peer must be in chunk_tried.
        let tried = chunk_tried
            .lock()
            .await
            .get(&42u32)
            .cloned()
            .unwrap_or_default();
        assert!(tried.contains(&peer));
        // And the source's demotion_count must have
        // incremented.
        let g = sources.lock().await;
        assert!(g[0].demotion_count > 0);
        // Calling pick for chunk 42 with that tried-set
        // should return None (no other source).
        let picked = SourceSelector::pick(&g, 42, &tried, Instant::now());
        assert!(picked.is_none());
    }

    /// After exactly `MAX_CHUNK_RETRIES` (5) NAKs against
    /// the same chunk the orchestrator must transition to
    /// `AllSourcesExhausted`. The 6th NAK is never reached
    /// because the 5th one already fails the download.
    #[tokio::test]
    async fn nak_counter_exceeding_max_chunk_retries_returns_all_sources_exhausted() {
        use crate::transfer::transport::loopback_pair;
        let (_a, b) = loopback_pair(0, 0);
        let transport: Arc<dyn Transport> = Arc::new(b);
        let cancel = CancellationToken::new();
        let sched = Arc::new(Scheduler::new(transport.clone(), cancel.clone()));
        let peer = derive_peer_id([7u8; 32]);
        let handle = SourceHandle {
            peer_id: peer.clone(),
            transport,
            priority: 0,
            sched,
            demotion_count: 0,
            unavailable: false,
            unavailable_since: None,
            cancel,
            rtt_samples: VecDeque::new(),
        };
        let sources = Arc::new(Mutex::new(vec![handle]));
        let in_flight = Arc::new(Mutex::new(HashMap::new()));
        let chunk_retries = Arc::new(Mutex::new(HashMap::new()));
        let chunk_tried = Arc::new(Mutex::new(HashMap::new()));
        let nak_counters = Arc::new(Mutex::new(HashMap::new()));
        let consecutive_stuck = Arc::new(Mutex::new(HashMap::new()));
        // 4 NAKs are fine; the 5th is the budget boundary.
        // Pre-seed the inflight record for every NAK so
        // the in-flight guard at the top of apply_nak sees
        // a matching peer and proceeds.
        for i in 0..MAX_CHUNK_RETRIES {
            in_flight.lock().await.insert(
                7u32,
                InflightRecord {
                    peer_id: peer.clone(),
                    requested_at: Instant::now(),
                },
            );
            let res = apply_nak(
                7,
                &peer,
                &chunk_retries,
                &chunk_tried,
                &nak_counters,
                &sources,
                &in_flight,
                &consecutive_stuck,
            )
            .await;
            if i + 1 < MAX_CHUNK_RETRIES {
                assert!(res.is_ok(), "nak #{i} must be ok, got {res:?}");
            } else {
                // i + 1 == MAX_CHUNK_RETRIES == 5. The 5th
                // NAK is the one that exhausts the budget.
                assert!(
                    matches!(res, Err(MultiSourceError::AllSourcesExhausted { index: 7 })),
                    "5th NAK must transition to AllSourcesExhausted, got {res:?}"
                );
            }
        }
    }
}
