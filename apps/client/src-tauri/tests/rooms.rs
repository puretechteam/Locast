//! P2-T04 end-to-end room lifecycle tests using the real
//! server + the native SignalingClient + RoomClient.
//!
//! Pattern: spawn the real server on `127.0.0.1:0`, point
//! two SignalingClients at it, drive each through the
//! handshake, then exercise ROOM_CREATE / ROOM_JOIN /
//! ROOM_LEAVE / PRESENCE through the RoomClient APIs.

#![allow(clippy::needless_return)]
#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use locast_client_lib::identity::keystore::{IdentityKeyring, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::RoomClient;
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::net::state::ConnPhase;
use locast_client_lib::storage::Storage;
use locast_protocol::handshake::Platform;
use tokio::net::TcpListener;

async fn open_storage() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("index.sqlite");
    let _ = Storage::open(&path).await.expect("open storage");
    dir
}

fn test_config(url: String) -> SignalingConfig {
    SignalingConfig::new_for_test(
        url,
        Duration::from_millis(2_000),
        1024 * 1024,
        Platform::Linux,
    )
}

async fn make_client(url: String, keyring: Arc<dyn IdentityKeyring>) -> SignalingClient {
    let (_dir, storage) = {
        let dir = open_storage().await;
        let storage = Storage::open(&dir.path().join("index.sqlite"))
            .await
            .expect("storage");
        (dir, storage)
    };
    let identity = Arc::new(
        locast_client_lib::identity::keystore::IdentityService::with_keyring(keyring, storage),
    );
    identity.get_or_create("tester").await.expect("identity");
    SignalingClient::new(test_config(url), identity)
}

async fn wait_for_phase(client: &SignalingClient, target: ConnPhase, timeout: Duration) {
    let start = std::time::Instant::now();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_create_and_join_via_room_client() {
    use locast_server::{
        AppState, Clock, Config, Db, Metrics, RoomRegistry, RoomRegistryConfig, SystemClock,
    };

    // Spawn the real server.
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
    };
    let app: Router = locast_server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let _server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let url = format!("ws://{addr}/ws");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let keyring_a: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let keyring_b: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let signaling_a = Arc::new(make_client(url.clone(), keyring_a).await);
    let signaling_b = Arc::new(make_client(url.clone(), keyring_b).await);
    let room_a = Arc::new(RoomClient::new(signaling_a.clone()));
    let room_b = Arc::new(RoomClient::new(signaling_b.clone()));
    room_a.init().await;
    room_b.init().await;
    // Drain inbound envelopes for both clients.
    {
        let rc = room_a.clone();
        tokio::spawn(async move { rc.run_inbound().await });
    }
    {
        let rc = room_b.clone();
        tokio::spawn(async move { rc.run_inbound().await });
    }

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

    // A creates a room.
    let summary = room_a
        .room_create("Movie".into(), false)
        .await
        .expect("create");
    assert_eq!(summary.title, "Movie");
    assert!(!summary.host_migration_enabled);
    let code = summary.code.clone();

    // B joins.
    let b_summary = room_b
        .room_join(code.clone(), "B".into())
        .await
        .expect("join");
    assert_eq!(b_summary.participants.len(), 2);

    // A leaves -> ROOM_CLOSED + PARTICIPANT_LEFT.
    room_a.room_leave().await.expect("leave");

    signaling_a.shutdown().await;
    signaling_b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_on_handoff_via_room_client() {
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
    };
    let app: Router = locast_server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let _server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let url = format!("ws://{addr}/ws");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let keyring_a: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let keyring_b: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let signaling_a = Arc::new(make_client(url.clone(), keyring_a).await);
    let signaling_b = Arc::new(make_client(url.clone(), keyring_b).await);
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

    let summary = room_a.room_create("M".into(), true).await.expect("create");
    let code = summary.code.clone();
    let _b = room_b.room_join(code, "B".into()).await.expect("join");
    // A leaves intentionally -> immediate handoff to B.
    room_a.room_leave().await.expect("leave");
    // The server publishes HOST_MIGRATED + PARTICIPANT_LEFT
    // to B; the room is still alive for B. A short settle
    // window lets the broadcast reach B's inbound loop.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Verify B's signaling session is still authenticated.
    let b_snap = signaling_b.snapshot().await;
    assert!(
        b_snap.connected,
        "B's signaling connection should still be open after host migration"
    );
    // Verify B can still send a PRESENCE; this requires a
    // valid bearer, which is only held while the WS is up.
    room_b.presence().await.expect("presence");
    signaling_a.shutdown().await;
    signaling_b.shutdown().await;
}
