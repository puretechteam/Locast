//! The host's first PLAY is lost in flight (the connection drops as it is
//! sent). The room never starts; the host's controls, which believe it did,
//! then send SEEK and PAUSE.
//!
//! The server accepts exactly `last_acked + 1` from a sender, and it checks the
//! room's state before the number: a SEEK or PAUSE in a room that has not
//! started is refused as `InvalidState` whatever its number. Before the fix
//! that refusal did not say where the sender's numbering stood, the client
//! could not find its way back, and every later SEEK and PAUSE was refused
//! until the host pressed PLAY again, while its own screen carried on and the
//! viewer saw nothing.
//!
//! Real pieces: the real in-process `locast-server` and two real
//! `SignalingClient` + `RoomClient` pairs. The loss is modelled with
//! `RoomClient::lose_playback_for_test`, which numbers a command exactly like
//! `send_playback` and never sends it.

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

async fn until<F: Fn() -> bool>(what: &str, secs: u64, f: F) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn seen(c: &Client) -> Vec<(String, u64, u64)> {
    c.recorder
        .playback
        .lock()
        .unwrap()
        .iter()
        .map(|e| (e.kind.clone(), e.media_position_ms, e.server_seq))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_first_play_does_not_strand_the_hosts_later_controls() {
    let server = start_server().await;
    let host = client(server, "host").await;
    let viewer = client(server, "viewer").await;
    let created = host
        .room
        .room_create("film night".into(), false)
        .await
        .expect("create");
    viewer
        .room
        .room_join(created.code.clone(), "viewer".into())
        .await
        .expect("join");

    // The host presses PLAY at 20 s; the packet is lost on the way.
    let lost = host
        .room
        .lose_playback_for_test(PlaybackAction::Play, 20_000);
    assert_eq!(lost, 1);
    // The host's screen is playing, so it seeks to 45 s and pauses there.
    host.room
        .send_playback(PlaybackAction::Seek, 45_000)
        .await
        .expect("seek is sent");
    host.room
        .send_playback(PlaybackAction::Pause, 45_000)
        .await
        .expect("pause is sent");

    // The room ends up where the host's screen is: it played from 20 s,
    // moved to 45 s and is paused. The viewer hears it in order.
    until("the viewer hears PLAY, SEEK, PAUSE in order", 15, || {
        seen(&viewer).len() >= 3
    })
    .await;
    assert_eq!(
        seen(&viewer),
        vec![
            ("play".to_string(), 20_000, 1),
            ("seek".to_string(), 45_000, 2),
            ("pause".to_string(), 45_000, 3),
        ]
    );

    // And the host's controls keep working afterwards.
    host.room
        .send_playback(PlaybackAction::Play, 45_000)
        .await
        .expect("play is sent");
    host.room
        .send_playback(PlaybackAction::Seek, 90_000)
        .await
        .expect("seek is sent");
    until("the viewer hears the later commands", 15, || {
        seen(&viewer).len() >= 5
    })
    .await;
    let all = seen(&viewer);
    assert_eq!(
        &all[3..],
        &[
            ("play".to_string(), 45_000, 4),
            ("seek".to_string(), 90_000, 5),
        ]
    );
    // Nothing was applied twice.
    assert_eq!(all.len(), 5, "{all:?}");
    assert!(host.signaling.snapshot().await.connected);
}

/// The ordinary case is untouched: with nothing lost, commands go out once,
/// in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_loss_commands_are_applied_once_in_order() {
    let server = start_server().await;
    let host = client(server, "host").await;
    let viewer = client(server, "viewer").await;
    let created = host
        .room
        .room_create("film night".into(), false)
        .await
        .expect("create");
    viewer
        .room
        .room_join(created.code.clone(), "viewer".into())
        .await
        .expect("join");
    for (a, p) in [
        (PlaybackAction::Play, 0u64),
        (PlaybackAction::Seek, 5_000),
        (PlaybackAction::Pause, 5_000),
    ] {
        host.room.send_playback(a, p).await.expect("send");
    }
    until("viewer hears three", 15, || seen(&viewer).len() >= 3).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        seen(&viewer),
        vec![
            ("play".to_string(), 0, 1),
            ("seek".to_string(), 5_000, 2),
            ("pause".to_string(), 5_000, 3),
        ]
    );
}
