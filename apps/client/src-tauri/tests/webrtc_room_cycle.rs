//! Leaving a room must not stop the WebRTC manager: a second room in the same
//! process needs the inbound loop and a live host-dispatch parent token.
//!
//! `on_room_left` used to cancel the one token that the inbound loop and the
//! host dispatcher were parented to, so after the first leave the loop exited
//! and no later room got peer connections or SIGNAL handling.

use std::sync::Arc;
use std::time::Duration;

use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::RoomClient;
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::net::webrtc::WebRtcManager;
use locast_client_lib::storage::Storage;
use locast_protocol::handshake::Platform;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leaving_a_room_keeps_the_manager_alive_for_the_next_room() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("index.sqlite"))
        .await
        .expect("storage");
    let keyring: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let identity = Arc::new(IdentityService::with_keyring(keyring, storage));
    identity.get_or_create("tester").await.expect("identity");
    let cfg = SignalingConfig::new_for_test(
        "ws://127.0.0.1:1/ws".to_string(),
        Duration::from_millis(500),
        1024 * 1024,
        Platform::Linux,
    );
    let signaling = Arc::new(SignalingClient::new(cfg, identity.clone()));
    let room_client = Arc::new(RoomClient::new(signaling.clone()));
    let manager = Arc::new(WebRtcManager::new(signaling, identity, room_client.clone()));

    let mut inbound_loop = manager.clone().start_with_room_client(room_client);

    manager.on_room_left().await;

    assert!(
        !manager.cancel_token().is_cancelled(),
        "leaving a room must not cancel the manager-level token"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(400), &mut inbound_loop)
            .await
            .is_err(),
        "the inbound loop must keep running after a room is left"
    );
    // Leaving again is harmless.
    manager.on_room_left().await;
    assert!(!manager.cancel_token().is_cancelled());
    inbound_loop.abort();
}
