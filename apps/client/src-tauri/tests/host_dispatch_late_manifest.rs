//! A viewer's `files` data channel opens when the viewer connects to the room,
//! which is normally BEFORE the host has picked, hashed and published a film
//! (hashing three hours of video takes many seconds). The host's sender
//! dispatcher used to drop such a channel on the spot ("no verified manifest
//! for room; dropping") and never looked at it again, so that viewer's
//! download could only fail with "all sources exhausted". It now holds the
//! channel until the manifest arrives.
//!
//! The dispatcher is driven for real here (a real `RoomClient` and identity;
//! only the data channel is a stub), the same way `WebRtcManager` drives it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::BytesMut;
use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::RoomClient;
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::storage::Storage;
use locast_client_lib::transfer::webrtc_transport::FilesChannel;
use locast_client_lib::transfer::{HostDispatchContext, HostSenderDispatcher};
use locast_manifest::MediaManifest;
use locast_protocol::handshake::Platform;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;
use webrtc::data_channel::{DataChannelEvent, RTCDataChannelState};
use webrtc::error::Result as WebRtcResult;

/// A data channel that is open until told otherwise and never receives
/// anything (so a sender attached to it just waits for a Hello).
struct StubDataChannel {
    open: AtomicBool,
    events_rx: Mutex<mpsc::UnboundedReceiver<DataChannelEvent>>,
    _events_tx: mpsc::UnboundedSender<DataChannelEvent>,
}

impl StubDataChannel {
    fn new() -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            open: AtomicBool::new(true),
            events_rx: Mutex::new(rx),
            _events_tx: tx,
        })
    }
}

#[async_trait]
impl FilesChannel for StubDataChannel {
    async fn ready_state(&self) -> WebRtcResult<RTCDataChannelState> {
        Ok(if self.open.load(Ordering::SeqCst) {
            RTCDataChannelState::Open
        } else {
            RTCDataChannelState::Closed
        })
    }
    async fn send(&self, _data: BytesMut) -> WebRtcResult<()> {
        Ok(())
    }
    async fn poll(&self) -> Option<DataChannelEvent> {
        self.events_rx.lock().await.recv().await
    }
    async fn close(&self) -> WebRtcResult<()> {
        self.open.store(false, Ordering::SeqCst);
        Ok(())
    }
}

struct Host {
    _dir: tempfile::TempDir,
    identity: Arc<IdentityService>,
    room: Arc<RoomClient>,
    dispatcher: Arc<HostSenderDispatcher>,
    cancel: tokio_util::sync::CancellationToken,
    room_id: Uuid,
    pubkey: [u8; 32],
}

async fn host() -> Host {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("index.sqlite"))
        .await
        .expect("storage");
    let keyring: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let identity = Arc::new(IdentityService::with_keyring(keyring, storage.clone()));
    identity.get_or_create("host").await.expect("identity");
    let pubkey = identity
        .load_keypair()
        .await
        .expect("keypair")
        .signing
        .verifying_key()
        .to_bytes();
    // Never started: the dispatcher only reads the room client's cache.
    let signaling = Arc::new(SignalingClient::new(
        SignalingConfig::new_for_test(
            String::from("ws://127.0.0.1:1/ws"),
            Duration::from_secs(1),
            1024 * 1024,
            Platform::Linux,
        ),
        identity.clone(),
    ));
    let room = Arc::new(RoomClient::new(signaling));
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = HostDispatchContext::new(
        storage,
        dir.path().to_path_buf(),
        room.clone(),
        pubkey,
        cancel.clone(),
    );
    let dispatcher = HostSenderDispatcher::new(ctx);
    let room_id = Uuid::now_v7();
    dispatcher.context().set_room(room_id).await;
    Host {
        _dir: dir,
        identity,
        room,
        dispatcher,
        cancel,
        room_id,
        pubkey,
    }
}

impl Host {
    /// The host shares media: its signed manifest lands in the verified cache.
    async fn publish_manifest(&self) {
        let manifest = MediaManifest {
            manifest_version: 1,
            room_id: self.room_id.to_string(),
            media: vec![],
            subtitles: vec![],
            created_at: 1_700_000_000_000,
            host_signature: None,
        };
        let signed = self.identity.sign_manifest(&manifest).await.expect("sign");
        self.room.set_expected_host_pubkey(self.pubkey);
        self.room
            .accept_manifest(signed, 1, 1_700_000_000_000, "TEST")
            .await
            .expect("the host accepts its own signed manifest");
    }

    async fn wait_for_sender(&self, want: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.dispatcher.live_sender_count().await != want {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {want} live sender(s)"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_that_opens_before_the_host_shares_media_is_served_once_it_does() {
    let h = host().await;
    let dc = StubDataChannel::new();
    let peer = Uuid::now_v7();
    let d = h.dispatcher.clone();
    let mut spawn = tokio::spawn(async move { d.spawn_for_dc(dc, peer).await });

    // No manifest yet: the channel must be HELD, not dropped.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut spawn)
            .await
            .is_err(),
        "the dispatcher gave up on a channel that opened before the manifest existed"
    );
    assert_eq!(h.dispatcher.live_sender_count().await, 0);

    h.publish_manifest().await;

    tokio::time::timeout(Duration::from_secs(5), &mut spawn)
        .await
        .expect("the dispatcher keeps waiting after the manifest arrived")
        .expect("join");
    h.wait_for_sender(1).await;
    h.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_is_served_when_the_room_is_bound_after_it_opened() {
    // The channel can open before the dispatcher even knows the room.
    let h = host().await;
    h.dispatcher.context().clear_room().await;
    h.publish_manifest().await;
    let dc = StubDataChannel::new();
    let d = h.dispatcher.clone();
    let peer = Uuid::now_v7();
    let mut spawn = tokio::spawn(async move { d.spawn_for_dc(dc, peer).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut spawn)
            .await
            .is_err(),
        "no room is bound yet"
    );

    h.dispatcher.context().set_room(h.room_id).await;

    tokio::time::timeout(Duration::from_secs(5), &mut spawn)
        .await
        .expect("binding the room wakes the waiting channel")
        .expect("join");
    h.wait_for_sender(1).await;
    h.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leaving_the_room_ends_a_waiting_channel_without_a_sender() {
    let h = host().await;
    let dc = StubDataChannel::new();
    let d = h.dispatcher.clone();
    let peer = Uuid::now_v7();
    let spawn = tokio::spawn(async move { d.spawn_for_dc(dc, peer).await });
    // What leaving the room does through the master token.
    h.cancel.cancel();

    tokio::time::timeout(Duration::from_secs(5), spawn)
        .await
        .expect("a cancelled wait must end, not leak a task")
        .expect("join");
    assert_eq!(h.dispatcher.live_sender_count().await, 0);
}

/// A channel nobody is serving yet must not be held forever once it is gone.
/// The dispatcher notices on its periodic liveness check (every 5 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_channel_that_closes_is_released() {
    let h = host().await;
    let dc = StubDataChannel::new();
    let d = h.dispatcher.clone();
    let peer = Uuid::now_v7();
    let dc_for_close = dc.clone();
    let mut spawn = tokio::spawn(async move { d.spawn_for_dc(dc, peer).await });

    // It is open and waiting for the host to share media...
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut spawn)
            .await
            .is_err(),
        "the dispatcher should be holding the open channel"
    );
    // ...and then the viewer goes away.
    dc_for_close.close().await.expect("close");

    tokio::time::timeout(Duration::from_secs(20), spawn)
        .await
        .expect("a closed channel must stop waiting for a manifest")
        // (bounded well above the 5 s recheck)
        .expect("join");
    assert_eq!(h.dispatcher.live_sender_count().await, 0);
}
