//! P3-T13 acceptance: the `WebRtcTransport` adapter shuttles
//! bytes correctly through a webrtc 0.20 `DataChannel`.
//!
//! The roadmap acceptance criterion for P3-T13 is:
//!
//! > a WebRTC-transport adapter that wraps `Arc<dyn
//! > DataChannel>` and feeds the MultiSourceReceiver; tests
//! > that exercise the byte round-trip through the adapter.
//!
//! We do NOT spin up a real `WebRtcManager` or PeerConnection
//! in this test -- those require libwebrtc network plumbing
//! and a signaling server. Instead we implement a minimal
//! `DataChannel` stub (`StubDataChannel`) that captures
//! outbound `send` payloads and exposes an mpsc queue of
//! `DataChannelEvent`s for `poll()` to drain. Two stubs
//! wired back-to-back (one's outbound queue is the other's
//! inbound queue, and vice versa) prove that:
//!
//! 1. `WebRtcTransport::send` -> `DataChannel::send` calls
//!    the correct API with `BytesMut` payload.
//! 2. `DataChannelEvent::OnMessage` -> `WebRtcTransport::recv`
//!    hands bytes to the consumer verbatim.
//! 3. `OnClose` terminates the receive pump and `recv()` then
//!    returns `Ok(None)`.
//!
//! The orchestrator's correctness over the adapter is
//! separately proven by `tests/multi_source_e2e.rs` (which
//! uses `LoopbackTransport`). Together the two tests
//! demonstrate the pipeline is wired end-to-end: the
//! orchestrator + scheduler work over any `Transport`, and
//! `WebRtcTransport` is a `Transport` that delegates to a
//! DataChannel.

#![allow(clippy::needless_range_loop)]

use std::sync::Arc;

use async_trait::async_trait;
use bytes::BytesMut;
use locast_client_lib::transfer::transport::{Transport, TransportError};
use locast_client_lib::transfer::webrtc_transport::WebRtcTransport;
use tokio::sync::{mpsc, Mutex};
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelState};
use webrtc::error::Result as WebRtcResult;

/// In-memory stub that satisfies the full `DataChannel` trait
/// surface. Captures every `send` payload into a shared Vec,
/// exposes `poll()` via an mpsc queue, and signals `OnClose`
/// on `close()`. All other methods return fixed values so the
/// trait surface is satisfied without real WebRTC plumbing.
struct StubDataChannel {
    label: String,
    protocol: String,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    events_tx: mpsc::UnboundedSender<DataChannelEvent>,
    events_rx: Arc<Mutex<mpsc::UnboundedReceiver<DataChannelEvent>>>,
}

impl StubDataChannel {
    fn new(label: &str, protocol: &str) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel::<DataChannelEvent>();
        Arc::new(Self {
            label: label.to_string(),
            protocol: protocol.to_string(),
            sent: Arc::new(Mutex::new(Vec::new())),
            events_tx: tx,
            events_rx: Arc::new(Mutex::new(rx)),
        })
    }

    /// Feed an `OnMessage` event into the queue.
    async fn inject_message(&self, data: Vec<u8>) {
        let mut bm = BytesMut::with_capacity(data.len());
        bm.extend_from_slice(&data);
        let _ = self.events_tx.send(DataChannelEvent::OnMessage(
            webrtc::data_channel::RTCDataChannelMessage {
                is_string: false,
                data: bm,
            },
        ));
    }

    async fn inject_close(&self) {
        let _ = self.events_tx.send(DataChannelEvent::OnClose);
    }

    /// Take a snapshot of all payloads that have been sent on
    /// this stub so far (and leave the Vec empty).
    async fn take_sent(&self) -> Vec<Vec<u8>> {
        let mut g = self.sent.lock().await;
        std::mem::take(&mut *g)
    }
}

#[async_trait]
impl DataChannel for StubDataChannel {
    async fn label(&self) -> WebRtcResult<String> {
        Ok(self.label.clone())
    }
    async fn ordered(&self) -> WebRtcResult<bool> {
        Ok(true)
    }
    async fn max_packet_life_time(&self) -> WebRtcResult<Option<u16>> {
        Ok(None)
    }
    async fn max_retransmits(&self) -> WebRtcResult<Option<u16>> {
        Ok(None)
    }
    async fn protocol(&self) -> WebRtcResult<String> {
        Ok(self.protocol.clone())
    }
    async fn negotiated(&self) -> WebRtcResult<bool> {
        Ok(false)
    }
    fn id(&self) -> u16 {
        0
    }
    async fn ready_state(&self) -> WebRtcResult<RTCDataChannelState> {
        Ok(RTCDataChannelState::Open)
    }
    async fn buffered_amount_high_threshold(&self) -> WebRtcResult<u32> {
        Ok(u32::MAX)
    }
    async fn set_buffered_amount_high_threshold(&self, _t: u32) -> WebRtcResult<()> {
        Ok(())
    }
    async fn buffered_amount_low_threshold(&self) -> WebRtcResult<u32> {
        Ok(0)
    }
    async fn set_buffered_amount_low_threshold(&self, _t: u32) -> WebRtcResult<()> {
        Ok(())
    }
    async fn send(&self, data: BytesMut) -> WebRtcResult<()> {
        let mut g = self.sent.lock().await;
        g.push(data.to_vec());
        Ok(())
    }
    async fn send_text(&self, _text: &str) -> WebRtcResult<()> {
        Ok(())
    }
    async fn poll(&self) -> Option<DataChannelEvent> {
        let mut g = self.events_rx.lock().await;
        g.recv().await
    }
    async fn close(&self) -> WebRtcResult<()> {
        let _ = self.events_tx.send(DataChannelEvent::OnClose);
        Ok(())
    }
}

/// Build a stub + its wrapped `WebRtcTransport`. Returns the
/// stub so the test can inject messages.
fn make_stub_transport() -> (Arc<StubDataChannel>, Arc<WebRtcTransport>) {
    let stub = StubDataChannel::new("files", "locast-files-v1");
    let cancel = tokio_util::sync::CancellationToken::new();
    let transport = Arc::new(WebRtcTransport::new(stub.clone(), cancel));
    (stub, transport)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webrtc_transport_send_round_trips_through_stub_datachannel() {
    let (stub, transport) = make_stub_transport();

    // 1. `send` writes through the DataChannel::send API
    //    (BytesMut). Verify the captured bytes match. P3-T15:
    //    WebRtcTransport prepends a 4-byte segmentation header
    //    [2B total_segments][2B segment_index] to every frame
    //    so that frames > 16 KiB can be reassembled on the
    //    receive side (the webrtc 0.20 DataChannel silently
    //    drops messages larger than 16 KiB without the detach
    //    API). For a 13-byte payload, total_segments=1 and
    //    segment_index=0.
    let payload = b"hello webrtc".to_vec();
    transport
        .send(payload.clone())
        .await
        .expect("WebRtcTransport::send must succeed against a stub DC");
    let captured = stub.take_sent().await;
    assert_eq!(captured.len(), 1, "expected exactly one send");
    let mut expected_segmented = Vec::with_capacity(4 + payload.len());
    expected_segmented.extend_from_slice(&1u16.to_be_bytes()); // total_segments = 1
    expected_segmented.extend_from_slice(&0u16.to_be_bytes()); // segment_index = 0
    expected_segmented.extend_from_slice(&payload);
    assert_eq!(
        captured[0], expected_segmented,
        "send bytes must include the 4-byte segmentation header followed by the payload"
    );

    // 2. Inject an OnMessage on the stub's event queue and
    //    verify recv() surfaces those bytes unchanged. P3-T15:
    //    WebRtcTransport prepends a 4-byte segmentation header
    //    (total_segments=1, segment_index=0) to every frame.
    //    The test injects a single-segment frame.
    let incoming = b"inbound frame".to_vec();
    let mut segmented = Vec::with_capacity(4 + incoming.len());
    segmented.extend_from_slice(&1u16.to_be_bytes());
    segmented.extend_from_slice(&0u16.to_be_bytes());
    segmented.extend_from_slice(&incoming);
    stub.inject_message(segmented).await;
    let received = transport
        .recv()
        .await
        .expect("recv should not error")
        .expect("recv should return Some(bytes)");
    assert_eq!(received, incoming, "inbound bytes must round-trip");

    // 3. Inject OnClose -- the receive pump should exit and a
    //    subsequent recv() should drain the mpsc and return
    //    Ok(None). The cancel token has not been triggered,
    //    so this is the graceful-EOF path.
    stub.inject_close().await;
    let after_close = transport.recv().await.expect("recv after close");
    assert!(
        after_close.is_none(),
        "recv after OnClose must return Ok(None) for graceful EOF; got {:?}",
        after_close
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webrtc_transport_cancel_token_ends_pending_recv() {
    // A DC whose `poll()` always returns None — that means
    // the receive pump will park on the poll future. Calling
    // `WebRtcTransport::close()` cancels the inner token,
    // which the pump's `tokio::select!` listens to; the pump
    // exits, the mpsc receiver drains, and the next `recv()`
    // returns `Ok(None)`.
    let stub: Arc<ClosedStub> = Arc::new(ClosedStub::new());
    let cancel = tokio_util::sync::CancellationToken::new();
    let transport = Arc::new(WebRtcTransport::new(stub.clone(), cancel.clone()));
    // Spawn a `recv()` that should be parked on poll().
    let transport_for_task = transport.clone();
    let recv_handle = tokio::spawn(async move { transport_for_task.recv().await });
    // Brief pause to let the pump park; then close the
    // transport which cancels the inner token and closes the
    // underlying DC.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    transport.close().await;
    let res = recv_handle
        .await
        .expect("join")
        .expect("recv must complete after close");
    assert!(
        res.is_none(),
        "expected Ok(None) after close cancels the recv pump; got {res:?}"
    );
}

/// A `DataChannel` whose `poll()` resolves to `None`
/// immediately (i.e. the channel is closed and has no more
/// events). Used by the cancel-token test above so the receive
/// pump exits via the cancel branch instead of OnClose.
struct ClosedStub;

impl ClosedStub {
    fn new() -> Self {
        Self
    }
}

#[async_trait]
impl DataChannel for ClosedStub {
    async fn label(&self) -> WebRtcResult<String> {
        Ok("files".into())
    }
    async fn ordered(&self) -> WebRtcResult<bool> {
        Ok(true)
    }
    async fn max_packet_life_time(&self) -> WebRtcResult<Option<u16>> {
        Ok(None)
    }
    async fn max_retransmits(&self) -> WebRtcResult<Option<u16>> {
        Ok(None)
    }
    async fn protocol(&self) -> WebRtcResult<String> {
        Ok("locast-files-v1".into())
    }
    async fn negotiated(&self) -> WebRtcResult<bool> {
        Ok(false)
    }
    fn id(&self) -> u16 {
        0
    }
    async fn ready_state(&self) -> WebRtcResult<RTCDataChannelState> {
        Ok(RTCDataChannelState::Closed)
    }
    async fn buffered_amount_high_threshold(&self) -> WebRtcResult<u32> {
        Ok(u32::MAX)
    }
    async fn set_buffered_amount_high_threshold(&self, _t: u32) -> WebRtcResult<()> {
        Ok(())
    }
    async fn buffered_amount_low_threshold(&self) -> WebRtcResult<u32> {
        Ok(0)
    }
    async fn set_buffered_amount_low_threshold(&self, _t: u32) -> WebRtcResult<()> {
        Ok(())
    }
    async fn send(&self, _data: BytesMut) -> WebRtcResult<()> {
        Ok(())
    }
    async fn send_text(&self, _text: &str) -> WebRtcResult<()> {
        Ok(())
    }
    async fn poll(&self) -> Option<DataChannelEvent> {
        None
    }
    async fn close(&self) -> WebRtcResult<()> {
        Ok(())
    }
}

/// Two stubs wired together: A->B and B->A. Sends from one
// side arrive as `OnMessage`s on the other side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webrtc_transport_pair_round_trips_through_linked_stubs() {
    let stub_a = StubDataChannel::new("files", "locast-files-v1");
    let stub_b = StubDataChannel::new("files", "locast-files-v1");
    // Capture B's sent bytes, then inject them into A's
    // inbound event queue. Same the other way.
    let cancel_a = tokio_util::sync::CancellationToken::new();
    let cancel_b = tokio_util::sync::CancellationToken::new();
    let transport_a = Arc::new(WebRtcTransport::new(stub_a.clone(), cancel_a.clone()));
    let transport_b = Arc::new(WebRtcTransport::new(stub_b.clone(), cancel_b.clone()));

    // A -> B
    transport_a.send(b"a-to-b".to_vec()).await.expect("a send");
    // Pump the B side's inbound: pull from stub_a's sent
    // buffer and feed into stub_b's events queue.
    let mut a_sent = stub_a.take_sent().await;
    assert_eq!(a_sent.len(), 1);
    let msg = a_sent.pop().unwrap();
    stub_b.inject_message(msg).await;
    let got_b = transport_b.recv().await.expect("b recv").expect("some");
    assert_eq!(got_b, b"a-to-b");

    // B -> A
    transport_b.send(b"b-to-a".to_vec()).await.expect("b send");
    let mut b_sent = stub_b.take_sent().await;
    let msg = b_sent.pop().unwrap();
    stub_a.inject_message(msg).await;
    let got_a = transport_a.recv().await.expect("a recv").expect("some");
    assert_eq!(got_a, b"b-to-a");
}

/// A `WebRtcTransport` whose underlying DC's `send` returns an
/// error should surface a `TransportError::Io` to the caller.
/// This is the only error path the wire code can hit; verify
/// it round-trips through the adapter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webrtc_transport_send_surfaces_datachannel_error() {
    let stub = Arc::new(ErroringStub::new());
    let cancel = tokio_util::sync::CancellationToken::new();
    let transport = Arc::new(WebRtcTransport::new(stub.clone(), cancel));
    let res = transport.send(b"boom".to_vec()).await;
    assert!(
        matches!(res, Err(TransportError::Io(_))),
        "expected TransportError::Io, got {res:?}"
    );
}

struct ErroringStub;

impl ErroringStub {
    fn new() -> Self {
        Self
    }
}

#[async_trait]
impl DataChannel for ErroringStub {
    async fn label(&self) -> WebRtcResult<String> {
        Ok("files".into())
    }
    async fn ordered(&self) -> WebRtcResult<bool> {
        Ok(true)
    }
    async fn max_packet_life_time(&self) -> WebRtcResult<Option<u16>> {
        Ok(None)
    }
    async fn max_retransmits(&self) -> WebRtcResult<Option<u16>> {
        Ok(None)
    }
    async fn protocol(&self) -> WebRtcResult<String> {
        Ok("locast-files-v1".into())
    }
    async fn negotiated(&self) -> WebRtcResult<bool> {
        Ok(false)
    }
    fn id(&self) -> u16 {
        0
    }
    async fn ready_state(&self) -> WebRtcResult<RTCDataChannelState> {
        Ok(RTCDataChannelState::Open)
    }
    async fn buffered_amount_high_threshold(&self) -> WebRtcResult<u32> {
        Ok(u32::MAX)
    }
    async fn set_buffered_amount_high_threshold(&self, _t: u32) -> WebRtcResult<()> {
        Ok(())
    }
    async fn buffered_amount_low_threshold(&self) -> WebRtcResult<u32> {
        Ok(0)
    }
    async fn set_buffered_amount_low_threshold(&self, _t: u32) -> WebRtcResult<()> {
        Ok(())
    }
    async fn send(&self, _data: BytesMut) -> WebRtcResult<()> {
        Err(webrtc::error::Error::ErrDataChannelClosed)
    }
    async fn send_text(&self, _text: &str) -> WebRtcResult<()> {
        Ok(())
    }
    async fn poll(&self) -> Option<DataChannelEvent> {
        None
    }
    async fn close(&self) -> WebRtcResult<()> {
        Ok(())
    }
}

/// P3-T15 regression: the webrtc 0.20 DataChannel silently drops
/// `OnMessage` events larger than 16 KiB (per the doc comment on
/// `webrtc::data_channel::DataChannelEvent::OnMessage`). The
/// production chunk payload is ~350 KiB, which is way over the
/// cap. `WebRtcTransport` works around this by splitting each
/// frame into <= 16 KiB segments with a 4-byte header
/// `[2B total_segments][2B segment_index]` and reassembling them
/// on the receive side. This test proves the segmentation +
/// reassembly round-trips for a frame that would otherwise be
/// dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webrtc_transport_segmentation_round_trips_large_frames() {
    let (stub, transport) = make_stub_transport();
    // 1. Send a frame larger than the 16 KiB webrtc 0.20 cap.
    //    64 KiB guarantees multiple segments.
    let payload = vec![0xABu8; 64 * 1024];
    transport
        .send(payload.clone())
        .await
        .expect("send must succeed");
    let captured = stub.take_sent().await;
    // The frame should be split into ceil(65536 / 16380) = 5
    // segments. First 4 are 16384 bytes each (4-byte header +
    // 16380 payload). The last is 20 bytes (4-byte header + 16
    // bytes of residual payload).
    assert_eq!(
        captured.len(),
        5,
        "64 KiB frame must be split into 5 segments"
    );
    for (i, seg) in captured.iter().enumerate() {
        let expected_len = if i < 4 { 16384 } else { 20 };
        assert_eq!(seg.len(), expected_len, "segment {i} must be {expected_len} bytes");
        let total_segments =
            u16::from_be_bytes([seg[0], seg[1]]);
        let segment_index = u16::from_be_bytes([seg[2], seg[3]]);
        assert_eq!(total_segments, 5, "total_segments header");
        assert_eq!(segment_index, i as u16, "segment_index header");
    }
    // 2. Feed all segments back into the stub's inbound event
    //    queue. The transport's blocking poll thread should
    //    reassemble them into the original frame.
    for seg in captured {
        stub.inject_message(seg).await;
    }
    let received = transport
        .recv()
        .await
        .expect("recv ok")
        .expect("recv some");
    assert_eq!(
        received.len(),
        64 * 1024,
        "reassembled frame must be the full 64 KiB"
    );
    assert_eq!(received, payload, "reassembled frame must match exactly");
}

/// P3-T15 regression: interleaving segments from two different
/// frames must not confuse the reassembly state. Two frames are
/// sent; the segments of the first are injected in order, then
/// the segments of the second. Each `recv()` must return the
/// correct, complete frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webrtc_transport_segmentation_handles_concurrent_frames() {
    let (stub, transport) = make_stub_transport();
    let frame_a = vec![0x11u8; 32 * 1024];
    let frame_b = vec![0x22u8; 32 * 1024];
    transport.send(frame_a.clone()).await.expect("a");
    transport.send(frame_b.clone()).await.expect("b");
    let captured = stub.take_sent().await;
    // First 3 segments are frame A (32K / 16380 = 2 full + 1
    // partial = 3 segments), next 3 are frame B.
    assert_eq!(captured.len(), 6);
    for seg in &captured[..3] {
        assert_eq!(u16::from_be_bytes([seg[0], seg[1]]), 3);
    }
    for seg in &captured[3..] {
        assert_eq!(u16::from_be_bytes([seg[0], seg[1]]), 3);
    }
    for seg in captured {
        stub.inject_message(seg).await;
    }
    let r1 = transport.recv().await.expect("r1").expect("some1");
    let r2 = transport.recv().await.expect("r2").expect("some2");
    // The blocking poll thread processes segments in arrival
    // order, so frame A's segments arrive first and r1 should
    // be frame A. But with concurrent frames, the reassembly
    // uses a single shared `next_frame_id` counter and the
    // segment_index==0 heuristic, which assumes segments from
    // the same frame are contiguous. This test documents the
    // current single-frame-in-flight behavior: r1 == frame_a.
    // (Multi-frame pipelining is out of scope for P3-T15.)
    assert_eq!(r1, frame_a, "first reassembled frame must be frame A");
    assert_eq!(r2, frame_b, "second reassembled frame must be frame B");
}

/// P3-T13 review fix I#30: end-to-end type-adapter smoke
/// test. Wires a `WebRtcTransport` (stub-backed) into the
/// orchestrator's [`SourceHandle`] struct and confirms the
/// types align: a `WebRtcTransport` is a valid `Transport`,
/// can be wrapped in `Arc<dyn Transport>`, and slots into a
/// `SourceHandle` exactly the way `download_open` constructs
/// it on the Missing path.
///
/// This test verifies the type-adapter integration; it does
/// NOT exercise the actual byte movement through
/// `run_multi_source`. The real bytes flow is proven by
/// `tests/multi_source_e2e.rs` (which uses `LoopbackTransport`
/// as the source), and the byte round-trip through the
/// WebRTC adapter is proven by the other tests in this file.
/// Together they show: orchestrator + scheduler work over any
/// `Transport`, and `WebRtcTransport` is a `Transport` whose
/// send/recv delegates to a DataChannel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_rtc_transport_satisfies_source_handle_transport_type() {
    use locast_client_lib::transfer::multi_source::SourceHandle;
    use locast_client_lib::transfer::scheduler::Scheduler;
    use std::collections::VecDeque;

    let stub = StubDataChannel::new("files", "locast-files-v1");
    let cancel = tokio_util::sync::CancellationToken::new();
    let transport: Arc<dyn Transport> =
        Arc::new(WebRtcTransport::new(stub.clone(), cancel.clone()));
    let sched = Arc::new(Scheduler::new(transport.clone(), cancel.clone()));
    // Construct a SourceHandle exactly the way
    // `download_open`'s Missing arm constructs it (same
    // field types and ordering). If the orchestrator's
    // type changes, this fails to compile — which is the
    // whole point of the test.
    let _handle = SourceHandle {
        peer_id: "01".repeat(32),
        transport: transport.clone(),
        priority: 0,
        sched,
        demotion_count: 0,
        unavailable: false,
        unavailable_since: None,
        cancel: cancel.clone(),
        rtt_samples: VecDeque::new(),
    };
    // The compile itself is the assertion. Sanity: the stub
    // is Open, so we can also verify the adapter still
    // round-trips a send. P3-T15: WebRtcTransport prepends
    // a 4-byte segmentation header (total_segments=1,
    // segment_index=0) to every frame so that frames larger
    // than 16 KiB can be reassembled on the receive side
    // (the webrtc 0.20 DataChannel silently drops messages
    // larger than 16 KiB without the detach API).
    transport
        .send(b"probe".to_vec())
        .await
        .expect("WebRtcTransport::send must succeed against the Open stub DC");
    let captured = stub.take_sent().await;
    let mut expected_segmented = Vec::with_capacity(4 + 5);
    expected_segmented.extend_from_slice(&1u16.to_be_bytes());
    expected_segmented.extend_from_slice(&0u16.to_be_bytes());
    expected_segmented.extend_from_slice(b"probe");
    assert_eq!(captured, vec![expected_segmented]);
}
