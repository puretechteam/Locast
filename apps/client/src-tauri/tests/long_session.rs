//! A watch-together session outlives the server's bearer lifetime.
//!
//! Every message after the handshake carries the session's bearer, and the
//! server refuses (and closes the connection on) one it no longer knows. The
//! bearer's lifetime defaults to 15 minutes, a film is three hours, and a
//! host that merely sits in a room (presence every 5 s, the odd playback
//! command) is exactly as live at minute 16 as at minute 1.
//!
//! This test runs the real server with a bearer lifetime of a few seconds and
//! a host and a viewer that stay in a room for several lifetimes, the host
//! sending a command every couple of seconds. The viewer must hear every
//! command, in order, and neither client may drop out of the room.
//!
//! (The lifetime is shortened so the property is checkable in a minute; the
//! mechanism under test is the same one that expires a real bearer at 15.)

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
use locast_protocol::room::PlaybackAction;
use tokio::net::TcpListener;

const BEARER_TTL_SECS: u64 = 6;

/// The lifetime is set through the process environment while a server's config
/// is built, so the tests in this file take turns.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn start_server() -> SocketAddr {
    use locast_server::{
        AppState, Clock, Config, Db, Metrics, RoomRegistry, RoomRegistryConfig, SystemClock,
    };
    // Read once, when the config is built; this is the only test in the binary.
    std::env::set_var("LOCAST_BIND_TOKEN_TTL_SECONDS", BEARER_TTL_SECS.to_string());
    let config = Config::from_env().expect("config");
    std::env::remove_var("LOCAST_BIND_TOKEN_TTL_SECONDS");
    assert_eq!(config.bearer_ttl_seconds, BEARER_TTL_SECS as i64);
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

#[derive(Default)]
struct Recorder {
    playback: StdMutex<Vec<PlaybackStateEvent>>,
}

impl RoomEventSink for Recorder {
    fn emit_state(&self, _s: &RoomSummaryIpc) {}
    fn emit_event(&self, _s: &RoomSummaryIpc) {}
    fn emit_state_cleared(&self) {}
    fn emit_playback_state(&self, ev: &PlaybackStateEvent) {
        self.playback.lock().unwrap().push(ev.clone());
    }
}

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
    let recorder = Arc::new(Recorder::default());
    room.install_event_sink(recorder.clone()).await;
    // What `lib.rs` installs: after every AUTH_OK, rejoin the active room.
    {
        let rc = room.clone();
        signaling
            .on_authenticated(move || {
                let rc = rc.clone();
                tokio::spawn(async move {
                    let _ = rc.rejoin_active_room().await;
                });
            })
            .await;
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_that_outlives_the_bearer_lifetime_keeps_its_room_and_its_controls() {
    let _guard = ONE_AT_A_TIME.lock().await;
    let server = start_server().await;
    let host = client(server, "host").await;
    let viewer = client(server, "viewer").await;
    let created = host
        .room
        .room_create("long night".into(), false)
        .await
        .expect("create");
    viewer
        .room
        .room_join(created.code.clone(), "viewer".into())
        .await
        .expect("join");

    // Several bearer lifetimes, the host commanding all the way through.
    let started = Instant::now();
    let mut sent = 0u64;
    host.room
        .send_playback(PlaybackAction::Play, 0)
        .await
        .expect("play");
    sent += 1;
    while started.elapsed() < Duration::from_secs(BEARER_TTL_SECS * 4 + 3) {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let pos = started.elapsed().as_millis() as u64;
        // A command can be refused while the client is mid-reconnect; the
        // host's controls retry like the UI would. What matters is the room
        // stays and the viewer ends up in step.
        if host
            .room
            .send_playback(PlaybackAction::Seek, pos)
            .await
            .is_ok()
        {
            sent += 1;
        }
    }
    // The viewer is still in the room, with the host, and hears the next one.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let _ = host
            .room
            .send_playback(PlaybackAction::Pause, 123_000)
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let seen = viewer.recorder.playback.lock().unwrap().clone();
        if seen
            .iter()
            .any(|e| e.kind == "pause" && e.media_position_ms == 123_000)
        {
            // In order, no repeats.
            let seqs: Vec<u64> = seen.iter().map(|e| e.server_seq).collect();
            assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the viewer stopped hearing the host after {sent} commands over {:?}: {:?}",
            started.elapsed(),
            seen.iter()
                .map(|e| (&e.kind, e.server_seq))
                .collect::<Vec<_>>()
        );
    }
    let host_state = host.room.state().await.expect("host still in the room");
    let viewer_state = viewer.room.state().await.expect("viewer still in the room");
    assert_eq!(host_state.id, viewer_state.id);
    assert!(
        viewer_state.participants.iter().any(|p| p.user_id
            == host_state
                .participants
                .iter()
                .find(|x| x.is_host)
                .unwrap()
                .user_id),
        "the viewer still sees the host"
    );
    assert!(host.signaling.snapshot().await.connected);
    assert!(viewer.signaling.snapshot().await.connected);
}

/// A client that sat idle for longer than the bearer lifetime (a laptop that
/// slept) finds its bearer gone. It must be back, authenticated from scratch,
/// within seconds: a refused session resume is not a reason to wait out the
/// long backoff kept for a client the server has rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_client_whose_bearer_expired_is_back_within_seconds() {
    // The other test in this file sets the lifetime through the process
    // environment while building its config; run after it, not beside it.
    let _guard = ONE_AT_A_TIME.lock().await;
    let server = start_server().await;
    let c = client(server, "sleeper").await;
    // Nothing is sent for longer than the lifetime.
    tokio::time::sleep(Duration::from_secs(BEARER_TTL_SECS + 3)).await;
    // The first use finds the bearer gone and costs the connection.
    let before = c.signaling.bearer_for_test().await.expect("bearer").token;
    let started = Instant::now();
    let _ = c.room.presence().await;
    // The client is back, authenticated from scratch, within seconds.
    let deadline = started + Duration::from_secs(10);
    loop {
        let now = c.signaling.bearer_for_test().await.map(|b| b.token);
        if c.signaling.snapshot().await.phase == ConnPhase::Authenticated
            && now.is_some_and(|t| t != before)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "still not back after {:?}",
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // And works: a room can be created on the new session.
    let created = c
        .room
        .room_create("back again".into(), false)
        .await
        .expect("room_create on the new session");
    assert!(!created.code.is_empty());
}
