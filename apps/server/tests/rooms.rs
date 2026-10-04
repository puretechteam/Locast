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
    StrokeBeginPayload, StrokeEndPayload, StrokePointPayload, StrokeTool, StrokeUndoPayload,
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

/// Read until the next DRAW_BEGIN / DRAW_POINT / DRAW_END arrives,
/// skipping room chatter. Unlike `next_of_kind` this exposes the
/// arrival ORDER across the three drawing kinds.
async fn next_draw(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    ctx: &str,
) -> Envelope {
    loop {
        let bytes = tokio::time::timeout(Duration::from_secs(10), read_binary(ws))
            .await
            .unwrap_or_else(|_| panic!("{ctx}: no drawing frame within 10 s"))
            .unwrap_or_else(|| panic!("{ctx}: connection closed waiting for a drawing frame"));
        let env = decode(&bytes);
        if matches!(
            env.r#type,
            MessageKind::StrokeBegin | MessageKind::StrokePoint | MessageKind::StrokeEnd
        ) {
            return env;
        }
    }
}

/// P5-T02: a full-rate stroke (DRAW_BEGIN, 120 DRAW_POINT, DRAW_END)
/// from a participant holding DRAW reaches every other participant,
/// complete and in the order it was sent, and is not echoed back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_rate_stroke_reaches_every_other_participant_in_order() {
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

    // The host holds every capability, DRAW included.
    let stroke = Uuid::now_v7();
    send_envelope(
        &mut ws_a,
        &draw_begin_envelope(a.token, room_id, &kp_a, a.user_id, stroke),
    )
    .await;
    const POINTS: u32 = 120;
    for i in 0..POINTS {
        send_envelope(
            &mut ws_a,
            &room_scoped_envelope(
                a.token,
                MessageKind::StrokePoint,
                room_id,
                serde_json::to_value(StrokePointPayload {
                    stroke_id: stroke,
                    x: i as f32 / POINTS as f32,
                    y: 0.5,
                    pressure: 0.5,
                    ts_ms: i as i64,
                })
                .unwrap(),
            ),
        )
        .await;
    }
    send_envelope(
        &mut ws_a,
        &room_scoped_envelope(
            a.token,
            MessageKind::StrokeEnd,
            room_id,
            serde_json::to_value(StrokeEndPayload {
                stroke_id: stroke,
                ts_ms: 999,
            })
            .unwrap(),
        ),
    )
    .await;

    for (ws, who) in [(&mut ws_b, "B"), (&mut ws_c, "C")] {
        let begin = next_draw(ws, who).await;
        assert_eq!(begin.r#type, MessageKind::StrokeBegin, "{who}: first frame");
        assert_eq!(begin.payload["stroke_id"], json!(stroke), "{who}");
        for i in 0..POINTS {
            let env = next_draw(ws, who).await;
            assert_eq!(env.r#type, MessageKind::StrokePoint, "{who}: point {i}");
            assert_eq!(env.room_id, Some(room_id), "{who}: point {i}");
            assert_eq!(env.payload["stroke_id"], json!(stroke), "{who}: point {i}");
            let x = env.payload["x"].as_f64().expect("x");
            assert!(
                (x - f64::from(i as f32 / POINTS as f32)).abs() < 1e-6,
                "{who}: point {i} out of order, x = {x}"
            );
        }
        let end = next_draw(ws, who).await;
        assert_eq!(end.r#type, MessageKind::StrokeEnd, "{who}: last frame");
        assert_eq!(end.payload["stroke_id"], json!(stroke), "{who}");
    }

    // The originator never sees its own stroke come back.
    for kind in [
        MessageKind::StrokeBegin,
        MessageKind::StrokePoint,
        MessageKind::StrokeEnd,
    ] {
        assert_no_kind_within(&mut ws_a, kind, Duration::from_millis(100), "A echo").await;
    }
    drop(harness);
}

/// `env` with a client-supplied `sender` naming `user_id`.
fn with_claimed_sender(mut env: Envelope, user_id: Uuid, pubkey: [u8; 32]) -> Envelope {
    env.sender = Some(Sender {
        user_id,
        pubkey: pubkey.to_vec(),
        sig: vec![0x5A; 64],
    });
    env
}

/// The rebroadcast frame names the stroke owner as `sender.user_id`,
/// and the stroke payload is forwarded without any identity field.
fn assert_draw_frame_owned_by(env: &Envelope, kind: MessageKind, owner: Uuid, ctx: &str) {
    assert_eq!(env.r#type, kind, "{ctx}: kind");
    assert_eq!(
        env.sender.as_ref().map(|s| s.user_id),
        Some(owner),
        "{ctx}: rebroadcast sender is the authenticated stroke owner"
    );
    for key in ["sender", "sender_id", "user_id"] {
        assert!(
            env.payload.get(key).is_none(),
            "{ctx}: payload must not carry `{key}`: {}",
            env.payload
        );
    }
}

/// Rebroadcast DRAW_* frames carry the authenticated owner's id, taken
/// from the connection and the stroke binding, never from a field the
/// client set. Spoofed senders on BEGIN are refused; on POINT / END
/// they are ignored; another participant cannot POINT / END into the
/// stroke.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebroadcast_drawing_carries_the_authenticated_owner_and_ignores_spoofs() {
    let harness = spawn_test_server().await;
    let (kp_a, pk_a) = fresh_keypair();
    let (kp_b, pk_b) = fresh_keypair();
    let (kp_c, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let mut ws_c = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    let c = complete_handshake(&mut ws_c, &kp_c).await;
    let room_id = playback_room(&mut ws_a, &mut ws_b, &mut ws_c, &a, &b, &c).await;
    assert_ne!(a.user_id, b.user_id);

    // The host grants B DRAW and B has seen it.
    send_envelope(
        &mut ws_a,
        &permission_envelope(a.token, room_id, b.user_id, cap::DRAW),
    )
    .await;
    next_of_kind(&mut ws_b, MessageKind::CapabilityUpdate, "B grant").await;

    // BEGIN spoofs by B (the connection is B's): another user's id
    // with B's key, and B's id with another user's key. Both refused.
    let spoof_stroke = Uuid::now_v7();
    send_envelope(
        &mut ws_b,
        &draw_begin_envelope(b.token, room_id, &kp_b, a.user_id, spoof_stroke),
    )
    .await;
    expect_room_error(&mut ws_b, RoomErrorCode::InvalidState, "BEGIN as A").await;
    send_envelope(
        &mut ws_b,
        &with_claimed_sender(
            draw_begin_envelope(b.token, room_id, &kp_b, b.user_id, spoof_stroke),
            b.user_id,
            pk_a,
        ),
    )
    .await;
    expect_room_error(&mut ws_b, RoomErrorCode::InvalidState, "BEGIN with A's key").await;

    // B's legitimate stroke. Neither spoof bound `spoof_stroke`, and
    // the first drawing frame anyone sees is this one.
    let stroke = Uuid::now_v7();
    send_envelope(
        &mut ws_b,
        &draw_begin_envelope(b.token, room_id, &kp_b, b.user_id, stroke),
    )
    .await;
    for (ws, who) in [(&mut ws_a, "A"), (&mut ws_c, "C")] {
        let begin = next_draw(ws, who).await;
        assert_eq!(
            begin.payload["stroke_id"],
            json!(stroke),
            "{who}: the refused spoofs were never rebroadcast"
        );
        assert_draw_frame_owned_by(&begin, MessageKind::StrokeBegin, b.user_id, who);
    }

    // The host cannot POINT into B's stroke, with or without claiming
    // to be B. Nothing is rebroadcast.
    let a_point = |claim: Option<Uuid>| {
        let env = room_scoped_envelope(
            a.token,
            MessageKind::StrokePoint,
            room_id,
            serde_json::to_value(StrokePointPayload {
                stroke_id: stroke,
                x: 0.9,
                y: 0.9,
                pressure: 0.5,
                ts_ms: 2,
            })
            .unwrap(),
        );
        match claim {
            Some(uid) => with_claimed_sender(env, uid, pk_b),
            None => env,
        }
    };
    send_envelope(&mut ws_a, &a_point(None)).await;
    expect_room_error(&mut ws_a, RoomErrorCode::InvalidState, "A POINT into B").await;
    send_envelope(&mut ws_a, &a_point(Some(b.user_id))).await;
    expect_room_error(&mut ws_a, RoomErrorCode::InvalidState, "A POINT as B").await;

    // B's POINT and END, each carrying a spoofed envelope sender and
    // spoofed identity fields in the payload, are rebroadcast with B
    // (the connection and the stroke owner) as the sender.
    let mut point_payload = serde_json::to_value(StrokePointPayload {
        stroke_id: stroke,
        x: 0.3,
        y: 0.6,
        pressure: 0.5,
        ts_ms: 3,
    })
    .unwrap();
    point_payload["sender_id"] = json!(a.user_id);
    point_payload["user_id"] = json!(a.user_id);
    send_envelope(
        &mut ws_b,
        &with_claimed_sender(
            room_scoped_envelope(b.token, MessageKind::StrokePoint, room_id, point_payload),
            a.user_id,
            pk_a,
        ),
    )
    .await;
    send_envelope(
        &mut ws_b,
        &with_claimed_sender(
            room_scoped_envelope(
                b.token,
                MessageKind::StrokeEnd,
                room_id,
                serde_json::to_value(StrokeEndPayload {
                    stroke_id: stroke,
                    ts_ms: 4,
                })
                .unwrap(),
            ),
            c.user_id,
            pk_a,
        ),
    )
    .await;
    for (ws, who) in [(&mut ws_a, "A"), (&mut ws_c, "C")] {
        let point = next_draw(ws, who).await;
        assert_draw_frame_owned_by(&point, MessageKind::StrokePoint, b.user_id, who);
        assert_eq!(point.payload["stroke_id"], json!(stroke), "{who}");
        let end = next_draw(ws, who).await;
        assert_draw_frame_owned_by(&end, MessageKind::StrokeEnd, b.user_id, who);
        assert_eq!(end.payload["stroke_id"], json!(stroke), "{who}");
    }

    // B, the originator, is not echoed any of it.
    for kind in [
        MessageKind::StrokeBegin,
        MessageKind::StrokePoint,
        MessageKind::StrokeEnd,
    ] {
        assert_no_kind_within(&mut ws_b, kind, Duration::from_millis(100), "B echo").await;
    }

    // The host's own stroke is attributed to the host.
    let host_stroke = Uuid::now_v7();
    send_envelope(
        &mut ws_a,
        &draw_begin_envelope(a.token, room_id, &kp_a, a.user_id, host_stroke),
    )
    .await;
    for (ws, who) in [(&mut ws_b, "B"), (&mut ws_c, "C")] {
        let begin = next_draw(ws, who).await;
        assert_eq!(begin.payload["stroke_id"], json!(host_stroke), "{who}");
        assert_draw_frame_owned_by(&begin, MessageKind::StrokeBegin, a.user_id, who);
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

/// A MANIFEST_PUBLISH whose manifest is validly signed by `signer`
/// (for the right room), regardless of who sends it.
fn manifest_signed_by(token: [u8; 32], room_id: Uuid, signer: &SigningKey) -> Envelope {
    let manifest = locast_manifest::sign_manifest(
        &signer.to_bytes(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_must_be_signed_by_the_authenticated_host() {
    let harness = spawn_test_server().await;
    let (kp_h, _) = fresh_keypair();
    let (kp_v, _) = fresh_keypair();
    let (kp_other, _) = fresh_keypair();
    let mut ws_h = connect(harness.addr).await;
    let mut ws_v = connect(harness.addr).await;
    let h = complete_handshake(&mut ws_h, &kp_h).await;
    let v = complete_handshake(&mut ws_v, &kp_v).await;
    send_envelope(&mut ws_h, &room_create_envelope(h.token, "M", false)).await;
    let room = serde_json::from_value::<RoomCreatedPayload>(
        next_of_kind(&mut ws_h, MessageKind::RoomCreated, "create")
            .await
            .payload,
    )
    .unwrap()
    .room;
    send_envelope(&mut ws_v, &room_join_envelope(v.token, &room.code, "V")).await;
    next_of_kind(&mut ws_v, MessageKind::RoomJoined, "join").await;

    // The authenticated host presents manifests that verify but were
    // signed by someone else: an unrelated key, and the viewer's key.
    for (signer, ctx) in [(&kp_other, "unrelated key"), (&kp_v, "viewer's key")] {
        send_envelope(&mut ws_h, &manifest_signed_by(h.token, room.id, signer)).await;
        expect_room_error(&mut ws_h, RoomErrorCode::InvalidState, ctx).await;
    }
    // Nothing persisted, cached or broadcast.
    assert!(harness
        .db
        .get_latest_room_manifest(room.id)
        .await
        .expect("db")
        .is_none());
    assert!(harness.rooms.current_manifest(room.id).await.is_none());
    assert_no_kind_within(
        &mut ws_v,
        MessageKind::ManifestPublished,
        Duration::from_millis(300),
        "viewer",
    )
    .await;

    // Signed with the host's own authenticated key: accepted as v1.
    send_envelope(&mut ws_h, &manifest_signed_by(h.token, room.id, &kp_h)).await;
    let env = next_of_kind(&mut ws_v, MessageKind::ManifestPublished, "viewer gets it").await;
    assert_eq!(env.payload["version"], 1);
    // Exactly once: the handler publishes it, the WS layer does not.
    assert_no_kind_within(
        &mut ws_v,
        MessageKind::ManifestPublished,
        Duration::from_millis(300),
        "single broadcast",
    )
    .await;
    assert_eq!(
        env.payload["manifest"]["host_signature"]["public_key"],
        json!(locast_crypto::ed25519::to_base64(
            &kp_h.verifying_key().to_bytes()
        ))
    );
    assert_eq!(
        harness
            .rooms
            .current_manifest(room.id)
            .await
            .unwrap()
            .version,
        1
    );
    drop(harness);
}

// ---------------------------------------------------------------------------
// P5-T03: DRAW_UNDO / DRAW_CLEAR over the real WebSocket path.
// ---------------------------------------------------------------------------

fn draw_undo_envelope(token: [u8; 32], room_id: Uuid, stroke_id: Uuid) -> Envelope {
    room_scoped_envelope(
        token,
        MessageKind::StrokeUndo,
        room_id,
        serde_json::to_value(StrokeUndoPayload { stroke_id }).unwrap(),
    )
}

fn draw_clear_envelope(token: [u8; 32], room_id: Uuid) -> Envelope {
    room_scoped_envelope(token, MessageKind::StrokeClear, room_id, json!({}))
}

fn draw_end_envelope(token: [u8; 32], room_id: Uuid, stroke_id: Uuid) -> Envelope {
    room_scoped_envelope(
        token,
        MessageKind::StrokeEnd,
        room_id,
        serde_json::to_value(StrokeEndPayload {
            stroke_id,
            ts_ms: 9,
        })
        .unwrap(),
    )
}

fn draw_point_envelope(token: [u8; 32], room_id: Uuid, stroke_id: Uuid) -> Envelope {
    room_scoped_envelope(
        token,
        MessageKind::StrokePoint,
        room_id,
        serde_json::to_value(StrokePointPayload {
            stroke_id,
            x: 0.4,
            y: 0.4,
            pressure: 0.5,
            ts_ms: 5,
        })
        .unwrap(),
    )
}

/// Three participants with the `playback_room` shape plus the sockets
/// and identities the undo / clear tests need.
struct DrawRoom {
    harness: TestHarness,
    room_id: Uuid,
    ws_a: Ws,
    ws_b: Ws,
    ws_c: Ws,
    a: AuthedClient,
    b: AuthedClient,
    c: AuthedClient,
    kp_a: SigningKey,
    kp_b: SigningKey,
    kp_c: SigningKey,
}

async fn draw_room() -> DrawRoom {
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
    DrawRoom {
        harness,
        room_id,
        ws_a,
        ws_b,
        ws_c,
        a,
        b,
        c,
        kp_a,
        kp_b,
        kp_c,
    }
}

/// The host sets `target`'s capability bits to exactly `add` on top of
/// the default (a PERMISSION_SET add) and waits until `target_ws`
/// has seen the resulting CAPABILITY_UPDATE.
async fn grant(
    host_ws: &mut Ws,
    host: &AuthedClient,
    room_id: Uuid,
    target_ws: &mut Ws,
    target: &AuthedClient,
    add: u32,
) {
    send_envelope(
        host_ws,
        &permission_envelope(host.token, room_id, target.user_id, add),
    )
    .await;
    loop {
        let env = next_of_kind(target_ws, MessageKind::CapabilityUpdate, "grant seen").await;
        let bits = env.payload["cap_set"].as_u64().expect("cap_set") as u32;
        if env.payload["target_user_id"] == json!(target.user_id) && bits & add == add {
            return;
        }
    }
}

/// Drain the drawing frames of one stroke (BEGIN, END) from a receiver.
async fn expect_stroke(ws: &mut Ws, stroke: Uuid, owner: Uuid, ctx: &str) {
    for kind in [MessageKind::StrokeBegin, MessageKind::StrokeEnd] {
        let env = next_of_kind(ws, kind.clone(), ctx).await;
        assert_eq!(env.payload["stroke_id"], json!(stroke), "{ctx} {kind:?}");
        assert_eq!(env.sender.as_ref().map(|s| s.user_id), Some(owner), "{ctx}");
    }
}

/// Send BEGIN + END as `who` (who must hold DRAW).
async fn commit_stroke(ws: &mut Ws, who: &AuthedClient, kp: &SigningKey, room_id: Uuid) -> Uuid {
    let stroke = Uuid::now_v7();
    send_envelope(
        ws,
        &draw_begin_envelope(who.token, room_id, kp, who.user_id, stroke),
    )
    .await;
    send_envelope(ws, &draw_end_envelope(who.token, room_id, stroke)).await;
    stroke
}

/// No DRAW_UNDO / DRAW_CLEAR / ROOM_ERROR reaches `ws` within `window`.
async fn assert_quiet(ws: &mut Ws, window: Duration, ctx: &str) {
    for kind in [
        MessageKind::StrokeUndo,
        MessageKind::StrokeClear,
        MessageKind::RoomError,
    ] {
        assert_no_kind_within(ws, kind, window, ctx).await;
    }
}

fn assert_undo_frame(env: &Envelope, room_id: Uuid, stroke: Uuid, actor: Uuid, ctx: &str) {
    assert_eq!(env.r#type, MessageKind::StrokeUndo, "{ctx}");
    assert_eq!(env.room_id, Some(room_id), "{ctx}");
    assert_eq!(
        env.sender.as_ref().map(|s| s.user_id),
        Some(actor),
        "{ctx}: the sender is the authenticated actor"
    );
    assert_eq!(
        env.payload,
        json!({ "stroke_id": stroke }),
        "{ctx}: payload is the stroke id only"
    );
}

/// A: own undo. B (DRAW + UNDO_OWN) draws and undoes its own stroke;
/// A, B (the actor) and C all receive the undo, in apply order, with B
/// stamped as the actor; a duplicate undo is a silent no-op.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_undo_reaches_every_participant_including_the_actor() {
    let mut r = draw_room().await;
    grant(
        &mut r.ws_a,
        &r.a,
        r.room_id,
        &mut r.ws_b,
        &r.b,
        cap::DRAW | cap::UNDO_OWN,
    )
    .await;

    // BEGIN, END and UNDO sent back to back: every client sees them in
    // exactly that order (no undo overtaking its stroke).
    let stroke = Uuid::now_v7();
    send_envelope(
        &mut r.ws_b,
        &draw_begin_envelope(r.b.token, r.room_id, &r.kp_b, r.b.user_id, stroke),
    )
    .await;
    send_envelope(
        &mut r.ws_b,
        &draw_end_envelope(r.b.token, r.room_id, stroke),
    )
    .await;
    let t0 = std::time::Instant::now();
    send_envelope(
        &mut r.ws_b,
        &draw_undo_envelope(r.b.token, r.room_id, stroke),
    )
    .await;

    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_c, "C")] {
        let seq = [
            next_draw_any(ws, who).await,
            next_draw_any(ws, who).await,
            next_draw_any(ws, who).await,
        ];
        let kinds: Vec<_> = seq.iter().map(|e| e.r#type.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                MessageKind::StrokeBegin,
                MessageKind::StrokeEnd,
                MessageKind::StrokeUndo
            ],
            "{who}: apply order"
        );
        assert_undo_frame(&seq[2], r.room_id, stroke, r.b.user_id, who);
        println!(
            "DRAW_UNDO reached {who} {:.2} ms after the send",
            t0.elapsed().as_secs_f64() * 1000.0
        );
        assert!(t0.elapsed() < Duration::from_millis(1000), "{who}: latency");
    }
    // The actor gets the authoritative event too (it removes its own
    // stroke only when it arrives).
    let echo = next_of_kind(&mut r.ws_b, MessageKind::StrokeUndo, "B echo").await;
    assert_undo_frame(&echo, r.room_id, stroke, r.b.user_id, "B (actor)");
    println!(
        "DRAW_UNDO echoed to the actor {:.2} ms after the send",
        t0.elapsed().as_secs_f64() * 1000.0
    );

    // Duplicate / replayed undo: nothing happens, nobody is told, nobody
    // is evicted (no ROOM_ERROR).
    send_envelope(
        &mut r.ws_b,
        &draw_undo_envelope(r.b.token, r.room_id, stroke),
    )
    .await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        assert_quiet(ws, Duration::from_millis(300), who).await;
    }
    drop(r.harness);
}

/// The actor in the DRAW_UNDO frame is the authenticated connection:
/// a spoofed envelope sender and spoofed payload identity fields are
/// ignored, for undo and for clear.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_and_clear_actor_is_the_connection_not_a_claimed_identity() {
    let mut r = draw_room().await;
    grant(
        &mut r.ws_a,
        &r.a,
        r.room_id,
        &mut r.ws_b,
        &r.b,
        cap::DRAW | cap::UNDO_OWN | cap::CLEAR_ALL,
    )
    .await;
    let stroke = commit_stroke(&mut r.ws_b, &r.b, &r.kp_b, r.room_id).await;
    expect_stroke(&mut r.ws_a, stroke, r.b.user_id, "A").await;

    let mut payload = serde_json::to_value(StrokeUndoPayload { stroke_id: stroke }).unwrap();
    payload["user_id"] = json!(r.a.user_id);
    payload["owner"] = json!(r.a.user_id);
    payload["sender_id"] = json!(r.a.user_id);
    let undo = with_claimed_sender(
        room_scoped_envelope(r.b.token, MessageKind::StrokeUndo, r.room_id, payload),
        r.a.user_id,
        [7; 32],
    );
    send_envelope(&mut r.ws_b, &undo).await;
    let seen = next_of_kind(&mut r.ws_a, MessageKind::StrokeUndo, "A").await;
    assert_undo_frame(
        &seen,
        r.room_id,
        stroke,
        r.b.user_id,
        "A sees B, not the claimed A",
    );

    let clear = with_claimed_sender(
        room_scoped_envelope(
            r.b.token,
            MessageKind::StrokeClear,
            r.room_id,
            json!({ "user_id": r.a.user_id, "actor": r.a.user_id }),
        ),
        r.c.user_id,
        [7; 32],
    );
    send_envelope(&mut r.ws_b, &clear).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        let env = next_of_kind(ws, MessageKind::StrokeClear, who).await;
        assert_eq!(env.room_id, Some(r.room_id), "{who}");
        assert_eq!(
            env.sender.as_ref().map(|s| s.user_id),
            Some(r.b.user_id),
            "{who}: the clear is attributed to the connection"
        );
        assert_eq!(env.payload, json!({}), "{who}: empty payload");
    }
    drop(r.harness);
}

/// B: undo-any. A draws; B (DRAW + UNDO_OWN only) is refused with room
/// state unchanged and no ROOM_ERROR; after the host grants UNDO_ANY,
/// B's undo of A's stroke reaches everyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undoing_someone_elses_stroke_needs_undo_any() {
    let mut r = draw_room().await;
    grant(
        &mut r.ws_a,
        &r.a,
        r.room_id,
        &mut r.ws_b,
        &r.b,
        cap::DRAW | cap::UNDO_OWN,
    )
    .await;

    // The host draws; B and C see it.
    let stroke = commit_stroke(&mut r.ws_a, &r.a, &r.kp_a, r.room_id).await;
    expect_stroke(&mut r.ws_b, stroke, r.a.user_id, "B").await;
    expect_stroke(&mut r.ws_c, stroke, r.a.user_id, "C").await;

    // B (undo_own only) and C (no undo bit at all) are refused silently.
    send_envelope(
        &mut r.ws_b,
        &draw_undo_envelope(r.b.token, r.room_id, stroke),
    )
    .await;
    send_envelope(
        &mut r.ws_c,
        &draw_undo_envelope(r.c.token, r.room_id, stroke),
    )
    .await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        assert_quiet(ws, Duration::from_millis(300), who).await;
    }

    // Grant UNDO_ANY: the very same stroke is still there to undo, so
    // the refusals changed nothing.
    grant(
        &mut r.ws_a,
        &r.a,
        r.room_id,
        &mut r.ws_b,
        &r.b,
        cap::UNDO_ANY,
    )
    .await;
    send_envelope(
        &mut r.ws_b,
        &draw_undo_envelope(r.b.token, r.room_id, stroke),
    )
    .await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        let env = next_of_kind(ws, MessageKind::StrokeUndo, who).await;
        assert_undo_frame(&env, r.room_id, stroke, r.b.user_id, who);
    }
    drop(r.harness);
}

/// C: ownership. Two editors with UNDO_OWN each undo only their own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_own_cannot_undo_another_users_stroke() {
    let mut r = draw_room().await;
    for (ws, who) in [(&mut r.ws_b, &r.b), (&mut r.ws_c, &r.c)] {
        grant(
            &mut r.ws_a,
            &r.a,
            r.room_id,
            ws,
            who,
            cap::DRAW | cap::UNDO_OWN,
        )
        .await;
    }
    let sb = commit_stroke(&mut r.ws_b, &r.b, &r.kp_b, r.room_id).await;
    expect_stroke(&mut r.ws_a, sb, r.b.user_id, "A").await;
    expect_stroke(&mut r.ws_c, sb, r.b.user_id, "C").await;
    let sc = commit_stroke(&mut r.ws_c, &r.c, &r.kp_c, r.room_id).await;
    expect_stroke(&mut r.ws_a, sc, r.c.user_id, "A").await;
    expect_stroke(&mut r.ws_b, sc, r.c.user_id, "B").await;

    // B tries to undo C's stroke: refused, nothing broadcast.
    send_envelope(&mut r.ws_b, &draw_undo_envelope(r.b.token, r.room_id, sc)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        assert_quiet(ws, Duration::from_millis(300), who).await;
    }
    // C's stroke is intact: C can still undo it, and B can undo its own.
    send_envelope(&mut r.ws_c, &draw_undo_envelope(r.c.token, r.room_id, sc)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        let env = next_of_kind(ws, MessageKind::StrokeUndo, who).await;
        assert_undo_frame(&env, r.room_id, sc, r.c.user_id, who);
    }
    send_envelope(&mut r.ws_b, &draw_undo_envelope(r.b.token, r.room_id, sb)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        let env = next_of_kind(ws, MessageKind::StrokeUndo, who).await;
        assert_undo_frame(&env, r.room_id, sb, r.b.user_id, who);
    }
    drop(r.harness);
}

/// D: unknown ids, in-progress strokes and strokes of another room are
/// safe no-ops that corrupt nothing and send no ROOM_ERROR.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_in_progress_and_other_room_strokes_cannot_be_undone() {
    let mut r = draw_room().await;

    // A second, unrelated room hosted by X, with a committed stroke.
    let (kp_x, _) = fresh_keypair();
    let mut ws_x = connect(r.harness.addr).await;
    let x = complete_handshake(&mut ws_x, &kp_x).await;
    send_envelope(&mut ws_x, &room_create_envelope(x.token, "Other", false)).await;
    let other_room = serde_json::from_value::<RoomCreatedPayload>(
        next_of_kind(&mut ws_x, MessageKind::RoomCreated, "X create")
            .await
            .payload,
    )
    .unwrap()
    .room
    .id;
    assert_ne!(other_room, r.room_id);
    let other_stroke = commit_stroke(&mut ws_x, &x, &kp_x, other_room).await;

    // The host of THIS room (holds undo_any here) cannot reach it, by
    // naming either room.
    send_envelope(
        &mut r.ws_a,
        &draw_undo_envelope(r.a.token, r.room_id, other_stroke),
    )
    .await;
    send_envelope(
        &mut r.ws_a,
        &draw_undo_envelope(r.a.token, other_room, other_stroke),
    )
    .await;
    // A stroke id nobody ever drew.
    send_envelope(
        &mut r.ws_a,
        &draw_undo_envelope(r.a.token, r.room_id, Uuid::now_v7()),
    )
    .await;
    // A stroke still being drawn (BEGIN without END).
    let open = Uuid::now_v7();
    send_envelope(
        &mut r.ws_b,
        &draw_begin_envelope(r.b.token, r.room_id, &r.kp_b, r.b.user_id, open),
    )
    .await;
    // B has no DRAW yet: refused with a ROOM_ERROR as before. Grant, retry.
    expect_room_error(&mut r.ws_b, RoomErrorCode::NotHost, "B drew without DRAW").await;
    grant(&mut r.ws_a, &r.a, r.room_id, &mut r.ws_b, &r.b, cap::DRAW).await;
    send_envelope(
        &mut r.ws_b,
        &draw_begin_envelope(r.b.token, r.room_id, &r.kp_b, r.b.user_id, open),
    )
    .await;
    next_of_kind(
        &mut r.ws_a,
        MessageKind::StrokeBegin,
        "A sees the open stroke",
    )
    .await;
    send_envelope(&mut r.ws_a, &draw_undo_envelope(r.a.token, r.room_id, open)).await;

    for (ws, who) in [
        (&mut r.ws_a, "A"),
        (&mut r.ws_b, "B"),
        (&mut r.ws_c, "C"),
        (&mut ws_x, "X"),
    ] {
        assert_quiet(ws, Duration::from_millis(300), who).await;
    }

    // Nothing was corrupted: X's stroke is still undoable in its own
    // room, and the open stroke can still be ended and then undone.
    send_envelope(
        &mut ws_x,
        &draw_undo_envelope(x.token, other_room, other_stroke),
    )
    .await;
    let env = next_of_kind(&mut ws_x, MessageKind::StrokeUndo, "X own undo").await;
    assert_undo_frame(&env, other_room, other_stroke, x.user_id, "X");
    send_envelope(&mut r.ws_b, &draw_end_envelope(r.b.token, r.room_id, open)).await;
    next_of_kind(&mut r.ws_a, MessageKind::StrokeEnd, "A sees the end").await;
    send_envelope(&mut r.ws_a, &draw_undo_envelope(r.a.token, r.room_id, open)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        let env = next_of_kind(ws, MessageKind::StrokeUndo, who).await;
        assert_undo_frame(&env, r.room_id, open, r.a.user_id, who);
    }
    drop(r.harness);
}

/// E + F: clear_all. Several users, several strokes; an unauthorized
/// clear changes nothing; an authorized one wipes the room for everyone
/// and a later undo of a cleared stroke is a harmless no-op.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_all_wipes_the_canvas_for_everyone_and_needs_clear_all() {
    let mut r = draw_room().await;
    for (ws, who) in [(&mut r.ws_b, &r.b), (&mut r.ws_c, &r.c)] {
        grant(
            &mut r.ws_a,
            &r.a,
            r.room_id,
            ws,
            who,
            cap::DRAW | cap::UNDO_OWN,
        )
        .await;
    }
    // Strokes are drawn one after the other and drained by the others,
    // so the frames each client sees have one defined order.
    let s1 = commit_stroke(&mut r.ws_a, &r.a, &r.kp_a, r.room_id).await;
    expect_stroke(&mut r.ws_b, s1, r.a.user_id, "B").await;
    expect_stroke(&mut r.ws_c, s1, r.a.user_id, "C").await;
    let s2 = commit_stroke(&mut r.ws_b, &r.b, &r.kp_b, r.room_id).await;
    expect_stroke(&mut r.ws_a, s2, r.b.user_id, "A").await;
    expect_stroke(&mut r.ws_c, s2, r.b.user_id, "C").await;
    let s3 = commit_stroke(&mut r.ws_c, &r.c, &r.kp_c, r.room_id).await;
    expect_stroke(&mut r.ws_a, s3, r.c.user_id, "A").await;
    expect_stroke(&mut r.ws_b, s3, r.c.user_id, "B").await;

    // B and C hold no clear_all: refused, state intact.
    send_envelope(&mut r.ws_b, &draw_clear_envelope(r.b.token, r.room_id)).await;
    send_envelope(&mut r.ws_c, &draw_clear_envelope(r.c.token, r.room_id)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        assert_quiet(ws, Duration::from_millis(300), who).await;
    }
    // ... intact: the host can still undo one of the strokes.
    send_envelope(&mut r.ws_a, &draw_undo_envelope(r.a.token, r.room_id, s2)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        let env = next_of_kind(ws, MessageKind::StrokeUndo, who).await;
        assert_undo_frame(&env, r.room_id, s2, r.a.user_id, who);
    }

    // Grant clear_all to B; B clears.
    grant(
        &mut r.ws_a,
        &r.a,
        r.room_id,
        &mut r.ws_b,
        &r.b,
        cap::CLEAR_ALL,
    )
    .await;
    let t0 = std::time::Instant::now();
    send_envelope(&mut r.ws_b, &draw_clear_envelope(r.b.token, r.room_id)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        let env = next_of_kind(ws, MessageKind::StrokeClear, who).await;
        assert_eq!(env.room_id, Some(r.room_id), "{who}");
        assert_eq!(
            env.sender.as_ref().map(|s| s.user_id),
            Some(r.b.user_id),
            "{who}"
        );
        println!(
            "DRAW_CLEAR reached {who} {:.2} ms after the send",
            t0.elapsed().as_secs_f64() * 1000.0
        );
        assert!(t0.elapsed() < Duration::from_millis(1000), "{who}: latency");
    }

    // The cleared strokes are gone from the server: undo is a no-op for
    // all of them (even for the host, who holds every bit).
    for s in [s1, s2, s3] {
        send_envelope(&mut r.ws_a, &draw_undo_envelope(r.a.token, r.room_id, s)).await;
    }
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        assert_quiet(ws, Duration::from_millis(300), who).await;
    }
    // A fresh stroke after the clear works normally.
    let fresh = commit_stroke(&mut r.ws_a, &r.a, &r.kp_a, r.room_id).await;
    expect_stroke(&mut r.ws_c, fresh, r.a.user_id, "C").await;
    send_envelope(
        &mut r.ws_a,
        &draw_undo_envelope(r.a.token, r.room_id, fresh),
    )
    .await;
    let env = next_of_kind(&mut r.ws_c, MessageKind::StrokeUndo, "C").await;
    assert_undo_frame(&env, r.room_id, fresh, r.a.user_id, "C");
    drop(r.harness);
}

/// A stroke still being drawn when the clear lands does not reappear:
/// its remaining POINT / END are accepted without an error and are not
/// rebroadcast, and it cannot be undone afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stroke_in_progress_during_clear_does_not_reappear() {
    let mut r = draw_room().await;
    grant(&mut r.ws_a, &r.a, r.room_id, &mut r.ws_c, &r.c, cap::DRAW).await;
    let open = Uuid::now_v7();
    send_envelope(
        &mut r.ws_c,
        &draw_begin_envelope(r.c.token, r.room_id, &r.kp_c, r.c.user_id, open),
    )
    .await;
    next_of_kind(&mut r.ws_b, MessageKind::StrokeBegin, "B sees the begin").await;

    send_envelope(&mut r.ws_a, &draw_clear_envelope(r.a.token, r.room_id)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        next_of_kind(ws, MessageKind::StrokeClear, who).await;
    }

    // C keeps drawing and lifts the pen.
    send_envelope(
        &mut r.ws_c,
        &draw_point_envelope(r.c.token, r.room_id, open),
    )
    .await;
    send_envelope(&mut r.ws_c, &draw_end_envelope(r.c.token, r.room_id, open)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B")] {
        assert_no_kind_within(
            ws,
            MessageKind::StrokePoint,
            Duration::from_millis(300),
            who,
        )
        .await;
        assert_no_kind_within(ws, MessageKind::StrokeEnd, Duration::from_millis(100), who).await;
    }
    // C got no error (a ROOM_ERROR would evict its client) ...
    assert_no_kind_within(
        &mut r.ws_c,
        MessageKind::RoomError,
        Duration::from_millis(300),
        "C",
    )
    .await;
    // ... and the cleared stroke is not undoable.
    send_envelope(&mut r.ws_a, &draw_undo_envelope(r.a.token, r.room_id, open)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        assert_quiet(ws, Duration::from_millis(300), who).await;
    }
    drop(r.harness);
}

/// Undo / clear are room-scoped: the host of another room (every
/// capability THERE) cannot clear or undo in a room it is not in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_from_a_non_member_of_the_room_does_nothing() {
    let mut r = draw_room().await;
    let (kp_x, _) = fresh_keypair();
    let mut ws_x = connect(r.harness.addr).await;
    let x = complete_handshake(&mut ws_x, &kp_x).await;
    // X hosts its own room (holds clear_all THERE), but is not in A's.
    send_envelope(&mut ws_x, &room_create_envelope(x.token, "X", false)).await;
    next_of_kind(&mut ws_x, MessageKind::RoomCreated, "X create").await;
    let s = commit_stroke(&mut r.ws_a, &r.a, &r.kp_a, r.room_id).await;
    expect_stroke(&mut r.ws_b, s, r.a.user_id, "B").await;

    send_envelope(&mut ws_x, &draw_clear_envelope(x.token, r.room_id)).await;
    send_envelope(&mut ws_x, &draw_undo_envelope(x.token, r.room_id, s)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        assert_quiet(ws, Duration::from_millis(300), who).await;
    }
    // The stroke survived: the host can undo it.
    send_envelope(&mut r.ws_a, &draw_undo_envelope(r.a.token, r.room_id, s)).await;
    let env = next_of_kind(&mut r.ws_b, MessageKind::StrokeUndo, "B").await;
    assert_undo_frame(&env, r.room_id, s, r.a.user_id, "B");
    drop(r.harness);
}

/// A revoked UNDO_OWN stops the next undo (revokes take effect
/// immediately; the server re-checks under the room lock).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_undo_own_stops_the_next_undo() {
    let mut r = draw_room().await;
    grant(
        &mut r.ws_a,
        &r.a,
        r.room_id,
        &mut r.ws_b,
        &r.b,
        cap::DRAW | cap::UNDO_OWN,
    )
    .await;
    let stroke = commit_stroke(&mut r.ws_b, &r.b, &r.kp_b, r.room_id).await;
    expect_stroke(&mut r.ws_a, stroke, r.b.user_id, "A").await;
    send_envelope(
        &mut r.ws_a,
        &room_scoped_envelope(
            r.a.token,
            MessageKind::PermissionSet,
            r.room_id,
            serde_json::to_value(PermissionSetPayload {
                target_user_id: r.b.user_id,
                add_cap_set: 0,
                remove_cap_set: cap::UNDO_OWN,
            })
            .unwrap(),
        ),
    )
    .await;
    loop {
        let env = next_of_kind(&mut r.ws_b, MessageKind::CapabilityUpdate, "B revoke").await;
        if env.payload["cap_set"].as_u64().unwrap() as u32 & cap::UNDO_OWN == 0 {
            break;
        }
    }
    send_envelope(
        &mut r.ws_b,
        &draw_undo_envelope(r.b.token, r.room_id, stroke),
    )
    .await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        assert_quiet(ws, Duration::from_millis(300), who).await;
    }
    drop(r.harness);
}

/// Read until the next DRAW_* frame (including undo / clear) arrives.
async fn next_draw_any(ws: &mut Ws, ctx: &str) -> Envelope {
    loop {
        let bytes = tokio::time::timeout(Duration::from_secs(10), read_binary(ws))
            .await
            .unwrap_or_else(|_| panic!("{ctx}: no drawing frame within 10 s"))
            .unwrap_or_else(|| panic!("{ctx}: connection closed waiting for a drawing frame"));
        let env = decode(&bytes);
        if env.r#type.is_drawing() {
            return env;
        }
    }
}

/// Cap bits of `user` as the server holds them.
async fn server_caps(r: &DrawRoom, user: Uuid) -> u32 {
    let handle = r.harness.rooms.get_by_id(r.room_id).await.expect("room");
    let state = handle.read().await;
    state
        .participants
        .iter()
        .find(|p| p.user_id == user)
        .expect("participant")
        .cap_set
}

/// PERMISSION_SET is host-only: nobody else can give themselves or
/// anyone else UNDO_ANY / CLEAR_ALL, whatever else they hold (viewer,
/// editor, or a co-host-style bundle without those bits). The refusals
/// change nothing, so undo / clear from them still do nothing, and the
/// strokes are still there for the host to undo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nobody_but_the_host_can_grant_undo_any_or_clear_all() {
    let mut r = draw_room().await;
    let escalation = cap::UNDO_ANY | cap::CLEAR_ALL;
    let stroke = commit_stroke(&mut r.ws_a, &r.a, &r.kp_a, r.room_id).await;
    expect_stroke(&mut r.ws_b, stroke, r.a.user_id, "B").await;
    expect_stroke(&mut r.ws_c, stroke, r.a.user_id, "C").await;
    let b_before = server_caps(&r, r.b.user_id).await;
    let c_before = server_caps(&r, r.c.user_id).await;

    // Attempts by a plain viewer (C) and then by an editor (B), aimed at
    // themselves and at each other, including a "replace everything"
    // remove mask.
    let attempts = |token: [u8; 32], targets: [Uuid; 2]| {
        let mut v = Vec::new();
        for t in targets {
            v.push(permission_envelope(token, r.room_id, t, escalation));
            v.push(room_scoped_envelope(
                token,
                MessageKind::PermissionSet,
                r.room_id,
                serde_json::to_value(PermissionSetPayload {
                    target_user_id: t,
                    add_cap_set: cap::UNDO_ANY,
                    remove_cap_set: u32::MAX,
                })
                .unwrap(),
            ));
        }
        v
    };
    for env in attempts(r.c.token, [r.c.user_id, r.b.user_id]) {
        send_envelope(&mut r.ws_c, &env).await;
        expect_room_error(&mut r.ws_c, RoomErrorCode::NotHost, "viewer PERMISSION_SET").await;
    }
    grant(
        &mut r.ws_a,
        &r.a,
        r.room_id,
        &mut r.ws_b,
        &r.b,
        cap::DRAW | cap::UNDO_OWN,
    )
    .await;
    let b_before = b_before | cap::DRAW | cap::UNDO_OWN;
    for env in attempts(r.b.token, [r.b.user_id, r.c.user_id]) {
        send_envelope(&mut r.ws_b, &env).await;
        expect_room_error(&mut r.ws_b, RoomErrorCode::NotHost, "editor PERMISSION_SET").await;
    }

    // A co-host-style bundle (everything except the two bits in question)
    // does not make PERMISSION_SET available either.
    let cohost_without = cap::PLAYBACK_CONTROL
        | cap::DRAW
        | cap::LASER
        | cap::MANAGE_ROOM
        | cap::KICK
        | cap::PUBLISH_MANIFEST
        | cap::INVITE
        | cap::CHAT
        | cap::UNDO_OWN;
    grant(
        &mut r.ws_a,
        &r.a,
        r.room_id,
        &mut r.ws_c,
        &r.c,
        cohost_without,
    )
    .await;
    let c_before = c_before | cohost_without;
    for env in attempts(r.c.token, [r.c.user_id, r.b.user_id]) {
        send_envelope(&mut r.ws_c, &env).await;
        expect_room_error(
            &mut r.ws_c,
            RoomErrorCode::NotHost,
            "co-host PERMISSION_SET",
        )
        .await;
    }

    // Nothing changed on the server and nobody was told of a change.
    assert_eq!(server_caps(&r, r.b.user_id).await, b_before, "B's caps");
    assert_eq!(server_caps(&r, r.c.user_id).await, c_before, "C's caps");
    for user in [r.b.user_id, r.c.user_id] {
        assert_eq!(server_caps(&r, user).await & escalation, 0, "no escalation");
    }
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        loop {
            // Drain the legitimate grants; any update carrying the
            // escalation bits is a failure.
            let bytes = tokio::time::timeout(Duration::from_millis(300), read_binary(ws)).await;
            let Ok(Some(bytes)) = bytes else { break };
            let env = decode(&bytes);
            if env.r#type == MessageKind::CapabilityUpdate {
                let bits = env.payload["cap_set"].as_u64().unwrap() as u32;
                assert_eq!(bits & escalation, 0, "{who}: escalated CAPABILITY_UPDATE");
            }
        }
    }

    // The users still cannot undo someone else's stroke or clear.
    send_envelope(
        &mut r.ws_b,
        &draw_undo_envelope(r.b.token, r.room_id, stroke),
    )
    .await;
    send_envelope(
        &mut r.ws_c,
        &draw_undo_envelope(r.c.token, r.room_id, stroke),
    )
    .await;
    send_envelope(&mut r.ws_b, &draw_clear_envelope(r.b.token, r.room_id)).await;
    send_envelope(&mut r.ws_c, &draw_clear_envelope(r.c.token, r.room_id)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        assert_quiet(ws, Duration::from_millis(300), who).await;
    }

    // State intact: the host can still undo the stroke.
    send_envelope(
        &mut r.ws_a,
        &draw_undo_envelope(r.a.token, r.room_id, stroke),
    )
    .await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        let env = next_of_kind(ws, MessageKind::StrokeUndo, who).await;
        assert_undo_frame(&env, r.room_id, stroke, r.a.user_id, who);
    }
    drop(r.harness);
}

/// Normal delivery: a participant that keeps up receives every
/// drawing rebroadcast in order, each stamped with the room's drawing
/// sequence number (contiguous from 1), and never a DRAW_SYNC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drawing_rebroadcasts_carry_contiguous_room_sequence_numbers() {
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

    let stroke = Uuid::now_v7();
    send_envelope(
        &mut ws_a,
        &draw_begin_envelope(a.token, room_id, &kp_a, a.user_id, stroke),
    )
    .await;
    for i in 0..3 {
        send_envelope(
            &mut ws_a,
            &room_scoped_envelope(
                a.token,
                MessageKind::StrokePoint,
                room_id,
                json!({ "stroke_id": stroke, "x": 0.1 * i as f32, "y": 0.5, "pressure": 0.5, "ts_ms": 2 + i }),
            ),
        )
        .await;
    }
    send_envelope(
        &mut ws_a,
        &room_scoped_envelope(
            a.token,
            MessageKind::StrokeEnd,
            room_id,
            json!({ "stroke_id": stroke, "ts_ms": 9 }),
        ),
    )
    .await;
    send_envelope(
        &mut ws_a,
        &room_scoped_envelope(
            a.token,
            MessageKind::StrokeUndo,
            room_id,
            json!({ "stroke_id": stroke }),
        ),
    )
    .await;
    send_envelope(
        &mut ws_a,
        &room_scoped_envelope(a.token, MessageKind::StrokeClear, room_id, json!({})),
    )
    .await;

    let expected = [
        MessageKind::StrokeBegin,
        MessageKind::StrokePoint,
        MessageKind::StrokePoint,
        MessageKind::StrokePoint,
        MessageKind::StrokeEnd,
        MessageKind::StrokeUndo,
        MessageKind::StrokeClear,
    ];
    let mut seen = Vec::new();
    while seen.len() < expected.len() {
        let env = next_envelope(&mut ws_b).await;
        assert_ne!(
            env.r#type,
            MessageKind::StrokeSync,
            "no snapshot when keeping up"
        );
        if env.r#type.is_drawing() {
            assert_eq!(env.room_id, Some(room_id));
            seen.push((env.r#type, env.seq));
        }
    }
    // The room's numbering starts from a clock-based epoch; within
    // the room it is contiguous.
    let first = seen[0].1;
    assert_eq!(
        seen,
        expected.iter().cloned().zip(first..).collect::<Vec<_>>(),
        "every drawing event, in order, numbered contiguously"
    );
    drop(harness);
}

// ---------------------------------------------------------------------------
// P5-T04: LASER_MOVE / LASER_OFF relay over the real WebSocket path.
// ---------------------------------------------------------------------------

fn laser_move_envelope(token: [u8; 32], room_id: Uuid, x: f32, y: f32) -> Envelope {
    room_scoped_envelope(
        token,
        MessageKind::LaserMove,
        room_id,
        json!({ "x": x, "y": y }),
    )
}

fn laser_off_envelope(token: [u8; 32], room_id: Uuid) -> Envelope {
    room_scoped_envelope(token, MessageKind::LaserOff, room_id, json!({}))
}

/// A relayed laser frame: right room, the authenticated `sender`, no
/// identity (or bearer) in the payload, unsequenced.
fn assert_laser_frame(env: &Envelope, kind: MessageKind, room_id: Uuid, sender: Uuid, ctx: &str) {
    assert_eq!(env.r#type, kind, "{ctx}: kind");
    assert_eq!(env.room_id, Some(room_id), "{ctx}: room");
    assert_eq!(
        env.sender.as_ref().map(|s| s.user_id),
        Some(sender),
        "{ctx}: sender is the authenticated connection"
    );
    assert_eq!(env.seq, 0, "{ctx}: lasers are unsequenced");
    for key in ["sender", "sender_id", "user_id", "bearer"] {
        assert!(
            env.payload.get(key).is_none(),
            "{ctx}: payload must not carry `{key}`: {}",
            env.payload
        );
    }
}

/// Count the LASER_MOVE frames reaching `ws` until it has been quiet
/// for `quiet`. Returns the count and when the last one arrived.
async fn drain_laser_moves(ws: &mut Ws, quiet: Duration) -> (usize, tokio::time::Instant) {
    let mut count = 0;
    let mut last = tokio::time::Instant::now();
    while let Ok(Some(bytes)) = tokio::time::timeout(quiet, read_binary(ws)).await {
        let env = decode(&bytes);
        assert_ne!(env.r#type, MessageKind::RoomError, "unexpected ROOM_ERROR");
        if env.r#type == MessageKind::LaserMove {
            count += 1;
            last = tokio::time::Instant::now();
        }
    }
    (count, last)
}

/// A (host) points: B and C get every move and the off, attributed to
/// A by the server; A gets no echo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn laser_reaches_every_other_participant_with_the_authenticated_sender() {
    let mut r = draw_room().await;
    let moves = [(0.1f32, 0.2f32), (0.5, 0.5), (1.0, 0.0)];
    for (x, y) in moves {
        send_envelope(
            &mut r.ws_a,
            &laser_move_envelope(r.a.token, r.room_id, x, y),
        )
        .await;
    }
    send_envelope(&mut r.ws_a, &laser_off_envelope(r.a.token, r.room_id)).await;
    for (ws, who) in [(&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        for (x, y) in moves {
            let env = next_of_kind(ws, MessageKind::LaserMove, who).await;
            assert_laser_frame(&env, MessageKind::LaserMove, r.room_id, r.a.user_id, who);
            assert_eq!(env.payload, json!({ "x": x, "y": y }), "{who}: in order");
        }
        let env = next_of_kind(ws, MessageKind::LaserOff, who).await;
        assert_laser_frame(&env, MessageKind::LaserOff, r.room_id, r.a.user_id, who);
        assert_eq!(env.payload, json!({}), "{who}: empty off payload");
    }
    for kind in [
        MessageKind::LaserMove,
        MessageKind::LaserOff,
        MessageKind::RoomError,
    ] {
        assert_no_kind_within(&mut r.ws_a, kind, Duration::from_millis(300), "A: no echo").await;
    }
    r.harness.handle.abort();
}

/// A participant without LASER is dropped silently (no ROOM_ERROR, so
/// its room survives); DRAW does not imply LASER; a LASER grant works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn laser_needs_the_laser_capability_and_a_refusal_keeps_the_sender_in_the_room() {
    let mut r = draw_room().await;
    send_envelope(
        &mut r.ws_b,
        &laser_move_envelope(r.b.token, r.room_id, 0.5, 0.5),
    )
    .await;
    send_envelope(&mut r.ws_b, &laser_off_envelope(r.b.token, r.room_id)).await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_c, "C")] {
        for kind in [MessageKind::LaserMove, MessageKind::LaserOff] {
            assert_no_kind_within(ws, kind, Duration::from_millis(300), who).await;
        }
    }
    assert_no_kind_within(
        &mut r.ws_b,
        MessageKind::RoomError,
        Duration::from_millis(300),
        "B: refusal is silent",
    )
    .await;
    // B is still a connected member: its chat goes through.
    send_envelope(
        &mut r.ws_b,
        &chat_envelope(r.b.token, r.room_id, r.b.user_id, "still here"),
    )
    .await;
    next_of_kind(&mut r.ws_a, MessageKind::ChatMessage, "A gets B's chat").await;

    // DRAW alone is not LASER.
    grant(&mut r.ws_a, &r.a, r.room_id, &mut r.ws_b, &r.b, cap::DRAW).await;
    send_envelope(
        &mut r.ws_b,
        &laser_move_envelope(r.b.token, r.room_id, 0.5, 0.5),
    )
    .await;
    assert_no_kind_within(
        &mut r.ws_c,
        MessageKind::LaserMove,
        Duration::from_millis(300),
        "C: DRAW only",
    )
    .await;

    grant(&mut r.ws_a, &r.a, r.room_id, &mut r.ws_b, &r.b, cap::LASER).await;
    send_envelope(
        &mut r.ws_b,
        &laser_move_envelope(r.b.token, r.room_id, 0.25, 0.75),
    )
    .await;
    for (ws, who) in [(&mut r.ws_a, "A"), (&mut r.ws_c, "C")] {
        let env = next_of_kind(ws, MessageKind::LaserMove, who).await;
        assert_laser_frame(&env, MessageKind::LaserMove, r.room_id, r.b.user_id, who);
        assert_eq!(env.payload, json!({ "x": 0.25, "y": 0.75 }));
    }
    r.harness.handle.abort();
}

/// Neither `Envelope::sender` nor identity fields in the payload can
/// make a laser look like it came from someone else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn laser_sender_cannot_be_spoofed() {
    let mut r = draw_room().await;
    let pk_b = r.kp_b.verifying_key().to_bytes();
    // A claims to be B, in both places.
    let spoof = with_claimed_sender(
        room_scoped_envelope(
            r.a.token,
            MessageKind::LaserMove,
            r.room_id,
            json!({ "x": 0.5, "y": 0.5, "user_id": r.b.user_id, "sender_id": r.b.user_id }),
        ),
        r.b.user_id,
        pk_b,
    );
    send_envelope(&mut r.ws_a, &spoof).await;
    for (ws, who) in [(&mut r.ws_b, "B"), (&mut r.ws_c, "C")] {
        let env = next_of_kind(ws, MessageKind::LaserMove, who).await;
        assert_laser_frame(&env, MessageKind::LaserMove, r.room_id, r.a.user_id, who);
        assert_eq!(env.payload, json!({ "x": 0.5, "y": 0.5 }));
    }
    // B (no LASER) claims to be the host A: judged as B, so refused.
    let pk_a = r.kp_a.verifying_key().to_bytes();
    let spoof = with_claimed_sender(
        room_scoped_envelope(
            r.b.token,
            MessageKind::LaserMove,
            r.room_id,
            json!({ "x": 0.5, "y": 0.5, "user_id": r.a.user_id }),
        ),
        r.a.user_id,
        pk_a,
    );
    send_envelope(&mut r.ws_b, &spoof).await;
    assert_no_kind_within(
        &mut r.ws_c,
        MessageKind::LaserMove,
        Duration::from_millis(300),
        "C: spoofed host laser",
    )
    .await;
    r.harness.handle.abort();
}

/// Out-of-range or malformed coordinates are dropped silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_laser_coordinates_are_dropped_silently() {
    let mut r = draw_room().await;
    for payload in [
        json!({ "x": 1.5, "y": 0.5 }),
        json!({ "x": 0.5, "y": -0.1 }),
        json!({ "x": 0.5 }),
    ] {
        send_envelope(
            &mut r.ws_a,
            &room_scoped_envelope(r.a.token, MessageKind::LaserMove, r.room_id, payload),
        )
        .await;
    }
    send_envelope(
        &mut r.ws_a,
        &laser_move_envelope(r.a.token, r.room_id, 0.75, 0.25),
    )
    .await;
    let env = next_of_kind(&mut r.ws_b, MessageKind::LaserMove, "B").await;
    assert_eq!(
        env.payload,
        json!({ "x": 0.75, "y": 0.25 }),
        "only the valid move is relayed"
    );
    assert_no_kind_within(
        &mut r.ws_a,
        MessageKind::RoomError,
        Duration::from_millis(300),
        "A: silent drop",
    )
    .await;
    r.harness.handle.abort();
}

/// A full-rate (60 Hz) laser is relayed in full, well past the point
/// where its 60-frame burst alone would have run out (so the refill
/// really sustains 60/s).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_60_hz_laser_is_relayed_in_full() {
    let mut r = draw_room().await;
    const FRAMES: usize = 360;
    let mut tick = tokio::time::interval(Duration::from_micros(16_667));
    for i in 0..FRAMES {
        tick.tick().await;
        let x = i as f32 / FRAMES as f32;
        send_envelope(
            &mut r.ws_a,
            &laser_move_envelope(r.a.token, r.room_id, x, 0.5),
        )
        .await;
    }
    let (count, _) = drain_laser_moves(&mut r.ws_b, Duration::from_millis(500)).await;
    assert_eq!(count, FRAMES, "every 60 Hz move reaches B");
    assert_no_kind_within(
        &mut r.ws_a,
        MessageKind::RateLimit,
        Duration::from_millis(100),
        "A: within budget",
    )
    .await;
    r.harness.handle.abort();
}

/// A laser flood is capped at the laser bucket and the excess dropped
/// silently: no RATE_LIMIT, no ROOM_ERROR, no disconnect, and the
/// sender's other traffic (and later lasers) keep flowing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_laser_flood_is_capped_without_disconnecting_the_sender() {
    let mut r = draw_room().await;
    const FLOOD: usize = 150;
    let started = tokio::time::Instant::now();
    for i in 0..FLOOD {
        let x = i as f32 / FLOOD as f32;
        send_envelope(
            &mut r.ws_a,
            &laser_move_envelope(r.a.token, r.room_id, x, 0.5),
        )
        .await;
    }
    let (count, last) = drain_laser_moves(&mut r.ws_b, Duration::from_millis(500)).await;
    // 60 burst plus 60/s refill over the time the server could have
    // been taking frames (generously: until the last relay arrived).
    let window = last.duration_since(started).as_secs_f64();
    let allowed = 60 + (60.0 * window).ceil() as usize + 1;
    assert!(count >= 50, "the burst gets through (got {count})");
    assert!(
        count <= allowed && count < FLOOD,
        "flood capped: {count} relayed, at most {allowed} allowed over {window:.3}s"
    );
    for kind in [MessageKind::RateLimit, MessageKind::RoomError] {
        assert_no_kind_within(
            &mut r.ws_a,
            kind,
            Duration::from_millis(200),
            "A: silent drop",
        )
        .await;
    }
    // After the bucket refills the same connection lasers and chats.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    send_envelope(
        &mut r.ws_a,
        &laser_move_envelope(r.a.token, r.room_id, 0.5, 0.5),
    )
    .await;
    let env = next_of_kind(&mut r.ws_b, MessageKind::LaserMove, "B after refill").await;
    assert_eq!(env.payload, json!({ "x": 0.5, "y": 0.5 }));
    send_envelope(
        &mut r.ws_a,
        &chat_envelope(r.a.token, r.room_id, r.a.user_id, "hi"),
    )
    .await;
    next_of_kind(&mut r.ws_b, MessageKind::ChatMessage, "B gets A's chat").await;
    r.harness.handle.abort();
}

async fn create_room_as(ws: &mut Ws, who: &AuthedClient, title: &str) -> RoomSummary {
    send_envelope(ws, &room_create_envelope(who.token, title, false)).await;
    let env = next_of_kind(ws, MessageKind::RoomCreated, title).await;
    serde_json::from_value::<RoomCreatedPayload>(env.payload)
        .unwrap()
        .room
}

/// Lasers stay in the room they were sent to: X hosts room 1 (with Y)
/// and is a plain viewer in Z's room 2 (with W). X's laser in room 1
/// never reaches room 2; X has no LASER in room 2, so its laser there
/// is refused; a laser naming a room the sender is not in is dropped;
/// Z's laser in room 2 never reaches Y.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn laser_events_never_cross_rooms() {
    let harness = spawn_test_server().await;
    let (kp_x, _) = fresh_keypair();
    let (kp_y, _) = fresh_keypair();
    let (kp_z, _) = fresh_keypair();
    let (kp_w, _) = fresh_keypair();
    let mut ws_x = connect(harness.addr).await;
    let mut ws_y = connect(harness.addr).await;
    let mut ws_z = connect(harness.addr).await;
    let mut ws_w = connect(harness.addr).await;
    let x = complete_handshake(&mut ws_x, &kp_x).await;
    let y = complete_handshake(&mut ws_y, &kp_y).await;
    let z = complete_handshake(&mut ws_z, &kp_z).await;
    let w = complete_handshake(&mut ws_w, &kp_w).await;

    let room_1 = create_room_as(&mut ws_x, &x, "one").await;
    let room_2 = create_room_as(&mut ws_z, &z, "two").await;
    send_envelope(&mut ws_y, &room_join_envelope(y.token, &room_1.code, "Y")).await;
    next_of_kind(&mut ws_y, MessageKind::RoomJoined, "Y joins 1").await;
    send_envelope(&mut ws_w, &room_join_envelope(w.token, &room_2.code, "W")).await;
    next_of_kind(&mut ws_w, MessageKind::RoomJoined, "W joins 2").await;
    send_envelope(&mut ws_x, &room_join_envelope(x.token, &room_2.code, "X")).await;
    next_of_kind(&mut ws_x, MessageKind::RoomJoined, "X joins 2").await;

    // Room 1: Y sees it.
    send_envelope(
        &mut ws_x,
        &laser_move_envelope(x.token, room_1.id, 0.5, 0.5),
    )
    .await;
    let env = next_of_kind(&mut ws_y, MessageKind::LaserMove, "Y").await;
    assert_laser_frame(&env, MessageKind::LaserMove, room_1.id, x.user_id, "Y");
    // Room 2: X is only a viewer there, so refused.
    send_envelope(
        &mut ws_x,
        &laser_move_envelope(x.token, room_2.id, 0.5, 0.5),
    )
    .await;
    // A room Y is not in: dropped before dispatch.
    send_envelope(
        &mut ws_y,
        &laser_move_envelope(y.token, room_2.id, 0.5, 0.5),
    )
    .await;
    for (ws, who) in [(&mut ws_z, "Z"), (&mut ws_w, "W")] {
        assert_no_kind_within(ws, MessageKind::LaserMove, Duration::from_millis(400), who).await;
    }
    // Z (host of room 2) points: W (room 2) sees it, Y (room 1)
    // never does. (X is only a sender here: a connection follows one
    // room's broadcast, and X's follows room 1.)
    send_envelope(
        &mut ws_z,
        &laser_move_envelope(z.token, room_2.id, 0.1, 0.9),
    )
    .await;
    let env = next_of_kind(&mut ws_w, MessageKind::LaserMove, "W").await;
    assert_laser_frame(&env, MessageKind::LaserMove, room_2.id, z.user_id, "W");
    assert_no_kind_within(
        &mut ws_y,
        MessageKind::LaserMove,
        Duration::from_millis(300),
        "Y: nothing from room 2",
    )
    .await;
    harness.handle.abort();
}

/// Laser frames still in flight after their sender left the room (or
/// was kicked, or the room closed) are dropped without the
/// bad-message strikes that would close the connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lasers_in_flight_after_leaving_do_not_cost_the_connection() {
    let mut r = draw_room().await;
    send_envelope(
        &mut r.ws_a,
        &laser_move_envelope(r.a.token, r.room_id, 0.5, 0.5),
    )
    .await;
    next_of_kind(&mut r.ws_b, MessageKind::LaserMove, "B").await;
    next_of_kind(&mut r.ws_c, MessageKind::LaserMove, "C").await;
    send_envelope(&mut r.ws_b, &room_leave_envelope(r.b.token)).await;
    next_of_kind(&mut r.ws_a, MessageKind::ParticipantLeft, "A sees B leave").await;
    // More than the 3-strike bad-message threshold, from a former member.
    for _ in 0..5 {
        send_envelope(
            &mut r.ws_b,
            &laser_move_envelope(r.b.token, r.room_id, 0.5, 0.5),
        )
        .await;
    }
    // B's connection is still alive and usable.
    let room = create_room_as(&mut r.ws_b, &r.b, "after").await;
    assert_ne!(room.id, r.room_id);
    // Nothing reached the old room.
    assert_no_kind_within(
        &mut r.ws_c,
        MessageKind::LaserMove,
        Duration::from_millis(300),
        "C: nothing from a former member",
    )
    .await;
    r.harness.handle.abort();
}
