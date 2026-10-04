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
    RoomClient, RoomEventSink, RoomSummaryIpc, StrokeBeginEvent, StrokeEndEvent, StrokePointEvent,
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
        assert_eq!(res.stroke_id, id, "command echoes the canonical id");
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
        })
        .collect();
    assert_eq!(shape, expected_shape, "begin -> points -> end per stroke");

    // Identity, ids and payload fidelity.
    let mut stroke_idx = 0;
    let mut point_idx = 0.0_f32;
    for s in &seen {
        match s {
            Seen::Begin(ev) => {
                stroke_idx = ids.iter().position(|i| *i == ev.stroke_id).expect("id");
                point_idx = 0.0;
                // NOTE: `ev.sender_id` is deliberately not asserted.
                // The server's rebroadcast envelope carries only the
                // payload (no `sender`), so the receive side currently
                // reports the nil UUID as the originator. That is a
                // receive-side gap outside P5-T02's send path.
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
        }
    }

    // The sender does not receive its own echo.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(a.sink.count(), 0, "server excludes the originator");

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
