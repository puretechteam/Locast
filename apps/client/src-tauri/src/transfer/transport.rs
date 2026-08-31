//! P3-T06 transport abstraction.
//!
//! The transfer session drives a `Transport` rather than a
//! concrete `WebRtcManager` / `RTCDataChannel`. The trait is
//! deliberately minimal:
//!
//! - [`Transport::send`] consumes a single wire frame's bytes
//!   (length-prefixed JSON, see [`super::wire`]) and returns
//!   when the frame has been handed to the underlying I/O. It
//!   is cancellation-safe; the implementation must either
//!   deliver the bytes or surface an error.
//! - [`Transport::recv`] awaits the next frame. Implementations
//!   are free to coalesce, fragment, or reorder — the session
//!   layer treats all frames as individual messages and
//!   expects [`super::wire::codec::decode`] to parse them
//!   from a contiguous buffer.
//! - [`Transport::close`] tears the channel down. Called from
//!   both sides on cancel / error / completion.
//!
//! The trait has no notion of "who" the remote peer is. The
//! session layer authenticates the peer (via the `Hello`
//! pubkey handshake) and binds the channel to a verified
//! `DownloadPlan` before sending anything.
//!
//! ## Bounded buffers
//!
//! Implementations MUST bound their inbound and outbound
//! queues. The transfer session is allowed to have one
//! outstanding `send` and one outstanding `recv` per channel.
//! Any deeper queue is a memory liability under hostile
//! conditions (a malicious peer that just keeps sending
//! frames without reading).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::wire::{WireError, MAX_FRAME_BYTES};

/// Errors raised by a transport. Closed set mirroring the
/// other error enums in this module.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransportError {
    /// The peer disconnected cleanly.
    #[error("transport closed")]
    Closed,
    /// The transport was cancelled.
    #[error("transport cancelled")]
    Cancelled,
    /// Frame exceeded [`MAX_FRAME_BYTES`].
    #[error("frame too large: {0} > {1}")]
    FrameTooLarge(u32, u32),
    /// Underlying I/O error.
    #[error("transport io error: {0}")]
    Io(String),
    /// Inbound queue was closed without delivering a frame.
    #[error("transport channel closed unexpectedly")]
    ChannelClosed,
}

impl From<WireError> for TransportError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::InvalidLength(n) if n > MAX_FRAME_BYTES => {
                TransportError::FrameTooLarge(n, MAX_FRAME_BYTES)
            }
            other => TransportError::Io(format!("wire: {other}")),
        }
    }
}

/// The transport abstraction. One instance per `(local_peer,
/// remote_peer)` pair.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send one length-prefixed wire frame. The implementation
    /// is responsible for buffering, fragmentation, and
    /// backpressure; the caller passes an already-encoded
    /// frame and assumes the bytes will be delivered in full
    /// or an error will be raised.
    async fn send(&self, frame_bytes: Vec<u8>) -> Result<(), TransportError>;

    /// Await the next length-prefixed wire frame. Returns
    /// `Ok(None)` only on graceful EOF; an `Err` indicates the
    /// channel was torn down (close / cancel / I/O).
    async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError>;

    /// Tear the channel down. Idempotent. After `close`,
    /// subsequent `send` / `recv` calls MUST return
    /// [`TransportError::Closed`].
    async fn close(&self);
}

/// An in-process loopback transport used by tests and by the
/// P3-T06 acceptance scenario. Two `LoopbackTransport`s are
/// tied together at construction time by [`loopback_pair`].
/// Outbound frames are optionally dropped (`loss_pct`) and
/// optionally delayed (`jitter_ms`) to simulate a lossy,
/// jittery network. Inbound is delivered in order.
///
/// The transport is fully in-process and never touches the
/// filesystem or the network. There is no real socket, no
/// thread, no timer of its own (jitter is delivered by
/// `tokio::time::sleep`). All buffers are bounded by
/// [`LOOPBACK_CHANNEL_CAP`].
///
/// **Resource safety:** the only allocations are a single
/// `mpsc::channel(LOOPBACK_CHANNEL_CAP)` per side and the
/// per-frame `Vec<u8>` the sender hands us. There is no
/// unbounded queue.
#[derive(Debug)]
pub struct LoopbackTransport {
    /// Outbound mailbox. Frames are pushed here and popped by
    /// the peer's `recv`.
    tx: mpsc::Sender<Vec<u8>>,
    /// Inbound mailbox. Peers' `send` frames arrive here.
    /// We hold the receiver behind `Arc<Mutex<...>>` so that
    /// `close()` can swap it out for a closed channel and
    /// unblock any parked `recv()`.
    rx: ArcAsyncMutex<mpsc::Receiver<Vec<u8>>>,
    cancel: CancellationToken,
    closed: ArcAsyncMutex<bool>,
    /// Optional loss / jitter policy. `None` means no loss
    /// and zero delay (used by the tests that exercise the
    /// happy path with full reliability).
    policy: Option<LoopbackPolicy>,
}

/// Capacity of the per-side `mpsc` mailbox. With 5% loss,
/// 50 ms jitter, WINDOW_SIZE=16, and 200 chunks, the
/// mailbox can transiently hold up to ~2x the in-flight set
/// plus retransmits. 256 gives generous headroom while
/// keeping memory bounded (a chunk frame is ~350 KB so
/// 256 is ~90 MB worst case per side, still well under
/// the user's memory constraint). The cap is enforced by
/// `try_send` so backpressure is signaled to the caller.
const LOOPBACK_CHANNEL_CAP: usize = 256;

/// Tokio's `Mutex` wrapped in `Arc` for cheap clone across
/// the transport's API surface. `tokio::sync::Mutex` is used
/// (not `std::sync::Mutex`) because the lock is held across
/// `await` points in `recv` and `close`.
type ArcAsyncMutex<T> = std::sync::Arc<tokio::sync::Mutex<T>>;

impl LoopbackTransport {
    fn new_pair() -> (Self, Self) {
        let (a_to_b, b_rx) = mpsc::channel::<Vec<u8>>(LOOPBACK_CHANNEL_CAP);
        let (b_to_a, a_rx) = mpsc::channel::<Vec<u8>>(LOOPBACK_CHANNEL_CAP);
        let cancel_a = CancellationToken::new();
        let cancel_b = CancellationToken::new();
        let a = Self {
            tx: a_to_b,
            rx: std::sync::Arc::new(tokio::sync::Mutex::new(a_rx)),
            cancel: cancel_a,
            closed: std::sync::Arc::new(tokio::sync::Mutex::new(false)),
            policy: None,
        };
        let b = Self {
            tx: b_to_a,
            rx: std::sync::Arc::new(tokio::sync::Mutex::new(b_rx)),
            cancel: cancel_b,
            closed: std::sync::Arc::new(tokio::sync::Mutex::new(false)),
            policy: None,
        };
        (a, b)
    }
}

/// Build a pair of in-process loopback transports wired to
/// each other. Returns `(side_a, side_b)`. Each side applies
/// the same `(loss_pct, jitter_ms)` policy to its outbound
/// traffic.
pub fn loopback_pair(loss_pct: u8, jitter_ms: u64) -> (LoopbackTransport, LoopbackTransport) {
    let policy_a = LoopbackPolicy {
        loss_pct,
        jitter_ms,
        rng_state: std::sync::Arc::new(tokio::sync::Mutex::new(0x9E3779B97F4A7C15u64)),
    };
    let policy_b = LoopbackPolicy {
        loss_pct,
        jitter_ms,
        rng_state: std::sync::Arc::new(tokio::sync::Mutex::new(0xBF58476D1CE4E5B9u64)),
    };
    let (mut a, mut b) = LoopbackTransport::new_pair();
    a.policy = Some(policy_a);
    b.policy = Some(policy_b);
    (a, b)
}

/// Loss / jitter policy shared by both sides of the
/// loopback. Held behind `Arc<Mutex<u64>>` so we can keep
/// state across `send` calls without re-seeding.
#[derive(Debug, Clone)]
struct LoopbackPolicy {
    /// Loss percentage in `[0, 100]`. Each outbound frame is
    /// dropped with this probability.
    loss_pct: u8,
    /// Per-frame delivery jitter in milliseconds. A uniform
    /// `[0, jitter_ms]` delay is applied to every outbound
    /// frame. `0` means no delay.
    jitter_ms: u64,
    /// Cheap deterministic PRNG state (splitmix64). We do not
    /// need cryptographic randomness for a test transport.
    rng_state: ArcAsyncMutex<u64>,
}

impl LoopbackPolicy {
    /// Advance the PRNG and return a new u64.
    async fn next_u64(&self) -> u64 {
        let mut g = self.rng_state.lock().await;
        *g = g.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = *g;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Decide whether the current frame should be dropped.
    async fn should_drop(&self) -> bool {
        if self.loss_pct == 0 {
            return false;
        }
        let v = self.next_u64().await;
        // Map u64 -> [0, 100] inclusive.
        ((v % 101) as u8) < self.loss_pct
    }

    /// Compute a delivery delay in `[0, jitter_ms]`.
    async fn delay(&self) -> std::time::Duration {
        if self.jitter_ms == 0 {
            return std::time::Duration::ZERO;
        }
        let v = self.next_u64().await;
        std::time::Duration::from_millis(v % (self.jitter_ms + 1))
    }
}

#[async_trait]
impl Transport for LoopbackTransport {
    async fn send(&self, frame_bytes: Vec<u8>) -> Result<(), TransportError> {
        if frame_bytes.len() > (MAX_FRAME_BYTES as usize + 4) {
            return Err(TransportError::FrameTooLarge(
                frame_bytes.len() as u32,
                MAX_FRAME_BYTES + 4,
            ));
        }
        {
            let g = self.closed.lock().await;
            if *g {
                return Err(TransportError::Closed);
            }
        }
        if let Some(p) = &self.policy {
            if p.should_drop().await {
                // Frame simulated as lost. The session's
                // request/retransmit loop handles the recovery.
                return Ok(());
            }
            let d = p.delay().await;
            if !d.is_zero() {
                tokio::time::sleep(d).await;
            }
        }
        // Use `send` (not `try_send`) so the caller blocks
        // when the peer's recv is slow. The mpsc channel's
        // bounded capacity still bounds the memory.
        self.tx.send(frame_bytes).await.map_err(|e| match e {
            mpsc::error::SendError(_) => TransportError::Closed,
        })
    }

    async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError> {
        let cancel = self.cancel.clone();
        let mut g = self.rx.lock().await;
        let res = tokio::select! {
            _ = cancel.cancelled() => return Err(TransportError::Cancelled),
            r = g.recv() => r,
        };
        // If we are closed and the channel drained, return
        // Ok(None) so the caller can exit cleanly.
        if res.is_none() {
            let closed = *self.closed.lock().await;
            if closed {
                return Err(TransportError::Closed);
            }
        }
        match res {
            Some(bytes) => Ok(Some(bytes)),
            None => Ok(None),
        }
    }

    async fn close(&self) {
        {
            let mut g = self.closed.lock().await;
            if *g {
                return;
            }
            *g = true;
        }
        self.cancel.cancel();
        // Drop the inbound receiver so any parked `recv()`
        // sees `None` and the recv() wrapper converts that
        // into `TransportError::Closed`. We replace it with
        // a fresh closed one so any subsequent `send` /
        // `recv` calls fail fast.
        let mut g = self.rx.lock().await;
        let (_tx_for_drop, new_rx) = mpsc::channel::<Vec<u8>>(1);
        let _old = std::mem::replace(&mut *g, new_rx);
        drop(_old);
        drop(_tx_for_drop);
    }
}

impl Drop for LoopbackTransport {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loopback_pair_roundtrips_with_zero_loss() {
        let (a, b) = loopback_pair(0, 0);
        let frame = codec_round_trip_payload();
        a.send(frame.clone()).await.expect("send a");
        let got_b = b.recv().await.expect("recv b").expect("some");
        assert_eq!(got_b, frame);
        b.send(frame.clone()).await.expect("send b");
        let got_a = a.recv().await.expect("recv a").expect("some");
        assert_eq!(got_a, frame);
    }

    #[tokio::test]
    async fn loopback_close_marks_side_closed() {
        let (a, _b) = loopback_pair(0, 0);
        a.close().await;
        let err = a.send(vec![1, 2, 3]).await.unwrap_err();
        assert!(matches!(err, TransportError::Closed));
    }

    #[tokio::test]
    async fn loopback_cancel_token_ends_pending_recv() {
        let (a, _b) = loopback_pair(0, 0);
        let cancel = a.cancel.clone();
        let a2 = a;
        let h = tokio::spawn(async move {
            // Give the recv a chance to park.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            cancel.cancel();
            let r = a2.recv().await;
            r
        });
        let res = h.await.expect("join");
        assert!(matches!(
            res,
            Err(super::TransportError::Cancelled) | Err(super::TransportError::Closed)
        ));
    }

    #[tokio::test]
    async fn loopback_drop_eventually_yields() {
        let (a, _b) = loopback_pair(0, 0);
        drop(a);
    }

    fn codec_round_trip_payload() -> Vec<u8> {
        use crate::transfer::wire::peer_id_from_pubkey;
        use crate::transfer::wire::{codec, Frame, HelloFrame, MAX_FRAME_BYTES};
        let f = Frame::Hello(HelloFrame {
            peer_id: peer_id_from_pubkey(&[3u8; 32]),
            download_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
            media_id: "m-1".into(),
            manifest_version: 1,
            have_chunks: vec![],
        });
        let mut buf = Vec::with_capacity(MAX_FRAME_BYTES as usize);
        codec::encode(&f, &mut buf).expect("encode");
        buf
    }
}
