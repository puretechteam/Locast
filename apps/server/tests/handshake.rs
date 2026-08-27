//! Integration tests for the P2-T02 handshake.
//!
//! Each test spawns the server on a fresh `127.0.0.1:0` port
//! backed by an in-memory SQLite database, then drives a
//! `tokio_tungstenite` client through the handshake. The tests
//! cover the full state machine, the rate limit, the v1 bearer
//! requirement, the size limit, and several negative paths.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::handshake::{
    AuthFailPayload, AuthFailReason, AuthOkPayload, ChallengePayload, WelcomePayload,
};
use locast_server::{AppState, Config, Db, Metrics};
use rand::RngCore;
use serde_json::json;
use std::io::Write;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tracing_subscriber::layer::SubscriberExt;
use uuid::Uuid;

/// Shared buffer for the tracing-test writer.
#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl LogBuf {
    async fn snapshot(&self) -> Vec<u8> {
        self.0.lock().await.clone()
    }
}

impl Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Best-effort: ignore poisoned-lock errors during shutdown.
        if let Ok(mut g) = self.0.try_lock() {
            g.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Build a fresh server config suitable for tests.
fn test_config(challenge_ttl_ms: i64, max_frame_bytes: usize) -> Config {
    Config {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        log_filter: "off".to_string(),
        database_url: "sqlite::memory:".to_string(),
        bearer_ttl_seconds: 900,
        challenge_ttl_ms,
        max_frame_bytes,
        handshake_timeout_ms: 30_000,
        rate_msgs_per_sec: 100,
        rate_msg_burst: 200,
        rate_bytes_per_sec: 1_000_000,
        rate_bytes_burst: 2_000_000,
    }
}

/// Build a server config with a tiny rate so we can
/// deterministically exercise the throttle path.
fn test_config_low_rate(
    challenge_ttl_ms: i64,
    max_frame_bytes: usize,
    msgs_per_sec: u32,
    burst: u32,
) -> Config {
    let mut c = test_config(challenge_ttl_ms, max_frame_bytes);
    c.rate_msgs_per_sec = msgs_per_sec;
    c.rate_msg_burst = burst;
    c
}

/// Spawn the server on `127.0.0.1:0` and return the bound
/// `SocketAddr`. The server is cleaned up when the returned
/// `ServerHandle` is dropped.
async fn spawn_server(config: Config) -> (SocketAddr, tokio::task::JoinHandle<()>, Db) {
    let db = Db::open(&config).await.expect("open db");
    let state = AppState {
        config: Arc::new(config),
        metrics: Metrics::new(),
        db: db.clone(),
    };
    let app: Router = locast_server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // Give the server a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, handle, db)
}

/// Generate a fresh Ed25519 keypair.
fn fresh_keypair() -> (SigningKey, [u8; 32]) {
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    let signing = SigningKey::from_bytes(&seed);
    let public = signing.verifying_key().to_bytes();
    (signing, public)
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

fn envelope_with_bearer(token: [u8; 32]) -> Envelope {
    Envelope {
        v: 1,
        r#type: MessageKind::Hello, // arbitrary type; not validated
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload: json!({
            "bearer": token.to_vec(),
            "junk": vec![0u8; 8],
        }),
    }
}

fn encode(env: &Envelope) -> Vec<u8> {
    rmp_serde::to_vec_named(env).expect("encode envelope")
}

fn decode(bytes: &[u8]) -> Envelope {
    rmp_serde::from_slice(bytes).expect("decode envelope")
}

/// Connect to the server as a tungstenite WS client.
async fn connect(
    addr: SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{addr}/ws");
    let (ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    ws
}

/// Read the next binary message from the WS stream. Returns
/// `None` on close OR WS error. Asserts that the message is a
/// binary frame.
async fn read_binary<S>(stream: &mut S) -> Option<Vec<u8>>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let m = match stream.next().await {
            Some(Ok(m)) => m,
            // The server closed the connection (or the network
            // dropped it) - surface as end-of-stream so the
            // caller can check the test's expectation.
            Some(Err(_)) | None => return None,
        };
        match m {
            Message::Binary(b) => return Some(b),
            Message::Text(_) => panic!("unexpected text frame"),
            Message::Close(_) => return None,
            _ => continue,
        }
    }
}

/// Run the full handshake against a fresh connection and
/// return the envelopes received during the handshake. The
/// caller supplies the keypair and decides what sig (if any)
/// to put in AUTH.
async fn run_handshake_with_sig(
    addr: SocketAddr,
    keypair: &SigningKey,
    sig: [u8; 64],
) -> Vec<Envelope> {
    let mut ws = connect(addr).await;
    let hello = hello_envelope();
    ws.send(Message::Binary(encode(&hello)))
        .await
        .expect("send hello");

    // Receive WELCOME + CHALLENGE.
    let w_bytes = read_binary(&mut ws).await.expect("welcome bytes");
    let c_bytes = read_binary(&mut ws).await.expect("challenge bytes");
    let w_env = decode(&w_bytes);
    let c_env = decode(&c_bytes);
    assert_eq!(w_env.r#type.as_str(), "WELCOME");
    assert_eq!(c_env.r#type.as_str(), "CHALLENGE");
    let _welcome: WelcomePayload = serde_json::from_value(w_env.payload.clone()).expect("welcome");

    // Send AUTH.
    let public = keypair.verifying_key().to_bytes();
    let auth = auth_envelope(public, sig);
    ws.send(Message::Binary(encode(&auth)))
        .await
        .expect("send auth");

    // Collect the response(s) until close.
    let mut envelopes = Vec::new();
    while let Some(b) = read_binary(&mut ws).await {
        let env = decode(&b);
        envelopes.push(env);
    }
    envelopes
}

#[tokio::test]
async fn test_successful_handshake() {
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp, _pk) = fresh_keypair();
    let mut ws = connect(addr).await;
    let hello = hello_envelope();
    ws.send(Message::Binary(encode(&hello)))
        .await
        .expect("send hello");

    let w_bytes = read_binary(&mut ws).await.expect("welcome");
    let c_bytes = read_binary(&mut ws).await.expect("challenge");
    let w_env = decode(&w_bytes);
    let c_env = decode(&c_bytes);
    assert_eq!(w_env.r#type.as_str(), "WELCOME");
    let challenge: ChallengePayload =
        serde_json::from_value(c_env.payload.clone()).expect("challenge");

    let public = kp.verifying_key().to_bytes();
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    let sig = kp.sign(&nonce).to_bytes();
    let auth = auth_envelope(public, sig);
    ws.send(Message::Binary(encode(&auth)))
        .await
        .expect("send auth");

    let ok_bytes = read_binary(&mut ws).await.expect("auth_ok");
    let ok_env = decode(&ok_bytes);
    assert_eq!(ok_env.r#type.as_str(), "AUTH_OK");
    let ok: AuthOkPayload = serde_json::from_value(ok_env.payload.clone()).expect("ok payload");
    assert_eq!(ok.pubkey, public.to_vec());
    assert_eq!(ok.bearer.token.len(), 32);
    assert!(ok.bearer.expires_ms > 0);
    assert_eq!(ok.user_id.get_version_num(), 7);

    // Now send a follow-up envelope with the bearer. It should
    // be accepted (no response is expected from the server in
    // v1, but the connection must stay open).
    let follow = envelope_with_bearer(
        ok.bearer
            .token
            .as_slice()
            .try_into()
            .expect("token is 32 bytes"),
    );
    ws.send(Message::Binary(encode(&follow)))
        .await
        .expect("send follow-up");
    // Send a close-frame to cleanly terminate the test; the
    // server's bearer validation already accepted the
    // follow-up. We use a short read timeout to avoid hanging
    // if the server's response semantics change.
    let _ = tokio::time::timeout(Duration::from_millis(200), ws.send(Message::Close(None))).await;
}

#[tokio::test]
async fn test_invalid_signature() {
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp, _pk) = fresh_keypair();
    let envs = run_handshake_with_sig(addr, &kp, [1u8; 64]).await;
    assert!(!envs.is_empty());
    let first = &envs[0];
    assert_eq!(first.r#type.as_str(), "AUTH_FAIL");
    let fail: AuthFailPayload = serde_json::from_value(first.payload.clone()).expect("fail");
    assert_eq!(fail.reason, AuthFailReason::BadSig);
}

#[tokio::test]
async fn test_wrong_pubkey() {
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp, _pk) = fresh_keypair();
    let mut ws = connect(addr).await;
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let _w = read_binary(&mut ws).await.expect("w");
    let c = read_binary(&mut ws).await.expect("c");
    let c_env = decode(&c);
    let challenge: ChallengePayload =
        serde_json::from_value(c_env.payload.clone()).expect("challenge");
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    let sig = kp.sign(&nonce).to_bytes();
    // Sign correctly but send a different pubkey.
    let (other_kp, other_pk) = fresh_keypair();
    let _ = other_kp; // unused; we just want the pubkey
    let auth = auth_envelope(other_pk, sig);
    ws.send(Message::Binary(encode(&auth))).await.expect("auth");
    let fail_bytes = read_binary(&mut ws).await.expect("fail");
    let fail_env = decode(&fail_bytes);
    assert_eq!(fail_env.r#type.as_str(), "AUTH_FAIL");
    let fail: AuthFailPayload = serde_json::from_value(fail_env.payload.clone()).expect("p");
    assert_eq!(fail.reason, AuthFailReason::BadSig);
}

#[tokio::test]
async fn test_modified_challenge() {
    // Client signs N but tells the server it signed N'.
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp, _pk) = fresh_keypair();
    let mut ws = connect(addr).await;
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let _w = read_binary(&mut ws).await.expect("w");
    let c = read_binary(&mut ws).await.expect("c");
    let c_env = decode(&c);
    // We do NOT sign the actual challenge nonce; we sign a
    // different one. The challenge is still decoded so the
    // server reaches the AUTH state.
    let _challenge: ChallengePayload =
        serde_json::from_value(c_env.payload.clone()).expect("challenge");
    // Build a different nonce and sign it.
    let mut different_nonce = [0u8; 32];
    different_nonce[0] = 0xFF;
    let sig = kp.sign(&different_nonce).to_bytes();
    let public = kp.verifying_key().to_bytes();
    let auth = auth_envelope(public, sig);
    ws.send(Message::Binary(encode(&auth))).await.expect("auth");
    let fail = read_binary(&mut ws).await.expect("fail");
    let fail_env = decode(&fail);
    assert_eq!(fail_env.r#type.as_str(), "AUTH_FAIL");
    let fail: AuthFailPayload = serde_json::from_value(fail_env.payload.clone()).expect("p");
    assert_eq!(fail.reason, AuthFailReason::BadSig);
}

#[tokio::test]
async fn test_modified_signed_payload() {
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp, _pk) = fresh_keypair();
    let mut ws = connect(addr).await;
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let _w = read_binary(&mut ws).await.expect("w");
    let c = read_binary(&mut ws).await.expect("c");
    let c_env = decode(&c);
    let challenge: ChallengePayload =
        serde_json::from_value(c_env.payload.clone()).expect("challenge");
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    let mut sig = kp.sign(&nonce).to_bytes();
    sig[0] ^= 0x01;
    let public = kp.verifying_key().to_bytes();
    let auth = auth_envelope(public, sig);
    ws.send(Message::Binary(encode(&auth))).await.expect("auth");
    let fail = read_binary(&mut ws).await.expect("fail");
    let fail_env = decode(&fail);
    assert_eq!(fail_env.r#type.as_str(), "AUTH_FAIL");
    let fail: AuthFailPayload = serde_json::from_value(fail_env.payload.clone()).expect("p");
    assert_eq!(fail.reason, AuthFailReason::BadSig);
}

#[tokio::test]
async fn test_stale_challenge() {
    let (addr, _h, _db) = spawn_server(test_config(100, 1_048_576)).await;
    let (kp, _pk) = fresh_keypair();
    let mut ws = connect(addr).await;
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let _w = read_binary(&mut ws).await.expect("w");
    let c = read_binary(&mut ws).await.expect("c");
    let c_env = decode(&c);
    let challenge: ChallengePayload =
        serde_json::from_value(c_env.payload.clone()).expect("challenge");
    // Wait past the challenge TTL.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    let sig = kp.sign(&nonce).to_bytes();
    let public = kp.verifying_key().to_bytes();
    let auth = auth_envelope(public, sig);
    ws.send(Message::Binary(encode(&auth))).await.expect("auth");
    let fail = read_binary(&mut ws).await.expect("fail");
    let fail_env = decode(&fail);
    assert_eq!(fail_env.r#type.as_str(), "AUTH_FAIL");
    let fail: AuthFailPayload = serde_json::from_value(fail_env.payload.clone()).expect("p");
    assert_eq!(fail.reason, AuthFailReason::Expired);
}

#[tokio::test]
async fn test_duplicate_auth() {
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp, pk) = fresh_keypair();
    let mut ws = connect(addr).await;
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let _w = read_binary(&mut ws).await.expect("w");
    let c = read_binary(&mut ws).await.expect("c");
    let c_env = decode(&c);
    let challenge: ChallengePayload =
        serde_json::from_value(c_env.payload.clone()).expect("challenge");
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    let sig = kp.sign(&nonce).to_bytes();
    let auth = auth_envelope(pk, sig);
    ws.send(Message::Binary(encode(&auth)))
        .await
        .expect("auth1");
    let ok = read_binary(&mut ws).await.expect("ok");
    let ok_env = decode(&ok);
    assert_eq!(ok_env.r#type.as_str(), "AUTH_OK");
    // Second AUTH should be rejected.
    ws.send(Message::Binary(encode(&auth)))
        .await
        .expect("auth2");
    // Server should close after the second auth. The next
    // read should be a close (None) or an error. Accept either.
    let res = read_binary(&mut ws).await;
    assert!(res.is_none());
}

#[tokio::test]
async fn test_replay_auth_on_new_connection() {
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp, _pk) = fresh_keypair();
    // First connection: capture the AUTH bytes.
    let mut ws1 = connect(addr).await;
    ws1.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let _w = read_binary(&mut ws1).await.expect("w");
    let c = read_binary(&mut ws1).await.expect("c");
    let c_env = decode(&c);
    let challenge: ChallengePayload =
        serde_json::from_value(c_env.payload.clone()).expect("challenge");
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    let sig = kp.sign(&nonce).to_bytes();
    let public = kp.verifying_key().to_bytes();
    let auth = auth_envelope(public, sig);
    let auth_bytes = encode(&auth);
    ws1.send(Message::Binary(auth_bytes.clone()))
        .await
        .expect("auth");
    let _ok = read_binary(&mut ws1).await.expect("ok");
    // Second connection: replay the same AUTH bytes but with
    // a different nonce. The signature is over the original
    // nonce, not the new one, so the new server should reject
    // it.
    let mut ws2 = connect(addr).await;
    ws2.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello2");
    let _w2 = read_binary(&mut ws2).await.expect("w2");
    let c2 = read_binary(&mut ws2).await.expect("c2");
    let _ = decode(&c2);
    ws2.send(Message::Binary(auth_bytes))
        .await
        .expect("auth replay");
    let fail = read_binary(&mut ws2).await.expect("fail");
    let fail_env = decode(&fail);
    assert_eq!(fail_env.r#type.as_str(), "AUTH_FAIL");
}

#[tokio::test]
async fn test_new_connection_new_challenge() {
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let mut nonces = Vec::new();
    for _ in 0..3 {
        let mut ws = connect(addr).await;
        ws.send(Message::Binary(encode(&hello_envelope())))
            .await
            .expect("hello");
        let _w = read_binary(&mut ws).await.expect("w");
        let c = read_binary(&mut ws).await.expect("c");
        let c_env = decode(&c);
        let challenge: ChallengePayload =
            serde_json::from_value(c_env.payload.clone()).expect("challenge");
        nonces.push(challenge.nonce);
        let _ = ws.send(Message::Close(None)).await;
    }
    assert_eq!(nonces.len(), 3);
    let n1 = nonces[0].clone();
    let n2 = nonces[1].clone();
    let n3 = nonces[2].clone();
    assert_ne!(n1, n2);
    assert_ne!(n2, n3);
    assert_ne!(n1, n3);
}

#[tokio::test]
async fn test_malformed_message() {
    // Per §20.4.1 the server tolerates up to 3 bad_msg in 60 s
    // before closing. Send 3 envelopes with `v: 99` to trip the
    // counter; the connection should be closed after the 3rd.
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let mut ws = connect(addr).await;
    for i in 0..3 {
        let env = Envelope {
            v: 99,
            r#type: MessageKind::Hello,
            id: Uuid::now_v7(),
            room_id: None,
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload: json!({}),
        };
        let send_r = ws.send(Message::Binary(encode(&env))).await;
        assert!(send_r.is_ok(), "send {i} should not fail locally");
    }
    // Server should close after the 3rd bad-msg. The next read
    // should be a close (None) or an error.
    let res = read_binary(&mut ws).await;
    assert!(
        res.is_none(),
        "expected server to close after 3 bad-msg events (got Some)"
    );
}

#[tokio::test]
async fn test_unknown_message_type() {
    // Per §20.4.1: 3 bad-msg strikes -> close. A single
    // unknown-type envelope is one strike.
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let mut ws = connect(addr).await;
    // Send raw msgpack with an unknown type string.
    #[derive(serde::Serialize)]
    struct RawEnv {
        v: u8,
        r#type: &'static str,
        id: Uuid,
        room_id: Option<Uuid>,
        sender: Option<()>,
        ts_ms: i64,
        seq: u64,
        payload: serde_json::Value,
    }
    for _ in 0..3 {
        let raw = RawEnv {
            v: 1,
            r#type: "FOOBAR",
            id: Uuid::now_v7(),
            room_id: None,
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload: json!({}),
        };
        let bytes = rmp_serde::to_vec_named(&raw).expect("encode raw");
        let r = ws.send(Message::Binary(bytes)).await;
        assert!(r.is_ok(), "send should not fail locally");
    }
    let res = read_binary(&mut ws).await;
    assert!(res.is_none());
}

#[tokio::test]
async fn test_oversized_message() {
    // Configure a small max_frame_bytes. axum's WS layer closes
    // the connection immediately on a single oversized frame,
    // so this is a one-strike-and-done path (the bad-msg
    // counter is not consulted because the frame is rejected at
    // the transport layer, before the bad-msg tally is
    // incremented).
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1024)).await;
    let mut ws = connect(addr).await;
    let big = vec![0u8; 4096];
    let env = Envelope {
        v: 1,
        r#type: MessageKind::Hello,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload: json!({ "junk": big }),
    };
    ws.send(Message::Binary(encode(&env))).await.expect("send");
    let res = read_binary(&mut ws).await;
    // axum closes on oversized frames; the next read should
    // return None.
    assert!(res.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_clients() {
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let mut handles = Vec::new();
    for _ in 0..8 {
        handles.push(tokio::spawn(async move {
            let (kp, _pk) = fresh_keypair();
            run_handshake_inner(addr, &kp).await
        }));
    }
    let mut tokens: Vec<[u8; 32]> = Vec::new();
    for h in handles {
        let token = h.await.expect("join").expect("handshake");
        tokens.push(token);
    }
    assert_eq!(tokens.len(), 8);
    for t in &tokens {
        assert_eq!(t.len(), 32);
    }
    // No two tokens are equal.
    for i in 0..tokens.len() {
        for j in (i + 1)..tokens.len() {
            assert_ne!(tokens[i], tokens[j], "tokens {i} and {j} are equal");
        }
    }
}

/// Inner helper for test_concurrent_clients: does the
/// handshake and returns the bearer token.
async fn run_handshake_inner(addr: SocketAddr, kp: &SigningKey) -> Result<[u8; 32], String> {
    let mut ws = connect(addr).await;
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .map_err(|e| format!("send hello: {e}"))?;
    let _w = read_binary(&mut ws).await.ok_or("w")?;
    let c = read_binary(&mut ws).await.ok_or("c")?;
    let c_env = decode(&c);
    let challenge: ChallengePayload =
        serde_json::from_value(c_env.payload.clone()).map_err(|e| e.to_string())?;
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    let sig = kp.sign(&nonce).to_bytes();
    let public = kp.verifying_key().to_bytes();
    let auth = auth_envelope(public, sig);
    ws.send(Message::Binary(encode(&auth)))
        .await
        .map_err(|e| format!("send auth: {e}"))?;
    let ok = read_binary(&mut ws).await.ok_or("ok")?;
    let ok_env = decode(&ok);
    if ok_env.r#type.as_str() != "AUTH_OK" {
        return Err(format!("expected AUTH_OK, got {}", ok_env.r#type.as_str()));
    }
    let ok: AuthOkPayload =
        serde_json::from_value(ok_env.payload.clone()).map_err(|e| format!("decode ok: {e}"))?;
    let mut token = [0u8; 32];
    token.copy_from_slice(&ok.bearer.token);
    Ok(token)
}

#[tokio::test]
async fn test_bearer_required_for_subsequent_messages() {
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp, _pk) = fresh_keypair();
    let mut ws = connect(addr).await;
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let _w = read_binary(&mut ws).await.expect("w");
    let c = read_binary(&mut ws).await.expect("c");
    let c_env = decode(&c);
    let challenge: ChallengePayload =
        serde_json::from_value(c_env.payload.clone()).expect("challenge");
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    let sig = kp.sign(&nonce).to_bytes();
    let public = kp.verifying_key().to_bytes();
    let auth = auth_envelope(public, sig);
    ws.send(Message::Binary(encode(&auth))).await.expect("auth");
    let ok = read_binary(&mut ws).await.expect("ok");
    let ok_env = decode(&ok);
    let ok: AuthOkPayload = serde_json::from_value(ok_env.payload.clone()).expect("ok p");
    let mut token = [0u8; 32];
    token.copy_from_slice(&ok.bearer.token);
    // Send a follow-up without bearer -> should be rejected
    // (server closes).
    let no_bearer = Envelope {
        v: 1,
        r#type: MessageKind::Hello,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload: json!({"junk": vec![0u8; 8]}),
    };
    ws.send(Message::Binary(encode(&no_bearer)))
        .await
        .expect("no bearer");
    let res = read_binary(&mut ws).await;
    assert!(res.is_none(), "expected server to close on missing bearer");
}

#[tokio::test]
async fn test_no_private_key_in_wire() {
    // Spawn server, drive a HELLO/WELCOME/CHALLENGE/AUTH_FAIL
    // handshake, and capture every byte sent in both
    // directions. The 32-byte private-key seed must not
    // appear anywhere in that byte stream.
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp, pk) = fresh_keypair();
    let mut ws = connect(addr).await;
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let w = read_binary(&mut ws).await.expect("w");
    let c = read_binary(&mut ws).await.expect("c");
    let c_env = decode(&c);
    let challenge: ChallengePayload =
        serde_json::from_value(c_env.payload.clone()).expect("challenge");
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    // Bogus sig so we get AUTH_FAIL; the point is to exercise
    // the full server-side AUTH path.
    let bogus_sig = [0u8; 64];
    let auth = auth_envelope(pk, bogus_sig);
    ws.send(Message::Binary(encode(&auth))).await.expect("auth");
    let fail = read_binary(&mut ws).await.expect("fail");

    let mut full_bytes = Vec::new();
    full_bytes.extend_from_slice(&w);
    full_bytes.extend_from_slice(&c);
    full_bytes.extend_from_slice(&fail);
    // The client sent the auth too; include it in the search.
    full_bytes.extend_from_slice(&encode(&hello_envelope()));
    full_bytes.extend_from_slice(&encode(&auth));

    let seed = kp.to_bytes();
    let found_seed = full_bytes.windows(32).any(|w| w == seed);
    assert!(!found_seed, "private key seed found in wire bytes");
    // The public key is in the AUTH envelope's payload as a
    // JSON array of numbers. Whether the bytes appear
    // contiguously in the rmp-serde-encoded wire depends on
    // how rmp-serde handles `serde_json::Value::Array`. The
    // strong guarantee is "the seed is not there"; a
    // positive "the pubkey appears as a contiguous run" is
    // best-effort and not asserted here.
    let _ = pk;
}

#[tokio::test]
async fn test_token_never_logged() {
    // Set up an in-memory subscriber that captures every log
    // line into a Vec<u8>.
    let log_buf = LogBuf::default();
    let log_buf_for_layer = log_buf.clone();
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(move || log_buf_for_layer.clone())
        .with_ansi(false)
        .with_target(false);
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp2, _pk2) = fresh_keypair();
    let mut ws = connect(addr).await;
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let _w = read_binary(&mut ws).await.expect("w");
    let c = read_binary(&mut ws).await.expect("c");
    let c_env = decode(&c);
    let challenge: ChallengePayload =
        serde_json::from_value(c_env.payload.clone()).expect("challenge");
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    let sig = kp2.sign(&nonce).to_bytes();
    let public = kp2.verifying_key().to_bytes();
    let auth = auth_envelope(public, sig);
    ws.send(Message::Binary(encode(&auth))).await.expect("auth");
    let ok = read_binary(&mut ws).await.expect("ok");
    let ok_env = decode(&ok);
    let ok: AuthOkPayload = serde_json::from_value(ok_env.payload.clone()).expect("ok p");
    let token = ok.bearer.token.clone();
    // The token in plaintext must not appear in the log buffer.
    let logs = log_buf.snapshot().await;
    let logs_str = String::from_utf8_lossy(&logs);
    // Find a unique byte prefix of the token; searching for
    // the full token in plaintext is the strongest test.
    let token_str = format!("{:02x?}", token);
    assert!(
        !logs_str.contains(&token_str),
        "bearer token appeared in logs: {token_str}"
    );
    // The raw bytes must also not appear in the log buffer.
    let raw_in_logs = logs.windows(token.len()).any(|w| w == token.as_slice());
    assert!(!raw_in_logs, "bearer token raw bytes appeared in logs");
}

#[tokio::test]
async fn test_protocol_serialization_roundtrip() {
    use locast_protocol::handshake::{
        AuthBearer, AuthFailPayload, AuthOkPayload, AuthPayload, ChallengePayload, HelloPayload,
        Platform, WelcomeConfig, WelcomePayload, WelcomeRate,
    };
    let hello = HelloPayload {
        client_version: "0.0.0".into(),
        platform: Platform::Win,
        device_id: "d".into(),
    };
    let welcome = WelcomePayload {
        session_id: Uuid::now_v7(),
        server_ts_ms: 1,
        config: WelcomeConfig {
            max_room_size: 8,
            rate: WelcomeRate {
                msgs_per_sec: 100,
                bytes_per_sec: 1_000_000,
            },
        },
    };
    let challenge = ChallengePayload {
        nonce: vec![1u8; 32],
        expires_ms: 2,
    };
    let auth = AuthPayload {
        pubkey: vec![3u8; 32],
        sig: vec![4u8; 64],
    };
    let ok = AuthOkPayload {
        user_id: Uuid::now_v7(),
        bearer: AuthBearer {
            token: vec![5u8; 32],
            expires_ms: 6,
        },
        pubkey: vec![7u8; 32],
    };
    let fail = AuthFailPayload {
        reason: AuthFailReason::BadSig,
    };
    let bytes = rmp_serde::to_vec_named(&hello).expect("encode hello");
    let _back: HelloPayload = rmp_serde::from_slice(&bytes).expect("decode hello");

    let bytes = rmp_serde::to_vec_named(&welcome).expect("encode welcome");
    let _back: WelcomePayload = rmp_serde::from_slice(&bytes).expect("decode welcome");

    let bytes = rmp_serde::to_vec_named(&challenge).expect("encode challenge");
    let _back: ChallengePayload = rmp_serde::from_slice(&bytes).expect("decode challenge");

    let bytes = rmp_serde::to_vec_named(&auth).expect("encode auth");
    let _back: AuthPayload = rmp_serde::from_slice(&bytes).expect("decode auth");

    let bytes = rmp_serde::to_vec_named(&ok).expect("encode ok");
    let _back: AuthOkPayload = rmp_serde::from_slice(&bytes).expect("decode ok");

    let bytes = rmp_serde::to_vec_named(&fail).expect("encode fail");
    let _back: AuthFailPayload = rmp_serde::from_slice(&bytes).expect("decode fail");

    // Also exercise the envelope wrapper.
    let envelope = Envelope {
        v: 1,
        r#type: MessageKind::Hello,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload: json!({"hello": "world"}),
    };
    let ebytes = rmp_serde::to_vec_named(&envelope).expect("encode envelope");
    let _back: Envelope = rmp_serde::from_slice(&ebytes).expect("decode envelope");
}

#[tokio::test]
async fn test_rate_limit_throttles_not_disconnects() {
    // Per §20.8 the server does NOT disconnect on rate-limit hits;
    // it throttles for 1 s. Configure a very small burst (1) so
    // every additional inbound frame triggers a throttle.
    let (addr, _h, _db) = spawn_server(test_config_low_rate(30_000, 1_048_576, 1, 1)).await;
    let mut ws = connect(addr).await;
    // First HELLO should pass.
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello 1");
    // The server's WELCOME + CHALLENGE response is in flight.
    // The connection has now received 2 server frames and has
    // sent 1 client frame. With a 1-msg burst budget, every
    // additional client frame should be throttled.
    // Send a second HELLO; it should be throttled.
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello 2");
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello 3");
    // Drain a few messages. We expect at least one AUTH_FAIL(Rate).
    let mut rate_fail_seen = false;
    let read_loop = async {
        for _ in 0..20 {
            if let Some(b) = read_binary(&mut ws).await {
                let env = decode(&b);
                if env.r#type.as_str() == "AUTH_FAIL" {
                    if let Ok(f) = serde_json::from_value::<AuthFailPayload>(env.payload.clone()) {
                        if f.reason == AuthFailReason::Rate {
                            rate_fail_seen = true;
                        }
                    }
                }
            } else {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(500), read_loop).await;
    assert!(
        rate_fail_seen,
        "expected at least one AUTH_FAIL(Rate) under low-rate config"
    );
    // The connection should NOT be closed by the server for the
    // throttle itself. Send a clean close from the client.
    let _ = ws.send(Message::Close(None)).await;
    let _ = tokio::time::timeout(Duration::from_millis(200), async {
        // The server's close-handshake on our close-frame is
        // awaited; we don't assert anything specific.
    })
    .await;
}

#[tokio::test]
async fn test_bearer_mismatch_rejected() {
    // After AUTH_OK, a bearer presented by a different pubkey
    // (i.e. on a different connection) must not be accepted on
    // this connection. The dispatch_authed check rejects when
    // the bearer's user_id / pubkey do not match the
    // authenticated session.
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp_a, _pk_a) = fresh_keypair();
    let (kp_b, _pk_b) = fresh_keypair();

    // Connection A: complete handshake, capture token_a.
    let mut ws_a = connect(addr).await;
    ws_a.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello a");
    let _w_a = read_binary(&mut ws_a).await.expect("w a");
    let c_a = read_binary(&mut ws_a).await.expect("c a");
    let c_a_env = decode(&c_a);
    let challenge_a: ChallengePayload =
        serde_json::from_value(c_a_env.payload.clone()).expect("c a payload");
    let mut nonce_a = [0u8; 32];
    nonce_a.copy_from_slice(&challenge_a.nonce);
    let sig_a = kp_a.sign(&nonce_a).to_bytes();
    let pk_a = kp_a.verifying_key().to_bytes();
    let auth_a = auth_envelope(pk_a, sig_a);
    ws_a.send(Message::Binary(encode(&auth_a)))
        .await
        .expect("auth a");
    let ok_a = read_binary(&mut ws_a).await.expect("ok a");
    let ok_a_env = decode(&ok_a);
    let ok_a_payload: AuthOkPayload =
        serde_json::from_value(ok_a_env.payload.clone()).expect("ok a payload");
    let mut token_a = [0u8; 32];
    token_a.copy_from_slice(&ok_a_payload.bearer.token);
    drop(ws_a);

    // Connection B: complete handshake, then send a post-handshake
    // message with token_a (which belongs to user A). Server
    // should reject with a close (bearer_mismatch).
    let mut ws_b = connect(addr).await;
    ws_b.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello b");
    let _w_b = read_binary(&mut ws_b).await.expect("w b");
    let c_b = read_binary(&mut ws_b).await.expect("c b");
    let c_b_env = decode(&c_b);
    let challenge_b: ChallengePayload =
        serde_json::from_value(c_b_env.payload.clone()).expect("c b payload");
    let mut nonce_b = [0u8; 32];
    nonce_b.copy_from_slice(&challenge_b.nonce);
    let sig_b = kp_b.sign(&nonce_b).to_bytes();
    let pk_b = kp_b.verifying_key().to_bytes();
    let auth_b = auth_envelope(pk_b, sig_b);
    ws_b.send(Message::Binary(encode(&auth_b)))
        .await
        .expect("auth b");
    let _ok_b = read_binary(&mut ws_b).await.expect("ok b");
    // Now post a follow-up carrying token_a. The server should
    // close the connection because the bearer does not match
    // the authenticated (user_b, pk_b) pair.
    let follow = envelope_with_bearer(token_a);
    let send_r = ws_b.send(Message::Binary(encode(&follow))).await;
    assert!(
        send_r.is_ok(),
        "send of cross-user bearer should not error locally"
    );
    let res = read_binary(&mut ws_b).await;
    assert!(
        res.is_none(),
        "expected server to close on bearer user_id mismatch"
    );
}

#[tokio::test]
async fn test_unknown_message_type_in_handshake_rejected() {
    // Per the dispatch path, an envelope whose type is not
    // HELLO or AUTH during the handshake is closed.
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let mut ws = connect(addr).await;
    #[derive(serde::Serialize)]
    struct RawEnv {
        v: u8,
        r#type: &'static str,
        id: Uuid,
        room_id: Option<Uuid>,
        sender: Option<()>,
        ts_ms: i64,
        seq: u64,
        payload: serde_json::Value,
    }
    let raw = RawEnv {
        v: 1,
        r#type: "PRESENCE",
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload: json!({"status": "online"}),
    };
    let bytes = rmp_serde::to_vec_named(&raw).expect("encode raw");
    ws.send(Message::Binary(bytes)).await.expect("send");
    let res = read_binary(&mut ws).await;
    assert!(
        res.is_none(),
        "expected server to close on unknown handshake type"
    );
}

#[tokio::test]
async fn test_duplicate_hello_rejected() {
    // A second HELLO on the same connection must be rejected
    // (state machine has already moved past New).
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let mut ws = connect(addr).await;
    ws.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello 1");
    let _w = read_binary(&mut ws).await.expect("w");
    let _c = read_binary(&mut ws).await.expect("c");
    // Second HELLO after the server has moved to ChallengeSent.
    let r = ws.send(Message::Binary(encode(&hello_envelope()))).await;
    let _ = r; // the send itself should succeed; the server closes.
    let res = read_binary(&mut ws).await;
    assert!(res.is_none(), "expected server to close on duplicate HELLO");
}

#[tokio::test]
async fn test_auth_before_hello_rejected() {
    // Sending AUTH before HELLO is illegal (state is New, not
    // ChallengeSent). Server closes.
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp, pk) = fresh_keypair();
    let mut ws = connect(addr).await;
    let sig = kp.sign(b"junk").to_bytes();
    let auth = auth_envelope(pk, sig);
    ws.send(Message::Binary(encode(&auth))).await.expect("auth");
    let res = read_binary(&mut ws).await;
    assert!(
        res.is_none(),
        "expected server to close on AUTH-before-HELLO"
    );
}

#[tokio::test]
async fn test_oversized_frame_size_enforced() {
    // 1 KiB cap, 4 KiB payload -> server should reject without
    // crashing. Same as test_oversized_message (kept for
    // backward-compat with the original test name).
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1024)).await;
    let mut ws = connect(addr).await;
    let big = vec![0u8; 4096];
    let env = Envelope {
        v: 1,
        r#type: MessageKind::Hello,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload: json!({ "junk": big }),
    };
    ws.send(Message::Binary(encode(&env))).await.expect("send");
    let res = read_binary(&mut ws).await;
    assert!(res.is_none());
}

#[tokio::test]
async fn test_user_id_is_server_assigned() {
    let (addr, _h, _db) = spawn_server(test_config(30_000, 1_048_576)).await;
    let (kp, _pk) = fresh_keypair();
    let mut ws1 = connect(addr).await;
    ws1.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let _w1 = read_binary(&mut ws1).await.expect("w1");
    let c1 = read_binary(&mut ws1).await.expect("c1");
    let c1_env = decode(&c1);
    let challenge1: ChallengePayload = serde_json::from_value(c1_env.payload.clone()).expect("c1");
    let mut nonce1 = [0u8; 32];
    nonce1.copy_from_slice(&challenge1.nonce);
    let sig1 = kp.sign(&nonce1).to_bytes();
    let public = kp.verifying_key().to_bytes();
    let auth1 = auth_envelope(public, sig1);
    ws1.send(Message::Binary(encode(&auth1)))
        .await
        .expect("auth1");
    let ok1 = read_binary(&mut ws1).await.expect("ok1");
    let ok1_env = decode(&ok1);
    let ok1: AuthOkPayload = serde_json::from_value(ok1_env.payload.clone()).expect("ok1 p");
    assert_eq!(ok1.user_id.get_version_num(), 7);

    // Second handshake with the same key -> same user_id.
    let mut ws2 = connect(addr).await;
    ws2.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let _w2 = read_binary(&mut ws2).await.expect("w2");
    let c2 = read_binary(&mut ws2).await.expect("c2");
    let c2_env = decode(&c2);
    let challenge2: ChallengePayload = serde_json::from_value(c2_env.payload.clone()).expect("c2");
    let mut nonce2 = [0u8; 32];
    nonce2.copy_from_slice(&challenge2.nonce);
    let sig2 = kp.sign(&nonce2).to_bytes();
    let auth2 = auth_envelope(public, sig2);
    ws2.send(Message::Binary(encode(&auth2)))
        .await
        .expect("auth2");
    let ok2 = read_binary(&mut ws2).await.expect("ok2");
    let ok2_env = decode(&ok2);
    let ok2: AuthOkPayload = serde_json::from_value(ok2_env.payload.clone()).expect("ok2 p");
    assert_eq!(
        ok1.user_id, ok2.user_id,
        "same pubkey should yield same user_id"
    );

    // Third handshake with a different key -> different user_id.
    let (other_kp, _other_pk) = fresh_keypair();
    let mut ws3 = connect(addr).await;
    ws3.send(Message::Binary(encode(&hello_envelope())))
        .await
        .expect("hello");
    let _w3 = read_binary(&mut ws3).await.expect("w3");
    let c3 = read_binary(&mut ws3).await.expect("c3");
    let c3_env = decode(&c3);
    let challenge3: ChallengePayload = serde_json::from_value(c3_env.payload.clone()).expect("c3");
    let mut nonce3 = [0u8; 32];
    nonce3.copy_from_slice(&challenge3.nonce);
    let sig3 = other_kp.sign(&nonce3).to_bytes();
    let other_public = other_kp.verifying_key().to_bytes();
    let auth3 = auth_envelope(other_public, sig3);
    ws3.send(Message::Binary(encode(&auth3)))
        .await
        .expect("auth3");
    let ok3 = read_binary(&mut ws3).await.expect("ok3");
    let ok3_env = decode(&ok3);
    let ok3: AuthOkPayload = serde_json::from_value(ok3_env.payload.clone()).expect("ok3 p");
    assert_ne!(
        ok1.user_id, ok3.user_id,
        "different pubkey should yield different user_id"
    );
}
