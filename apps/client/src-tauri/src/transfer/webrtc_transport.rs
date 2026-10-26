//! P3-T13: WebRtcTransport -- bridges a webrtc 0.21 DataChannel to the
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
//! P3-T15 segmentation: under webrtc 0.20 `DataChannel::poll` silently
//! dropped incoming messages larger than 16 KiB, and 0.21 still
//! documents the cap, so it is kept as a conservative interop limit
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
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelState};

use crate::transfer::transport::{Transport, TransportError};
use crate::transfer::wire::MAX_FRAME_BYTES;

/// The calls the transfer layer makes on a `files` data channel.
///
/// webrtc 0.21 sealed `webrtc::data_channel::DataChannel`, so only the
/// crate's own channels can implement it and a test can no longer stand
/// in for one. Transfer code takes this narrower trait instead: a real
/// channel gets it through [`files_channel`], test doubles implement it
/// directly.
#[async_trait::async_trait]
pub trait FilesChannel: Send + Sync + 'static {
    /// The channel's current state (`DataChannel::ready_state`).
    async fn ready_state(&self) -> webrtc::error::Result<RTCDataChannelState>;
    /// Queue one binary message (`DataChannel::send`).
    async fn send(&self, data: BytesMut) -> webrtc::error::Result<()>;
    /// The next event, or `None` once the channel is finished
    /// (`DataChannel::poll`).
    async fn poll(&self) -> Option<DataChannelEvent>;
    /// Close the channel (`DataChannel::close`).
    async fn close(&self) -> webrtc::error::Result<()>;
    /// Identity of the underlying channel: the same for every handle on
    /// one channel, so transports on it share one receive pump. Defaults
    /// to this object's address, which is right for a test double that is
    /// itself the channel.
    fn channel_key(&self) -> usize {
        self as *const Self as *const () as usize
    }
}

/// A real webrtc data channel as a [`FilesChannel`].
struct RtcFilesChannel(Arc<dyn DataChannel>);

#[async_trait::async_trait]
impl FilesChannel for RtcFilesChannel {
    async fn ready_state(&self) -> webrtc::error::Result<RTCDataChannelState> {
        self.0.ready_state().await
    }
    async fn send(&self, data: BytesMut) -> webrtc::error::Result<()> {
        self.0.send(data).await
    }
    async fn poll(&self) -> Option<DataChannelEvent> {
        self.0.poll().await
    }
    async fn close(&self) -> webrtc::error::Result<()> {
        self.0.close().await
    }
    /// The webrtc channel's address, not the wrapper's: each call to
    /// [`files_channel`] makes a new wrapper around the same channel.
    fn channel_key(&self) -> usize {
        Arc::as_ptr(&self.0) as *const () as usize
    }
}

/// Wrap a webrtc data channel for the transfer layer.
pub fn files_channel(dc: Arc<dyn DataChannel>) -> Arc<dyn FilesChannel> {
    Arc::new(RtcFilesChannel(dc))
}

/// Maximum payload bytes per SCTP DataChannel message. webrtc 0.20's
/// `DataChannelEvent::OnMessage` silently dropped messages larger than
/// 16384 bytes, and 0.21 still documents that cap (the "detach API" is
/// required for larger sizes), so it stays as the interop limit.
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

/// Which half of the transfer protocol a transport consumes.
///
/// One data channel can carry both halves at once: the answerer of a
/// connection is often the one that DOWNLOADS (the host opened the channel and
/// serves), yet every peer also runs a sender for channels it did not open. If
/// both consumers polled the channel themselves they would steal each other's
/// segments (the download saw "continuation segment with no open frame" and
/// failed, some of the time). So there is ONE receive pump per channel; it
/// reassembles whole frames and routes each to the role that consumes its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Serves: receives `hello`, `request`, `ack`, `nak`, `cancel`.
    Sender,
    /// Downloads: receives `offer`, `chunk`.
    Requester,
}

/// The role a complete frame (length prefix + JSON) belongs to, read from its
/// leading `"kind"` tag without parsing the body (a chunk is ~350 KiB).
/// `None` for `error` and anything unrecognised: both roles get those.
fn role_of_frame(frame: &[u8]) -> Option<Role> {
    let json = frame.get(4..)?;
    let rest = json.strip_prefix(b"{\"kind\":\"")?;
    let end = rest.iter().position(|b| *b == b'"')?;
    match &rest[..end] {
        b"hello" | b"request" | b"ack" | b"nak" | b"cancel" => Some(Role::Sender),
        b"offer" | b"chunk" => Some(Role::Requester),
        _ => None,
    }
}

/// One role's end of a channel's pump.
struct RoleEnd {
    /// The pump's way in; taken when the pump ends so readers see end of
    /// stream.
    tx: std::sync::Mutex<Option<mpsc::Sender<Inbound>>>,
    /// The role's reader side. A transport holds a clone while it lives, so
    /// "someone is reading" is "more than the pump's own handle exists".
    rx: Arc<Mutex<mpsc::Receiver<Inbound>>>,
}

impl RoleEnd {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Inbound>(INBOUND_QUEUE_FRAMES);
        Self {
            tx: std::sync::Mutex::new(Some(tx)),
            rx: Arc::new(Mutex::new(rx)),
        }
    }

    fn sender(&self) -> Option<mpsc::Sender<Inbound>> {
        self.tx.lock().expect("role tx").clone()
    }

    /// The pump is done: readers get end of stream once they have drained.
    fn close(&self) {
        self.tx.lock().expect("role tx").take();
    }

    /// A reader for this role. Frames left from an earlier reader are
    /// discarded: a peer sends a role nothing before that role has asked.
    fn attach(&self) -> Arc<Mutex<mpsc::Receiver<Inbound>>> {
        if let Ok(mut rx) = self.rx.try_lock() {
            while rx.try_recv().is_ok() {}
        }
        self.rx.clone()
    }

    fn is_taken(&self) -> bool {
        Arc::strong_count(&self.rx) > 1
    }
}

/// The shared receive pump of one data channel.
struct Demux {
    sender: RoleEnd,
    requester: RoleEnd,
}

impl Demux {
    fn end(&self, role: Role) -> &RoleEnd {
        match role {
            Role::Sender => &self.sender,
            Role::Requester => &self.requester,
        }
    }
}

/// Live pumps by channel identity, so a second transport on the same channel
/// joins the existing pump instead of starting a competing one.
fn pumps() -> &'static std::sync::Mutex<std::collections::HashMap<usize, std::sync::Weak<Demux>>> {
    static PUMPS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<usize, std::sync::Weak<Demux>>>,
    > = std::sync::OnceLock::new();
    PUMPS.get_or_init(Default::default)
}

/// Adapter that exposes a [`FilesChannel`] as the
/// transfer layer's [`Transport`] trait. Bytes-in, bytes-out; no
/// framing layer lives here (the wire codec lives in
/// `transfer::wire::codec`).
pub struct WebRtcTransport {
    dc: Arc<dyn FilesChannel>,
    rx: Arc<Mutex<mpsc::Receiver<Inbound>>>,
    cancel: CancellationToken,
    /// The channel's shared pump handle.
    _demux: Arc<Demux>,
}

impl WebRtcTransport {
    /// A transport that DOWNLOADS over `dc` (see [`Role::Requester`]).
    pub fn new(dc: Arc<dyn FilesChannel>, cancel: CancellationToken) -> Self {
        Self::new_for(dc, cancel, Role::Requester)
    }

    /// A transport for `role` over `dc`. Joins the channel's shared receive
    /// pump, starting it if this is the first.
    pub fn new_for(dc: Arc<dyn FilesChannel>, cancel: CancellationToken, role: Role) -> Self {
        let key = dc.channel_key();
        let demux = {
            let mut map = pumps().lock().expect("pumps");
            map.retain(|_, w| w.strong_count() > 0);
            match map.get(&key).and_then(|w| w.upgrade()) {
                Some(d) => d,
                None => {
                    let d = Arc::new(Demux {
                        sender: RoleEnd::new(),
                        requester: RoleEnd::new(),
                    });
                    map.insert(key, Arc::downgrade(&d));
                    spawn_pump(dc.clone(), d.clone());
                    d
                }
            }
        };
        let rx = demux.end(role).attach();
        Self {
            dc,
            rx,
            cancel,
            _demux: demux,
        }
    }
}

/// Start the one receive pump of `dc`. It lives until the channel closes or a
/// protocol violation closes it; transports for either role join and leave
/// it meanwhile.
fn spawn_pump(dc: Arc<dyn FilesChannel>, d: Arc<Demux>) {
    tokio::spawn(async move {
        let mut reassembler = Reassembler::default();
        loop {
            let ev = match dc.poll().await {
                Some(e) => e,
                None => break,
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
                            if !route(&d, frame).await {
                                break;
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "files data channel segmentation violation; closing the channel");
                            for role in [Role::Sender, Role::Requester] {
                                let end = d.end(role);
                                if let (true, Some(tx)) = (end.is_taken(), end.sender()) {
                                    let err =
                                        TransportError::Io(format!("webrtc segmentation: {e}"));
                                    let _ = tx.send(Err(err)).await;
                                }
                            }
                            let _ = dc.close().await;
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
                // webrtc 0.21 made the enum `#[non_exhaustive]` without
                // adding a variant (0.21.1 has the same seven as 0.20).
                // A variant added later cannot carry inbound data we
                // understand, and the end of the channel still shows up
                // as `OnClose` or `poll()` returning `None`, so keep
                // pumping.
                other => {
                    tracing::debug!(event = ?other, "files data channel: unhandled event");
                }
            }
        }
        // The channel is finished for everyone reading it.
        d.sender.close();
        d.requester.close();
    });
}

/// Hand a complete frame to the role(s) that consume it. Waits while the
/// reader's queue is full (flow control). A role nobody reads is skipped, not
/// waited for. Returns false when no reader is left.
async fn route(d: &Demux, frame: Vec<u8>) -> bool {
    let targets: &[Role] = match role_of_frame(&frame) {
        Some(Role::Sender) => &[Role::Sender],
        Some(Role::Requester) => &[Role::Requester],
        None => &[Role::Sender, Role::Requester],
    };
    for role in targets {
        let end = d.end(*role);
        // A role with no reader (none attached yet, or it finished) is
        // skipped: a peer sends a role nothing before that role has asked.
        if let (true, Some(tx)) = (end.is_taken(), end.sender()) {
            let _ = tx.send(Ok(frame.clone())).await;
        }
    }
    true
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
        let next = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Ok(None),
            n = g.recv() => n,
        };
        match next {
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
    use std::time::Duration;

    fn push_all(r: &mut Reassembler, total: u16, parts: &[&[u8]]) -> Option<Vec<u8>> {
        let mut out = None;
        for (i, p) in parts.iter().enumerate() {
            out = r.push(total, i as u16, p).expect("valid segment");
        }
        out
    }

    #[test]
    fn a_frames_role_is_read_from_its_kind_tag() {
        use crate::transfer::wire::{codec, Frame};
        // Every real frame's tag is found, and routes to the right half.
        let id = "0190f4a0-0000-7000-8000-000000000001".to_string();
        let peer = "a".repeat(64);
        let frames = vec![
            (
                Frame::Hello(crate::transfer::wire::HelloFrame {
                    peer_id: peer.clone(),
                    download_id: id.clone(),
                    media_id: "m".into(),
                    manifest_version: 1,
                    have_chunks: vec![],
                }),
                Some(Role::Sender),
            ),
            (
                Frame::Cancel(crate::transfer::wire::CancelFrame {
                    download_id: id.clone(),
                    reason: "x".into(),
                }),
                Some(Role::Sender),
            ),
        ];
        for (f, want) in frames {
            let mut bytes = Vec::new();
            codec::encode(&f, &mut bytes).expect("encode");
            assert_eq!(role_of_frame(&bytes), want, "{:?}", f.kind());
        }
        // Not a tagged frame: both roles get it.
        assert_eq!(role_of_frame(b"\0\0\0\x02{}"), None);
        assert_eq!(role_of_frame(b"\0\0"), None);
        assert_eq!(
            role_of_frame(b"\0\0\0\x20{\"kind\":\"chunk\",\"download_id\":\"x\"}"),
            Some(Role::Requester)
        );
        assert_eq!(
            role_of_frame(b"\0\0\0\x20{\"kind\":\"offer\",\"download_id\":\"x\"}"),
            Some(Role::Requester)
        );
        assert_eq!(
            role_of_frame(b"\0\0\0\x20{\"kind\":\"error\",\"download_id\":\"x\"}"),
            None
        );
    }

    /// A channel fed by the test, standing in for a real DataChannel.
    struct MockDc {
        events: Mutex<mpsc::UnboundedReceiver<DataChannelEvent>>,
    }

    #[async_trait::async_trait]
    impl FilesChannel for MockDc {
        async fn ready_state(&self) -> webrtc::error::Result<RTCDataChannelState> {
            Ok(RTCDataChannelState::Open)
        }
        async fn send(&self, _data: BytesMut) -> webrtc::error::Result<()> {
            Ok(())
        }
        async fn poll(&self) -> Option<DataChannelEvent> {
            self.events.lock().await.recv().await
        }
        async fn close(&self) -> webrtc::error::Result<()> {
            Ok(())
        }
    }

    /// `frame` as the SCTP messages a peer would send for it.
    fn segments(frame: &[u8]) -> Vec<DataChannelEvent> {
        let total = frame.len().div_ceil(MAX_SCTP_SEGMENT_PAYLOAD) as u16;
        frame
            .chunks(MAX_SCTP_SEGMENT_PAYLOAD)
            .enumerate()
            .map(|(i, c)| {
                let mut b = BytesMut::new();
                b.extend_from_slice(&total.to_be_bytes());
                b.extend_from_slice(&(i as u16).to_be_bytes());
                b.extend_from_slice(c);
                DataChannelEvent::OnMessage(webrtc::data_channel::RTCDataChannelMessage {
                    is_string: false,
                    data: b,
                })
            })
            .collect()
    }

    fn tagged(kind: &str, pad: usize) -> Vec<u8> {
        let json = format!("{{\"kind\":\"{kind}\",\"pad\":\"{}\"}}", "x".repeat(pad));
        let mut f = (json.len() as u32).to_be_bytes().to_vec();
        f.extend_from_slice(json.as_bytes());
        f
    }

    /// The viewer both downloads (it reads the host's `offer`/`chunk`
    /// frames) and runs a sender on the same channel (which reads
    /// `hello`/`request`). They used to poll the channel separately, each
    /// taking some of every frame's segments: "continuation segment with no
    /// open frame", the channel closed, the download failed (about one run in
    /// three on two real desktop clients). Now each role gets whole frames of
    /// its own kinds.
    #[tokio::test]
    async fn a_downloader_and_a_sender_on_one_channel_do_not_steal_each_others_frames() {
        let (tx, rx) = mpsc::unbounded_channel();
        let dc: Arc<dyn FilesChannel> = Arc::new(MockDc {
            events: Mutex::new(rx),
        });
        let sender = WebRtcTransport::new_for(dc.clone(), CancellationToken::new(), Role::Sender);
        let requester =
            WebRtcTransport::new_for(dc.clone(), CancellationToken::new(), Role::Requester);

        // Multi-segment frames of both kinds, interleaved in arrival order.
        let offer = tagged("offer", 50_000); // 4 segments
        let hello = tagged("hello", 40_000); // 3 segments
        let chunk = tagged("chunk", 120_000); // 8 segments
        let request = tagged("request", 20_000);
        for f in [&offer, &hello, &chunk, &request] {
            for ev in segments(f) {
                tx.send(ev).unwrap();
            }
        }
        let t = Duration::from_secs(5);
        assert_eq!(
            tokio::time::timeout(t, requester.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            offer
        );
        assert_eq!(
            tokio::time::timeout(t, requester.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            chunk
        );
        assert_eq!(
            tokio::time::timeout(t, sender.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            hello
        );
        assert_eq!(
            tokio::time::timeout(t, sender.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            request
        );
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
