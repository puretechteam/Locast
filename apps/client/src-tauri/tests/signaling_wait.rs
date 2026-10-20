//! `room_connect_signaling` must not return before the connection can carry a
//! room command. The UI calls it and then immediately creates or joins a room;
//! when it returned as soon as the connection loop was spawned, the first
//! create after launch failed with "identity not initialized" (no session yet).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::RoomClient;
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::storage::Storage;
use locast_protocol::handshake::Platform;
use tokio::net::TcpListener;

async fn start_server() -> SocketAddr {
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
        rooms,
        clock,
        signal_relay: Default::default(),
        epoch_counter: Arc::new(StdMutex::new(locast_server::auth::EpochCounter::default())),
    };
    let app: axum::Router = locast_server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

async fn signaling_to(url: String) -> (tempfile::TempDir, Arc<SignalingClient>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("index.sqlite"))
        .await
        .expect("storage");
    let keyring: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let identity = Arc::new(IdentityService::with_keyring(keyring, storage));
    identity.get_or_create("tester").await.expect("identity");
    let signaling = Arc::new(SignalingClient::new(
        SignalingConfig::new_for_test(url, Duration::from_secs(5), 1024 * 1024, Platform::Linux),
        identity,
    ));
    (dir, signaling)
}

#[tokio::test]
async fn a_room_can_be_created_right_after_the_connect_returns() {
    let addr = start_server().await;
    let (_dir, signaling) = signaling_to(format!("ws://{addr}/ws")).await;
    let room = Arc::new(RoomClient::new(signaling.clone()));
    room.init().await;
    let rc = room.clone();
    tokio::spawn(async move { rc.run_inbound().await });

    // What `room_connect_signaling` does, and what the UI does next.
    signaling.start().await.expect("start");
    signaling
        .wait_authenticated(Duration::from_secs(10))
        .await
        .expect("authenticated");
    let created = room.room_create("movie night".into(), true).await;
    assert!(created.is_ok(), "create right after connect: {created:?}");
}

#[tokio::test]
async fn an_unreachable_server_is_reported_as_such_and_not_as_a_missing_identity() {
    // Nothing listens on port 1.
    let (_dir, signaling) = signaling_to("ws://127.0.0.1:1/ws".to_string()).await;
    signaling.start().await.expect("start");
    let err = signaling
        .wait_authenticated(Duration::from_millis(1500))
        .await
        .expect_err("no server to authenticate with");
    let text = err.to_string();
    assert!(
        text.contains("could not connect to the signaling server"),
        "{text}"
    );
    assert!(!text.contains("identity not initialized"), "{text}");
    signaling.shutdown().await;
}

#[tokio::test]
async fn sending_without_a_session_says_not_connected_not_identity() {
    let addr = start_server().await;
    let (_dir, signaling) = signaling_to(format!("ws://{addr}/ws")).await;
    let room = Arc::new(RoomClient::new(signaling.clone()));
    room.init().await;
    // Never started: there is no session to send on.
    let err = room
        .room_create("movie night".into(), true)
        .await
        .expect_err("no connection yet");
    let text = err.to_string();
    assert!(
        text.contains("not connected to the signaling server"),
        "{text}"
    );
    assert!(!text.contains("identity not initialized"), "{text}");
}
