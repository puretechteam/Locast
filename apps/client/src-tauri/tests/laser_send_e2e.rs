//! P5-T04: end-to-end test of the PRODUCTION laser pointer path.
//!
//! Participant A calls the body of the real `laser_send` Tauri
//! command (`send_laser`, which the registered wrapper calls with
//! the managed `RoomClient` and `SignalingClient`). The command
//! forwards LASER_MOVE / LASER_OFF through the real
//! `SignalingClient` WebSocket to a real in-process
//! `locast-server`, which checks `cap::LASER` and relays to every
//! other participant with the authenticated sender stamped on the
//! envelope. The receivers' real `RoomClient::run_inbound` turns
//! the relay into the `laser://*` events the React layer consumes;
//! a recording `RoomEventSink` captures them.
//!
//! Modeled on `tests/drawing_send_e2e.rs`. Every test sends far
//! fewer than the server's 60 laser messages per second per
//! connection, so the rate limiter never drops anything here.

#![allow(clippy::needless_return)]

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use locast_client_lib::commands::laser::{send_laser, LaserSendInput, LaserSendResult};
use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::{
    LaserMoveEvent, LaserOffEvent, RoomClient, RoomEventSink, RoomSummaryIpc,
};
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::net::state::ConnPhase;
use locast_client_lib::storage::Storage;
use locast_protocol::handshake::Platform;
use tokio::net::TcpListener;
use uuid::Uuid;

// ---------------------------------------------------------------
// Harness
// ---------------------------------------------------------------

#[derive(Debug, Clone)]
enum Seen {
    Move(LaserMoveEvent),
    Off(LaserOffEvent),
}

#[derive(Default)]
struct RecordingSink {
    events: StdMutex<Vec<Seen>>,
}

impl RecordingSink {
    fn snapshot(&self) -> Vec<Seen> {
        self.events.lock().expect("sink lock").clone()
    }
    fn count(&self) -> usize {
        self.events.lock().expect("sink lock").len()
    }
}

impl RoomEventSink for RecordingSink {
    fn emit_state(&self, _summary: &RoomSummaryIpc) {}
    fn emit_event(&self, _summary: &RoomSummaryIpc) {}
    fn emit_state_cleared(&self) {}
    fn emit_laser_move(&self, ev: &LaserMoveEvent) {
        self.events
            .lock()
            .expect("sink lock")
            .push(Seen::Move(ev.clone()));
    }
    fn emit_laser_off(&self, ev: &LaserOffEvent) {
        self.events
            .lock()
            .expect("sink lock")
            .push(Seen::Off(ev.clone()));
    }
}

struct Client {
    signaling: Arc<SignalingClient>,
    room: Arc<RoomClient>,
    sink: Arc<RecordingSink>,
    // Keeps the sqlite file alive for the test's duration.
    _dir: tempfile::TempDir,
}

async fn spawn_server() -> String {
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
    let app = locast_server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    format!("ws://{addr}/ws")
}

async fn connect_client(url: &str) -> Client {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("index.sqlite"))
        .await
        .expect("storage");
    let keyring: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let identity = Arc::new(IdentityService::with_keyring(keyring, storage));
    identity.get_or_create("tester").await.expect("identity");
    let cfg = SignalingConfig::new_for_test(
        url.to_string(),
        Duration::from_millis(2_000),
        1024 * 1024,
        Platform::Linux,
    );
    let signaling = Arc::new(SignalingClient::new(cfg, identity.clone()));
    let room = Arc::new(RoomClient::new(signaling.clone()));
    let sink = Arc::new(RecordingSink::default());
    room.install_event_sink(sink.clone()).await;
    signaling.start().await.expect("start");
    wait_authenticated(&signaling).await;
    room.init().await;
    {
        let rc = room.clone();
        tokio::spawn(async move { rc.run_inbound().await });
    }
    Client {
        signaling,
        room,
        sink,
        _dir: dir,
    }
}

async fn wait_authenticated(client: &SignalingClient) {
    let start = Instant::now();
    loop {
        if client.snapshot().await.phase == ConnPhase::Authenticated {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for auth"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_until<F: Fn() -> bool>(what: &str, timeout: Duration, cond: F) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Wait until `c` has learned its server-assigned user id.
async fn wait_user_id(c: &Client) -> Uuid {
    let start = Instant::now();
    loop {
        if let Some(id) = c.room.local_user_id().await {
            return id;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "client never learned its server-assigned user id"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Host A creates a room; each of `n` viewers joins it. Viewers
/// hold the default capabilities (CHAT only, so no LASER).
async fn room_with_viewers(url: &str, n: usize) -> (Client, Vec<Client>) {
    let a = connect_client(url).await;
    let summary = a
        .room
        .room_create("Laser".into(), false)
        .await
        .expect("create");
    let mut viewers = Vec::new();
    for i in 0..n {
        let v = connect_client(url).await;
        v.room
            .room_join(summary.code.clone(), format!("V{i}"))
            .await
            .expect("join");
        wait_user_id(&v).await;
        viewers.push(v);
    }
    wait_user_id(&a).await;
    (a, viewers)
}

async fn room_id_of(c: &Client) -> Uuid {
    c.room
        .state()
        .await
        .expect("in a room")
        .id
        .parse()
        .expect("room id")
}

async fn send(
    c: &Client,
    input: LaserSendInput,
) -> Result<LaserSendResult, locast_client_lib::commands::error::AppError> {
    send_laser(input, &c.room, &c.signaling).await
}

fn mv(x: f32, y: f32) -> LaserSendInput {
    LaserSendInput::Move { x, y }
}

/// The host grants `bits` (on top of what the target holds) and
/// waits until the target's own room client has applied the update.
async fn grant(host: &Client, target: &Client, bits: u32) {
    let room_id = room_id_of(host).await;
    let target_id = target.room.local_user_id().await.expect("target id");
    host.room
        .permission_set(room_id, target_id, bits, 0)
        .await
        .expect("permission_set");
    let start = Instant::now();
    loop {
        let caps = target.room.state().await.and_then(|s| s.you_cap_set);
        if caps.is_some_and(|c| c & bits == bits) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "target never saw the grant {bits:#x}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------
// Real client -> real server -> real client
// ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_laser_moves_and_off_reach_the_other_participant_attributed_to_the_host() {
    let url = spawn_server().await;
    let (a, viewers) = room_with_viewers(&url, 1).await;
    let b = &viewers[0];
    let a_id = a.room.local_user_id().await.expect("A user id");
    let room_id = room_id_of(&a).await;

    let positions = [(0.1_f32, 0.2_f32), (0.5, 0.5), (1.0, 0.0)];
    for (x, y) in positions {
        let res = send(&a, mv(x, y)).await.expect("move accepted");
        assert!(Uuid::parse_str(&res.envelope_id).is_ok());
    }
    send(&a, LaserSendInput::Off).await.expect("off accepted");

    wait_until(
        "B to receive three moves and an off",
        Duration::from_secs(5),
        || b.sink.count() == positions.len() + 1,
    )
    .await;

    let seen = b.sink.snapshot();
    for (i, (x, y)) in positions.iter().enumerate() {
        match &seen[i] {
            Seen::Move(ev) => {
                assert_eq!(ev.sender_id, a_id.to_string(), "attributed to A");
                assert_eq!(ev.room_id, room_id.to_string(), "A's room");
                assert_eq!((ev.x, ev.y), (*x, *y), "position {i} intact");
            }
            other => panic!("expected move {i}, got {other:?}"),
        }
    }
    match &seen[positions.len()] {
        Seen::Off(ev) => {
            assert_eq!(ev.sender_id, a_id.to_string());
            assert_eq!(ev.room_id, room_id.to_string());
        }
        other => panic!("expected off, got {other:?}"),
    }

    // The sender never receives its own laser.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(a.sink.count(), 0, "no echo to the sender");

    // Once signaling is down a laser is refused, not queued for a
    // stale burst on reconnect.
    a.signaling.shutdown().await;
    let err = send(&a, mv(0.5, 0.5))
        .await
        .expect_err("no laser while disconnected");
    let msg = err.to_string();
    assert!(msg.contains("not connected"), "{msg}");
    b.signaling.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewer_without_laser_is_silently_ignored_and_stays_in_the_room() {
    let url = spawn_server().await;
    let (a, viewers) = room_with_viewers(&url, 2).await;
    let b = &viewers[0];
    let c = &viewers[1];
    let b_id = b.room.local_user_id().await.expect("B user id");
    let caps = b.room.state().await.and_then(|s| s.you_cap_set);
    assert!(
        caps.is_some_and(|c| c & locast_protocol::room::cap::LASER == 0),
        "a default viewer does not hold LASER: {caps:?}"
    );

    // The command itself does not gate on caps: the envelopes go
    // out, and the server drops them without a ROOM_ERROR.
    send(b, mv(0.3, 0.3)).await.expect("move sent");
    send(b, LaserSendInput::Off).await.expect("off sent");

    // `send` returns once the frame is queued locally, and the server
    // handles B's connection and A's PERMISSION_SET independently, so
    // granting right away could let the server see the grant before
    // B's denied frames. The server handles one connection's frames in
    // order, so a round trip on B's own connection returns only after
    // it has processed (and refused) the two frames before it. That is
    // an ordering barrier, where a fixed sleep only made the race rare.
    b.room
        .clock_skew_probe()
        .await
        .expect("barrier round trip on B's connection");
    assert_eq!(a.sink.count(), 0, "A: B's denied laser was not relayed");
    assert_eq!(c.sink.count(), 0, "C: B's denied laser was not relayed");
    grant(&a, b, locast_protocol::room::cap::LASER).await;
    send(b, mv(0.7, 0.8)).await.expect("granted move sent");
    wait_until(
        "A and C to see B's granted move",
        Duration::from_secs(5),
        || a.sink.count() >= 1 && c.sink.count() >= 1,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    for (who, cl) in [("A", &a), ("C", c)] {
        let seen = cl.sink.snapshot();
        assert_eq!(seen.len(), 1, "{who} saw only the granted move: {seen:?}");
        match &seen[0] {
            Seen::Move(ev) => {
                assert_eq!(ev.sender_id, b_id.to_string());
                assert_eq!((ev.x, ev.y), (0.7, 0.8), "{who}: the granted move");
            }
            other => panic!("{who}: expected the granted move, got {other:?}"),
        }
    }

    // The denial did not end B's room locally (an unsolicited
    // ROOM_ERROR would have cleared the cached state).
    assert!(b.room.state().await.is_some(), "B was not evicted");
    assert!(b.room.local_user_id().await.is_some(), "B kept its user id");
    assert_eq!(b.sink.count(), 0, "B got no echo of its own laser");

    a.signaling.shutdown().await;
    b.signaling.shutdown().await;
    c.signaling.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_other_participant_receives_the_laser() {
    let url = spawn_server().await;
    let (a, viewers) = room_with_viewers(&url, 2).await;
    let a_id = a.room.local_user_id().await.expect("A user id");

    send(&a, mv(0.4, 0.6)).await.expect("move");
    for v in &viewers {
        wait_until(
            "each viewer to see A's move",
            Duration::from_secs(5),
            || v.sink.count() == 1,
        )
        .await;
        match &v.sink.snapshot()[0] {
            Seen::Move(ev) => assert_eq!(ev.sender_id, a_id.to_string()),
            other => panic!("expected a move, got {other:?}"),
        }
    }

    a.signaling.shutdown().await;
    for v in &viewers {
        v.signaling.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_range_coordinates_are_rejected_before_sending() {
    let url = spawn_server().await;
    let (a, viewers) = room_with_viewers(&url, 1).await;
    let b = &viewers[0];

    for (x, y) in [
        (-0.1, 0.5),
        (0.5, 1.1),
        (f32::NAN, 0.5),
        (0.5, f32::INFINITY),
    ] {
        assert!(
            send(&a, mv(x, y)).await.is_err(),
            "({x}, {y}) is rejected locally, not clamped"
        );
    }
    // A valid move afterwards still goes through: nothing was sent
    // for the bad ones, and A is still in the room.
    send(&a, mv(0.5, 0.5)).await.expect("valid move");
    wait_until("B to see the valid move", Duration::from_secs(5), || {
        b.sink.count() == 1
    })
    .await;
    match &b.sink.snapshot()[0] {
        Seen::Move(ev) => assert_eq!((ev.x, ev.y), (0.5, 0.5)),
        other => panic!("expected the valid move, got {other:?}"),
    }
    assert!(a.room.state().await.is_some(), "A still in the room");

    a.signaling.shutdown().await;
    b.signaling.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn laser_send_outside_a_room_fails_cleanly() {
    let url = spawn_server().await;
    let a = connect_client(&url).await;
    assert!(send(&a, mv(0.1, 0.1)).await.is_err(), "no room -> Err");
    assert!(
        send(&a, LaserSendInput::Off).await.is_err(),
        "no room -> Err"
    );
    a.signaling.shutdown().await;
}
