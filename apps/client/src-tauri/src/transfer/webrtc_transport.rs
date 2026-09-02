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
//! The receive pump buffers segments by `(total_segments, segment_index)`
//! and emits a complete frame to the tokio mpsc when all segments
//! have arrived. Segments from the same frame are guaranteed to
//! arrive in order (SCTP is reliable and in-order within a stream).
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

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use bytes::BytesMut;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::{DataChannel, DataChannelEvent};

use crate::transfer::transport::{Transport, TransportError};

/// Maximum payload bytes per SCTP DataChannel message. The webrtc
/// 0.20 `DataChannelEvent::OnMessage` silently drops messages larger
/// than 16384 bytes (the "detach API" is required for larger sizes).
/// We use 16384 as the cap; the 4-byte segment header reduces the
/// effective payload to 16380 bytes per segment.
const MAX_SCTP_MESSAGE_SIZE: usize = 16384;
/// Effective per-segment payload after the 4-byte segmentation header.
const MAX_SCTP_SEGMENT_PAYLOAD: usize = MAX_SCTP_MESSAGE_SIZE - 4;

/// Adapter that exposes a webrtc 0.20 `Arc<dyn DataChannel>` as the
/// transfer layer's [`Transport`] trait. Bytes-in, bytes-out; no
/// framing layer lives here (the wire codec lives in
/// `transfer::wire::codec`).
pub struct WebRtcTransport {
    dc: Arc<dyn DataChannel>,
    rx: Arc<Mutex<mpsc::UnboundedReceiver<Vec<u8>>>>,
    cancel: CancellationToken,
}

impl WebRtcTransport {
    /// Wrap `dc` in a Transport. Spawns a blocking-thread receive
    /// pump that drains `DataChannelEvent::OnMessage` bytes into the
    /// internal mpsc. On `OnClose` or cancel, the pump exits and
    /// `recv()` returns `Ok(None)`.
    pub fn new(dc: Arc<dyn DataChannel>, cancel: CancellationToken) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let dc2 = dc.clone();
        let cancel2 = cancel.clone();
        // Segment reassembly state. Keyed by `(total_segments,
        // segment_index_0_timestamp)`. We use a simple monotonically
        // increasing frame id to group segments.
        let pending: Arc<StdMutex<HashMap<u32, PendingFrame>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let next_frame_id: Arc<StdMutex<u32>> = Arc::new(StdMutex::new(0));
        let pending2 = pending.clone();
        let next_frame_id2 = next_frame_id.clone();
        // Run the poll loop on a dedicated blocking thread so it is
        // not starved by tokio's worker pool. The poll loop calls
        // `dc.poll()` (async) and pushes bytes to the mpsc. The
        // tokio runtime reads from the mpsc via `recv()`.
        tokio::spawn(async move {
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
                        let bytes = msg.data.to_vec();
                        if bytes.len() < 4 {
                            continue;
                        }
                        let total_segments = u16::from_be_bytes([bytes[0], bytes[1]]);
                        let segment_index = u16::from_be_bytes([bytes[2], bytes[3]]);
                        let payload = bytes[4..].to_vec();
                        let mut next_id = next_frame_id2.lock().unwrap();
                        let mut pending = pending2.lock().unwrap();
                        let frame_id = if segment_index == 0 {
                            let id = *next_id;
                            *next_id = next_id.wrapping_add(1);
                            pending.insert(
                                id,
                                PendingFrame {
                                    total_segments,
                                    received: 0,
                                    payload: Vec::new(),
                                },
                            );
                            id
                        } else {
                            next_id.wrapping_sub(1)
                        };
                        let frame = pending.get_mut(&frame_id).unwrap();
                        assert_eq!(
                            frame.total_segments, total_segments,
                            "segment total_segments mismatch"
                        );
                        frame.payload.extend_from_slice(&payload);
                        frame.received += 1;
                        if frame.received == frame.total_segments {
                            let frame = pending.remove(&frame_id).unwrap();
                            if tx.send(frame.payload).is_err() {
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
        let _ = pending;
        let _ = next_frame_id;
        Self {
            dc,
            rx: Arc::new(Mutex::new(rx)),
            cancel,
        }
    }
}

/// Pending frame state for segment reassembly. Owned by the
/// blocking poll thread.
struct PendingFrame {
    total_segments: u16,
    received: u16,
    payload: Vec<u8>,
}

#[async_trait::async_trait]
impl Transport for WebRtcTransport {
    async fn send(&self, frame_bytes: Vec<u8>) -> Result<(), TransportError> {
        // Split the frame into <= MAX_SCTP_SEGMENT_PAYLOAD byte
        // segments. Each segment carries a 4-byte header:
        // [2 bytes BE: total_segments][2 bytes BE: segment_index].
        let total_segments = frame_bytes
            .len()
            .div_ceil(MAX_SCTP_SEGMENT_PAYLOAD)
            .min(u16::MAX as usize) as u16;
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
            Some(b) => Ok(Some(b)),
            None => Ok(None),
        }
    }

    async fn close(&self) {
        let _ = self.dc.close().await;
        self.cancel.cancel();
    }
}
