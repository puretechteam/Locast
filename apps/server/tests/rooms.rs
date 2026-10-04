//! Integration tests for P2-T04 (room lifecycle + host
//! migration).
//!
//! Each test spawns the server on `127.0.0.1:0` backed by an
//! in-memory SQLite database, drives `tokio_tungstenite`
//! clients through the handshake, and exercises the
//! ROOM_CREATE / ROOM_JOIN_REQUEST / ROOM_LEAVE / PRESENCE
//! envelopes plus the optional host-migration flow.
//!
//! The server is configured with a 200ms
//! `LOCAST_HOST_DISCONNECT_GRACE_MS` so the migration grace
//! path completes in a reasonable time.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use locast_protocol::envelope::Sender;
use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::room::{
    cap, PermissionSetPayload, PlaybackAcceptedEvent, PlaybackAction, PlaybackCommandPayload,
    StrokeBeginPayload, StrokeEndPayload, StrokePointPayload, StrokeTool,
};
use locast_protocol::room::{
    HostDisconnectedPayload, HostMigratedPayload, HostReconnectedPayload, ParticipantJoinedPayload,
    ParticipantLeftPayload, RoomClosedPayload, RoomCreatePayload, RoomCreatedPayload,
    RoomErrorCode, RoomErrorPayload, RoomJoinRequestPayload, RoomJoinedPayload, RoomLeavePayload,
    RoomSummary,
};
use locast_server::time::MockClock;
use locast_server::{AppState, Clock, Config, Db, Metrics, RoomRegistry, RoomRegistryConfig};
use rand::RngCore;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

const ROOM_CODE_LEN: usize = 6;

fn test_config() -> Config {
    Config {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        log_filter: "off".to_string(),
        database_url: "sqlite::memory:".to_string(),
        bearer_ttl_seconds: 900,
        challenge_ttl_ms: 30_000,
        max_frame_bytes: 1_048_576,
        handshake_timeout_ms: 30_000,
        rate_msgs_per_sec: 100,
        rate_msg_burst: 200,
        rate_bytes_per_sec: 1_000_000,
        rate_bytes_burst: 2_000_000,
        room_code_length: 6,
        room_code_alphabet: "ABCDEFGHJKLMNPQRSTUVWXYZ23456789".to_string(),
        room_max_participants: 8,
        host_disconnect_grace_ms: 200,
        room_create_max_collisions: 5,
        participant_stale_after_ms: 300_000,
        participant_disconnect_after_ms: 15_000,
        sensitive: Default::default(),
    }
}

struct TestHarness {
    addr: SocketAddr,
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
    #[allow(dead_code)]
    clock: Arc<MockClock>,
    db: Db,
    rooms: Arc<RoomRegistry>,
}

async fn spawn_test_server() -> TestHarness {
    spawn_test_server_with_config(test_config()).await
}

async fn spawn_test_server_with_config(config: Config) -> TestHarness {
    let db = Db::open(&config).await.expect("open db");
    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&config)));
    let clock = Arc::new(MockClock::new(1_000_000));
    let state = AppState {
        config: Arc::new(config),
        metrics: Metrics::new(),
        db: db.clone(),
        rooms: rooms.clone(),
        clock: clock.clone(),
        signal_relay: locast_server::SignalRelay::new(),
        epoch_counter: std::sync::Arc::new(std::sync::Mutex::new(
            locast_server::auth::EpochCounter::default(),
        )),
    };
    let app: Router = locast_server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let _ticker_handle = {
        let rooms = rooms.clone();
        let clock = clock.clone();
        let store: Arc<dyn locast_server::rooms::RoomStore> =
            Arc::new(locast_server::rooms::DbRoomStore::new(db.clone()));
        tokio::spawn(async move {
            locast_server::rooms::spawn_room_ticker_for_test(
                rooms,
                store,
                clock,
                std::time::Duration::from_millis(50),
            )
            .await;
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    TestHarness {
        addr,
        handle,
        clock,
        db,
        rooms,
    }
}

fn fresh_keypair() -> (SigningKey, [u8; 32]) {
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    let signing = SigningKey::from_bytes(&seed);
    let public = signing.verifying_key().to_bytes();
    (signing, public)
}

fn encode(env: &Envelope) -> Vec<u8> {
    rmp_serde::to_vec_named(env).expect("encode envelope")
}

fn decode(bytes: &[u8]) -> Envelope {
    rmp_serde::from_slice(bytes).expect("decode envelope")
}

fn hello_envelope() -> Envelope {
    Envelope {
        v: 1,
        r#type: MessageKind::Hello,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload: json!({
            "client_version": "0.0.0",
            "platform": "win",
            "device_id": "test-device",
        }),
    }
}

fn auth_envelope(pubkey: [u8; 32], sig: [u8; 64]) -> Envelope {
    Envelope {
        v: 1,
        r#type: MessageKind::Auth,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload: json!({
            "pubkey": pubkey.to_vec(),
            "sig": sig.to_vec(),
        }),
    }
}

async fn connect(
    addr: SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{addr}/ws");
    let (ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    ws
}

async fn read_binary<S>(stream: &mut S) -> Option<Vec<u8>>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let m = match stream.next().await {
            Some(Ok(m)) => m,
            Some(_) | None => return None,
        };
        match m {
            Message::Binary(b) => return Some(b),
            Message::Text(_) => panic!("unexpected text frame"),
            Message::Close(_) => return None,
            _ => continue,
        }
    }
}

#[derive(Debug, Clone)]
struct AuthedClient {
    token: [u8; 32],
    user_id: Uuid,
}

async fn complete_handshake(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    kp: &SigningKey,
) -> AuthedClient {
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let w = read_binary(ws).await.expect("welcome");
    let c = read_binary(ws).await.expect("challenge");
    let _ = decode(&w);
    let c_env = decode(&c);
    let challenge: locast_protocol::handshake::ChallengePayload =
        serde_json::from_value(c_env.payload).expect("challenge");
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    let sig = kp.sign(&nonce).to_bytes();
    let public = kp.verifying_key().to_bytes();
    ws.send(Message::Binary(encode(&auth_envelope(public, sig))))
        .await
        .expect("auth");
    let ok = read_binary(ws).await.expect("auth_ok");
    let ok_env = decode(&ok);
    let ok_p: locast_protocol::handshake::AuthOkPayload =
        serde_json::from_value(ok_env.payload).expect("ok payload");
    let mut token = [0u8; 32];
    token.copy_from_slice(&ok_p.bearer.token);
    AuthedClient {
        token,
        user_id: ok_p.user_id,
    }
}

fn room_create_envelope(token: [u8; 32], title: &str, migration_enabled: bool) -> Envelope {
    let mut payload = json!({
        "bearer": token.to_vec(),
    });
    let obj = payload.as_object_mut().unwrap();
    let inner = serde_json::to_value(RoomCreatePayload {
        title: title.to_string(),
        migration_enabled,
    })
    .unwrap();
    for (k, v) in inner.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    Envelope {
        v: 1,
        r#type: MessageKind::RoomCreate,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload,
    }
}

fn room_join_envelope(token: [u8; 32], code: &str, display_name: &str) -> Envelope {
    let mut payload = json!({
        "bearer": token.to_vec(),
    });
    let obj = payload.as_object_mut().unwrap();
    let inner = serde_json::to_value(RoomJoinRequestPayload {
        code: code.to_string(),
        display_name: display_name.to_string(),
    })
    .unwrap();
    for (k, v) in inner.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    Envelope {
        v: 1,
        r#type: MessageKind::RoomJoinRequest,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload,
    }
}

fn room_leave_envelope(token: [u8; 32]) -> Envelope {
    let mut payload = json!({
        "bearer": token.to_vec(),
    });
    let obj = payload.as_object_mut().unwrap();
    let inner = serde_json::to_value(RoomLeavePayload {}).unwrap();
    for (k, v) in inner.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    Envelope {
        v: 1,
        r#type: MessageKind::RoomLeave,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload,
    }
}

async fn send_envelope(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    env: &Envelope,
) {
    ws.send(Message::Binary(encode(env))).await.expect("send");
}

/// Drain the next envelope and assert its type matches. Returns
/// the decoded envelope.
async fn expect_envelope(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    expected: MessageKind,
) -> Envelope {
    // 30 s budget per call. The original 10 s budget was tight
    // enough to flake on the GitHub Actions ubuntu-latest image
    // under concurrent test load (multiple `cargo test` workers
    // running on a shared host). macOS / Windows CI runners
    // did not exhibit the flake. Bumped to a generous value so
    // the suite stays green on every platform without changing
    // any semantic check.
    let bytes = tokio::time::timeout(Duration::from_secs(30), read_binary(ws))
        .await
        .expect("timeout waiting for envelope")
        .expect("connection closed");
    let env = decode(&bytes);
    assert_eq!(
        env.r#type, expected,
        "expected {expected:?} got {:?}",
        env.r#type
    );
    env
}

/// Like [`expect_envelope`], but with a caller-chosen timeout and
/// a context label, and every failure message includes the
/// received kind, `room_id` and payload. Used by the multi-room
/// and immediate-join tests so a CI log can tell a lost broadcast
/// apart from an event delivered for the wrong room.
async fn expect_envelope_verbose(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    expected: MessageKind,
    budget: Duration,
    ctx: &str,
) -> Envelope {
    let started = std::time::Instant::now();
    let bytes = match tokio::time::timeout(budget, read_binary(ws)).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => panic!("{ctx}: connection closed while waiting for {expected:?}"),
        Err(_) => panic!("{ctx}: no frame within {budget:?} while waiting for {expected:?}"),
    };
    let env = decode(&bytes);
    assert_eq!(
        env.r#type,
        expected,
        "{ctx}: expected {expected:?}, got {:?} after {:?} (room_id={:?}, payload={})",
        env.r#type,
        started.elapsed(),
        env.room_id,
        env.payload
    );
    env
}

/// Read the next envelope but allow skipping over `MessageKind::Other`
/// types if they ever appear.
async fn next_envelope(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Envelope {
    // See `expect_envelope` above for why this is 30 s rather
    // than the previous 10 s.
    let bytes = tokio::time::timeout(Duration::from_secs(30), read_binary(ws))
        .await
        .expect("timeout waiting for envelope")
        .expect("connection closed");
    decode(&bytes)
}

// ---------------------------------------------------------------------------
// 1. Basic lifecycle: A creates with migration OFF, B joins, A leaves ends room.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn basic_lifecycle_create_join_leave_ends_room() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;

    // A creates
    send_envelope(&mut ws_a, &room_create_envelope(a.token, "Movie", false)).await;
    let env = expect_envelope(&mut ws_a, MessageKind::RoomCreated).await;
    let created: RoomCreatedPayload = serde_json::from_value(env.payload).unwrap();
    let code = created.room.code.clone();
    assert_eq!(created.room.participants.len(), 1);
    assert_eq!(created.room.participants[0].user_id, a.user_id);
    assert_eq!(created.you.user_id, a.user_id);
    assert!(!created.room.host_migration_enabled);
    assert_eq!(code.len(), ROOM_CODE_LEN);
    for c in code.chars() {
        assert!(locast_server::rooms::ALPHABET.contains(c), "bad char {c}");
        assert!(!"0O1I".contains(c), "ambiguous char {c} in code");
    }

    // B joins
    send_envelope(&mut ws_b, &room_join_envelope(b.token, &code, "B")).await;
    let env_b = expect_envelope(&mut ws_b, MessageKind::RoomJoined).await;
    let joined_b: RoomJoinedPayload = serde_json::from_value(env_b.payload).unwrap();
    assert_eq!(joined_b.room.participants.len(), 2);
    let env_a = expect_envelope(&mut ws_a, MessageKind::ParticipantJoined).await;
    let pj: ParticipantJoinedPayload = serde_json::from_value(env_a.payload).unwrap();
    assert_eq!(pj.participant.user_id, b.user_id);
    assert!(!pj.participant.is_host);

    // A leaves -> room ends, B receives ROOM_CLOSED
    send_envelope(&mut ws_a, &room_leave_envelope(a.token)).await;
    let env_b_closed = expect_envelope(&mut ws_b, MessageKind::RoomClosed).await;
    let closed: RoomClosedPayload = serde_json::from_value(env_b_closed.payload).unwrap();
    assert_eq!(closed.reason, "host_left");
    // A does NOT receive a PARTICIPANT_LEFT echo (A was
    // the originator; the forwarder filters the user's
    // own events). The room is also ended, so even the
    // server-side `is_user_in_room` check would skip.
    let _ = expect_envelope(&mut ws_b, MessageKind::ParticipantLeft).await;

    // A is gone.
    drop(ws_a);
    drop(ws_b);
    drop(harness);
}

// ---------------------------------------------------------------------------
// 2. Room-code format (covered by basic_lifecycle's inline assertion + codes.rs).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_code_format_is_6_alphabet_chars_no_ambiguous() {
    // Drive 5 creates and assert code shape on every one.
    let harness = spawn_test_server().await;
    let mut tokens = Vec::new();
    for _i in 0..5 {
        let (kp, _) = fresh_keypair();
        let mut ws = connect(harness.addr).await;
        let ac = complete_handshake(&mut ws, &kp).await;
        send_envelope(&mut ws, &room_create_envelope(ac.token, "T", false)).await;
        let env = expect_envelope(&mut ws, MessageKind::RoomCreated).await;
        let created: RoomCreatedPayload = serde_json::from_value(env.payload).unwrap();
        let code = created.room.code;
        assert_eq!(code.len(), ROOM_CODE_LEN);
        for c in code.chars() {
            assert!(locast_server::rooms::ALPHABET.contains(c));
            assert!(!"0O1I".contains(c));
        }
        tokens.push(ac.token);
    }
}

// ---------------------------------------------------------------------------
// 3. Code collision: the spec says the server retries with rejection
//    sampling up to N times. The MockClock-based unit test in
//    `rooms::registry::tests` covers the collision-retry; here we
//    just confirm the server does not panic and produces valid
//    codes for 50 concurrent creates.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_concurrent_creates_yield_unique_codes() {
    let harness = spawn_test_server().await;
    let mut handles = Vec::new();
    for _ in 0..50 {
        handles.push(tokio::spawn({
            let addr = harness.addr;
            async move {
                let (kp, _) = fresh_keypair();
                let mut ws = connect(addr).await;
                let ac = complete_handshake(&mut ws, &kp).await;
                send_envelope(&mut ws, &room_create_envelope(ac.token, "X", false)).await;
                let env = expect_envelope(&mut ws, MessageKind::RoomCreated).await;
                let created: RoomCreatedPayload = serde_json::from_value(env.payload).unwrap();
                created.room.code
            }
        }));
    }
    let mut codes = Vec::new();
    for h in handles {
        codes.push(h.await.unwrap());
    }
    let n = codes.len();
    for i in 0..n {
        for j in (i + 1)..n {
            assert_ne!(codes[i], codes[j], "duplicate code {0}", codes[i]);
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Host migration ON: host rejoin within grace.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_rejoin_within_grace_restores_host() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    send_envelope(&mut ws_a, &room_create_envelope(a.token, "M", true)).await;
    let env = expect_envelope(&mut ws_a, MessageKind::RoomCreated).await;
    let code = serde_json::from_value::<RoomCreatedPayload>(env.payload)
        .unwrap()
        .room
        .code;
    send_envelope(&mut ws_b, &room_join_envelope(b.token, &code, "B")).await;
    let _ = expect_envelope(&mut ws_b, MessageKind::RoomJoined).await;
    let _ = expect_envelope(&mut ws_a, MessageKind::ParticipantJoined).await;

    // A's transport drops.
    drop(ws_a);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let env_b_hd = expect_envelope(&mut ws_b, MessageKind::HostDisconnected).await;
    let hd: HostDisconnectedPayload = serde_json::from_value(env_b_hd.payload).unwrap();
    assert_eq!(hd.previous_host_user_id, a.user_id);
    assert!(hd.new_host_user_id.is_none());
    assert!(hd.reconnect_deadline_ms > 0);

    // A reconnects within the 200ms grace. The server's
    // `handle_auth` calls `state.rooms.rejoin` for every
    // successful authentication, which restores the host
    // and publishes `HOST_RECONNECTED` to the room's
    // broadcast channel (and directly to A2's WS).
    let mut ws_a2 = connect(harness.addr).await;
    let a2 = complete_handshake(&mut ws_a2, &kp_a).await;
    assert_eq!(a2.user_id, a.user_id);
    // B sees HOST_RECONNECTED. The 50ms ticker forwards
    // the broadcast to B's WS; wait up to 1s for it.
    let mut got = None;
    for _ in 0..100 {
        let env = next_envelope(&mut ws_b).await;
        if env.r#type == MessageKind::HostReconnected {
            got = Some(env);
            break;
        }
    }
    let env = got.expect("expected HostReconnected within 1s");
    let hr: HostReconnectedPayload = serde_json::from_value(env.payload).unwrap();
    assert_eq!(hr.host_user_id, a.user_id);
}

// ---------------------------------------------------------------------------
// 5. Host absent past grace: server ends room (v1, no migration).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_absent_past_grace_ends_room() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    send_envelope(&mut ws_a, &room_create_envelope(a.token, "M", true)).await;
    let env = expect_envelope(&mut ws_a, MessageKind::RoomCreated).await;
    let code = serde_json::from_value::<RoomCreatedPayload>(env.payload)
        .unwrap()
        .room
        .code;
    send_envelope(&mut ws_b, &room_join_envelope(b.token, &code, "B")).await;
    let _ = expect_envelope(&mut ws_b, MessageKind::RoomJoined).await;
    let _ = expect_envelope(&mut ws_a, MessageKind::ParticipantJoined).await;
    // Drop A.
    drop(ws_a);
    tokio::time::sleep(Duration::from_millis(20)).await;
    // B sees HOST_DISCONNECTED.
    let env = expect_envelope(&mut ws_b, MessageKind::HostDisconnected).await;
    let _: HostDisconnectedPayload = serde_json::from_value(env.payload).unwrap();
    // The 50ms background ticker advances the MockClock and
    // calls tick_grace; v1 ends the room once the 200ms grace
    // elapses. Wait up to 1s for B to receive ROOM_CLOSED.
    let mut got = None;
    for _ in 0..100 {
        let env = next_envelope(&mut ws_b).await;
        match env.r#type {
            MessageKind::RoomClosed => {
                got = Some(env);
                break;
            }
            MessageKind::RoomState
            | MessageKind::ParticipantLeft
            | MessageKind::ParticipantJoined
            | MessageKind::Presence
            | MessageKind::Other(_) => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
    let env = got.expect("expected RoomClosed within 1s");
    let closed: RoomClosedPayload = serde_json::from_value(env.payload).unwrap();
    assert_eq!(closed.reason, "host_disconnected_no_migration");
}

// ---------------------------------------------------------------------------
// 5a. Host disconnect grace expiry via MockClock advance.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_disconnect_grace_expiry_via_mock_clock_advance() {
    let mut config = test_config();
    config.host_disconnect_grace_ms = 30_000;
    let harness = spawn_test_server_with_config(config).await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;

    send_envelope(&mut ws_a, &room_create_envelope(a.token, "M", true)).await;
    let env = expect_envelope(&mut ws_a, MessageKind::RoomCreated).await;
    let code = serde_json::from_value::<RoomCreatedPayload>(env.payload)
        .unwrap()
        .room
        .code;

    send_envelope(&mut ws_b, &room_join_envelope(b.token, &code, "B")).await;
    let _ = expect_envelope(&mut ws_b, MessageKind::RoomJoined).await;
    let _ = expect_envelope(&mut ws_a, MessageKind::ParticipantJoined).await;

    drop(ws_a);
    tokio::time::sleep(Duration::from_millis(20)).await;

    let env = expect_envelope(&mut ws_b, MessageKind::HostDisconnected).await;
    let _: HostDisconnectedPayload = serde_json::from_value(env.payload).unwrap();

    let store: Arc<dyn locast_server::rooms::RoomStore> =
        Arc::new(locast_server::rooms::DbRoomStore::new(harness.db.clone()));

    harness.clock.advance(30_001);
    let _events = harness
        .rooms
        .tick_grace(store.as_ref(), harness.clock.now_ms())
        .await;

    let env = expect_envelope(&mut ws_b, MessageKind::RoomClosed).await;
    let closed: RoomClosedPayload = serde_json::from_value(env.payload).unwrap();
    assert_eq!(closed.reason, "host_disconnected_no_migration");

    drop(ws_b);
    drop(harness);
}

// ---------------------------------------------------------------------------
// 6. Old host returns after migration -> joins as a viewer.
//    Skipped here; covered by the registry unit test which exercises the
//    in-memory path deterministically.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 7. Intentional leave with migration ON: immediate handoff.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intentional_leave_migration_on_immediate_handoff() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    send_envelope(&mut ws_a, &room_create_envelope(a.token, "M", true)).await;
    let env = expect_envelope(&mut ws_a, MessageKind::RoomCreated).await;
    let code = serde_json::from_value::<RoomCreatedPayload>(env.payload)
        .unwrap()
        .room
        .code;
    send_envelope(&mut ws_b, &room_join_envelope(b.token, &code, "B")).await;
    let _ = expect_envelope(&mut ws_b, MessageKind::RoomJoined).await;
    let _ = expect_envelope(&mut ws_a, MessageKind::ParticipantJoined).await;
    // A intentionally leaves.
    send_envelope(&mut ws_a, &room_leave_envelope(a.token)).await;
    // B should see HOST_MIGRATED with no grace, then A's LEFT.
    let env = expect_envelope(&mut ws_b, MessageKind::HostMigrated).await;
    let hm: HostMigratedPayload = serde_json::from_value(env.payload).unwrap();
    assert_eq!(hm.previous_host_user_id, a.user_id);
    assert_eq!(hm.new_host_user_id, b.user_id);
    let _env = expect_envelope(&mut ws_b, MessageKind::ParticipantLeft).await;
}

// ---------------------------------------------------------------------------
// 8. Intentional leave with migration OFF: room ends.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intentional_leave_migration_off_ends_room() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    send_envelope(&mut ws_a, &room_create_envelope(a.token, "M", false)).await;
    let env = expect_envelope(&mut ws_a, MessageKind::RoomCreated).await;
    let code = serde_json::from_value::<RoomCreatedPayload>(env.payload)
        .unwrap()
        .room
        .code;
    send_envelope(&mut ws_b, &room_join_envelope(b.token, &code, "B")).await;
    let _ = expect_envelope(&mut ws_b, MessageKind::RoomJoined).await;
    let _ = expect_envelope(&mut ws_a, MessageKind::ParticipantJoined).await;
    send_envelope(&mut ws_a, &room_leave_envelope(a.token)).await;
    let env = expect_envelope(&mut ws_b, MessageKind::RoomClosed).await;
    let closed: RoomClosedPayload = serde_json::from_value(env.payload).unwrap();
    assert_eq!(closed.reason, "host_left");
    let _ = expect_envelope(&mut ws_b, MessageKind::ParticipantLeft).await;
}

// ---------------------------------------------------------------------------
// 10. Unauth create rejected (no bearer).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauth_create_rejected() {
    let harness = spawn_test_server().await;
    let (kp, _) = fresh_keypair();
    let mut ws = connect(harness.addr).await;
    let _ = complete_handshake(&mut ws, &kp).await;
    // Send ROOM_CREATE without a bearer in the payload.
    let env = Envelope {
        v: 1,
        r#type: MessageKind::RoomCreate,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload: json!({
            "title": "T",
            "migration_enabled": false,
        }),
    };
    ws.send(Message::Binary(encode(&env))).await.expect("send");
    // The server should close.
    let next = tokio::time::timeout(Duration::from_secs(1), read_binary(&mut ws)).await;
    assert!(next.is_err() || next.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// 11. Viewer cannot claim host. We don't have a host-only message in v1
//     beyond ROOM_LEAVE. A viewer's ROOM_LEAVE should NOT trigger host
//     handoff or room end. Assert it just removes the viewer.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn viewer_leave_does_not_end_room() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    send_envelope(&mut ws_a, &room_create_envelope(a.token, "M", true)).await;
    let env = expect_envelope(&mut ws_a, MessageKind::RoomCreated).await;
    let code = serde_json::from_value::<RoomCreatedPayload>(env.payload)
        .unwrap()
        .room
        .code;
    send_envelope(&mut ws_b, &room_join_envelope(b.token, &code, "B")).await;
    let _ = expect_envelope(&mut ws_b, MessageKind::RoomJoined).await;
    let _ = expect_envelope(&mut ws_a, MessageKind::ParticipantJoined).await;
    // B (a viewer) sends ROOM_LEAVE.
    send_envelope(&mut ws_b, &room_leave_envelope(b.token)).await;
    // A receives PARTICIPANT_LEFT, NOT host-migrated/closed.
    let env = expect_envelope(&mut ws_a, MessageKind::ParticipantLeft).await;
    let pl: ParticipantLeftPayload = serde_json::from_value(env.payload).unwrap();
    assert_eq!(pl.user_id, b.user_id);
    assert_eq!(pl.reason, "leave");
    // B's socket is closed (room still alive for A).
    let _ = b.user_id;
}

// ---------------------------------------------------------------------------
// 13. Malformed room message (invalid code characters).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_room_message_invalid_code() {
    let harness = spawn_test_server().await;
    let (kp, _) = fresh_keypair();
    let mut ws = connect(harness.addr).await;
    let ac = complete_handshake(&mut ws, &kp).await;
    // Code "BAD0" contains '0' which is not in alphabet.
    send_envelope(&mut ws, &room_join_envelope(ac.token, "BAD0", "X")).await;
    let env = expect_envelope(&mut ws, MessageKind::RoomError).await;
    let p: RoomErrorPayload = serde_json::from_value(env.payload).unwrap();
    assert_eq!(p.code, RoomErrorCode::InvalidCode);
}

// ---------------------------------------------------------------------------
// 14. Unknown room.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_room_returns_not_found() {
    let harness = spawn_test_server().await;
    let (kp, _) = fresh_keypair();
    let mut ws = connect(harness.addr).await;
    let ac = complete_handshake(&mut ws, &kp).await;
    send_envelope(&mut ws, &room_join_envelope(ac.token, "ZZZZZZ", "X")).await;
    let env = expect_envelope(&mut ws, MessageKind::RoomError).await;
    let p: RoomErrorPayload = serde_json::from_value(env.payload).unwrap();
    assert_eq!(p.code, RoomErrorCode::RoomNotFound);
}

// ---------------------------------------------------------------------------
// 15. Invalid lifecycle: ROOM_LEAVE when not joined.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leave_when_not_joined_returns_not_joined() {
    let harness = spawn_test_server().await;
    let (kp, _) = fresh_keypair();
    let mut ws = connect(harness.addr).await;
    let ac = complete_handshake(&mut ws, &kp).await;
    send_envelope(&mut ws, &room_leave_envelope(ac.token)).await;
    let env = expect_envelope(&mut ws, MessageKind::RoomError).await;
    let p: RoomErrorPayload = serde_json::from_value(env.payload).unwrap();
    assert_eq!(p.code, RoomErrorCode::NotJoined);
}

// ---------------------------------------------------------------------------
// 16. Duplicate join.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_join_returns_already_joined() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    send_envelope(&mut ws_a, &room_create_envelope(a.token, "M", false)).await;
    let env = expect_envelope(&mut ws_a, MessageKind::RoomCreated).await;
    let code = serde_json::from_value::<RoomCreatedPayload>(env.payload)
        .unwrap()
        .room
        .code;
    send_envelope(&mut ws_b, &room_join_envelope(b.token, &code, "B")).await;
    let _ = expect_envelope(&mut ws_b, MessageKind::RoomJoined).await;
    let _ = expect_envelope(&mut ws_a, MessageKind::ParticipantJoined).await;
    // B tries to join again.
    send_envelope(&mut ws_b, &room_join_envelope(b.token, &code, "B")).await;
    let env = expect_envelope(&mut ws_b, MessageKind::RoomError).await;
    let p: RoomErrorPayload = serde_json::from_value(env.payload).unwrap();
    assert_eq!(p.code, RoomErrorCode::AlreadyJoined);
}

// ---------------------------------------------------------------------------
// 19. Concurrent creates: A and B both create rooms; codes differ.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_creates_distinct_codes() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    send_envelope(&mut ws_a, &room_create_envelope(a.token, "A", false)).await;
    send_envelope(&mut ws_b, &room_create_envelope(b.token, "B", false)).await;
    let env_a = expect_envelope(&mut ws_a, MessageKind::RoomCreated).await;
    let env_b = expect_envelope(&mut ws_b, MessageKind::RoomCreated).await;
    let code_a: String = serde_json::from_value::<RoomCreatedPayload>(env_a.payload)
        .unwrap()
        .room
        .code;
    let code_b: String = serde_json::from_value::<RoomCreatedPayload>(env_b.payload)
        .unwrap()
        .room
        .code;
    assert_ne!(code_a, code_b);
}

// ---------------------------------------------------------------------------
// 17. Multiple rooms in parallel: A creates room 1, B creates room 2,
//     A joins room 2, B joins room 1. A is host of 1 and viewer in 2.
//
//     Covers two rooms alive at the same time with a cross-join (each
//     user hosts one room and views the other). The final step (A
//     receiving ParticipantJoined for B joining R1 while A is also a
//     member of R2) relies on the server's current single-room
//     subscription per connection, which sticks to the first room the
//     user joined (R1 for A). Delivery for users in several rooms at
//     once is unspecified by the architecture, so this step encodes
//     current behaviour, not a documented guarantee. The room_id
//     checks below make a future change in that rule fail loudly.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_rooms_in_parallel() {
    const BUDGET: Duration = Duration::from_secs(30);
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    send_envelope(&mut ws_a, &room_create_envelope(a.token, "R1", false)).await;
    let env_a1 =
        expect_envelope_verbose(&mut ws_a, MessageKind::RoomCreated, BUDGET, "A create R1").await;
    let r1: RoomSummary = serde_json::from_value::<RoomCreatedPayload>(env_a1.payload)
        .unwrap()
        .room;
    let r1_id = r1.id;
    send_envelope(&mut ws_b, &room_create_envelope(b.token, "R2", false)).await;
    let env_b1 =
        expect_envelope_verbose(&mut ws_b, MessageKind::RoomCreated, BUDGET, "B create R2").await;
    let r2: RoomSummary = serde_json::from_value::<RoomCreatedPayload>(env_b1.payload)
        .unwrap()
        .room;
    let r2_id = r2.id;
    assert_ne!(r1_id, r2_id);
    // A joins R2.
    send_envelope(&mut ws_a, &room_join_envelope(a.token, &r2.code, "A")).await;
    let env = expect_envelope_verbose(
        &mut ws_a,
        MessageKind::RoomJoined,
        BUDGET,
        "A join R2 reply",
    )
    .await;
    let joined: RoomJoinedPayload = serde_json::from_value(env.payload.clone()).unwrap();
    assert_eq!(
        joined.room.id, r2_id,
        "A join R2 reply: wrong room (room_id={:?}, payload={})",
        env.room_id, env.payload
    );
    let env = expect_envelope_verbose(
        &mut ws_b,
        MessageKind::ParticipantJoined,
        BUDGET,
        "B told A joined R2",
    )
    .await;
    assert_eq!(
        env.room_id,
        Some(r2_id),
        "B told A joined R2: wrong room_id (payload={})",
        env.payload
    );
    let pj: ParticipantJoinedPayload = serde_json::from_value(env.payload.clone()).unwrap();
    assert_eq!(
        pj.participant.user_id, a.user_id,
        "B told A joined R2: wrong participant (room_id={:?}, payload={})",
        env.room_id, env.payload
    );
    // B joins R1.
    send_envelope(&mut ws_b, &room_join_envelope(b.token, &r1.code, "B")).await;
    let env = expect_envelope_verbose(
        &mut ws_b,
        MessageKind::RoomJoined,
        BUDGET,
        "B join R1 reply",
    )
    .await;
    let joined: RoomJoinedPayload = serde_json::from_value(env.payload.clone()).unwrap();
    assert_eq!(
        joined.room.id, r1_id,
        "B join R1 reply: wrong room (room_id={:?}, payload={})",
        env.room_id, env.payload
    );
    // Relies on the sticky first-room subscription (see header).
    let env = expect_envelope_verbose(
        &mut ws_a,
        MessageKind::ParticipantJoined,
        BUDGET,
        "A told B joined R1",
    )
    .await;
    assert_eq!(
        env.room_id,
        Some(r1_id),
        "A told B joined R1: wrong room_id (payload={})",
        env.payload
    );
    let pj: ParticipantJoinedPayload = serde_json::from_value(env.payload.clone()).unwrap();
    assert_eq!(
        pj.participant.user_id, b.user_id,
        "A told B joined R1: wrong participant (room_id={:?}, payload={})",
        env.room_id, env.payload
    );
}

// ---------------------------------------------------------------------------
// Regression: a room creator must always be told about a joiner that joins
// immediately after RoomCreated (no lost broadcast before the creator's
// forwarder subscribes). The 6 s per-frame budget is generous enough for a
// loaded CI runner yet well under the 15 s presence timeout, so a lost event
// fails fast with a clear message instead of surfacing as the presence-timeout
// ParticipantLeft.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_creator_always_sees_immediate_joiner_announced() {
    const FRAME_BUDGET: Duration = Duration::from_secs(6);
    let harness = spawn_test_server().await;
    for iter in 0..100usize {
        let (kp_a, _) = fresh_keypair();
        let (kp_b, _) = fresh_keypair();
        let mut ws_a = connect(harness.addr).await;
        let mut ws_b = connect(harness.addr).await;
        let a = complete_handshake(&mut ws_a, &kp_a).await;
        let b = complete_handshake(&mut ws_b, &kp_b).await;
        send_envelope(&mut ws_b, &room_create_envelope(b.token, "R", false)).await;
        let env_b = expect_envelope(&mut ws_b, MessageKind::RoomCreated).await;
        let room: RoomSummary = serde_json::from_value::<RoomCreatedPayload>(env_b.payload)
            .unwrap()
            .room;
        // Immediately (no sleep) join from the already-authenticated A.
        send_envelope(&mut ws_a, &room_join_envelope(a.token, &room.code, "A")).await;
        let _ = expect_envelope(&mut ws_a, MessageKind::RoomJoined).await;
        let ctx = format!("iteration {iter}: creator waiting for ParticipantJoined");
        let env = expect_envelope_verbose(
            &mut ws_b,
            MessageKind::ParticipantJoined,
            FRAME_BUDGET,
            &ctx,
        )
        .await;
        assert_eq!(
            env.room_id,
            Some(room.id),
            "iteration {iter}: ParticipantJoined for wrong room (expected {}, payload={})",
            room.id,
            env.payload
        );
        let pj: ParticipantJoinedPayload = serde_json::from_value(env.payload.clone())
            .unwrap_or_else(|e| {
                panic!(
                    "iteration {iter}: bad ParticipantJoined payload ({e}): room_id={:?}, payload={}",
                    env.room_id, env.payload
                )
            });
        assert_eq!(
            pj.participant.user_id, a.user_id,
            "iteration {iter}: ParticipantJoined names the wrong user (room_id={:?}, payload={})",
            env.room_id, env.payload
        );
        // Close both sockets with a close frame so the server tears the
        // connections down promptly instead of waiting on a dead TCP peer.
        let _ = ws_a.close(None).await;
        let _ = ws_b.close(None).await;
    }
}

// ---------------------------------------------------------------------------
// 21. Stale participant cleanup after DISCONNECTED (P7-T02).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_participant_removed_after_disconnect_timeout() {
    let mut config = test_config();
    config.participant_disconnect_after_ms = 200;
    config.participant_stale_after_ms = 500;
    let harness = spawn_test_server_with_config(config).await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;

    send_envelope(&mut ws_a, &room_create_envelope(a.token, "M", false)).await;
    let env = expect_envelope(&mut ws_a, MessageKind::RoomCreated).await;
    let created: RoomCreatedPayload = serde_json::from_value(env.payload).unwrap();
    let room_id = created.room.id;

    send_envelope(
        &mut ws_b,
        &room_join_envelope(b.token, &created.room.code, "B"),
    )
    .await;
    let _ = expect_envelope(&mut ws_b, MessageKind::RoomJoined).await;
    let _ = expect_envelope(&mut ws_a, MessageKind::ParticipantJoined).await;

    drop(ws_b);
    tokio::time::sleep(Duration::from_millis(20)).await;

    let store: Arc<dyn locast_server::rooms::RoomStore> =
        Arc::new(locast_server::rooms::DbRoomStore::new(harness.db.clone()));
    let now = harness.clock.now_ms();
    harness.rooms.tick_presence_timeout(now).await;
    harness.clock.advance(600);
    let now = harness.clock.now_ms();
    harness
        .rooms
        .tick_stale_participants(store.as_ref(), now)
        .await;

    let handle = harness.rooms.get_by_id(room_id).await.expect("room exists");
    let state = handle.read().await;
    assert!(
        !state.participants.iter().any(|p| p.user_id == b.user_id),
        "stale participant should be removed from in-memory state"
    );

    let rows = harness
        .db
        .list_room_participants(room_id)
        .await
        .expect("list participants");
    assert!(
        !rows.iter().any(|r| r.user_id == b.user_id),
        "stale participant row should be deleted from DB"
    );

    drop(ws_a);
    drop(harness);
}

// ---------------------------------------------------------------------------
// Helper: extract RoomSummary (asserts the room_id matches for sanity).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// P4-T01: PLAYBACK_CMD validation + ordering over the real WebSocket path.
// ---------------------------------------------------------------------------

/// A room-scoped envelope with the bearer merged into the payload, the
/// same shape the client sends after the handshake.
fn room_scoped_envelope(
    token: [u8; 32],
    kind: MessageKind,
    room_id: Uuid,
    inner: serde_json::Value,
) -> Envelope {
    let mut payload = json!({ "bearer": token.to_vec() });
    let obj = payload.as_object_mut().unwrap();
    for (k, v) in inner.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    Envelope {
        v: 1,
        r#type: kind,
        id: Uuid::now_v7(),
        room_id: Some(room_id),
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload,
    }
}

fn playback_cmd(
    token: [u8; 32],
    room_id: Uuid,
    action: PlaybackAction,
    monotonic_seq: u64,
    media_position_ms: u64,
) -> Envelope {
    room_scoped_envelope(
        token,
        MessageKind::PlaybackCmd,
        room_id,
        serde_json::to_value(PlaybackCommandPayload {
            action,
            monotonic_seq,
            media_position_ms,
            // A client clock unrelated to the server's.
            client_ts_ms: 7,
        })
        .unwrap(),
    )
}

/// Read until an envelope of `kind` arrives, skipping room chatter
/// (PARTICIPANT_JOINED, ROOM_STATE, CAPABILITY_UPDATE, ...).
async fn next_of_kind(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    kind: MessageKind,
    ctx: &str,
) -> Envelope {
    loop {
        let bytes = tokio::time::timeout(Duration::from_secs(30), read_binary(ws))
            .await
            .unwrap_or_else(|_| panic!("{ctx}: no {kind:?} within 30 s"))
            .unwrap_or_else(|| panic!("{ctx}: connection closed waiting for {kind:?}"));
        let env = decode(&bytes);
        if env.r#type == kind {
            return env;
        }
        assert_ne!(
            env.r#type,
            MessageKind::PlaybackCmd,
            "{ctx}: unexpected PLAYBACK_CMD while waiting for {kind:?}: {}",
            env.payload
        );
    }
}

async fn next_playback(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    ctx: &str,
) -> PlaybackAcceptedEvent {
    let env = next_of_kind(ws, MessageKind::PlaybackCmd, ctx).await;
    serde_json::from_value(env.payload).expect("PLAYBACK_CMD payload")
}

/// No frame of `kind` within `window` (other kinds are ignored).
async fn assert_no_kind_within(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    kind: MessageKind,
    window: Duration,
    ctx: &str,
) {
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Some(bytes)) = tokio::time::timeout_at(deadline, read_binary(ws)).await {
        let env = decode(&bytes);
        assert_ne!(
            env.r#type, kind,
            "{ctx}: unexpected {kind:?}: {}",
            env.payload
        );
    }
}

/// Host A creates a room; B and C join. Returns the room id.
async fn playback_room(
    ws_a: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    ws_b: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    ws_c: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    a: &AuthedClient,
    b: &AuthedClient,
    c: &AuthedClient,
) -> Uuid {
    send_envelope(ws_a, &room_create_envelope(a.token, "Playback", false)).await;
    let created: RoomCreatedPayload = serde_json::from_value(
        expect_envelope(ws_a, MessageKind::RoomCreated)
            .await
            .payload,
    )
    .unwrap();
    let code = created.room.code.clone();
    send_envelope(ws_b, &room_join_envelope(b.token, &code, "B")).await;
    next_of_kind(ws_b, MessageKind::RoomJoined, "B join").await;
    send_envelope(ws_c, &room_join_envelope(c.token, &code, "C")).await;
    next_of_kind(ws_c, MessageKind::RoomJoined, "C join").await;
    // Everyone has seen C arrive before playback starts.
    for (ws, who) in [(&mut *ws_a, "A"), (&mut *ws_b, "B")] {
        loop {
            let env = next_of_kind(ws, MessageKind::ParticipantJoined, who).await;
            let pj: ParticipantJoinedPayload = serde_json::from_value(env.payload).unwrap();
            if pj.participant.user_id == c.user_id {
                break;
            }
        }
    }
    created.room.id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn playback_non_host_is_forbidden_and_host_commands_broadcast_in_order() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let (kp_c, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let mut ws_c = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    let c = complete_handshake(&mut ws_c, &kp_c).await;
    let room_id = playback_room(&mut ws_a, &mut ws_b, &mut ws_c, &a, &b, &c).await;

    // 1-3. Viewer B: PLAY, PAUSE, SEEK are each refused to B alone.
    for (i, action) in [
        PlaybackAction::Play,
        PlaybackAction::Pause,
        PlaybackAction::Seek,
    ]
    .into_iter()
    .enumerate()
    {
        send_envelope(
            &mut ws_b,
            &playback_cmd(b.token, room_id, action, i as u64 + 1, 1_000),
        )
        .await;
        let err = next_of_kind(&mut ws_b, MessageKind::RoomError, "viewer refusal").await;
        let p: RoomErrorPayload = serde_json::from_value(err.payload).unwrap();
        assert_eq!(
            p.code,
            RoomErrorCode::NotHost,
            "{action:?} must be forbidden"
        );
    }
    // Nothing was relayed to the host or the other viewer.
    assert_no_kind_within(
        &mut ws_a,
        MessageKind::PlaybackCmd,
        Duration::from_millis(300),
        "A",
    )
    .await;
    assert_no_kind_within(
        &mut ws_c,
        MessageKind::PlaybackCmd,
        Duration::from_millis(300),
        "C",
    )
    .await;

    // 4-6. Host A: four commands sent back to back are accepted and
    // broadcast to every other participant in server_seq order. The
    // refusals above consumed no server_seq.
    let sent = [
        (PlaybackAction::Play, 0u64),
        (PlaybackAction::Pause, 2_000),
        (PlaybackAction::Seek, 9_000),
        (PlaybackAction::Play, 9_000),
    ];
    // The test ticker keeps the server's MockClock on wall time, so
    // server_ts is checked against a window, not an exact value.
    let before = harness.clock.now_ms();
    for (i, (action, pos)) in sent.iter().enumerate() {
        send_envelope(
            &mut ws_a,
            &playback_cmd(a.token, room_id, *action, i as u64 + 1, *pos),
        )
        .await;
    }
    for (ws, who) in [(&mut ws_b, "B"), (&mut ws_c, "C")] {
        let mut last_ts = before;
        for (i, (action, pos)) in sent.iter().enumerate() {
            let evt = next_playback(ws, who).await;
            assert_eq!(evt.server_seq, i as u64 + 1, "{who}: server_seq order");
            assert_eq!(evt.monotonic_seq, i as u64 + 1, "{who}: send order");
            assert_eq!(evt.action, *action, "{who}: action");
            assert_eq!(evt.media_position_ms, *pos, "{who}: position");
            assert_eq!(evt.sender_id, a.user_id, "{who}: sender");
            // 7. server_ts is the server's clock at acceptance (not the
            // client's), and never goes backwards along server_seq.
            assert!(
                evt.server_ts_ms >= last_ts && evt.server_ts_ms <= harness.clock.now_ms(),
                "{who}: server_ts {} outside [{last_ts}, now]",
                evt.server_ts_ms
            );
            last_ts = evt.server_ts_ms;
            assert_eq!(evt.client_ts_ms, 7, "{who}: client_ts echoed");
        }
    }
    // The host gets no error and no echo of its own commands.
    assert_no_kind_within(
        &mut ws_a,
        MessageKind::PlaybackCmd,
        Duration::from_millis(300),
        "A",
    )
    .await;
    drop(harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn playback_cohost_follows_the_permission_set_capability() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let (kp_c, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let mut ws_c = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    let c = complete_handshake(&mut ws_c, &kp_c).await;
    let room_id = playback_room(&mut ws_a, &mut ws_b, &mut ws_c, &a, &b, &c).await;

    // The host delegates playback control to B. This is the registry
    // call the existing P6-T02 PERMISSION_SET handler makes; the
    // envelope itself is not routed by the WS layer yet (separate
    // finding), so the grant is applied directly.
    let set_caps = |bits: u32| {
        let rooms = harness.rooms.clone();
        let now = harness.clock.now_ms();
        let target = b.user_id;
        async move {
            rooms
                .update_participant_cap_set(room_id, target, bits, now)
                .await
                .expect("update cap_set")
        }
    };
    set_caps(cap::CHAT | cap::PLAYBACK_CONTROL).await;

    let before = harness.clock.now_ms();
    send_envelope(
        &mut ws_a,
        &playback_cmd(a.token, room_id, PlaybackAction::Play, 1, 0),
    )
    .await;
    let first = next_playback(&mut ws_c, "C").await;
    assert_eq!((first.server_seq, first.sender_id), (1, a.user_id));
    assert!(first.server_ts_ms >= before && first.server_ts_ms <= harness.clock.now_ms());
    // B is a participant too and sees the host's command.
    assert_eq!(next_playback(&mut ws_b, "B").await.server_seq, 1);

    // B's own monotonic_seq starts at 1; the room sequence continues.
    send_envelope(
        &mut ws_b,
        &playback_cmd(b.token, room_id, PlaybackAction::Pause, 1, 3_000),
    )
    .await;
    let second = next_playback(&mut ws_c, "C").await;
    assert_eq!((second.server_seq, second.sender_id), (2, b.user_id));
    assert_eq!(second.action, PlaybackAction::Pause);
    // The host sees the co-host's command too (B is the originator).
    let host_view = next_playback(&mut ws_a, "A").await;
    assert_eq!((host_view.server_seq, host_view.sender_id), (2, b.user_id));

    // Revoking the capability makes B a plain viewer again.
    set_caps(cap::CHAT).await;
    send_envelope(
        &mut ws_b,
        &playback_cmd(b.token, room_id, PlaybackAction::Play, 2, 3_000),
    )
    .await;
    let err = next_of_kind(&mut ws_b, MessageKind::RoomError, "revoked B").await;
    let p: RoomErrorPayload = serde_json::from_value(err.payload).unwrap();
    assert_eq!(p.code, RoomErrorCode::NotHost);
    // Nothing from B was relayed: C's next command is the host's, at seq 3.
    send_envelope(
        &mut ws_a,
        &playback_cmd(a.token, room_id, PlaybackAction::Play, 2, 3_000),
    )
    .await;
    let third = next_playback(&mut ws_c, "C").await;
    assert_eq!((third.server_seq, third.sender_id), (3, a.user_id));
    drop(harness);
}

// ---------------------------------------------------------------------------
// Room message routing (CHAT_MESSAGE, DRAW_*, PERMISSION_SET) and room-scoped
// authorization, over the real WebSocket path.
// ---------------------------------------------------------------------------

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn chat_envelope(token: [u8; 32], room_id: Uuid, sender: Uuid, text: &str) -> Envelope {
    room_scoped_envelope(
        token,
        MessageKind::ChatMessage,
        room_id,
        json!({ "sender_id": sender, "text": text, "sent_ms": 0 }),
    )
}

fn permission_envelope(token: [u8; 32], room_id: Uuid, target: Uuid, add: u32) -> Envelope {
    room_scoped_envelope(
        token,
        MessageKind::PermissionSet,
        room_id,
        serde_json::to_value(PermissionSetPayload {
            target_user_id: target,
            add_cap_set: add,
            remove_cap_set: 0,
        })
        .unwrap(),
    )
}

/// A DRAW_BEGIN signed the way the client signs it.
fn draw_begin_envelope(
    token: [u8; 32],
    room_id: Uuid,
    kp: &SigningKey,
    user_id: Uuid,
    stroke_id: Uuid,
) -> Envelope {
    let payload = StrokeBeginPayload {
        stroke_id,
        tool: StrokeTool::Pen,
        color: "#ff0000".into(),
        width: 2.0,
        x: 0.25,
        y: 0.5,
        pressure: 0.5,
        ts_ms: 1,
    };
    let signed = locast_crypto::drawing_signed_bytes(&payload).expect("signed bytes");
    let mut env = room_scoped_envelope(
        token,
        MessageKind::StrokeBegin,
        room_id,
        serde_json::to_value(&payload).unwrap(),
    );
    env.sender = Some(Sender {
        user_id,
        pubkey: kp.verifying_key().to_bytes().to_vec(),
        sig: kp.sign(&signed).to_bytes().to_vec(),
    });
    env
}

fn signed_manifest_envelope(token: [u8; 32], room_id: Uuid, kp: &SigningKey) -> Envelope {
    let manifest = locast_manifest::sign_manifest(
        &kp.to_bytes(),
        &locast_manifest::MediaManifest {
            manifest_version: 1,
            room_id: room_id.to_string(),
            media: vec![],
            subtitles: vec![],
            created_at: 1,
            host_signature: None,
        },
    )
    .expect("sign manifest");
    room_scoped_envelope(
        token,
        MessageKind::ManifestPublish,
        room_id,
        json!({ "manifest": manifest }),
    )
}

async fn expect_room_error(ws: &mut Ws, code: RoomErrorCode, ctx: &str) {
    let env = next_of_kind(ws, MessageKind::RoomError, ctx).await;
    let p: RoomErrorPayload = serde_json::from_value(env.payload).unwrap();
    assert_eq!(p.code, code, "{ctx}: {}", p.message);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_permission_and_drawing_messages_reach_their_handlers() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let (kp_c, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let mut ws_c = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    let c = complete_handshake(&mut ws_c, &kp_c).await;
    let room_id = playback_room(&mut ws_a, &mut ws_b, &mut ws_c, &a, &b, &c).await;

    // CHAT_MESSAGE: a viewer chats (CHAT is a default capability);
    // the other participants get it attributed to the bearer.
    send_envelope(
        &mut ws_b,
        &chat_envelope(b.token, room_id, c.user_id, "hello room"),
    )
    .await;
    for (ws, who) in [(&mut ws_a, "A"), (&mut ws_c, "C")] {
        let env = next_of_kind(ws, MessageKind::ChatMessage, who).await;
        assert_eq!(env.room_id, Some(room_id), "{who}");
        assert_eq!(env.payload["text"], "hello room", "{who}");
        assert_eq!(
            env.payload["sender_id"],
            json!(b.user_id),
            "{who}: sender comes from the bearer, not the payload"
        );
    }

    // DRAW_BEGIN before any grant: the handler is reached and the
    // capability gate refuses it (viewers do not have DRAW).
    let stroke = Uuid::now_v7();
    send_envelope(
        &mut ws_b,
        &draw_begin_envelope(b.token, room_id, &kp_b, b.user_id, stroke),
    )
    .await;
    expect_room_error(&mut ws_b, RoomErrorCode::NotHost, "B draws without DRAW").await;

    // PERMISSION_SET from a viewer is refused; from the host it is
    // applied and CAPABILITY_UPDATE reaches everyone.
    send_envelope(
        &mut ws_c,
        &permission_envelope(c.token, room_id, b.user_id, cap::DRAW),
    )
    .await;
    expect_room_error(&mut ws_c, RoomErrorCode::NotHost, "viewer PERMISSION_SET").await;
    send_envelope(
        &mut ws_a,
        &permission_envelope(a.token, room_id, b.user_id, cap::DRAW),
    )
    .await;
    for (ws, who) in [(&mut ws_a, "A"), (&mut ws_b, "B"), (&mut ws_c, "C")] {
        let env = next_of_kind(ws, MessageKind::CapabilityUpdate, who).await;
        assert_eq!(env.payload["target_user_id"], json!(b.user_id), "{who}");
        let bits = env.payload["cap_set"].as_u64().expect("cap_set") as u32;
        assert_ne!(bits & cap::DRAW, 0, "{who}: DRAW granted");
        assert_eq!(
            bits & cap::PLAYBACK_CONTROL,
            0,
            "{who}: nothing else granted"
        );
    }

    // DRAW_BEGIN / DRAW_POINT / DRAW_END now flow to the others.
    send_envelope(
        &mut ws_b,
        &draw_begin_envelope(b.token, room_id, &kp_b, b.user_id, stroke),
    )
    .await;
    let env = next_of_kind(&mut ws_c, MessageKind::StrokeBegin, "C sees B's stroke").await;
    assert_eq!(env.payload["stroke_id"], json!(stroke));
    // The host also holds DRAW, but may not end B's stroke.
    send_envelope(
        &mut ws_a,
        &room_scoped_envelope(
            a.token,
            MessageKind::StrokeEnd,
            room_id,
            serde_json::to_value(StrokeEndPayload {
                stroke_id: stroke,
                ts_ms: 3,
            })
            .unwrap(),
        ),
    )
    .await;
    expect_room_error(&mut ws_a, RoomErrorCode::InvalidState, "A ends B's stroke").await;
    send_envelope(
        &mut ws_b,
        &room_scoped_envelope(
            b.token,
            MessageKind::StrokePoint,
            room_id,
            serde_json::to_value(StrokePointPayload {
                stroke_id: stroke,
                x: 0.3,
                y: 0.6,
                pressure: 0.5,
                ts_ms: 2,
            })
            .unwrap(),
        ),
    )
    .await;
    send_envelope(
        &mut ws_b,
        &room_scoped_envelope(
            b.token,
            MessageKind::StrokeEnd,
            room_id,
            serde_json::to_value(StrokeEndPayload {
                stroke_id: stroke,
                ts_ms: 3,
            })
            .unwrap(),
        ),
    )
    .await;
    // The stroke survived A's attempt: B's point and end go through.
    for (ws, who) in [(&mut ws_a, "A"), (&mut ws_c, "C")] {
        for kind in [MessageKind::StrokePoint, MessageKind::StrokeEnd] {
            let env = next_of_kind(ws, kind.clone(), who).await;
            assert_eq!(env.room_id, Some(room_id), "{who} {kind:?}");
            assert_eq!(env.payload["stroke_id"], json!(stroke), "{who} {kind:?}");
        }
    }
    drop(harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_privileges_in_one_room_do_not_authorize_another_room() {
    let harness = spawn_test_server().await;
    let (kp_x, _) = fresh_keypair();
    let (kp_y, _) = fresh_keypair();
    let (kp_z, _) = fresh_keypair();
    let mut ws_x = connect(harness.addr).await;
    let mut ws_y = connect(harness.addr).await;
    let mut ws_z = connect(harness.addr).await;
    let x = complete_handshake(&mut ws_x, &kp_x).await;
    let y = complete_handshake(&mut ws_y, &kp_y).await;
    let z = complete_handshake(&mut ws_z, &kp_z).await;

    // X hosts several rooms (so the old "pick one of the user's
    // rooms" gate would almost always land on a hosted one) ...
    let mut hosted = Vec::new();
    for i in 0..3 {
        send_envelope(
            &mut ws_x,
            &room_create_envelope(x.token, &format!("X{i}"), false),
        )
        .await;
        let env = next_of_kind(&mut ws_x, MessageKind::RoomCreated, "X create").await;
        let created: RoomCreatedPayload = serde_json::from_value(env.payload).unwrap();
        hosted.push(created.room.id);
    }
    let room_a = hosted[0];
    // ... and is a plain viewer in Y's room B.
    send_envelope(&mut ws_y, &room_create_envelope(y.token, "B", false)).await;
    let env = next_of_kind(&mut ws_y, MessageKind::RoomCreated, "Y create").await;
    let room_b: RoomCreatedPayload = serde_json::from_value(env.payload).unwrap();
    let (room_b, code_b) = (room_b.room.id, room_b.room.code);
    send_envelope(&mut ws_x, &room_join_envelope(x.token, &code_b, "X")).await;
    next_of_kind(&mut ws_x, MessageKind::RoomJoined, "X joins B").await;
    next_of_kind(&mut ws_y, MessageKind::ParticipantJoined, "Y sees X").await;
    // Z hosts room C, which X never joins.
    send_envelope(&mut ws_z, &room_create_envelope(z.token, "C", false)).await;
    let env = next_of_kind(&mut ws_z, MessageKind::RoomCreated, "Z create").await;
    let room_c = serde_json::from_value::<RoomCreatedPayload>(env.payload)
        .unwrap()
        .room
        .id;

    // Every host / capability action X attempts in B is refused,
    // every time, and nothing reaches B.
    for round in 0..3u64 {
        let ctx = format!("round {round}");
        send_envelope(&mut ws_x, &signed_manifest_envelope(x.token, room_b, &kp_x)).await;
        expect_room_error(
            &mut ws_x,
            RoomErrorCode::NotHost,
            &format!("{ctx}: manifest in B"),
        )
        .await;
        send_envelope(
            &mut ws_x,
            &permission_envelope(x.token, room_b, y.user_id, cap::DRAW),
        )
        .await;
        expect_room_error(
            &mut ws_x,
            RoomErrorCode::NotHost,
            &format!("{ctx}: permission in B"),
        )
        .await;
        send_envelope(
            &mut ws_x,
            &playback_cmd(x.token, room_b, PlaybackAction::Play, round + 1, 0),
        )
        .await;
        expect_room_error(
            &mut ws_x,
            RoomErrorCode::NotHost,
            &format!("{ctx}: playback in B"),
        )
        .await;
        send_envelope(
            &mut ws_x,
            &draw_begin_envelope(x.token, room_b, &kp_x, x.user_id, Uuid::now_v7()),
        )
        .await;
        expect_room_error(
            &mut ws_x,
            RoomErrorCode::NotHost,
            &format!("{ctx}: draw in B"),
        )
        .await;
        let stray = Uuid::now_v7();
        for (kind, payload) in [
            (
                MessageKind::StrokePoint,
                serde_json::to_value(StrokePointPayload {
                    stroke_id: stray,
                    x: 0.1,
                    y: 0.1,
                    pressure: 0.0,
                    ts_ms: 1,
                })
                .unwrap(),
            ),
            (
                MessageKind::StrokeEnd,
                serde_json::to_value(StrokeEndPayload {
                    stroke_id: stray,
                    ts_ms: 1,
                })
                .unwrap(),
            ),
        ] {
            send_envelope(
                &mut ws_x,
                &room_scoped_envelope(x.token, kind.clone(), room_b, payload),
            )
            .await;
            expect_room_error(
                &mut ws_x,
                RoomErrorCode::NotHost,
                &format!("{ctx}: {kind:?} in B"),
            )
            .await;
        }
    }
    for kind in [
        MessageKind::ManifestPublished,
        MessageKind::CapabilityUpdate,
        MessageKind::PlaybackCmd,
        MessageKind::StrokeBegin,
    ] {
        assert_no_kind_within(
            &mut ws_y,
            kind.clone(),
            Duration::from_millis(200),
            "B host",
        )
        .await;
    }

    // What X may do as a viewer of B still works: chat.
    send_envelope(
        &mut ws_x,
        &chat_envelope(x.token, room_b, x.user_id, "hi B"),
    )
    .await;
    let env = next_of_kind(&mut ws_y, MessageKind::ChatMessage, "Y gets X's chat").await;
    assert_eq!(env.room_id, Some(room_b));

    // The same privileged actions succeed in a room X hosts.
    send_envelope(&mut ws_x, &signed_manifest_envelope(x.token, room_a, &kp_x)).await;
    let env = next_of_kind(&mut ws_x, MessageKind::ManifestPublished, "manifest in A").await;
    assert_eq!(env.room_id, Some(room_a));
    send_envelope(
        &mut ws_x,
        &playback_cmd(x.token, room_a, PlaybackAction::Play, 1, 0),
    )
    .await;
    send_envelope(
        &mut ws_x,
        &draw_begin_envelope(x.token, room_a, &kp_x, x.user_id, Uuid::now_v7()),
    )
    .await;
    assert_no_kind_within(
        &mut ws_x,
        MessageKind::RoomError,
        Duration::from_millis(300),
        "A",
    )
    .await;
    // B's own host keeps full control of B.
    send_envelope(
        &mut ws_y,
        &playback_cmd(y.token, room_b, PlaybackAction::Play, 1, 0),
    )
    .await;
    assert_no_kind_within(
        &mut ws_y,
        MessageKind::RoomError,
        Duration::from_millis(300),
        "B",
    )
    .await;

    // A room X is not in at all: nothing X sends reaches it (the WS
    // layer's membership check drops it before dispatch).
    send_envelope(
        &mut ws_x,
        &chat_envelope(x.token, room_c, x.user_id, "intruder"),
    )
    .await;
    send_envelope(&mut ws_x, &signed_manifest_envelope(x.token, room_c, &kp_x)).await;
    for kind in [MessageKind::ChatMessage, MessageKind::ManifestPublished] {
        assert_no_kind_within(
            &mut ws_z,
            kind.clone(),
            Duration::from_millis(200),
            "C host",
        )
        .await;
    }
    drop(harness);
}

fn room_leave_in(token: [u8; 32], room_id: Uuid) -> Envelope {
    let mut env = room_leave_envelope(token);
    env.room_id = Some(room_id);
    env
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_leave_with_a_room_id_leaves_exactly_that_room() {
    let harness = spawn_test_server().await;
    let (kp_x, _) = fresh_keypair();
    let (kp_w, _) = fresh_keypair();
    let (kp_y, _) = fresh_keypair();
    let mut ws_x = connect(harness.addr).await;
    let mut ws_w = connect(harness.addr).await;
    let mut ws_y = connect(harness.addr).await;
    let x = complete_handshake(&mut ws_x, &kp_x).await;
    let w = complete_handshake(&mut ws_w, &kp_w).await;
    let y = complete_handshake(&mut ws_y, &kp_y).await;

    // X hosts room A (migration off: X leaving A would end it) with
    // W in it, and is a viewer in Y's room B.
    send_envelope(&mut ws_x, &room_create_envelope(x.token, "A", false)).await;
    let a: RoomCreatedPayload = serde_json::from_value(
        next_of_kind(&mut ws_x, MessageKind::RoomCreated, "X create")
            .await
            .payload,
    )
    .unwrap();
    send_envelope(&mut ws_w, &room_join_envelope(w.token, &a.room.code, "W")).await;
    next_of_kind(&mut ws_w, MessageKind::RoomJoined, "W joins A").await;
    send_envelope(&mut ws_y, &room_create_envelope(y.token, "B", false)).await;
    let b: RoomCreatedPayload = serde_json::from_value(
        next_of_kind(&mut ws_y, MessageKind::RoomCreated, "Y create")
            .await
            .payload,
    )
    .unwrap();
    send_envelope(&mut ws_x, &room_join_envelope(x.token, &b.room.code, "X")).await;
    next_of_kind(&mut ws_x, MessageKind::RoomJoined, "X joins B").await;
    next_of_kind(&mut ws_y, MessageKind::ParticipantJoined, "Y sees X").await;

    // X leaves B by name.
    send_envelope(&mut ws_x, &room_leave_in(x.token, b.room.id)).await;
    let left = next_of_kind(&mut ws_y, MessageKind::ParticipantLeft, "Y sees X leave").await;
    assert_eq!(left.room_id, Some(b.room.id));
    let p: ParticipantLeftPayload = serde_json::from_value(left.payload).unwrap();
    assert_eq!(p.user_id, x.user_id);
    // Published once (not again by the WS layer), and B is not closed.
    assert_no_kind_within(
        &mut ws_y,
        MessageKind::ParticipantLeft,
        Duration::from_millis(300),
        "Y",
    )
    .await;
    assert_no_kind_within(
        &mut ws_y,
        MessageKind::RoomClosed,
        Duration::from_millis(100),
        "Y",
    )
    .await;
    // Room A is untouched: W is not told it closed, and X is still its host.
    assert_no_kind_within(
        &mut ws_w,
        MessageKind::RoomClosed,
        Duration::from_millis(300),
        "W",
    )
    .await;
    assert!(harness.rooms.is_room_host(a.room.id, x.user_id).await);
    assert!(!harness.rooms.is_user_in_room(x.user_id, b.room.id).await);
    drop(harness);
}

/// A MANIFEST_PUBLISH into `publish_room` carrying a manifest validly
/// signed by `kp` for `signed_room`.
fn manifest_signed_for(
    token: [u8; 32],
    publish_room: Uuid,
    signed_room: &str,
    kp: &SigningKey,
) -> Envelope {
    let manifest = locast_manifest::sign_manifest(
        &kp.to_bytes(),
        &locast_manifest::MediaManifest {
            manifest_version: 1,
            room_id: signed_room.to_string(),
            media: vec![],
            subtitles: vec![],
            created_at: 1,
            host_signature: None,
        },
    )
    .expect("sign manifest");
    room_scoped_envelope(
        token,
        MessageKind::ManifestPublish,
        publish_room,
        json!({ "manifest": manifest }),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_signed_for_another_room_is_rejected_before_persistence() {
    let harness = spawn_test_server().await;
    let (kp_x, _) = fresh_keypair();
    let (kp_y, _) = fresh_keypair();
    let mut ws_x = connect(harness.addr).await;
    let mut ws_y = connect(harness.addr).await;
    let x = complete_handshake(&mut ws_x, &kp_x).await;
    let y = complete_handshake(&mut ws_y, &kp_y).await;

    // X hosts both A and B (so the gate allows X to publish into
    // either); Y watches B.
    let mut rooms = Vec::new();
    for title in ["A", "B"] {
        send_envelope(&mut ws_x, &room_create_envelope(x.token, title, false)).await;
        let env = next_of_kind(&mut ws_x, MessageKind::RoomCreated, "X create").await;
        rooms.push(
            serde_json::from_value::<RoomCreatedPayload>(env.payload)
                .unwrap()
                .room,
        );
    }
    let (room_a, room_b) = (rooms[0].id, rooms[1].id);
    send_envelope(&mut ws_y, &room_join_envelope(y.token, &rooms[1].code, "Y")).await;
    next_of_kind(&mut ws_y, MessageKind::RoomJoined, "Y joins B").await;

    // A manifest validly signed by B's host, but for room A, is
    // refused in B: nothing stored, cached or broadcast.
    send_envelope(
        &mut ws_x,
        &manifest_signed_for(x.token, room_b, &room_a.to_string(), &kp_x),
    )
    .await;
    expect_room_error(
        &mut ws_x,
        RoomErrorCode::InvalidState,
        "A's manifest into B",
    )
    .await;
    // B's own id in a non-canonical spelling is refused too.
    send_envelope(
        &mut ws_x,
        &manifest_signed_for(x.token, room_b, &room_b.to_string().to_uppercase(), &kp_x),
    )
    .await;
    expect_room_error(&mut ws_x, RoomErrorCode::InvalidState, "uppercase room id").await;
    assert_no_kind_within(
        &mut ws_y,
        MessageKind::ManifestPublished,
        Duration::from_millis(300),
        "Y",
    )
    .await;
    assert!(harness
        .db
        .get_latest_room_manifest(room_b)
        .await
        .expect("db")
        .is_none());
    assert!(harness.rooms.current_manifest(room_b).await.is_none());

    // The same host's manifest signed for B is accepted into B.
    send_envelope(
        &mut ws_x,
        &manifest_signed_for(x.token, room_b, &room_b.to_string(), &kp_x),
    )
    .await;
    let env = next_of_kind(
        &mut ws_y,
        MessageKind::ManifestPublished,
        "Y gets B's manifest",
    )
    .await;
    assert_eq!(env.room_id, Some(room_b));
    assert_eq!(
        env.payload["manifest"]["room_id"],
        json!(room_b.to_string())
    );
    assert!(harness
        .db
        .get_latest_room_manifest(room_b)
        .await
        .expect("db")
        .is_some());
    drop(harness);
}

/// End-to-end behavior after a revoke. Over the wire the capability
/// gate refuses the BEGIN first; the in-lock re-check for a revoke
/// that lands between the gate and the lock is covered by
/// `draw_begin_rechecks_draw_under_the_room_lock` (dispatch.rs).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_draw_stops_the_next_stroke() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let (kp_c, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let mut ws_c = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    let c = complete_handshake(&mut ws_c, &kp_c).await;
    let room_id = playback_room(&mut ws_a, &mut ws_b, &mut ws_c, &a, &b, &c).await;

    let change = |add: u32, remove: u32| {
        room_scoped_envelope(
            a.token,
            MessageKind::PermissionSet,
            room_id,
            serde_json::to_value(PermissionSetPayload {
                target_user_id: b.user_id,
                add_cap_set: add,
                remove_cap_set: remove,
            })
            .unwrap(),
        )
    };
    send_envelope(&mut ws_a, &change(cap::DRAW, 0)).await;
    next_of_kind(&mut ws_b, MessageKind::CapabilityUpdate, "B granted").await;
    let first = Uuid::now_v7();
    send_envelope(
        &mut ws_b,
        &draw_begin_envelope(b.token, room_id, &kp_b, b.user_id, first),
    )
    .await;
    let env = next_of_kind(&mut ws_c, MessageKind::StrokeBegin, "C sees B's stroke").await;
    assert_eq!(env.payload["stroke_id"], json!(first));

    send_envelope(&mut ws_a, &change(0, cap::DRAW)).await;
    let env = next_of_kind(&mut ws_b, MessageKind::CapabilityUpdate, "B revoked").await;
    assert_eq!(
        env.payload["cap_set"].as_u64().unwrap() as u32 & cap::DRAW,
        0
    );
    let second = Uuid::now_v7();
    send_envelope(
        &mut ws_b,
        &draw_begin_envelope(b.token, room_id, &kp_b, b.user_id, second),
    )
    .await;
    expect_room_error(&mut ws_b, RoomErrorCode::NotHost, "B draws after revoke").await;
    assert_no_kind_within(
        &mut ws_c,
        MessageKind::StrokeBegin,
        Duration::from_millis(300),
        "C",
    )
    .await;
    drop(harness);
}
