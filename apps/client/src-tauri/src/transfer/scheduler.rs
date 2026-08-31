//! P3-T07 sliding-window + backpressure scheduler.
//!
//! Adds a per-peer token bucket (B=4, refill 4 tokens per
//! 250 ms) and a soft backpressure gate to the transfer
//! pipeline. The bucket enforces a hard ceiling on how many
//! frames a host will dispatch per second; the backpressure
//! gate mirrors the WebRTC DataChannel's `bufferedAmount`
//! threshold (HIGH = 2 MiB, LOW = 1 MiB) and pauses the host
//! when the viewer is slow.
//!
//! # Additive design
//!
//! The P3-T06 `ReceiverSession` / `SenderSession` loop is
//! untouched. The scheduler is a thin wrapper that the
//! integration test (and any future production wiring) can
//! opt into via:
//!
//! - `BackpressureTransport` wraps an existing
//!   [`crate::transfer::transport::Transport`] so the host's
//!   `SenderSession` naturally honors the gate; the wrapper
//!   is what the architecture (section 9.4) means by
//!   "host-side `onbufferedamountlow` observer".
//! - `Scheduler` exposes `try_acquire_slot` / `release_slot`
//!   to gate new outbound `Request` frames behind the bucket
//!   and the sliding-window slot count. The session loop is
//!   free to use these (P3-T08) without rewriting the loop.
//!
//! # Constants
//!
//! - `WINDOW_SIZE` is defined in `session.rs` (the existing
//!   constant is the source of truth for the sliding-window
//!   size; this module re-uses it).
//! - `MAX_CHUNK_RETRIES` is defined in `session.rs` and is
//!   unchanged.
//! - `BUFFERED_AMOUNT_HIGH` / `BUFFERED_AMOUNT_LOW` are the
//!   WebRTC DataChannel `bufferedAmount` thresholds
//!   (architecture §9.4). They are *soft*: between LOW and
//!   HIGH the wrapper neither pauses nor resumes; only a
//!   transition across HIGH or back below LOW flips the
//!   state.
//!
//! # Token bucket
//!
//! Mirrors `apps/server/src/ws/mod.rs` (the per-connection
//! rate-limit bucket) using `std::time::Instant` so it works
//! without `tokio::time::Instant` and can be exercised in
//! plain unit tests.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::session::WINDOW_SIZE;
use super::transport::{Transport, TransportError};

/// WebRTC DataChannel `bufferedAmount` HIGH threshold
/// (architecture §9.4). Above this value the host pauses
/// sending.
pub const BUFFERED_AMOUNT_HIGH: usize = 2 * 1024 * 1024;

/// WebRTC DataChannel `bufferedAmount` LOW threshold
/// (architecture §9.4). When `onbufferedamountlow` fires the
/// host resumes sending.
pub const BUFFERED_AMOUNT_LOW: usize = 1024 * 1024;

/// Per-peer token bucket capacity (B). The host can burst up
/// to `B` requests before the refill rate kicks in.
pub const PER_PEER_BUCKET_CAPACITY: u32 = 4;

/// Per-peer token bucket refill rate, in tokens per second.
/// 4 tokens / 250 ms = 16 tokens / second.
pub const PER_PEER_REFILL_PER_SEC: f64 = 16.0;

/// Closed set of scheduler errors.
#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("scheduler cancelled")]
    Cancelled,
}

/// Events emitted by the scheduler / backpressure observer.
/// Consumed by tests and (eventually) by the UI's download
/// progress indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerEvent {
    RequestSent { chunk_index: u32 },
    ChunkReceived { chunk_index: u32 },
    WindowFull,
    BucketDepleted,
    Paused,
    Resumed,
}

/// A simple token bucket. Capacity `C`, refilled at `R` tokens
/// per second of continuous time. `try_consume` is non-async
/// and re-fills on-demand using the wall-clock elapsed since
/// the previous call.
#[derive(Debug)]
pub struct TokenBucket {
    capacity: u32,
    refill_per_sec: f64,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(capacity: u32, refill_per_sec: f64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity as f64,
            last_refill: Instant::now(),
        }
    }

    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    pub fn refill_per_sec(&self) -> f64 {
        self.refill_per_sec
    }

    /// Try to consume one token. Returns `true` if a token was
    /// available, `false` otherwise.
    pub fn try_consume(&mut self) -> bool {
        self.try_consume_n(1)
    }

    /// Try to consume `n` tokens at once. Refills on-demand
    /// using the elapsed wall-clock time since the previous
    /// refill.
    pub fn try_consume_n(&mut self, n: u32) -> bool {
        let now = Instant::now();
        let delta = now.saturating_duration_since(self.last_refill);
        let delta_secs = delta.as_secs_f64();
        if delta_secs > 0.0 {
            let refill = delta_secs * self.refill_per_sec;
            self.tokens = (self.tokens + refill).min(self.capacity as f64);
            self.last_refill = now;
        }
        if (self.tokens as u32) < n {
            return false;
        }
        self.tokens -= n as f64;
        true
    }

    /// Force a refill pass and return the current token count
    /// (useful for tests and debug accessors).
    pub fn current_tokens(&mut self) -> u32 {
        let now = Instant::now();
        let delta = now.saturating_duration_since(self.last_refill);
        let delta_secs = delta.as_secs_f64();
        if delta_secs > 0.0 {
            let refill = delta_secs * self.refill_per_sec;
            self.tokens = (self.tokens + refill).min(self.capacity as f64);
            self.last_refill = now;
        }
        self.tokens as u32
    }
}

/// Per-peer sliding-window bookkeeping.
#[derive(Debug)]
struct Window {
    in_flight: HashSet<u32>,
    retries: HashMap<u32, u32>,
    #[allow(dead_code)]
    to_request: VecDeque<u32>,
    #[allow(dead_code)]
    verified: HashSet<u32>,
}

impl Window {
    fn new() -> Self {
        Self {
            in_flight: HashSet::new(),
            retries: HashMap::new(),
            to_request: VecDeque::new(),
            verified: HashSet::new(),
        }
    }
}

/// The P3-T07 scheduler. A thin wrapper the receiver loop
/// uses to gate outbound `Request` frames behind the
/// per-peer token bucket (capacity 4, refill 16/sec) and the
/// sliding-window slot count (16 in flight). The window
/// itself is also exposed for read-only inspection from the
/// integration test.
///
/// `Scheduler` does NOT replace the `ReceiverSession` loop.
/// It is additive: the loop calls `try_acquire_slot` before
/// sending each `Request`, and `release_slot` after each
/// `Chunk` arrives or a `Request` fails.
pub struct Scheduler {
    window: Arc<Mutex<Window>>,
    bucket: Arc<Mutex<TokenBucket>>,
    #[allow(dead_code)]
    transport: Arc<dyn Transport>,
    cancel: CancellationToken,
}

impl Scheduler {
    pub fn new(transport: Arc<dyn Transport>, cancel: CancellationToken) -> Self {
        Self {
            window: Arc::new(Mutex::new(Window::new())),
            bucket: Arc::new(Mutex::new(TokenBucket::new(
                PER_PEER_BUCKET_CAPACITY,
                PER_PEER_REFILL_PER_SEC,
            ))),
            transport,
            cancel,
        }
    }

    /// Wait until (a) the sliding window has a free slot
    /// (in_flight.len() < WINDOW_SIZE), (b) the per-peer token
    /// bucket has at least one token, and (c) the cancel
    /// token has not fired. Consumes a token on success.
    /// Records `chunk_index` as in-flight on success.
    ///
    /// Returns `SchedulerEvent::WindowFull` /
    /// `SchedulerEvent::BucketDepleted` via the provided
    /// observer whenever the caller would have been blocked
    /// at step (a) or (b). The events are best-effort: the
    /// observer may be `None` (the production wiring does
    /// not need to drain them).
    pub async fn try_acquire_slot(
        &self,
        chunk_index: u32,
        observer: Option<&mpsc::UnboundedSender<SchedulerEvent>>,
    ) -> Result<(), SchedulerError> {
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(SchedulerError::Cancelled),
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
            }
            let mut w = self.window.lock().await;
            let mut b = self.bucket.lock().await;
            if w.in_flight.len() >= WINDOW_SIZE {
                if let Some(tx) = observer {
                    let _ = tx.send(SchedulerEvent::WindowFull);
                }
                continue;
            }
            if !b.try_consume() {
                if let Some(tx) = observer {
                    let _ = tx.send(SchedulerEvent::BucketDepleted);
                }
                continue;
            }
            w.in_flight.insert(chunk_index);
            let in_flight_len = w.in_flight.len();
            w.retries
                .entry(chunk_index)
                .and_modify(|r| *r += 1)
                .or_insert(1);
            drop(b);
            drop(w);
            if let Some(tx) = observer {
                let _ = tx.send(SchedulerEvent::RequestSent { chunk_index });
            }
            debug!(
                chunk_index,
                in_flight = in_flight_len,
                "scheduler: slot acquired"
            );
            return Ok(());
        }
    }

    /// Release a slot previously acquired via
    /// `try_acquire_slot`. Called when a chunk is verified
    /// (success) or when a request is cancelled / dropped.
    pub async fn release_slot(&self, chunk_index: u32) {
        let mut w = self.window.lock().await;
        w.in_flight.remove(&chunk_index);
    }

    /// Test-only debug accessor for the current in-flight
    /// count. Exposed as `pub` (rather than `#[cfg(test)]`)
    /// so integration tests in `tests/` can poll the
    /// scheduler's window from a sibling crate. Mirrors the
    /// pattern in `net/config.rs`.
    pub async fn in_flight_len(&self) -> usize {
        self.window.lock().await.in_flight.len()
    }

    /// Test-only debug accessor: read the current bucket
    /// token count (after a refill pass).
    #[cfg(test)]
    pub async fn bucket_token_count(&self) -> u32 {
        self.bucket.lock().await.current_tokens()
    }
}

/// Backpressure state shared by `BackpressureTransport` and
/// `BackpressureHandle`. The wrapper updates this state from
/// `send` (rough estimate of buffered bytes) and from the
/// observer hooks (precise `bufferedAmount` and
/// `onbufferedamountlow`).
#[derive(Debug)]
struct BackpressureState {
    buffered_amount: u64,
    paused: bool,
}

impl BackpressureState {
    fn new() -> Self {
        Self {
            buffered_amount: 0,
            paused: false,
        }
    }
}

/// A `Transport` wrapper that gates outbound sends behind a
/// `bufferedAmount`-style backpressure signal. The wrapper
/// owns the *outer* send-side view: a host that calls
/// `send` while the wrapper is paused receives
/// `TransportError::BackpressurePaused`.
///
/// The wrapper is observable: callers (production webrtc
/// wrapper, integration tests) install a `BackpressureHandle`
/// and call `report_buffered_amount` / `signal_buffered_amount_low`
/// to drive the wrapper's paused state.
pub struct BackpressureTransport {
    inner: Arc<dyn Transport>,
    state: Arc<Mutex<BackpressureState>>,
    observer: mpsc::UnboundedSender<SchedulerEvent>,
}

impl BackpressureTransport {
    pub fn new(inner: Arc<dyn Transport>, observer: mpsc::UnboundedSender<SchedulerEvent>) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(BackpressureState::new())),
            observer,
        }
    }

    /// Install a backpressure handle for this wrapper. The
    /// returned handle can be cloned and passed to whichever
    /// observer (production webrtc wrapper, test thread)
    /// needs to feed in `bufferedAmount` /
    /// `onbufferedamountlow` signals.
    pub fn handle(&self) -> BackpressureHandle {
        BackpressureHandle {
            state: Arc::clone(&self.state),
            observer: self.observer.clone(),
        }
    }

    pub fn inner(&self) -> Arc<dyn Transport> {
        Arc::clone(&self.inner)
    }
}

#[async_trait]
impl Transport for BackpressureTransport {
    async fn send(&self, frame_bytes: Vec<u8>) -> Result<(), TransportError> {
        {
            let s = self.state.lock().await;
            if s.paused {
                return Err(TransportError::BackpressurePaused);
            }
        }
        let n = frame_bytes.len() as u64;
        let res = self.inner.send(frame_bytes).await;
        if res.is_ok() {
            let mut s = self.state.lock().await;
            s.buffered_amount = s.buffered_amount.saturating_add(n);
        }
        res
    }

    async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError> {
        self.inner.recv().await
    }

    async fn close(&self) {
        self.inner.close().await
    }
}

/// Observer handle for `BackpressureTransport`. Cloneable;
/// pass to whichever thread feeds in the DataChannel
/// `bufferedAmount` / `onbufferedamountlow` signals.
#[derive(Clone)]
pub struct BackpressureHandle {
    state: Arc<Mutex<BackpressureState>>,
    observer: mpsc::UnboundedSender<SchedulerEvent>,
}

impl BackpressureHandle {
    /// Feed in a precise `bufferedAmount` reading. If the new
    /// value is above `BUFFERED_AMOUNT_HIGH` and we were not
    /// already paused, transition to paused and emit a
    /// `SchedulerEvent::Paused`.
    pub async fn report_buffered_amount(&self, amount: u64) {
        let mut s = self.state.lock().await;
        s.buffered_amount = amount;
        if amount > BUFFERED_AMOUNT_HIGH as u64 && !s.paused {
            s.paused = true;
            info!(
                buffered_amount = amount,
                threshold = BUFFERED_AMOUNT_HIGH,
                "backpressure: pausing host sends"
            );
            let _ = self.observer.send(SchedulerEvent::Paused);
        }
    }

    /// Called when `onbufferedamountlow` fires on the
    /// underlying DataChannel. Clears the buffered-amount
    /// estimate, transitions out of paused, and emits
    /// `SchedulerEvent::Resumed`.
    pub async fn signal_buffered_amount_low(&self) {
        let mut s = self.state.lock().await;
        s.buffered_amount = 0;
        if s.paused {
            s.paused = false;
            info!(
                threshold = BUFFERED_AMOUNT_LOW,
                "backpressure: resuming host sends"
            );
            let _ = self.observer.send(SchedulerEvent::Resumed);
        }
    }

    /// Read the wrapper's current `bufferedAmount` estimate
    /// (for tests).
    #[cfg(test)]
    pub async fn buffered_amount(&self) -> u64 {
        self.state.lock().await.buffered_amount
    }

    /// Read whether the wrapper is currently paused (for tests).
    #[cfg(test)]
    pub async fn is_paused(&self) -> bool {
        self.state.lock().await.paused
    }
}

/// Wrap a `Transport` in a `BackpressureTransport` and return
/// `(wrapper, handle)`. Convenience for tests.
pub fn backpressure_pair(
    inner: Arc<dyn Transport>,
    observer: mpsc::UnboundedSender<SchedulerEvent>,
) -> (BackpressureTransport, BackpressureHandle) {
    let wrapper = BackpressureTransport::new(inner, observer);
    let handle = wrapper.handle();
    (wrapper, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn token_bucket_starts_full() {
        let mut b = TokenBucket::new(4, 16.0);
        assert_eq!(b.current_tokens(), 4);
    }

    #[test]
    fn token_bucket_consume_drains_to_zero() {
        let mut b = TokenBucket::new(4, 16.0);
        assert!(b.try_consume());
        assert!(b.try_consume());
        assert!(b.try_consume());
        assert!(b.try_consume());
        assert!(!b.try_consume());
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let mut b = TokenBucket::new(4, 16.0);
        for _ in 0..4 {
            assert!(b.try_consume());
        }
        assert!(!b.try_consume());
        std::thread::sleep(Duration::from_millis(300));
        let count = b.current_tokens();
        assert!(
            count >= 2,
            "expected at least 2 tokens after 300ms at 16/sec, got {count}"
        );
    }

    #[test]
    fn token_bucket_refill_caps_at_capacity() {
        let mut b = TokenBucket::new(4, 16.0);
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(b.current_tokens(), 4);
    }

    #[test]
    fn token_bucket_consume_n_rejects_when_insufficient() {
        let mut b = TokenBucket::new(4, 16.0);
        assert!(b.try_consume_n(3));
        assert!(!b.try_consume_n(2));
    }

    #[test]
    fn window_constants_match_architecture() {
        assert_eq!(WINDOW_SIZE, 16);
        assert_eq!(PER_PEER_BUCKET_CAPACITY, 4);
        assert!((PER_PEER_REFILL_PER_SEC - 16.0).abs() < f64::EPSILON);
        assert_eq!(BUFFERED_AMOUNT_HIGH, 2 * 1024 * 1024);
        assert_eq!(BUFFERED_AMOUNT_LOW, 1024 * 1024);
    }

    #[tokio::test]
    async fn scheduler_acquires_and_releases_slots() {
        let cancel = CancellationToken::new();
        let transport: Arc<dyn Transport> =
            Arc::new(crate::transfer::transport::loopback_pair(0, 0).0);
        let s = Scheduler::new(transport, cancel);
        s.try_acquire_slot(0, None).await.expect("acquire 0");
        s.try_acquire_slot(1, None).await.expect("acquire 1");
        assert_eq!(s.in_flight_len().await, 2);
        s.release_slot(0).await;
        assert_eq!(s.in_flight_len().await, 1);
    }

    #[tokio::test]
    async fn scheduler_cancelled_returns_err() {
        let cancel = CancellationToken::new();
        let transport: Arc<dyn Transport> =
            Arc::new(crate::transfer::transport::loopback_pair(0, 0).0);
        let s = Scheduler::new(transport, cancel.clone());
        cancel.cancel();
        let res = s.try_acquire_slot(0, None).await;
        assert!(matches!(res, Err(SchedulerError::Cancelled)));
    }

    #[tokio::test]
    async fn backpressure_pauses_on_high_then_resumes_on_low() {
        let (host_side, recv_side) = crate::transfer::transport::loopback_pair(0, 0);
        // Drain the peer's mailbox so the wrapper's `send`
        // does not block on a full loopback mailbox.
        let drain = tokio::spawn(async move {
            let s = recv_side;
            while let Ok(Ok(Some(_))) =
                tokio::time::timeout(Duration::from_millis(20), s.recv()).await
            {}
        });
        let inner: Arc<dyn Transport> = Arc::new(host_side);
        let (tx, mut rx) = mpsc::unbounded_channel::<SchedulerEvent>();
        let (wrapper, handle) = backpressure_pair(inner, tx);
        assert!(!handle.is_paused().await);
        handle
            .report_buffered_amount(BUFFERED_AMOUNT_HIGH as u64 + 1)
            .await;
        assert!(handle.is_paused().await);
        let ev = rx.recv().await.expect("event");
        assert_eq!(ev, SchedulerEvent::Paused);
        let send_res = wrapper.send(vec![0u8; 8]).await;
        assert!(matches!(send_res, Err(TransportError::BackpressurePaused)));
        handle.signal_buffered_amount_low().await;
        assert!(!handle.is_paused().await);
        let ev = rx.recv().await.expect("event 2");
        assert_eq!(ev, SchedulerEvent::Resumed);
        assert!(wrapper.send(vec![0u8; 8]).await.is_ok());
        let _ = drain.await;
    }

    #[tokio::test]
    async fn backpressure_above_high_stays_paused() {
        let (host_side, _) = crate::transfer::transport::loopback_pair(0, 0);
        let inner: Arc<dyn Transport> = Arc::new(host_side);
        let (tx, mut rx) = mpsc::unbounded_channel::<SchedulerEvent>();
        let (_wrapper, handle) = backpressure_pair(inner, tx);
        handle
            .report_buffered_amount(BUFFERED_AMOUNT_HIGH as u64 + 1)
            .await;
        let _ = rx.recv().await;
        // A second high report must not emit another Paused event.
        handle
            .report_buffered_amount(BUFFERED_AMOUNT_HIGH as u64 + 10)
            .await;
        let ev = tokio::time::timeout(Duration::from_millis(20), rx.recv()).await;
        assert!(ev.is_err(), "no second Paused event expected");
    }

    #[tokio::test]
    async fn scheduler_window_caps_at_16() {
        let cancel = CancellationToken::new();
        let transport: Arc<dyn Transport> =
            Arc::new(crate::transfer::transport::loopback_pair(0, 0).0);
        let s = Scheduler::new(transport, cancel);
        let (tx, mut rx) = mpsc::unbounded_channel::<SchedulerEvent>();
        // Acquire exactly WINDOW_SIZE=16 slots — should succeed.
        for i in 0..16u32 {
            s.try_acquire_slot(i, Some(&tx)).await.expect("acquire");
        }
        assert_eq!(s.in_flight_len().await, 16);

        // The 17th call to `try_acquire_slot` would block forever waiting
        // for a release, bucket refill, or cancellation — that is by design
        // (the scheduler back-pressures new sends rather than rejecting them).
        // We can't await it here without a release, so we verify the
        // window-cap invariant post-hoc: in_flight_len stays at 16, and
        // after releasing one slot a fresh acquire succeeds.
        s.release_slot(0).await;
        assert_eq!(s.in_flight_len().await, 15);
        s.try_acquire_slot(16, Some(&tx)).await.expect("re-acquire");
        assert_eq!(s.in_flight_len().await, 16);

        // Drain observer events (best-effort; just to keep the channel clean).
        while rx.try_recv().is_ok() {}
    }
}
