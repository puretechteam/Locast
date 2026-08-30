//! P3-T05 end-to-end WebRTC signaling test.
//!
//! Spins up the real server in-process, points two
//! `SignalingClient`s + two `RoomClient`s + two
//! `WebRtcManager`s at it, drives A through ROOM_CREATE and B
//! through ROOM_JOIN, then waits for the manager-to-manager
//! SDP/ICE relay to settle.
//!
//! ## Pass condition
//!
//! With `RUN_WEBRTC_TESTS=1`, both managers must:
//! - detect the other peer (peer_count >= 1) within 5 s of join, and
//! - reach `is_connected == true` for at least one side
//!   within 15 s of join (network conditions on the CI
//!   runner may delay the answerer; the symmetric case is
//!   covered by the in-process `webrtc_basic.rs` test).
//!
//! Gated by `RUN_WEBRTC_TESTS=1`. Default `cargo test` skips.

#![allow(clippy::needless_return)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::RoomClient;
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::net::state::ConnPhase;
use locast_client_lib::net::webrtc::WebRtcManager;
use locast_client_lib::storage::Storage;
use locast_protocol::handshake::Platform;
use tokio::net::TcpListener;
use uuid::Uuid;

fn webrtc_tests_enabled() -> bool {
    std::env::var("RUN_WEBRTC_TESTS").ok().as_deref() == Some("1")
}

async fn open_storage_handle() -> Storage {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("index.sqlite");
    Storage::open(&path).await.expect("open storage")
}

fn test_config(url: String) -> SignalingConfig {
    SignalingConfig::new_for_test(
        url,
        Duration::from_millis(2_000),
        1024 * 1024,
        Platform::Linux,
    )
}

async fn make_client_and_identity(url: String) -> (Arc<SignalingClient>, Arc<IdentityService>) {
    let storage = open_storage_handle().await;
    let keyring: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let identity = Arc::new(IdentityService::with_keyring(keyring, storage));
    identity.get_or_create("tester").await.expect("identity");
    let signaling = Arc::new(SignalingClient::new(test_config(url), identity.clone()));
    (signaling, identity)
}

async fn wait_for_phase(client: &SignalingClient, target: ConnPhase, timeout: Duration) {
    let start = Instant::now();
    loop {
        let s = client.snapshot().await;
        if s.phase == target {
            return;
        }
        if start.elapsed() > timeout {
            panic!("timed out waiting for {target:?}; last = {s:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_peer_detected(manager: &WebRtcManager, timeout: Duration) -> Vec<Uuid> {
    let deadline = Instant::now() + timeout;
    loop {
        let ids = manager.peer_ids().await;
        if !ids.is_empty() {
            return ids;
        }
        if Instant::now() >= deadline {
            panic!("manager did not detect any peer within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_any_connected(
    manager_a: &WebRtcManager,
    manager_b: &WebRtcManager,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        for id in manager_a.peer_ids().await {
            if manager_a.is_connected(id).await {
                return true;
            }
        }
        for id in manager_b.peer_ids().await {
            if manager_b.is_connected(id).await {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webrtc_signaling_end_to_end() {
    if !webrtc_tests_enabled() {
        eprintln!("skipping (RUN_WEBRTC_TESTS != 1)");
        return;
    }
    use locast_server::{
        AppState, Clock, Config, Db, Metrics, RoomRegistry, RoomRegistryConfig, SystemClock,
    };

    let config = Config::from_env().expect("config");
    let db = Db::open(&config).await.expect("open db");
    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&config)));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let state = AppState {
        config: Arc::new(config),
        metrics: Metrics::new(),
        db,
        rooms: rooms.clone(),
        clock: clock.clone(),
        signal_relay: Default::default(),
    };
    let app: Router = locast_server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let _server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let url = format!("ws://{addr}/ws");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (signaling_a, identity_a) = make_client_and_identity(url.clone()).await;
    let (signaling_b, identity_b) = make_client_and_identity(url.clone()).await;
    signaling_a.start().await.expect("start a");
    signaling_b.start().await.expect("start b");
    wait_for_phase(
        &signaling_a,
        ConnPhase::Authenticated,
        Duration::from_secs(5),
    )
    .await;
    wait_for_phase(
        &signaling_b,
        ConnPhase::Authenticated,
        Duration::from_secs(5),
    )
    .await;

    let room_a = Arc::new(RoomClient::new(signaling_a.clone()));
    let room_b = Arc::new(RoomClient::new(signaling_b.clone()));
    room_a.init().await;
    room_b.init().await;
    {
        let rc = room_a.clone();
        tokio::spawn(async move { rc.run_inbound().await });
    }
    {
        let rc = room_b.clone();
        tokio::spawn(async move { rc.run_inbound().await });
    }

    let manager_a = Arc::new(WebRtcManager::new(
        signaling_a.clone(),
        identity_a.clone(),
        room_a.clone(),
    ));
    let manager_b = Arc::new(WebRtcManager::new(
        signaling_b.clone(),
        identity_b.clone(),
        room_b.clone(),
    ));
    let _ha = manager_a.clone().start_with_room_client(room_a.clone());
    let _hb = manager_b.clone().start_with_room_client(room_b.clone());

    let summary = room_a
        .room_create("Movie".into(), false)
        .await
        .expect("create");
    let code = summary.code.clone();

    let _b_summary = room_b.room_join(code, "B".into()).await.expect("join");

    let _ids_a = wait_for_peer_detected(&manager_a, Duration::from_secs(5)).await;
    let _ids_b = wait_for_peer_detected(&manager_b, Duration::from_secs(5)).await;

    let connected = wait_for_any_connected(&manager_a, &manager_b, Duration::from_secs(15)).await;
    assert!(
        connected,
        "no manager reached PeerPhase::Connected within 15s"
    );

    // H2 from reviewer: also assert that at least one side has
    // adopted / created the `files` DataChannel.
    let mut files_dc_visible = false;
    for id in manager_a.peer_ids().await {
        if manager_a.has_files_dc(id).await {
            files_dc_visible = true;
            break;
        }
    }
    if !files_dc_visible {
        for id in manager_b.peer_ids().await {
            if manager_b.has_files_dc(id).await {
                files_dc_visible = true;
                break;
            }
        }
    }
    assert!(
        files_dc_visible,
        "files DataChannel not visible on either manager after Connected"
    );

    let _ = room_a.room_leave().await;
    manager_a.on_room_left().await;
    manager_b.on_room_left().await;
    signaling_a.shutdown().await;
    signaling_b.shutdown().await;
}
