//! P5-T02: end-to-end test of the PRODUCTION drawing send path.
//!
//! Participant A calls the body of the real `drawing_send` Tauri
//! command (`send_drawing`, which the registered `drawing_send`
//! wrapper calls with the managed `RoomClient`, `SignalingClient`
//! and `IdentityService`). The command signs
//! DRAW_BEGIN with the identity key, forwards the envelopes through
//! the real `SignalingClient` WebSocket to a real in-process
//! `locast-server`, which verifies the signature and rebroadcasts to
//! participant B. B's real `RoomClient::run_inbound` turns the
//! rebroadcast into the `drawing://*` events the React layer
//! consumes; a recording `RoomEventSink` captures them.
//!
//! What this proves that the server-only tests in
//! `apps/server/tests/rooms.rs` cannot: the client builds envelopes
//! the server accepts (stroke id format, the server-assigned
//! `sender.user_id`, the Ed25519 signature over the canonical bytes),
//! and that the receive side decodes what the send side produced.

#![allow(clippy::needless_return)]

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use locast_client_lib::commands::drawing::{
    parse_stroke_id, send_drawing, DrawingSendInput, DrawingSendResult,
};
use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::{
    RoomClient, RoomEventSink, RoomSummaryIpc, StrokeBeginEvent, StrokeClearEvent, StrokeEndEvent,
    StrokePointEvent, StrokeUndoEvent,
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
    Begin(StrokeBeginEvent),
    Point(StrokePointEvent),
    End(StrokeEndEvent),
    Undo(StrokeUndoEvent),
    Clear(StrokeClearEvent),
}

#[derive(Default)]
struct RecordingSink {
    events: StdMutex<Vec<(Instant, Seen)>>,
}

impl RecordingSink {
    fn snapshot(&self) -> Vec<(Instant, Seen)> {
        self.events.lock().expect("sink lock").clone()
    }
    fn count(&self) -> usize {
        self.events.lock().expect("sink lock").len()
    }
    /// The undo events seen so far, in arrival order.
    fn undos(&self) -> Vec<(Instant, StrokeUndoEvent)> {
        self.snapshot()
            .into_iter()
            .filter_map(|(t, s)| match s {
                Seen::Undo(ev) => Some((t, ev)),
                _ => None,
            })
            .collect()
    }
    /// The clear events seen so far, in arrival order.
    fn clears(&self) -> Vec<(Instant, StrokeClearEvent)> {
        self.snapshot()
            .into_iter()
            .filter_map(|(t, s)| match s {
                Seen::Clear(ev) => Some((t, ev)),
                _ => None,
            })
            .collect()
    }
}

impl RoomEventSink for RecordingSink {
    fn emit_state(&self, _summary: &RoomSummaryIpc) {}
    fn emit_event(&self, _summary: &RoomSummaryIpc) {}
    fn emit_state_cleared(&self) {}
    fn emit_stroke_begin(&self, ev: &StrokeBeginEvent) {
        self.events
            .lock()
            .expect("sink lock")
            .push((Instant::now(), Seen::Begin(ev.clone())));
    }
    fn emit_stroke_point(&self, ev: &StrokePointEvent) {
        self.events
            .lock()
            .expect("sink lock")
            .push((Instant::now(), Seen::Point(ev.clone())));
    }
    fn emit_stroke_end(&self, ev: &StrokeEndEvent) {
        self.events
            .lock()
            .expect("sink lock")
            .push((Instant::now(), Seen::End(ev.clone())));
    }
    fn emit_stroke_undo(&self, ev: &StrokeUndoEvent) {
        self.events
            .lock()
            .expect("sink lock")
            .push((Instant::now(), Seen::Undo(ev.clone())));
    }
    fn emit_stroke_clear(&self, ev: &StrokeClearEvent) {
        self.events
            .lock()
            .expect("sink lock")
            .push((Instant::now(), Seen::Clear(ev.clone())));
    }
}

fn sender_of(s: &Seen) -> &str {
    match s {
        Seen::Begin(ev) => &ev.sender_id,
        Seen::Point(ev) => &ev.sender_id,
        Seen::End(ev) => &ev.sender_id,
        Seen::Undo(ev) => &ev.sender_id,
        Seen::Clear(ev) => &ev.sender_id,
    }
}

struct Client {
    signaling: Arc<SignalingClient>,
    room: Arc<RoomClient>,
    identity: Arc<IdentityService>,
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
        identity,
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

/// The state the `drawing_send` command receives from Tauri
/// (`Arc<RoomClient>`, `Arc<SignalingClient>`,
/// `Arc<IdentityService>`), passed to the command's body
/// `send_drawing` by reference exactly as the wrapper does.
struct App<'a>(&'a Client);

fn mock_app_for(c: &Client) -> App<'_> {
    App(c)
}

async fn send(
    app: &App<'_>,
    input: DrawingSendInput,
) -> Result<DrawingSendResult, locast_client_lib::commands::error::AppError> {
    send_drawing(input, &app.0.room, &app.0.signaling, &app.0.identity).await
}

fn begin(stroke_id: &str, tool: &str, x: f32, y: f32) -> DrawingSendInput {
    DrawingSendInput::Begin {
        stroke_id: stroke_id.to_string(),
        tool: tool.to_string(),
        color: "#ff5c69".to_string(),
        width: 3.0,
        x,
        y,
        pressure: 0.5,
        ts_ms: 1_000,
        client_seq: 1,
    }
}

fn point(stroke_id: &str, x: f32, y: f32, seq: u64) -> DrawingSendInput {
    DrawingSendInput::Point {
        stroke_id: stroke_id.to_string(),
        x,
        y,
        pressure: 0.25,
        ts_ms: 1_000 + seq as i64,
        client_seq: seq,
    }
}

fn end(stroke_id: &str, seq: u64) -> DrawingSendInput {
    DrawingSendInput::End {
        stroke_id: stroke_id.to_string(),
        ts_ms: 2_000,
        client_seq: seq,
    }
}

/// Host A creates a room, viewer B joins it.
async fn two_in_a_room(url: &str) -> (Client, Client) {
    let a = connect_client(url).await;
    let b = connect_client(url).await;
    let summary = a
        .room
        .room_create("Draw".into(), false)
        .await
        .expect("create");
    b.room
        .room_join(summary.code.clone(), "B".into())
        .await
        .expect("join");
    // `room_join` can return before the inbound loop has stored the
    // server-assigned user id, so wait for both ids before a test reads
    // them (under load the id was briefly `None`).
    let start = Instant::now();
    loop {
        if a.room.local_user_id().await.is_some() && b.room.local_user_id().await.is_some() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "both clients learn their server-assigned user ids"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (a, b)
}

// ---------------------------------------------------------------
// Stroke id validation (pure)
// ---------------------------------------------------------------

#[test]
fn stroke_id_validation_accepts_canonical_uuids_only() {
    let v7 = Uuid::now_v7().to_string();
    let v4 = Uuid::new_v4().to_string();
    assert_eq!(parse_stroke_id(&v7).expect("v7").to_string(), v7);
    assert_eq!(parse_stroke_id(&v4).expect("v4").to_string(), v4);

    // The id the old `services/drawing.ts` produced: three groups
    // and a trailing '-'.
    assert!(parse_stroke_id("018f3a2b4c5d-7abc-1234-").is_err());
    // The old local `makeStrokeId("stroke")` shape.
    assert!(parse_stroke_id("stroke-018f3a2b4c5d-7abc-1234").is_err());
    assert!(parse_stroke_id("").is_err());
    assert!(parse_stroke_id("not-a-uuid").is_err());
    // Valid to `Uuid::parse_str` but not canonical: would be
    // normalized on the wire and break id correlation.
    assert!(parse_stroke_id(&v7.to_uppercase()).is_err());
    assert!(parse_stroke_id(&v7.replace('-', "")).is_err());
    assert!(parse_stroke_id(&format!("{{{v7}}}")).is_err());
    assert!(parse_stroke_id(&format!("urn:uuid:{v7}")).is_err());
    assert!(parse_stroke_id("00000000-0000-0000-0000-000000000000").is_err());
}

// ---------------------------------------------------------------
// Real client -> real server -> real client
// ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drawing_send_delivers_a_stroke_to_the_other_participant_in_order() {
    let url = spawn_server().await;
    let (a, b) = two_in_a_room(&url).await;
    assert!(
        a.room.local_user_id().await.is_some(),
        "A has a server-assigned user id (what DRAW_BEGIN's sender must carry)"
    );
    let app = mock_app_for(&a);

    // Two strokes: `pen`, then `rect` (every protocol tool must
    // be sendable, not just `pen`).
    let mut ids = Vec::new();
    for (tool, n_points) in [("pen", 10_u64), ("rect", 3)] {
        let id = Uuid::now_v7().to_string();
        ids.push(id.clone());
        let res = send(&app, begin(&id, tool, 0.10, 0.20))
            .await
            .expect("begin accepted by the command");
        assert_eq!(
            res.stroke_id.as_deref(),
            Some(id.as_str()),
            "command echoes the canonical id"
        );
        for i in 0..n_points {
            let seq = 2 + i;
            send(&app, point(&id, 0.2 + i as f32 * 0.05, 0.5, seq))
                .await
                .expect("point");
        }
        send(&app, end(&id, 2 + n_points)).await.expect("end");
    }

    wait_until("B to receive both strokes", Duration::from_secs(5), || {
        b.sink.count() == 2 + 10 + 3 + 2
    })
    .await;

    let seen: Vec<Seen> = b.sink.snapshot().into_iter().map(|(_, s)| s).collect();

    // Order on the wire: begin, 10 x point, end, begin, 3 x point, end.
    let mut expected_shape = vec!['B'];
    expected_shape.extend(std::iter::repeat_n('P', 10));
    expected_shape.push('E');
    expected_shape.push('B');
    expected_shape.extend(std::iter::repeat_n('P', 3));
    expected_shape.push('E');
    let shape: Vec<char> = seen
        .iter()
        .map(|s| match s {
            Seen::Begin(_) => 'B',
            Seen::Point(_) => 'P',
            Seen::End(_) => 'E',
            Seen::Undo(_) => 'U',
            Seen::Clear(_) => 'X',
        })
        .collect();
    assert_eq!(shape, expected_shape, "begin -> points -> end per stroke");

    // Every event B saw is attributed to A's server-assigned user id,
    // the same id A reads from `RoomClient::local_user_id` (never nil).
    let a_id = a.room.local_user_id().await.expect("A user id").to_string();
    assert_ne!(a_id, Uuid::nil().to_string());
    for s in &seen {
        assert_eq!(sender_of(s), a_id, "every event carries A's id: {s:?}");
    }

    // Identity, ids and payload fidelity.
    let mut stroke_idx = 0;
    let mut point_idx = 0.0_f32;
    for s in &seen {
        match s {
            Seen::Begin(ev) => {
                stroke_idx = ids.iter().position(|i| *i == ev.stroke_id).expect("id");
                point_idx = 0.0;
                assert_eq!(ev.tool, ["pen", "rect"][stroke_idx]);
                assert_eq!(ev.color, "#ff5c69");
                assert_eq!(ev.width, 3.0);
                assert_eq!((ev.x, ev.y, ev.pressure), (0.10, 0.20, 0.5));
            }
            Seen::Point(ev) => {
                assert_eq!(ev.stroke_id, ids[stroke_idx]);
                assert_eq!(ev.x, 0.2 + point_idx * 0.05);
                assert_eq!(ev.y, 0.5);
                point_idx += 1.0;
            }
            Seen::End(ev) => assert_eq!(ev.stroke_id, ids[stroke_idx]),
            Seen::Undo(_) | Seen::Clear(_) => panic!("no undo or clear was sent"),
        }
    }

    // The sender does not receive its own echo.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(a.sink.count(), 0, "server excludes the originator");

    a.signaling.shutdown().await;
    b.signaling.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_strokes_carry_their_authors_distinct_user_ids() {
    // Two authors drawing at the same time: each stroke's events
    // (begin, points, end) reach the other participant attributed to
    // that stroke's author, never nil, never the receiver's own id,
    // and never swapped between the interleaved strokes.
    let url = spawn_server().await;
    let (a, b) = two_in_a_room(&url).await;
    let a_id = a.room.local_user_id().await.expect("A user id");
    let b_id = b.room.local_user_id().await.expect("B user id");
    assert_ne!(a_id, b_id, "distinct server-assigned ids");
    assert!(!a_id.is_nil() && !b_id.is_nil());

    // B joined as a viewer: the host grants DRAW and waits until B
    // has seen the update.
    let room_id: Uuid = a
        .room
        .state()
        .await
        .expect("A in a room")
        .id
        .parse()
        .expect("room id");
    a.room
        .permission_set(room_id, b_id, locast_protocol::room::cap::DRAW, 0)
        .await
        .expect("grant DRAW");
    let start = Instant::now();
    loop {
        let caps = b.room.state().await.and_then(|s| s.you_cap_set);
        if caps.is_some_and(|c| c & locast_protocol::room::cap::DRAW != 0) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "B never saw the DRAW grant"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let app_a = mock_app_for(&a);
    let app_b = mock_app_for(&b);
    let stroke_a = Uuid::now_v7().to_string();
    let stroke_b = Uuid::now_v7().to_string();
    send(&app_a, begin(&stroke_a, "pen", 0.1, 0.1))
        .await
        .expect("A begin");
    send(&app_b, begin(&stroke_b, "pen", 0.9, 0.9))
        .await
        .expect("B begin");
    for i in 0..4_u64 {
        send(&app_a, point(&stroke_a, 0.2 + i as f32 * 0.1, 0.3, 2 + i))
            .await
            .expect("A point");
        send(&app_b, point(&stroke_b, 0.8 - i as f32 * 0.1, 0.7, 2 + i))
            .await
            .expect("B point");
    }
    send(&app_a, end(&stroke_a, 6)).await.expect("A end");
    send(&app_b, end(&stroke_b, 6)).await.expect("B end");

    // 1 begin + 4 points + 1 end each way.
    wait_until(
        "both sides to see the other stroke",
        Duration::from_secs(5),
        || a.sink.count() == 6 && b.sink.count() == 6,
    )
    .await;

    let seen_by_b: Vec<Seen> = b.sink.snapshot().into_iter().map(|(_, s)| s).collect();
    let seen_by_a: Vec<Seen> = a.sink.snapshot().into_iter().map(|(_, s)| s).collect();
    let stroke_of = |s: &Seen| match s {
        Seen::Begin(ev) => ev.stroke_id.clone(),
        Seen::Point(ev) => ev.stroke_id.clone(),
        Seen::End(ev) => ev.stroke_id.clone(),
        Seen::Undo(ev) => ev.stroke_id.clone(),
        Seen::Clear(_) => String::new(),
    };
    for s in &seen_by_b {
        assert_eq!(stroke_of(s), stroke_a, "B only sees A's stroke");
        assert_eq!(sender_of(s), a_id.to_string(), "B sees A as the author");
    }
    for s in &seen_by_a {
        assert_eq!(stroke_of(s), stroke_b, "A only sees B's stroke");
        assert_eq!(sender_of(s), b_id.to_string(), "A sees B as the author");
    }

    // Neither side was echoed its own stroke.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(a.sink.count(), 6, "A got no echo of its own stroke");
    assert_eq!(b.sink.count(), 6, "B got no echo of its own stroke");

    a.signaling.shutdown().await;
    b.signaling.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_120_point_burst_is_rebroadcast_within_the_latency_budget() {
    // Roadmap P5-T02: "the server rebroadcasts to all other
    // participants within 50 ms". Measured here as the time from
    // the command returning (envelope queued on the WS) to B's
    // recording sink seeing the corresponding event.
    let url = spawn_server().await;
    let (a, b) = two_in_a_room(&url).await;
    let app = mock_app_for(&a);
    let id = Uuid::now_v7().to_string();
    send(&app, begin(&id, "pen", 0.0, 0.0))
        .await
        .expect("begin");

    let mut sent_at = Vec::new();
    for i in 0..120_u64 {
        send(&app, point(&id, i as f32 / 120.0, 0.5, 2 + i))
            .await
            .expect("point");
        sent_at.push(Instant::now());
        // ~125 Hz pacing: deliberately faster than the 80 Hz client cap, as a latency stress.
        tokio::time::sleep(Duration::from_millis(8)).await;
    }
    send(&app, end(&id, 200)).await.expect("end");

    wait_until("all 122 events", Duration::from_secs(5), || {
        b.sink.count() == 122
    })
    .await;
    let events = b.sink.snapshot();
    let mut lat_ms: Vec<f64> = events
        .iter()
        .filter(|(_, s)| matches!(s, Seen::Point(_)))
        .zip(sent_at.iter())
        .map(|((recv, _), sent)| recv.saturating_duration_since(*sent).as_secs_f64() * 1000.0)
        .collect();
    lat_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = lat_ms[lat_ms.len() / 2];
    let max = *lat_ms.last().unwrap();
    println!("DRAW_POINT rebroadcast latency: median {median:.2} ms, max {max:.2} ms");
    assert_eq!(lat_ms.len(), 120);
    assert!(
        median < 50.0,
        "median {median:.2} ms exceeds the 50 ms budget"
    );
    // Generous: shared CI runners can stall for hundreds of ms.
    assert!(max < 2000.0, "max {max:.2} ms is pathological");

    a.signaling.shutdown().await;
    b.signaling.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_stroke_ids_are_rejected_before_anything_is_sent() {
    let url = spawn_server().await;
    let (a, b) = two_in_a_room(&url).await;
    let app = mock_app_for(&a);
    let good = Uuid::now_v7().to_string();
    for bad in [
        "018f3a2b4c5d-7abc-1234-",
        "stroke-018f3a2b4c5d-7abc-1234",
        "",
        &good.to_uppercase(),
        &good.replace('-', ""),
    ] {
        let err = send(&app, begin(bad, "pen", 0.1, 0.1)).await;
        assert!(err.is_err(), "begin with {bad:?} must be rejected");
        let err = send(&app, point(bad, 0.1, 0.1, 2)).await;
        assert!(err.is_err(), "point with {bad:?} must be rejected");
        let err = send(&app, end(bad, 3)).await;
        assert!(err.is_err(), "end with {bad:?} must be rejected");
    }
    // An unknown tool is refused too.
    assert!(send(&app, begin(&good, "laser", 0.1, 0.1)).await.is_err());

    // Nothing reached B; a valid stroke afterwards is the first
    // thing B sees.
    send(&app, begin(&good, "pen", 0.1, 0.1))
        .await
        .expect("begin");
    wait_until("B to see the valid begin", Duration::from_secs(5), || {
        b.sink.count() >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let seen = b.sink.snapshot();
    assert_eq!(seen.len(), 1, "only the valid begin arrived");
    match &seen[0].1 {
        Seen::Begin(ev) => assert_eq!(ev.stroke_id, good),
        other => panic!("expected begin, got {other:?}"),
    }

    a.signaling.shutdown().await;
    b.signaling.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_range_values_are_rejected_locally_and_do_not_evict_the_user() {
    // The server answers a bad value with ROOM_ERROR, which the room
    // client treats as the end of the room. The command must refuse
    // such input itself: Err, nothing sent, room state intact.
    let url = spawn_server().await;
    let (a, b) = two_in_a_room(&url).await;
    let app = mock_app_for(&a);
    let id = Uuid::now_v7().to_string();

    let begin_with = |tool: &str, color: &str, width: f32, x: f32, y: f32, pressure: f32| {
        DrawingSendInput::Begin {
            stroke_id: id.clone(),
            tool: tool.to_string(),
            color: color.to_string(),
            width,
            x,
            y,
            pressure,
            ts_ms: 1,
            client_seq: 1,
        }
    };
    let long_color = "#".repeat(65);
    let bad_begins = [
        ("x > 1", begin_with("pen", "#fff", 3.0, 1.5, 0.5, 0.5)),
        ("x < 0", begin_with("pen", "#fff", 3.0, -0.1, 0.5, 0.5)),
        ("x NaN", begin_with("pen", "#fff", 3.0, f32::NAN, 0.5, 0.5)),
        ("y > 1", begin_with("pen", "#fff", 3.0, 0.5, 1.0001, 0.5)),
        (
            "y inf",
            begin_with("pen", "#fff", 3.0, 0.5, f32::INFINITY, 0.5),
        ),
        (
            "pressure > 1",
            begin_with("pen", "#fff", 3.0, 0.5, 0.5, 2.0),
        ),
        (
            "pressure < 0",
            begin_with("pen", "#fff", 3.0, 0.5, 0.5, -0.5),
        ),
        ("width 0", begin_with("pen", "#fff", 0.0, 0.5, 0.5, 0.5)),
        ("width < 0", begin_with("pen", "#fff", -1.0, 0.5, 0.5, 0.5)),
        (
            "width NaN",
            begin_with("pen", "#fff", f32::NAN, 0.5, 0.5, 0.5),
        ),
        (
            "width inf",
            begin_with("pen", "#fff", f32::INFINITY, 0.5, 0.5, 0.5),
        ),
        ("empty color", begin_with("pen", "", 3.0, 0.5, 0.5, 0.5)),
        (
            "overlong color",
            begin_with("pen", &long_color, 3.0, 0.5, 0.5, 0.5),
        ),
        (
            "control char in color",
            begin_with(
                "pen", "#f
f", 3.0, 0.5, 0.5, 0.5,
            ),
        ),
        (
            "unknown tool",
            begin_with("laser", "#fff", 3.0, 0.5, 0.5, 0.5),
        ),
    ];
    for (what, input) in bad_begins {
        assert!(
            send(&app, input).await.is_err(),
            "begin with {what} must be Err"
        );
    }
    let bad_points = [
        ("x > 1", point(&id, 1.1, 0.5, 2)),
        ("y < 0", point(&id, 0.5, -1.0, 2)),
        ("y NaN", point(&id, 0.5, f32::NAN, 2)),
        (
            "pressure inf",
            DrawingSendInput::Point {
                stroke_id: id.clone(),
                x: 0.5,
                y: 0.5,
                pressure: f32::INFINITY,
                ts_ms: 1,
                client_seq: 2,
            },
        ),
    ];
    for (what, input) in bad_points {
        assert!(
            send(&app, input).await.is_err(),
            "point with {what} must be Err"
        );
    }

    // Nothing was sent, so nothing came back as ROOM_ERROR: A is
    // still in the room and B saw nothing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(b.sink.count(), 0, "no rejected input reached B");
    assert!(
        a.room.state().await.is_some(),
        "A is still in the room (in_room=true)"
    );

    // The boundary values are valid, and a valid stroke afterwards
    // still works end to end.
    send(&app, begin_with("pen", "#ff5c69", 0.001, 0.0, 1.0, 1.0))
        .await
        .expect("boundary begin");
    send(&app, point(&id, 1.0, 0.0, 2)).await.expect("point");
    send(&app, end(&id, 3)).await.expect("end");
    wait_until("B to see the valid stroke", Duration::from_secs(5), || {
        b.sink.count() == 3
    })
    .await;
    assert!(a.room.state().await.is_some(), "A still in the room");

    a.signaling.shutdown().await;
    b.signaling.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewer_without_draw_cannot_draw_on_other_screens() {
    // B joined as a viewer: no DRAW capability. The command
    // itself succeeds (the envelope is queued; the server's
    // refusal arrives asynchronously as ROOM_ERROR), but nothing
    // is rebroadcast to A.
    let url = spawn_server().await;
    let (a, b) = two_in_a_room(&url).await;
    let app = mock_app_for(&b);
    let id = Uuid::now_v7().to_string();
    let res = send(&app, begin(&id, "pen", 0.1, 0.1)).await;
    println!("viewer drawing_send(begin) -> {res:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a.sink.count(), 0, "the host never sees a refused stroke");
    // Informational: how the viewer's own room client reacts to the
    // server's ROOM_ERROR (see the report: an unsolicited ROOM_ERROR
    // clears the cached room state).
    println!(
        "viewer room state after refused draw: in_room={}",
        b.room.state().await.is_some()
    );

    a.signaling.shutdown().await;
    b.signaling.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drawing_send_outside_a_room_fails_cleanly() {
    let url = spawn_server().await;
    let a = connect_client(&url).await;
    let app = mock_app_for(&a);
    let id = Uuid::now_v7().to_string();
    let err = send(&app, begin(&id, "pen", 0.1, 0.1)).await;
    assert!(err.is_err(), "no room -> Err, not a panic");
    a.signaling.shutdown().await;
}

// ---------------------------------------------------------------
// P5-T03: undo + clear_all through the production send path
// ---------------------------------------------------------------

fn undo(stroke_id: &str) -> DrawingSendInput {
    DrawingSendInput::Undo {
        stroke_id: stroke_id.to_string(),
    }
}

fn clear() -> DrawingSendInput {
    DrawingSendInput::Clear {}
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

/// The host grants `bits` (on top of what the target holds) and waits
/// until the target's own room client has applied the update.
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

/// Draw a complete stroke (begin, two points, end) through the command.
async fn draw(app: &App<'_>, stroke_id: &str) {
    send(app, begin(stroke_id, "pen", 0.1, 0.1))
        .await
        .expect("begin");
    send(app, point(stroke_id, 0.2, 0.2, 2))
        .await
        .expect("point");
    send(app, point(stroke_id, 0.3, 0.3, 3))
        .await
        .expect("point");
    send(app, end(stroke_id, 4)).await.expect("end");
}

/// Wait until the server has handled everything `c` sent so far. The server
/// handles one connection's frames in order, so a round trip on `c`'s own
/// connection returns only after it has processed the frames before it. This
/// is an ordering barrier: a command's `send` returns once the frame is queued
/// locally, and the server treats two connections independently, so without
/// it a later message from another client (a grant) could be handled first.
async fn server_has_handled_everything_from(c: &Client) {
    c.room
        .clock_skew_probe()
        .await
        .expect("barrier round trip on the connection");
}

/// `c` has seen exactly `expected` events (of any kind) so far.
fn assert_event_count(c: &Client, expected: usize, what: &str) {
    assert_eq!(c.sink.count(), expected, "{what}");
}

/// Host + two more participants in one room, all with the DRAW + own
/// undo capabilities an Editor holds.
async fn three_in_a_room(url: &str) -> (Client, Client, Client) {
    let a = connect_client(url).await;
    let b = connect_client(url).await;
    let c = connect_client(url).await;
    let summary = a
        .room
        .room_create("Draw".into(), false)
        .await
        .expect("create");
    b.room
        .room_join(summary.code.clone(), "B".into())
        .await
        .expect("join b");
    c.room
        .room_join(summary.code.clone(), "C".into())
        .await
        .expect("join c");
    (a, b, c)
}

/// Acceptance A + G: own undo. B (DRAW + UNDO_OWN) draws and undoes its
/// own stroke; the undo reaches A (remote) and B (the actor, echoed)
/// attributed to B, well inside the latency budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_undo_reaches_both_clients_with_the_actor_attributed() {
    let url = spawn_server().await;
    let (a, b) = two_in_a_room(&url).await;
    let b_id = b.room.local_user_id().await.expect("b id").to_string();
    grant(
        &a,
        &b,
        locast_protocol::room::cap::DRAW | locast_protocol::room::cap::UNDO_OWN,
    )
    .await;
    let app_b = mock_app_for(&b);
    let id = Uuid::now_v7().to_string();
    draw(&app_b, &id).await;
    wait_until("A to see the stroke", Duration::from_secs(5), || {
        a.sink.count() == 4
    })
    .await;

    let t0 = Instant::now();
    let res = send(&app_b, undo(&id)).await.expect("undo accepted");
    assert_eq!(res.stroke_id.as_deref(), Some(id.as_str()));
    wait_until("A and B to see the undo", Duration::from_secs(5), || {
        a.sink.undos().len() == 1 && b.sink.undos().len() == 1
    })
    .await;
    for (who, c) in [("A", &a), ("B", &b)] {
        let (at, ev) = c.sink.undos().remove(0);
        let ms = at.saturating_duration_since(t0).as_secs_f64() * 1000.0;
        println!("undo reached {who} {ms:.2} ms after the command returned");
        assert!(ms < 1000.0, "{who}: undo latency {ms:.2} ms");
        assert_eq!(ev.stroke_id, id, "{who}");
        assert_eq!(ev.sender_id, b_id, "{who}: the actor is B (server-stamped)");
        assert_eq!(ev.room_id, room_id_of(&a).await.to_string(), "{who}");
    }
    // A duplicate undo is harmless: no second event, nobody evicted.
    send(&app_b, undo(&id))
        .await
        .expect("duplicate undo accepted");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a.sink.undos().len(), 1);
    assert_eq!(b.sink.undos().len(), 1);
    assert!(a.room.state().await.is_some() && b.room.state().await.is_some());

    a.signaling.shutdown().await;
    b.signaling.shutdown().await;
}

/// Acceptance B + F: undo-any. A draws; B without undo_any is refused
/// (nothing changes, B is not evicted); after the host grants undo_any,
/// B undoes A's stroke and both clients see it removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undoing_another_users_stroke_needs_undo_any_and_refusal_changes_nothing() {
    let url = spawn_server().await;
    let (a, b) = two_in_a_room(&url).await;
    let a_id = a.room.local_user_id().await.expect("a id").to_string();
    let b_id = b.room.local_user_id().await.expect("b id").to_string();
    grant(
        &a,
        &b,
        locast_protocol::room::cap::DRAW | locast_protocol::room::cap::UNDO_OWN,
    )
    .await;
    let app_a = mock_app_for(&a);
    let app_b = mock_app_for(&b);
    let id = Uuid::now_v7().to_string();
    draw(&app_a, &id).await;
    wait_until("B to see A's stroke", Duration::from_secs(5), || {
        b.sink.count() == 4
    })
    .await;

    // B holds undo_own only: the command queues the envelope, the server
    // refuses it silently. Nothing is broadcast, B stays in the room.
    send(&app_b, undo(&id)).await.expect("queued");
    server_has_handled_everything_from(&b).await;
    assert_event_count(&a, 0, "A saw nothing");
    assert_event_count(&b, 4, "B saw nothing new");
    assert!(b.room.state().await.is_some(), "B was not evicted");

    // Granted: the stroke is still there to undo.
    grant(&a, &b, locast_protocol::room::cap::UNDO_ANY).await;
    send(&app_b, undo(&id)).await.expect("undo");
    wait_until("A and B to see the undo", Duration::from_secs(5), || {
        a.sink.undos().len() == 1 && b.sink.undos().len() == 1
    })
    .await;
    for c in [&a, &b] {
        let ev = &c.sink.undos()[0].1;
        assert_eq!(ev.stroke_id, id);
        assert_eq!(ev.sender_id, b_id, "B is the actor, A was the owner");
        assert_ne!(ev.sender_id, a_id);
    }

    a.signaling.shutdown().await;
    b.signaling.shutdown().await;
}

/// Acceptance C + D: ownership, unknown ids, duplicates and a stroke of
/// another room.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_own_is_limited_to_own_strokes_and_odd_ids_are_safe_no_ops() {
    let url = spawn_server().await;
    let (a, b, c) = three_in_a_room(&url).await;
    let caps = locast_protocol::room::cap::DRAW | locast_protocol::room::cap::UNDO_OWN;
    grant(&a, &b, caps).await;
    grant(&a, &c, caps).await;
    let app_a = mock_app_for(&a);
    let app_b = mock_app_for(&b);
    let app_c = mock_app_for(&c);

    let sb = Uuid::now_v7().to_string();
    let sc = Uuid::now_v7().to_string();
    draw(&app_b, &sb).await;
    wait_until("A and C to see B's stroke", Duration::from_secs(5), || {
        a.sink.count() == 4 && c.sink.count() == 4
    })
    .await;
    draw(&app_c, &sc).await;
    wait_until("A and B to see C's stroke", Duration::from_secs(5), || {
        a.sink.count() == 8 && b.sink.count() == 4
    })
    .await;

    // C holds only undo_own and tries B's stroke: refused, nothing moves.
    send(&app_c, undo(&sb)).await.expect("queued");
    // Unknown id, and the same stroke twice.
    send(&app_a, undo(&Uuid::now_v7().to_string()))
        .await
        .expect("queued");
    tokio::time::sleep(Duration::from_millis(300)).await;
    for (who, cl) in [("A", &a), ("B", &b), ("C", &c)] {
        assert!(cl.sink.undos().is_empty(), "{who}: nothing was undone");
        assert!(cl.room.state().await.is_some(), "{who}: not evicted");
    }

    // A stroke of ANOTHER room: D hosts its own room and draws there.
    let d = connect_client(&url).await;
    d.room
        .room_create("Other".into(), false)
        .await
        .expect("d room");
    let app_d = mock_app_for(&d);
    let sd = Uuid::now_v7().to_string();
    draw(&app_d, &sd).await;
    // A (host of room 1, every bit) names D's stroke: unknown in room 1.
    send(&app_a, undo(&sd)).await.expect("queued");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(d.sink.undos().is_empty(), "D's stroke is untouched");
    assert!(a.sink.undos().is_empty() && b.sink.undos().is_empty());
    // ... D can still undo it in its own room.
    send(&app_d, undo(&sd)).await.expect("d undo");
    wait_until("D to see its own undo", Duration::from_secs(5), || {
        d.sink.undos().len() == 1
    })
    .await;
    assert!(a.sink.undos().is_empty(), "room 1 never hears about room 2");

    // Everyone can undo their own, exactly once.
    send(&app_b, undo(&sb)).await.expect("b undo");
    wait_until("all three to see B's undo", Duration::from_secs(5), || {
        a.sink.undos().len() == 1 && b.sink.undos().len() == 1 && c.sink.undos().len() == 1
    })
    .await;
    send(&app_c, undo(&sc)).await.expect("c undo");
    wait_until("all three to see C's undo", Duration::from_secs(5), || {
        a.sink.undos().len() == 2 && b.sink.undos().len() == 2 && c.sink.undos().len() == 2
    })
    .await;
    send(&app_b, undo(&sb)).await.expect("replay");
    send(&app_c, undo(&sc)).await.expect("replay");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a.sink.undos().len(), 2, "replays are no-ops");

    for cl in [&a, &b, &c, &d] {
        cl.signaling.shutdown().await;
    }
}

/// Acceptance E + F + G: clear_all. Unauthorized clear changes nothing;
/// an authorized one reaches every client (the actor included) quickly,
/// and a later undo of a cleared stroke is a no-op.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_all_needs_the_capability_and_reaches_every_client() {
    let url = spawn_server().await;
    let (a, b, c) = three_in_a_room(&url).await;
    let caps = locast_protocol::room::cap::DRAW | locast_protocol::room::cap::UNDO_OWN;
    grant(&a, &b, caps).await;
    grant(&a, &c, caps).await;
    let app_a = mock_app_for(&a);
    let app_b = mock_app_for(&b);
    let app_c = mock_app_for(&c);
    let a_id = a.room.local_user_id().await.expect("a id").to_string();

    let s1 = Uuid::now_v7().to_string();
    let s2 = Uuid::now_v7().to_string();
    let s3 = Uuid::now_v7().to_string();
    draw(&app_a, &s1).await;
    wait_until("B and C to see s1", Duration::from_secs(5), || {
        b.sink.count() == 4 && c.sink.count() == 4
    })
    .await;
    draw(&app_b, &s2).await;
    wait_until("A and C to see s2", Duration::from_secs(5), || {
        a.sink.count() == 4 && c.sink.count() == 8
    })
    .await;
    draw(&app_c, &s3).await;
    wait_until("A and B to see s3", Duration::from_secs(5), || {
        a.sink.count() == 8 && b.sink.count() == 8
    })
    .await;

    // B and C hold no clear_all: refused, nothing changes anywhere.
    send(&app_b, clear()).await.expect("queued");
    send(&app_c, clear()).await.expect("queued");
    tokio::time::sleep(Duration::from_millis(300)).await;
    for (who, cl) in [("A", &a), ("B", &b), ("C", &c)] {
        assert!(cl.sink.clears().is_empty(), "{who}: no clear");
        assert!(cl.room.state().await.is_some(), "{who}: not evicted");
    }
    // The room still holds the strokes: an authorized undo works.
    send(&app_a, undo(&s2)).await.expect("undo");
    wait_until(
        "everyone to see the undo of s2",
        Duration::from_secs(5),
        || a.sink.undos().len() == 1 && b.sink.undos().len() == 1 && c.sink.undos().len() == 1,
    )
    .await;

    // The host clears (the host holds clear_all implicitly).
    let t0 = Instant::now();
    let res = send(&app_a, clear()).await.expect("clear");
    assert_eq!(res.stroke_id, None, "a clear concerns no single stroke");
    wait_until(
        "every client to see the clear",
        Duration::from_secs(5),
        || a.sink.clears().len() == 1 && b.sink.clears().len() == 1 && c.sink.clears().len() == 1,
    )
    .await;
    for (who, cl) in [("A (actor)", &a), ("B", &b), ("C", &c)] {
        let (at, ev) = cl.sink.clears().remove(0);
        let ms = at.saturating_duration_since(t0).as_secs_f64() * 1000.0;
        println!("clear reached {who} {ms:.2} ms after the command returned");
        assert!(ms < 1000.0, "{who}: clear latency {ms:.2} ms");
        assert_eq!(ev.sender_id, a_id, "{who}: attributed to the actor");
    }

    // Cleared strokes are gone server-side too: undoing them is a no-op.
    for s in [&s1, &s3] {
        send(&app_a, undo(s)).await.expect("queued");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a.sink.undos().len(), 1, "no undo of a cleared stroke");
    assert_eq!(b.sink.undos().len(), 1);

    // A viewer holding clear_all (granted) can clear too.
    grant(&a, &b, locast_protocol::room::cap::CLEAR_ALL).await;
    send(&app_b, clear()).await.expect("b clear");
    wait_until("a second clear everywhere", Duration::from_secs(5), || {
        a.sink.clears().len() == 2 && b.sink.clears().len() == 2 && c.sink.clears().len() == 2
    })
    .await;
    let b_id = b.room.local_user_id().await.expect("b id").to_string();
    assert_eq!(c.sink.clears()[1].1.sender_id, b_id);

    for cl in [&a, &b, &c] {
        cl.signaling.shutdown().await;
    }
}

/// The new send actions reject a malformed id before anything is sent
/// (an unsolicited server error would evict the user).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_undo_ids_are_rejected_before_anything_is_sent() {
    let url = spawn_server().await;
    let (a, b) = two_in_a_room(&url).await;
    let app = mock_app_for(&a);
    let good = Uuid::now_v7().to_string();
    for bad in ["", "nope", &good.to_uppercase(), &good.replace('-', "")] {
        assert!(send(&app, undo(bad)).await.is_err(), "undo {bad:?}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(b.sink.count(), 0);
    assert!(a.room.state().await.is_some());
    // Outside a room both actions fail cleanly.
    let lone = connect_client(&url).await;
    let app = mock_app_for(&lone);
    assert!(send(&app, undo(&good)).await.is_err());
    assert!(send(&app, clear()).await.is_err());
    for cl in [&a, &b, &lone] {
        cl.signaling.shutdown().await;
    }
}
