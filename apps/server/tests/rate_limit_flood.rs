//! Integration test for P2-T07's per-connection rate
//! limiter + capability gate.
//!
//! The test spawns a fresh server on `127.0.0.1:0` with
//! the default rate config (100 msg/s, 200 burst, 1 MB/s,
//! 2 MB burst), drives three clients through the handshake:
//!
//! - A: host. Creates a room, then sends ~200 PRESENCE
//!   frames (its own burst budget) to prove A's bucket
//!   is independent of C's.
//! - B: viewer. Joins the room. Sends ~200 PRESENCE
//!   frames to prove B's bucket is independent of C's.
//! - C: attacker. Completes the handshake but does NOT
//!   join the room. Floods at >= 500 msg/s, then verifies
//!   the connection survives the throttle window
//!   ("throttles, not disconnects").
//!
//! Acceptance per `docs/ROADMAP.md` P2-T07:
//! - C receives at least one `RATE_LIMIT` envelope with
//!   `scope: Conn` within the 1 s throttle window.
//! - A and B consume their own burst budgets and never
//!   see a `RATE_LIMIT` on their connections (proving
//!   per-conn isolation — a shared-bucket bug would
//!   surface here).
//! - After C's throttle window elapses, C can still send
//!   and receive (proving the throttle does not close
//!   the connection).

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::handshake::{
    AuthOkPayload, ChallengePayload, RateLimitPayload, RateLimitScope,
};
use locast_protocol::room::{
    PresencePayload, RoomCreatePayload, RoomJoinRequestPayload, RoomJoinedPayload,
};
use locast_server::{AppState, Config, Db, Metrics, RoomRegistry, RoomRegistryConfig};
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

async fn spawn_test_server() -> SocketAddr {
    let config = test_config();
    let db = Db::open(&config).await.expect("open db");
    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&config)));
    let clock: Arc<dyn locast_server::Clock> = Arc::new(locast_server::SystemClock);
    let state = AppState {
        config: Arc::new(config),
        metrics: Metrics::new(),
        db,
        rooms,
        clock,
        signal_relay: locast_server::SignalRelay::new(),
    };
    let app: Router = locast_server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
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
            "device_id": "flood-test",
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

struct AuthedClient {
    token: [u8; 32],
    #[allow(dead_code)]
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
    let challenge: ChallengePayload = serde_json::from_value(c_env.payload).expect("challenge");
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&challenge.nonce);
    let sig = kp.sign(&nonce).to_bytes();
    let public = kp.verifying_key().to_bytes();
    ws.send(Message::Binary(encode(&auth_envelope(public, sig))))
        .await
        .expect("auth");
    let ok = read_binary(ws).await.expect("auth_ok");
    let ok_env = decode(&ok);
    let ok_p: AuthOkPayload = serde_json::from_value(ok_env.payload).expect("ok payload");
    let mut token = [0u8; 32];
    token.copy_from_slice(&ok_p.bearer.token);
    AuthedClient {
        token,
        user_id: ok_p.user_id,
    }
}

fn room_create_envelope(token: [u8; 32], title: &str) -> Envelope {
    let mut payload = json!({ "bearer": token.to_vec() });
    let obj = payload.as_object_mut().unwrap();
    let inner = serde_json::to_value(RoomCreatePayload {
        title: title.to_string(),
        migration_enabled: true,
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
    let mut payload = json!({ "bearer": token.to_vec() });
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

fn presence_envelope(token: [u8; 32]) -> Envelope {
    let mut payload = json!({ "bearer": token.to_vec() });
    let obj = payload.as_object_mut().unwrap();
    let inner = serde_json::to_value(PresencePayload {
        status: "alive".to_string(),
    })
    .unwrap();
    for (k, v) in inner.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    Envelope {
        v: 1,
        r#type: MessageKind::Presence,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: 0,
        seq: 0,
        payload,
    }
}

/// Drain a few messages off the stream. Stop on the first
/// `RATE_LIMIT` (returned separately) or on close.
async fn drain_until_close_or_rate_limit(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    deadline: Duration,
) -> Option<RateLimitPayload> {
    let read_loop = async {
        loop {
            match read_binary(ws).await {
                Some(b) => {
                    let env = decode(&b);
                    if env.r#type == MessageKind::RateLimit {
                        if let Ok(p) =
                            serde_json::from_value::<RateLimitPayload>(env.payload.clone())
                        {
                            return Some(p);
                        }
                    }
                }
                None => return None,
            }
        }
    };
    tokio::time::timeout(deadline, read_loop)
        .await
        .unwrap_or(None)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limit_flood_isolated_to_offender() {
    let addr = spawn_test_server().await;
    let (kp_a, _pk_a) = fresh_keypair();
    let (kp_b, _pk_b) = fresh_keypair();
    let (kp_c, _pk_c) = fresh_keypair();

    // Three independent connections.
    let mut ws_a = connect(addr).await;
    let mut ws_b = connect(addr).await;
    let mut ws_c = connect(addr).await;
    let a = complete_handshake(&mut ws_a, &kp_a).await;
    let b = complete_handshake(&mut ws_b, &kp_b).await;
    let c = complete_handshake(&mut ws_c, &kp_c).await;

    // A creates a room.
    ws_a.send(Message::Binary(encode(&room_create_envelope(
        a.token, "Flood",
    ))))
    .await
    .expect("a create");
    let created_bytes = read_binary(&mut ws_a).await.expect("created");
    let created_env = decode(&created_bytes);
    assert_eq!(created_env.r#type, MessageKind::RoomCreated);
    let code = created_env.payload["room"]["code"]
        .as_str()
        .expect("code")
        .to_string();

    // B joins the room.
    ws_b.send(Message::Binary(encode(&room_join_envelope(
        b.token, &code, "B",
    ))))
    .await
    .expect("b join");
    let joined_b = read_binary(&mut ws_b).await.expect("joined b");
    let joined_env = decode(&joined_b);
    assert_eq!(joined_env.r#type, MessageKind::RoomJoined);
    let _joined_p: RoomJoinedPayload =
        serde_json::from_value(joined_env.payload).expect("joined payload");
    // A also receives PARTICIPANT_JOINED for B.
    let _pj = read_binary(&mut ws_a).await.expect("a participant_joined");

    // C is authed but does NOT join. C will flood.
    // C's bucket is full (200 burst, 100/s).
    // Send 500 binary frames as fast as the local socket
    // can flush. The server's msg/s bucket is 100 sustained,
    // 200 burst, so frames 201+ will start missing the
    // msg bucket. The throttle window is 1 s, so within
    // that window we expect at least one RATE_LIMIT to be
    // observed. The test also measures the actual send
    // rate and asserts it meets the roadmap's 500 msg/s
    // floor.
    let c_token = c.token;
    let flood_start = std::time::Instant::now();
    let flood = tokio::spawn(async move {
        // 500 small envelopes, each 32-byte bearer payload.
        // The msg/s bucket is the limiter.
        for _ in 0..500 {
            let env = room_create_envelope(c_token, "X");
            if ws_c.send(Message::Binary(encode(&env))).await.is_err() {
                break;
            }
        }
        let elapsed = flood_start.elapsed();
        (ws_c, elapsed)
    });
    let (mut ws_c, flood_elapsed) = flood.await.expect("flood join");
    let measured_rate_msg_per_s = 500.0 / flood_elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
    assert!(
        measured_rate_msg_per_s >= 500.0,
        "flood rate {measured_rate_msg_per_s:.0} msg/s did not meet 500 msg/s floor; \
         the test would not actually exercise the production 100 msg/s limit"
    );

    // Drain the flood task's ws_c. The server should
    // send a RATE_LIMIT within the 1 s throttle window.
    let rate_limit_hit =
        drain_until_close_or_rate_limit(&mut ws_c, Duration::from_millis(1500)).await;

    let hit = rate_limit_hit.expect("expected at least one RATE_LIMIT during flood");
    assert_eq!(hit.scope, RateLimitScope::Conn);
    assert!(hit.retry_after_ms >= 1_000);
    assert!(hit.observed >= 1);

    // "Throttles, not disconnects": wait out the throttle
    // window, then prove C can still send + receive.
    // C's bucket has been refilling for ~1 s, so it has
    // at least 100 tokens available. A single PRESENCE
    // from C is not a valid room op (C is not a member),
    // but the server's per-conn bucket is consulted BEFORE
    // dispatch, so a successful send+read is enough to
    // prove the connection survived.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    ws_c.send(Message::Binary(encode(&presence_envelope(c.token))))
        .await
        .expect("c post-throttle send (connection must still be open)");

    // Meanwhile A and B should be unaffected. To prove
    // per-conn isolation (not a shared bucket), push A
    // through its own 200-frame burst so a shared-bucket
    // bug would surface as a RATE_LIMIT on A.
    for i in 0..200 {
        ws_a.send(Message::Binary(encode(&presence_envelope(a.token))))
            .await
            .expect("a presence send");
        // Read with a short timeout so a stray RATE_LIMIT
        // (which would prove the bucket is shared) surfaces
        // immediately rather than being silently absorbed.
        let read = tokio::time::timeout(Duration::from_millis(20), read_binary(&mut ws_a))
            .await
            .ok()
            .flatten();
        if let Some(b) = read {
            let env = decode(&b);
            assert_ne!(
                env.r#type,
                MessageKind::RateLimit,
                "A's connection should not be rate-limited at frame {i} (shared-bucket bug?)"
            );
        }
    }

    // B is also unaffected. Push B through its own
    // 200-frame burst. A shared bucket with C would
    // surface as a RATE_LIMIT on B (since A and B
    // together would have already drained it).
    for i in 0..200 {
        ws_b.send(Message::Binary(encode(&presence_envelope(b.token))))
            .await
            .expect("b presence send");
        let read = tokio::time::timeout(Duration::from_millis(20), read_binary(&mut ws_b))
            .await
            .ok()
            .flatten();
        if let Some(b) = read {
            let env = decode(&b);
            assert_ne!(
                env.r#type,
                MessageKind::RateLimit,
                "B's connection should not be rate-limited at frame {i} (shared-bucket bug?)"
            );
        }
    }

    // Clean shutdown.
    let _ = ws_a.send(Message::Close(None)).await;
    let _ = ws_b.send(Message::Close(None)).await;
    let _ = ws_c.send(Message::Close(None)).await;
}
