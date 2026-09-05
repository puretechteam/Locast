//! P4-T06 integration tests for the SKEW_PROBE /
//! SKEW_REPLY round trip (architecture §13.3).
//!
//! These tests prove the server-side handler:
//!  1. Accepts a bearer-bearing SKEW_PROBE envelope.
//!  2. Decodes the `SkewProbePayload.client_send_ms`.
//!  3. Stamps the server's `clock.now_ms()` into
//!     `SkewReplyPayload.server_ts_ms`.
//!  4. Echoes `client_send_ms` back unchanged.
//!  5. Returns a single `SKEW_REPLY` envelope (no
//!     broadcast, no room context).
//!  6. Closes the connection on a malformed payload.
//!
//! The pure NTP math is covered in
//! `apps/client/src-tauri/src/room/skew.rs::tests`; this
//! file covers the wire-level exchange.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::handshake::{AuthOkPayload, ChallengePayload, WelcomePayload};
use locast_protocol::room::SkewReplyPayload;
use locast_server::time::MockClock;
use locast_server::{AppState, Config, Db, Metrics, RoomRegistry, RoomRegistryConfig, SignalRelay};
use rand::RngCore;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

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
        host_disconnect_grace_ms: 30_000,
        room_create_max_collisions: 5,
        participant_stale_after_ms: 300_000,
    }
}

struct TestHarness {
    addr: SocketAddr,
    _handle: tokio::task::JoinHandle<()>,
    _clock: Arc<MockClock>,
    _db: Db,
}

async fn spawn_test_server() -> TestHarness {
    let config = test_config();
    let db = Db::open(&config).await.expect("open db");
    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&config)));
    let clock = Arc::new(MockClock::new(1_700_000_000_000));
    let state = AppState {
        config: Arc::new(config),
        metrics: Metrics::new(),
        db: db.clone(),
        rooms: rooms.clone(),
        clock: clock.clone(),
        signal_relay: SignalRelay::new(),
    };
    let app: Router = locast_server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    TestHarness {
        addr,
        _handle: handle,
        _clock: clock,
        _db: db,
    }
}

fn fresh_keypair() -> (SigningKey, [u8; 32]) {
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    let kp = SigningKey::from_bytes(&seed);
    let pk = kp.verifying_key().to_bytes();
    (kp, pk)
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

fn skew_probe_envelope(bearer_token: [u8; 32], client_send_ms: i64) -> Envelope {
    Envelope {
        v: 1,
        r#type: MessageKind::SkewProbe,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload: json!({
            "bearer": bearer_token.to_vec(),
            "client_send_ms": client_send_ms,
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

async fn complete_handshake(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    kp: &SigningKey,
) -> [u8; 32] {
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let w = read_binary(ws).await.expect("w");
    let c = read_binary(ws).await.expect("c");
    let _ = decode(&w);
    let c_env = decode(&c);
    let challenge: ChallengePayload = serde_json::from_value(c_env.payload).expect("challenge");
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    let sig = kp.sign(&nonce).to_bytes();
    let public = kp.verifying_key().to_bytes();
    let auth = auth_envelope(public, sig);
    ws.send(Message::Binary(encode(&auth))).await.expect("auth");
    let ok = read_binary(ws).await.expect("ok");
    let ok_env = decode(&ok);
    let ok_p: AuthOkPayload = serde_json::from_value(ok_env.payload).expect("ok p");
    let _: WelcomePayload = serde_json::from_value(decode(&w).payload).expect("welcome p");
    ok_p.bearer.token.as_slice().try_into().expect("token 32b")
}

#[tokio::test]
async fn test_skew_probe_round_trip_echoes_client_send_ms() {
    let harness = spawn_test_server().await;
    let mut ws = connect(harness.addr).await;
    let (kp, _pk) = fresh_keypair();
    let token = complete_handshake(&mut ws, &kp).await;

    let client_send_ms: i64 = 1_700_000_000_000;
    let env = skew_probe_envelope(token, client_send_ms);
    ws.send(Message::Binary(encode(&env)))
        .await
        .expect("send skew probe");

    let reply_bytes = read_binary(&mut ws).await.expect("skew reply");
    let reply_env = decode(&reply_bytes);
    assert_eq!(
        reply_env.r#type.as_str(),
        "SKEW_REPLY",
        "expected SKEW_REPLY, got {}",
        reply_env.r#type.as_str()
    );
    let reply: SkewReplyPayload =
        serde_json::from_value(reply_env.payload.clone()).expect("decode skew reply");
    assert_eq!(reply.client_send_ms, client_send_ms);
    // server_ts_ms is read from the MockClock; we initialized it
    // to 1_700_000_000_000 in spawn_test_server. The server's
    // handler reads it via state.clock.now_ms().
    assert_eq!(reply.server_ts_ms, 1_700_000_000_000);

    let _ = tokio::time::timeout(Duration::from_millis(200), ws.send(Message::Close(None))).await;
}

#[tokio::test]
async fn test_skew_probe_malformed_payload_closes_connection() {
    let harness = spawn_test_server().await;
    let mut ws = connect(harness.addr).await;
    let (kp, _pk) = fresh_keypair();
    let token = complete_handshake(&mut ws, &kp).await;

    // Send a SKEW_PROBE whose payload does not match the
    // expected shape (missing `client_send_ms`).
    let env = Envelope {
        v: 1,
        r#type: MessageKind::SkewProbe,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload: json!({ "bearer": token.to_vec(), "not_client_send_ms": 0 }),
    };
    ws.send(Message::Binary(encode(&env)))
        .await
        .expect("send malformed probe");
    // Server should close (decode failure => bad_msg => close).
    let res = read_binary(&mut ws).await;
    assert!(
        res.is_none(),
        "expected server to close on malformed SKEW_PROBE payload"
    );
}

#[tokio::test]
async fn test_skew_probe_without_bearer_is_rejected() {
    let harness = spawn_test_server().await;
    let mut ws = connect(harness.addr).await;
    let (kp, _pk) = fresh_keypair();
    let _ = complete_handshake(&mut ws, &kp).await;

    let env = Envelope {
        v: 1,
        r#type: MessageKind::SkewProbe,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload: json!({ "client_send_ms": 1_700_000_000_000i64 }),
    };
    ws.send(Message::Binary(encode(&env)))
        .await
        .expect("send no-bearer probe");
    let res = read_binary(&mut ws).await;
    assert!(
        res.is_none(),
        "expected server to close on SKEW_PROBE without bearer"
    );
}

#[tokio::test]
async fn test_skew_probe_4_samples_produce_4_replies() {
    let harness = spawn_test_server().await;
    let mut ws = connect(harness.addr).await;
    let (kp, _pk) = fresh_keypair();
    let token = complete_handshake(&mut ws, &kp).await;

    for i in 0..4 {
        let client_send_ms: i64 = 1_700_000_000_000 + i * 50;
        let env = skew_probe_envelope(token, client_send_ms);
        ws.send(Message::Binary(encode(&env)))
            .await
            .expect("send probe {i}");
        let reply_bytes = read_binary(&mut ws).await.expect("skew reply {i}");
        let reply_env = decode(&reply_bytes);
        assert_eq!(reply_env.r#type.as_str(), "SKEW_REPLY");
        let reply: SkewReplyPayload =
            serde_json::from_value(reply_env.payload.clone()).expect("decode reply {i}");
        assert_eq!(reply.client_send_ms, client_send_ms);
    }

    let _ = tokio::time::timeout(Duration::from_millis(200), ws.send(Message::Close(None))).await;
}
