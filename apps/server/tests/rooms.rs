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
use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::room::{
    HostDisconnectedPayload, HostMigratedPayload, HostReconnectedPayload, ParticipantJoinedPayload,
    ParticipantLeftPayload, RoomClosedPayload, RoomCreatePayload, RoomCreatedPayload,
    RoomErrorCode, RoomErrorPayload, RoomJoinRequestPayload, RoomJoinedPayload, RoomLeavePayload,
    RoomSummary,
};
use locast_server::time::MockClock;
use locast_server::{AppState, Config, Db, Metrics, RoomRegistry, RoomRegistryConfig};
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
    }
}

struct TestHarness {
    addr: SocketAddr,
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
    #[allow(dead_code)]
    clock: Arc<MockClock>,
    _db: Db,
}

async fn spawn_test_server() -> TestHarness {
    let config = test_config();
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
    };
    let app: Router = locast_server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // Start the room ticker that drives the host-disconnect
    // grace and the stale-participant cleanup. The test
    // harness uses a 50ms interval so the 200ms grace
    // resolves within ~4 ticks. The handle is dropped
    // (the test process exiting is fine; the spawn keeps
    // the ticker running until the runtime shuts down).
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
        _db: db,
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
// 5. Host absent past grace: server elects new host.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_absent_past_grace_migrates() {
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
    // The 50ms background ticker (started by the harness)
    // reads the AppState clock and calls tick_grace, which
    // elects the new host once `now_ms >= deadline`. The
    // AppState clock in the test harness is a `MockClock`
    // that the ticker syncs to wall-clock on every tick,
    // so the 200ms grace resolves within ~4-5 ticks. Wait
    // up to 1s for B to receive HOST_MIGRATED.
    let mut got = None;
    for _ in 0..100 {
        let env = next_envelope(&mut ws_b).await;
        match env.r#type {
            MessageKind::HostMigrated => {
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
    let env = got.expect("expected HostMigrated within 1s");
    let hm: HostMigratedPayload = serde_json::from_value(env.payload).unwrap();
    assert_eq!(hm.previous_host_user_id, a.user_id);
    assert_eq!(hm.new_host_user_id, b.user_id);
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
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_rooms_in_parallel() {
    let harness = spawn_test_server().await;
    let (kp_a, _) = fresh_keypair();
    let (kp_b, _) = fresh_keypair();
    let mut ws_a = connect(harness.addr).await;
    let mut ws_b = connect(harness.addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    send_envelope(&mut ws_a, &room_create_envelope(a.token, "R1", false)).await;
    let env_a1 = expect_envelope(&mut ws_a, MessageKind::RoomCreated).await;
    let r1: RoomSummary = serde_json::from_value::<RoomCreatedPayload>(env_a1.payload)
        .unwrap()
        .room;
    let r1_id = r1.id;
    send_envelope(&mut ws_b, &room_create_envelope(b.token, "R2", false)).await;
    let env_b1 = expect_envelope(&mut ws_b, MessageKind::RoomCreated).await;
    let r2: RoomSummary = serde_json::from_value::<RoomCreatedPayload>(env_b1.payload)
        .unwrap()
        .room;
    // A joins R2.
    send_envelope(&mut ws_a, &room_join_envelope(a.token, &r2.code, "A")).await;
    let _ = expect_envelope(&mut ws_a, MessageKind::RoomJoined).await;
    let _ = expect_envelope(&mut ws_b, MessageKind::ParticipantJoined).await;
    // B joins R1.
    send_envelope(&mut ws_b, &room_join_envelope(b.token, &r1.code, "B")).await;
    let _ = expect_envelope(&mut ws_b, MessageKind::RoomJoined).await;
    let _ = expect_envelope(&mut ws_a, MessageKind::ParticipantJoined).await;
    let _ = r1_id;
}

// ---------------------------------------------------------------------------
// Helper: extract RoomSummary (asserts the room_id matches for sanity).
// ---------------------------------------------------------------------------
