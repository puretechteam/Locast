//! P3-T13: WebRtcTransport -- bridges a webrtc 0.20 DataChannel to the
//! `Transport` trait used by MultiSourceReceiver.
//!
//! Spawns a per-channel receive pump that pushes incoming bytes into
//! an mpsc channel; `recv()` awaits the next message. `send()` writes
//! bytes via the DataChannel's send API (which takes `BytesMut`).
//!
//! The transport is a thin adapter. All framing (length-prefix + JSON)
//! is done by `transfer::wire::codec`. The DataChannel itself is
//! already a binary-clean stream (no line discipline), so the adapter
//! shuttles raw bytes end-to-end.
//!
//! P3-T15 segmentation: the webrtc 0.20 `DataChannel::poll` silently
//! drops incoming messages larger than 16 KiB
//! (`webrtc::data_channel::DataChannelEvent::OnMessage` doc comment:
//! "OnMessage can currently receive messages up to 16384 bytes in
//! size. Check out the detach API if you want to use larger message
//! sizes."). The transfer wire codec's `Frame::Chunk` carries a
//! base64-encoded 256 KiB payload, which is ~350 KiB -- well over the
//! 16 KiB cap. To work around the cap without changing the manifest
//! format or the wire codec, this transport splits each outgoing
//! frame into <= [`MAX_SCTP_SEGMENT_PAYLOAD`] byte segments and
//! reassembles them on the receive side. The segmentation header is:
//!
//! ```text
//! [2 bytes BE: total_segments]
//! [2 bytes BE: segment_index]
//! [N bytes: payload (up to MAX_SCTP_SEGMENT_PAYLOAD)]
//! ```
//!
//! The receive side treats the header as untrusted peer input. A
//! [`Reassembler`] holds at most one open frame and accepts segments
//! only in order (`segment_index == received so far`), with a constant
//! `total_segments` of 1..=[`MAX_SEGMENTS_PER_FRAME`] and a reassembled
//! size of at most `MAX_FRAME_BYTES + 4`. A segment with index 0 starts
//! a new frame and abandons an unfinished one. Anything else (a
//! continuation with no open frame, a changed total, a gap or repeat, an
//! oversize frame) is a protocol violation: the pump closes the data
//! channel and `recv()` returns an error. Completed frames go through a
//! bounded queue, so a consumer that stops reading stalls the pump
//! (and, through SCTP flow control, the sender) instead of growing
//! memory.
//!
//! The poll loop runs on a dedicated blocking thread
//! (`std::thread::Builder`) so it is not subject to tokio scheduling
//! pressure from the many other tasks competing for the worker threads
//! (signaling, room inbound, webrtc peer event pumps, multi-source
//! orchestrator, etc.). The blocking thread uses
//! `futures::executor::block_on` to drive the async `dc.poll()` and a
//! `std::sync::mpsc` channel to forward bytes to the tokio runtime.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;

use bytes::BytesMut;
use thiserror::Error;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use webrtc::data_channel::{DataChannel, DataChannelEvent};

use crate::transfer::transport::{Transport, TransportError};
use crate::transfer::wire::MAX_FRAME_BYTES;

/// Maximum payload bytes per SCTP DataChannel message. The webrtc
/// 0.20 `DataChannelEvent::OnMessage` silently drops messages larger
/// than 16384 bytes (the "detach API" is required for larger sizes).
/// We use 16384 as the cap; the 4-byte segment header reduces the
/// effective payload to 16380 bytes per segment.
const MAX_SCTP_MESSAGE_SIZE: usize = 16384;
/// Effective per-segment payload after the 4-byte segmentation header.
const MAX_SCTP_SEGMENT_PAYLOAD: usize = MAX_SCTP_MESSAGE_SIZE - 4;
/// Largest frame (length prefix included) that is sent or reassembled.
const MAX_REASSEMBLED_BYTES: usize = MAX_FRAME_BYTES as usize + 4;
/// Most segments a frame of [`MAX_REASSEMBLED_BYTES`] can need.
const MAX_SEGMENTS_PER_FRAME: usize = MAX_REASSEMBLED_BYTES.div_ceil(MAX_SCTP_SEGMENT_PAYLOAD);
/// Completed frames buffered between the receive pump and `recv()`.
/// Frames are at most about 1 MiB, so this bounds the queue to a
/// few MiB; a reader that stops draining stalls the pump.
const INBOUND_QUEUE_FRAMES: usize = 16;

type Inbound = Result<Vec<u8>, TransportError>;

/// Adapter that exposes a webrtc 0.20 `Arc<dyn DataChannel>` as the
/// transfer layer's [`Transport`] trait. Bytes-in, bytes-out; no
/// framing layer lives here (the wire codec lives in
/// `transfer::wire::codec`).
pub struct WebRtcTransport {
    dc: Arc<dyn DataChannel>,
    rx: Arc<Mutex<mpsc::Receiver<Inbound>>>,
    cancel: CancellationToken,
}

impl WebRtcTransport {
    /// Wrap `dc` in a Transport. Spawns a blocking-thread receive
    /// pump that drains `DataChannelEvent::OnMessage` bytes into the
    /// internal mpsc. On `OnClose` or cancel, the pump exits and
    /// `recv()` returns `Ok(None)`.
    pub fn new(dc: Arc<dyn DataChannel>, cancel: CancellationToken) -> Self {
        let (tx, rx) = mpsc::channel::<Inbound>(INBOUND_QUEUE_FRAMES);
        let dc2 = dc.clone();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            let mut reassembler = Reassembler::default();
            loop {
                let ev = tokio::select! {
                    biased;
                    _ = cancel2.cancelled() => break,
                    ev = dc2.poll() => match ev {
                        Some(e) => e,
                        None => break,
                    }
                };
                match ev {
                    DataChannelEvent::OnMessage(msg) => {
                        let bytes = &msg.data[..];
                        if bytes.len() < 4 {
                            continue;
                        }
                        let total_segments = u16::from_be_bytes([bytes[0], bytes[1]]);
                        let segment_index = u16::from_be_bytes([bytes[2], bytes[3]]);
                        match reassembler.push(total_segments, segment_index, &bytes[4..]) {
                            Ok(None) => {}
                            Ok(Some(frame)) => {
                                if !deliver(&tx, &cancel2, Ok(frame)).await {
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "files data channel segmentation violation; closing the channel");
                                let err = TransportError::Io(format!("webrtc segmentation: {e}"));
                                deliver(&tx, &cancel2, Err(err)).await;
                                let _ = dc2.close().await;
                                break;
                            }
                        }
                    }
                    DataChannelEvent::OnClose => break,
                    DataChannelEvent::OnOpen
                    | DataChannelEvent::OnClosing
                    | DataChannelEvent::OnError
                    | DataChannelEvent::OnBufferedAmountLow
                    | DataChannelEvent::OnBufferedAmountHigh => {}
                }
            }
        });
        Self {
            dc,
            rx: Arc::new(Mutex::new(rx)),
            cancel,
        }
    }
}

/// Queue `item` for `recv()`, waiting while the queue is full. Returns
/// false when the reader is gone or the transport was cancelled.
async fn deliver(tx: &mpsc::Sender<Inbound>, cancel: &CancellationToken, item: Inbound) -> bool {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        r = tx.send(item) => r.is_ok(),
    }
}

/// Why an inbound segment was refused. Every variant is a peer protocol
/// violation; an honest sender never produces one.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
enum SegmentError {
    #[error("continuation segment with no open frame")]
    NoOpenFrame,
    #[error("total_segments {0} outside 1..={MAX_SEGMENTS_PER_FRAME}")]
    BadTotal(u16),
    #[error("total_segments changed mid-frame ({expected} then {got})")]
    TotalChanged { expected: u16, got: u16 },
    #[error("segment index {got} out of order (expected {expected})")]
    OutOfOrder { expected: u16, got: u16 },
    #[error("reassembled frame exceeds {MAX_REASSEMBLED_BYTES} bytes")]
    TooLarge,
}

/// The one frame currently being reassembled.
struct PendingFrame {
    total_segments: u16,
    received: u16,
    payload: Vec<u8>,
}

/// Validating segment reassembler. Holds at most one open frame, so its
/// memory is bounded by [`MAX_REASSEMBLED_BYTES`] whatever the peer sends.
#[derive(Default)]
struct Reassembler {
    open: Option<PendingFrame>,
}

impl Reassembler {
    /// Feed one segment. `Ok(Some(frame))` when it completes a frame. Any
    /// error also discards the open frame.
    fn push(
        &mut self,
        total_segments: u16,
        segment_index: u16,
        payload: &[u8],
    ) -> Result<Option<Vec<u8>>, SegmentError> {
        let result = self.push_inner(total_segments, segment_index, payload);
        if result.is_err() {
            self.open = None;
        }
        result
    }

    fn push_inner(
        &mut self,
        total_segments: u16,
        segment_index: u16,
        payload: &[u8],
    ) -> Result<Option<Vec<u8>>, SegmentError> {
        if total_segments == 0 || usize::from(total_segments) > MAX_SEGMENTS_PER_FRAME {
            return Err(SegmentError::BadTotal(total_segments));
        }
        if segment_index == 0 {
            // A new frame; an unfinished one (a sender that failed
            // mid-frame) is abandoned.
            self.open = Some(PendingFrame {
                total_segments,
                received: 0,
                payload: Vec::new(),
            });
        }
        let frame = self.open.as_mut().ok_or(SegmentError::NoOpenFrame)?;
        if frame.total_segments != total_segments {
            return Err(SegmentError::TotalChanged {
                expected: frame.total_segments,
                got: total_segments,
            });
        }
        if segment_index != frame.received {
            return Err(SegmentError::OutOfOrder {
                expected: frame.received,
                got: segment_index,
            });
        }
        if frame.payload.len() + payload.len() > MAX_REASSEMBLED_BYTES {
            return Err(SegmentError::TooLarge);
        }
        frame.payload.extend_from_slice(payload);
        frame.received += 1;
        if frame.received == frame.total_segments {
            return Ok(self.open.take().map(|f| f.payload));
        }
        Ok(None)
    }
}

#[async_trait::async_trait]
impl Transport for WebRtcTransport {
    async fn send(&self, frame_bytes: Vec<u8>) -> Result<(), TransportError> {
        if frame_bytes.len() > MAX_REASSEMBLED_BYTES {
            return Err(TransportError::FrameTooLarge(
                u32::try_from(frame_bytes.len()).unwrap_or(u32::MAX),
                MAX_FRAME_BYTES + 4,
            ));
        }
        // Split the frame into <= MAX_SCTP_SEGMENT_PAYLOAD byte
        // segments. Each segment carries a 4-byte header:
        // [2 bytes BE: total_segments][2 bytes BE: segment_index].
        // At most MAX_SEGMENTS_PER_FRAME (65), so this fits in a u16.
        let total_segments = frame_bytes.len().div_ceil(MAX_SCTP_SEGMENT_PAYLOAD) as u16;
        for (i, chunk) in frame_bytes.chunks(MAX_SCTP_SEGMENT_PAYLOAD).enumerate() {
            let seg_index = i as u16;
            let mut buf = BytesMut::with_capacity(4 + chunk.len());
            buf.extend_from_slice(&total_segments.to_be_bytes());
            buf.extend_from_slice(&seg_index.to_be_bytes());
            buf.extend_from_slice(chunk);
            self.dc
                .send(buf)
                .await
                .map_err(|e| TransportError::Io(format!("webrtc dc send: {e}")))?;
        }
        Ok(())
    }

    async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError> {
        let mut g = self.rx.lock().await;
        match g.recv().await {
            Some(Ok(b)) => Ok(Some(b)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    async fn close(&self) {
        let _ = self.dc.close().await;
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_all(r: &mut Reassembler, total: u16, parts: &[&[u8]]) -> Option<Vec<u8>> {
        let mut out = None;
        for (i, p) in parts.iter().enumerate() {
            out = r.push(total, i as u16, p).expect("valid segment");
        }
        out
    }

    #[test]
    fn segment_bound_matches_the_frame_cap() {
        // 1 MiB + 4 bytes at 16380 bytes per segment.
        assert_eq!(MAX_SEGMENTS_PER_FRAME, 65);
        assert!(MAX_SEGMENTS_PER_FRAME <= usize::from(u16::MAX));
    }

    #[test]
    fn reassembles_a_multi_segment_frame_and_a_following_one() {
        let mut r = Reassembler::default();
        assert_eq!(
            push_all(&mut r, 3, &[b"ab", b"cd", b"e"]).as_deref(),
            Some(&b"abcde"[..])
        );
        assert_eq!(push_all(&mut r, 1, &[b"xyz"]).as_deref(), Some(&b"xyz"[..]));
        assert!(r.open.is_none());
    }

    #[test]
    fn continuation_without_an_open_frame_is_an_error_not_a_panic() {
        let mut r = Reassembler::default();
        assert_eq!(r.push(2, 1, b"x"), Err(SegmentError::NoOpenFrame));
        // Also after a frame completed.
        assert!(r.push(1, 0, b"a").unwrap().is_some());
        assert_eq!(r.push(2, 1, b"x"), Err(SegmentError::NoOpenFrame));
    }

    #[test]
    fn total_segments_outside_the_valid_range_is_rejected() {
        let mut r = Reassembler::default();
        assert_eq!(r.push(0, 0, b"x"), Err(SegmentError::BadTotal(0)));
        let too_many = (MAX_SEGMENTS_PER_FRAME + 1) as u16;
        assert_eq!(
            r.push(too_many, 0, b"x"),
            Err(SegmentError::BadTotal(too_many))
        );
        assert_eq!(
            r.push(u16::MAX, 0, b"x"),
            Err(SegmentError::BadTotal(u16::MAX))
        );
        // The largest legal total is accepted.
        assert_eq!(r.push(MAX_SEGMENTS_PER_FRAME as u16, 0, b"x"), Ok(None));
    }

    #[test]
    fn changed_total_is_rejected_and_discards_the_frame() {
        let mut r = Reassembler::default();
        assert_eq!(r.push(3, 0, b"a"), Ok(None));
        assert_eq!(
            r.push(4, 1, b"b"),
            Err(SegmentError::TotalChanged {
                expected: 3,
                got: 4
            })
        );
        assert!(r.open.is_none());
    }

    #[test]
    fn repeated_or_skipped_index_is_rejected() {
        let mut r = Reassembler::default();
        assert_eq!(r.push(4, 0, b"a"), Ok(None));
        assert_eq!(r.push(4, 1, b"b"), Ok(None));
        // A repeat of index 1 must not append again.
        assert_eq!(
            r.push(4, 1, b"b"),
            Err(SegmentError::OutOfOrder {
                expected: 2,
                got: 1
            })
        );
        let mut r = Reassembler::default();
        assert_eq!(r.push(4, 0, b"a"), Ok(None));
        assert_eq!(
            r.push(4, 2, b"c"),
            Err(SegmentError::OutOfOrder {
                expected: 1,
                got: 2
            })
        );
    }

    #[test]
    fn index_zero_restarts_an_unfinished_frame_without_keeping_its_bytes() {
        let mut r = Reassembler::default();
        assert_eq!(r.push(3, 0, b"stale"), Ok(None));
        assert_eq!(r.push(3, 0, b"a"), Ok(None));
        assert_eq!(r.push(3, 1, b"b"), Ok(None));
        assert_eq!(r.push(3, 2, b"c"), Ok(Some(b"abc".to_vec())));
    }

    #[test]
    fn repeated_index_zero_never_accumulates_memory() {
        let mut r = Reassembler::default();
        let seg = vec![0u8; MAX_SCTP_SEGMENT_PAYLOAD];
        for _ in 0..10_000 {
            assert_eq!(r.push(2, 0, &seg), Ok(None));
        }
        let held = r.open.as_ref().map_or(0, |f| f.payload.len());
        assert_eq!(held, MAX_SCTP_SEGMENT_PAYLOAD);
    }

    #[test]
    fn oversize_reassembly_is_rejected_at_the_cap() {
        let seg = vec![0u8; MAX_SCTP_SEGMENT_PAYLOAD];
        // Exactly MAX_REASSEMBLED_BYTES is accepted.
        let mut r = Reassembler::default();
        let full = MAX_REASSEMBLED_BYTES / MAX_SCTP_SEGMENT_PAYLOAD;
        let tail = vec![0u8; MAX_REASSEMBLED_BYTES % MAX_SCTP_SEGMENT_PAYLOAD];
        let total = MAX_SEGMENTS_PER_FRAME as u16;
        for i in 0..full {
            assert_eq!(r.push(total, i as u16, &seg), Ok(None));
        }
        let frame = r
            .push(total, full as u16, &tail)
            .unwrap()
            .expect("complete");
        assert_eq!(frame.len(), MAX_REASSEMBLED_BYTES);
        // One byte more is rejected.
        let mut r = Reassembler::default();
        for i in 0..full {
            assert_eq!(r.push(total, i as u16, &seg), Ok(None));
        }
        let long_tail = vec![0u8; tail.len() + 1];
        assert_eq!(
            r.push(total, full as u16, &long_tail),
            Err(SegmentError::TooLarge)
        );
        assert!(r.open.is_none());
    }
}
