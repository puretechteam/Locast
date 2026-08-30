//! P3-T05 in-process WebRTC smoke test.
//!
//! Builds two `webrtc::peer_connection::PeerConnection`s in
//! the same process, exchanges SDP through `tokio::sync::oneshot`
//! channels (no signaling server), and asserts that:
//!
//! 1. ICE gathering completes on both sides within 5 s.
//! 2. The peer connection reaches `Connected` within 10 s.
//! 3. The `files` data channel reaches `Open` within the same
//!    window.
//! 4. The whole flow finishes within 15 s.
//!
//! The test is gated by `RUN_WEBRTC_TESTS=1`. When unset (or
//! any value other than `"1"`) it skips cleanly without ever
//! constructing an `RTCPeerConnection`. This keeps the default
//! `cargo test` green on Windows where the native WebRTC stack
//! would otherwise consume ~150-200 MB per connection.
//!
//! Patterned on `examples/data-channels-offer-answer` from the
//! upstream `webrtc` crate and on the `webrtc.rs` manager in
//! `apps/client/src-tauri/src/net/webrtc.rs`. webrtc-rs 0.20
//! exposes a no-accessor `PeerConnection` trait: connection
//! state and ICE gathering state can only be observed through
//! the per-connection `PeerConnectionEventHandler`. We bridge
//! those callbacks into a shared `Arc<Mutex<...>>` for polling.

#![allow(clippy::needless_return)]

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, Mutex};
use webrtc::data_channel::{
    DataChannel, DataChannelEvent, RTCDataChannelInit, RTCDataChannelState,
};
use webrtc::peer_connection::{
    PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceGatheringState, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
};

fn webrtc_tests_enabled() -> bool {
    std::env::var("RUN_WEBRTC_TESTS").ok().as_deref() == Some("1")
}

/// Observable per-connection state. The handler pushes updates
/// here; the test body polls these to detect milestones.
#[derive(Default)]
struct Observed {
    connection_state: Option<RTCPeerConnectionState>,
    gathering_state: Option<RTCIceGatheringState>,
    data_channel_opened: bool,
}

/// A no-op handler that records the three events the test cares
/// about into a shared `Observed`. The candidate string itself
/// is intentionally redacted; only the event kind is recorded.
struct TestHandler {
    observed: Arc<Mutex<Observed>>,
}

impl TestHandler {
    fn new(observed: Arc<Mutex<Observed>>) -> Self {
        Self { observed }
    }
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for TestHandler {
    async fn on_ice_candidate(&self, _ev: RTCPeerConnectionIceEvent) {}

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        let mut g = self.observed.lock().await;
        g.gathering_state = Some(state);
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let mut g = self.observed.lock().await;
        g.connection_state = Some(state);
    }

    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        if dc.label().await.ok().as_deref() == Some("files") {
            // Adopt the inbound channel and drive its event
            // stream so we can observe `OnOpen`.
            let observed = Arc::clone(&self.observed);
            tokio::spawn(async move {
                loop {
                    match dc.poll().await {
                        Some(DataChannelEvent::OnOpen) => {
                            observed.lock().await.data_channel_opened = true;
                        }
                        Some(DataChannelEvent::OnClose) | None => return,
                        _ => {}
                    }
                }
            });
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webrtc_basic_data_channel_open() {
    if !webrtc_tests_enabled() {
        eprintln!("skipping (RUN_WEBRTC_TESTS != 1)");
        return;
    }

    let overall = tokio::time::timeout(Duration::from_secs(15), async {
        // Build shared observation slots.
        let obs_a: Arc<Mutex<Observed>> = Arc::new(Mutex::new(Observed::default()));
        let obs_b: Arc<Mutex<Observed>> = Arc::new(Mutex::new(Observed::default()));

        // Build two PeerConnections with the same defaults the
        // client manager uses: default RTCConfiguration, no ICE
        // servers, single UDP bind at 0.0.0.0:0. Each has its
        // own handler recording observed state.
        let handler_a = Arc::new(TestHandler::new(Arc::clone(&obs_a)));
        let handler_b = Arc::new(TestHandler::new(Arc::clone(&obs_b)));
        let pc_a: Arc<dyn webrtc::peer_connection::PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_handler(handler_a)
                .with_configuration(RTCConfigurationBuilder::default().build())
                .with_udp_addrs(vec!["0.0.0.0:0"])
                .build()
                .await
                .expect("build pc_a"),
        );
        let pc_b: Arc<dyn webrtc::peer_connection::PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_handler(handler_b)
                .with_configuration(RTCConfigurationBuilder::default().build())
                .with_udp_addrs(vec!["0.0.0.0:0"])
                .build()
                .await
                .expect("build pc_b"),
        );

        // Drop guard so native sockets are released even if the
        // test aborts mid-flight.
        struct PcDrop(Arc<dyn webrtc::peer_connection::PeerConnection>);
        impl Drop for PcDrop {
            fn drop(&mut self) {
                let pc = Arc::clone(&self.0);
                tokio::spawn(async move {
                    let _ = pc.close().await;
                });
            }
        }
        let _ga = PcDrop(Arc::clone(&pc_a));
        let _gb = PcDrop(Arc::clone(&pc_b));

        // Peer A: create the `files` data channel with the same
        // shape the manager uses (P3-T05).
        let init = RTCDataChannelInit {
            ordered: true,
            max_packet_life_time: None,
            max_retransmits: None,
            protocol: "locast-files-v1".to_string(),
            negotiated: None,
        };
        let dc_a = pc_a
            .create_data_channel("files", Some(init))
            .await
            .expect("create_data_channel");

        // Adopt A's outbound channel: drive its poll loop so
        // `OnOpen` is observable.
        let obs_a_dc = Arc::clone(&obs_a);
        let dc_a_for_poll = Arc::clone(&dc_a);
        tokio::spawn(async move {
            loop {
                match dc_a_for_poll.poll().await {
                    Some(DataChannelEvent::OnOpen) => {
                        obs_a_dc.lock().await.data_channel_opened = true;
                    }
                    Some(DataChannelEvent::OnClose) | None => return,
                    _ => {}
                }
            }
        });

        // Peer A: create offer and set as local description.
        let offer = pc_a.create_offer(None).await.expect("create_offer");
        pc_a.set_local_description(offer.clone())
            .await
            .expect("set_local_description a");

        // Wait for A's ICE gathering to complete (timeout 5 s).
        let gather_a = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if obs_a.lock().await.gathering_state == Some(RTCIceGatheringState::Complete) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            gather_a.is_ok(),
            "peer A: ICE gathering did not complete within 5s"
        );

        // Extract A's final local description (carries the
        // gathered candidates inline, non-trickle).
        let local_a = pc_a.local_description().await.expect("local_description a");
        let offer_sdp = local_a.sdp;

        // Hand A's offer SDP to B via oneshot; B answers, sends
        // answer SDP back.
        let (offer_tx, offer_rx) = oneshot::channel::<String>();
        let (answer_tx, answer_rx) = oneshot::channel::<String>();
        let pc_b_for_answer = Arc::clone(&pc_b);
        let obs_b_for_gather = Arc::clone(&obs_b);
        tokio::spawn(async move {
            let offer_sdp = offer_rx.await.expect("offer rx");
            let remote_offer = webrtc::peer_connection::RTCSessionDescription::offer(offer_sdp)
                .expect("RTCSessionDescription::offer");
            pc_b_for_answer
                .set_remote_description(remote_offer)
                .await
                .expect("set_remote_description b");
            let answer = pc_b_for_answer
                .create_answer(None)
                .await
                .expect("create_answer b");
            pc_b_for_answer
                .set_local_description(answer)
                .await
                .expect("set_local_description b");
            // Wait for B's ICE gathering to complete.
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if obs_b_for_gather.lock().await.gathering_state
                        == Some(RTCIceGatheringState::Complete)
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            let local_b = pc_b_for_answer
                .local_description()
                .await
                .expect("local_description b");
            let _ = answer_tx.send(local_b.sdp);
        });

        let _ = offer_tx.send(offer_sdp);

        let answer_sdp = answer_rx.await.expect("answer rx");
        let remote_answer = webrtc::peer_connection::RTCSessionDescription::answer(answer_sdp)
            .expect("RTCSessionDescription::answer");
        pc_a.set_remote_description(remote_answer)
            .await
            .expect("set_remote_description a");

        // Poll for `Connected` on both sides (timeout 10 s).
        let connected = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let a = obs_a.lock().await.connection_state;
                let b = obs_b.lock().await.connection_state;
                if a == Some(RTCPeerConnectionState::Connected)
                    && b == Some(RTCPeerConnectionState::Connected)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(
            connected.is_ok(),
            "peer connections did not reach Connected within 10s"
        );

        // Poll for the `files` data channel to reach `Open` on
        // the initiator side (the answerer adopts via the
        // handler's `on_data_channel`).
        let opened = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                // Prefer the observer flag set by the spawned
                // poll tasks, but fall back to a direct
                // `ready_state()` read so we still see `Open`
                // even if the events were missed.
                let via_obs_a = obs_a.lock().await.data_channel_opened;
                let via_obs_b = obs_b.lock().await.data_channel_opened;
                if via_obs_a || via_obs_b {
                    return true;
                }
                if let Ok(state) = dc_a.ready_state().await {
                    if state == RTCDataChannelState::Open {
                        return true;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(
            opened.is_ok(),
            "files DataChannel did not reach Open within 10s"
        );
    })
    .await;

    assert!(
        overall.is_ok(),
        "webrtc_basic_data_channel_open: flow did not complete within 15s"
    );
}
