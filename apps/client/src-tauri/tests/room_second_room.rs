//! A host who creates a second room in the same session sees who joins it.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::{PlaybackStateEvent, RoomClient, RoomEventSink, RoomSummaryIpc};
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::net::state::ConnPhase;
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

struct Recorder;

impl RoomEventSink for Recorder {
    fn emit_state(&self, _s: &RoomSummaryIpc) {}
    fn emit_event(&self, _s: &RoomSummaryIpc) {}
    fn emit_state_cleared(&self) {}
    fn emit_playback_state(&self, _ev: &PlaybackStateEvent) {}
}

#[allow(dead_code)]
struct Client {
    _dir: tempfile::TempDir,
    signaling: Arc<SignalingClient>,
    room: Arc<RoomClient>,
    recorder: Arc<Recorder>,
}

async fn client(addr: SocketAddr, label: &str) -> Client {
    if std::env::var("RUST_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("index.sqlite"))
        .await
        .expect("storage");
    let keyring: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let identity = Arc::new(IdentityService::with_keyring(keyring, storage));
    identity.get_or_create(label).await.expect("identity");
    let signaling = Arc::new(SignalingClient::new(
        SignalingConfig::new_for_test(
            format!("ws://{addr}/ws"),
            Duration::from_secs(5),
            1024 * 1024,
            Platform::Linux,
        ),
        identity,
    ));
    let room = Arc::new(RoomClient::new(signaling.clone()));
    let recorder = Arc::new(Recorder);
    room.install_event_sink(recorder.clone()).await;
    room.init().await;
    signaling.start().await.expect("start");
    let rc = room.clone();
    tokio::spawn(async move { rc.run_inbound().await });
    let deadline = Instant::now() + Duration::from_secs(15);
    while signaling.snapshot().await.phase != ConnPhase::Authenticated {
        assert!(Instant::now() < deadline, "{label}: not authenticated");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Client {
        _dir: dir,
        signaling,
        room,
        recorder,
    }
}

async fn until<F, Fut>(what: &str, secs: u64, f: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !f().await {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn roster(c: &Client) -> Vec<String> {
    c.room
        .state()
        .await
        .map(|s| {
            s.participants
                .iter()
                .map(|p| p.display_name.clone())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_who_makes_a_second_room_sees_who_joins_it() {
    let server = start_server().await;
    let host = client(server, "host").await;
    let viewer = client(server, "viewer").await;
    let first = host
        .room
        .room_create("one".into(), true)
        .await
        .expect("create 1");
    viewer
        .room
        .room_join(first.code.clone(), "Bob".into())
        .await
        .expect("join 1");
    until("host sees Bob in room one", 10, || async {
        roster(&host).await.len() >= 2
    })
    .await;
    viewer.room.room_leave().await.expect("leave 1");

    // The same host makes another room (leaving the first if it must).
    let second = match host.room.room_create("two".into(), true).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("second create refused ({e}); leaving first");
            host.room.room_leave().await.expect("leave");
            host.room
                .room_create("two".into(), true)
                .await
                .expect("create 2")
        }
    };
    assert_ne!(first.id, second.id);
    viewer
        .room
        .room_join(second.code.clone(), "Bob".into())
        .await
        .expect("join 2");
    until("host sees Bob in room two", 10, || async {
        roster(&host).await.len() >= 2
    })
    .await;
}
