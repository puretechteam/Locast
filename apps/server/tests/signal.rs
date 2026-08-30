//! Integration tests for P3-T05 SIGNAL relay (server side).
//!
//! These are pure server tests that exercise `handle_signal`
//! via `dispatch_room_message` (the WS layer's
//! `dispatch_authed` calls into it). The two-client e2e
//! WebRTC signaling test lives in
//! `apps/client/src-tauri/tests/webrtc_signaling.rs` (which
//! is NOT this crate's test target).
//!
//! The tests cover:
//!
//! - the per-envelope Ed25519 signature check
//!   (good / bad / wrong-pubkey / missing-sender)
//! - the room-membership checks for sender and recipient
//! - the 64 KiB application-layer size cap
//! - the self-signal rejection
//! - the SignalRelay forward path (B receives what A sent)

#![forbid(unsafe_code)]

use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey};
use locast_protocol::envelope::{Envelope, MessageKind, Sender};
use locast_protocol::room::{RoomErrorCode, RoomErrorPayload, SignalKind, SignalPayload};
use locast_server::rooms::{
    dispatch_room_message, DbRoomStore, RoomRegistry, RoomRegistryConfig, SignalRelay,
};
use locast_server::time::MockClock;
use locast_server::Clock;
use locast_server::{rooms::DispatchContext, Config, Db};
use rand::RngCore;
use serde_json::json;
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
        host_disconnect_grace_ms: 200,
        room_create_max_collisions: 5,
        participant_stale_after_ms: 300_000,
    }
}

fn fresh_keypair() -> (SigningKey, [u8; 32]) {
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    let signing = SigningKey::from_bytes(&seed);
    let public = signing.verifying_key().to_bytes();
    (signing, public)
}

/// Build a room + second participant directly in the
/// registry. Returns `(host_sk, host_uid, host_pk, room_id,
/// viewer_uid, viewer_pk)` so the caller can sign with the
/// host's actual signing key (NOT with the public key bytes
/// as a seed — that would produce a different keypair).
async fn build_room_with_two_users(
    db: &Db,
    rooms: &RoomRegistry,
    clock: &MockClock,
) -> (SigningKey, Uuid, [u8; 32], Uuid, Uuid, [u8; 32]) {
    let store = DbRoomStore::new(db.clone());
    let (host_sk, host_pk) = fresh_keypair();
    let host_user_id = db.upsert_user(&host_pk).await.expect("upsert host");
    let (room, _self_view) = rooms
        .create(
            &store,
            "T".into(),
            host_user_id,
            host_pk,
            true,
            clock.now_ms(),
        )
        .await
        .expect("create room");
    let (_viewer_sk, viewer_pk) = fresh_keypair();
    let viewer_user_id = db.upsert_user(&viewer_pk).await.expect("upsert viewer");
    let (_joined, _evt) = rooms
        .join(
            &store,
            &room.code,
            viewer_user_id,
            viewer_pk,
            "viewer".into(),
            clock.now_ms(),
        )
        .await
        .expect("viewer joins");
    (
        host_sk,
        host_user_id,
        host_pk,
        room.id,
        viewer_user_id,
        viewer_pk,
    )
}

fn signal_envelope(
    sender_uid: Uuid,
    sender_pubkey: [u8; 32],
    sig: [u8; 64],
    to_user_id: Uuid,
    room_id: Uuid,
    kind: SignalKind,
    sdp: Option<&str>,
) -> Envelope {
    Envelope {
        v: 1,
        r#type: MessageKind::Signal,
        id: Uuid::now_v7(),
        room_id: Some(room_id),
        sender: Some(Sender {
            user_id: sender_uid,
            pubkey: sender_pubkey.to_vec(),
            sig: sig.to_vec(),
        }),
        ts_ms: 0,
        seq: 1,
        payload: json!(SignalPayload {
            to_user_id,
            kind,
            sdp: sdp.map(|s| s.to_string()),
            candidates: None,
        }),
    }
}

fn sign_signal(sk: &SigningKey, payload: &SignalPayload) -> [u8; 64] {
    let signed = locast_crypto::signal_signed_bytes(payload).expect("encode");
    sk.sign(&signed).to_bytes()
}

#[tokio::test]
async fn signal_with_bad_signature_returns_invalid_state() {
    let cfg = test_config();
    let db = Db::open(&cfg).await.expect("open db");
    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&cfg)));
    let clock = MockClock::new(1_000_000);
    let store: Arc<dyn locast_server::rooms::RoomStore> = Arc::new(DbRoomStore::new(db.clone()));
    let relay = SignalRelay::new();
    let ctx = DispatchContext {
        registry: &rooms,
        store: store.as_ref(),
        db: &db,
        clock: &clock,
        relay: &relay,
    };

    let (host_sk, host_uid, host_pk, room_id, viewer_uid, _viewer_pk) =
        build_room_with_two_users(&db, &rooms, &clock).await;

    let payload = SignalPayload {
        to_user_id: viewer_uid,
        kind: SignalKind::Offer,
        sdp: Some("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n".into()),
        candidates: None,
    };
    // Sign with the wrong key. The envelope's `sender.pubkey`
    // still claims host_pubkey, but the signature itself
    // is over a different payload (signed by an unrelated
    // key). The server must reject as InvalidState.
    let (_other_sk, _other_pk) = fresh_keypair();
    let bad_sig = [0xAAu8; 64];
    let _ = host_sk; // unused in this test (we inject a fake sig)
    let env = signal_envelope(
        host_uid,
        host_pk,
        bad_sig,
        viewer_uid,
        room_id,
        SignalKind::Offer,
        Some("v=0\r\n"),
    );
    let _ = &payload; // payload reference is unused in this path; the env payload is canonical

    let out = dispatch_room_message(env, &ctx, host_uid, host_pk).await;
    assert_eq!(out.to_caller.len(), 1);
    assert_eq!(out.to_caller[0].r#type, MessageKind::RoomError);
    let p: RoomErrorPayload = serde_json::from_value(out.to_caller[0].payload.clone()).unwrap();
    assert_eq!(p.code, RoomErrorCode::InvalidState);
    assert!(!relay.is_registered(viewer_uid).await);
}

#[tokio::test]
async fn signal_with_missing_sender_returns_invalid_state() {
    let cfg = test_config();
    let db = Db::open(&cfg).await.expect("open db");
    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&cfg)));
    let clock = MockClock::new(1_000_000);
    let store: Arc<dyn locast_server::rooms::RoomStore> = Arc::new(DbRoomStore::new(db.clone()));
    let relay = SignalRelay::new();
    let ctx = DispatchContext {
        registry: &rooms,
        store: store.as_ref(),
        db: &db,
        clock: &clock,
        relay: &relay,
    };

    let (_host_sk, host_uid, host_pk, room_id, viewer_uid, _viewer_pk) =
        build_room_with_two_users(&db, &rooms, &clock).await;

    let mut env = signal_envelope(
        host_uid,
        host_pk,
        [0u8; 64],
        viewer_uid,
        room_id,
        SignalKind::Offer,
        Some("v=0\r\n"),
    );
    env.sender = None; // no sender block at all

    let out = dispatch_room_message(env, &ctx, host_uid, host_pk).await;
    assert_eq!(out.to_caller.len(), 1);
    assert_eq!(out.to_caller[0].r#type, MessageKind::RoomError);
    let p: RoomErrorPayload = serde_json::from_value(out.to_caller[0].payload.clone()).unwrap();
    assert_eq!(p.code, RoomErrorCode::InvalidState);
}

#[tokio::test]
async fn signal_with_identity_mismatch_returns_invalid_state() {
    let cfg = test_config();
    let db = Db::open(&cfg).await.expect("open db");
    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&cfg)));
    let clock = MockClock::new(1_000_000);
    let store: Arc<dyn locast_server::rooms::RoomStore> = Arc::new(DbRoomStore::new(db.clone()));
    let relay = SignalRelay::new();
    let ctx = DispatchContext {
        registry: &rooms,
        store: store.as_ref(),
        db: &db,
        clock: &clock,
        relay: &relay,
    };

    let (host_sk, host_uid, host_pk, room_id, viewer_uid, _viewer_pk) =
        build_room_with_two_users(&db, &rooms, &clock).await;

    // Build a valid SIGNAL with the host's signature, but
    // rewrite `envelope.sender.pubkey` to a different key.
    let payload = SignalPayload {
        to_user_id: viewer_uid,
        kind: SignalKind::Offer,
        sdp: Some("v=0\r\n".into()),
        candidates: None,
    };
    let sig = sign_signal(&host_sk, &payload);
    let (_attacker_sk, attacker_pk) = fresh_keypair();

    let env = signal_envelope(
        host_uid,
        attacker_pk, // claimed pubkey != bearer pubkey
        sig,
        viewer_uid,
        room_id,
        SignalKind::Offer,
        Some("v=0\r\n"),
    );

    let out = dispatch_room_message(env, &ctx, host_uid, host_pk).await;
    assert_eq!(out.to_caller.len(), 1);
    assert_eq!(out.to_caller[0].r#type, MessageKind::RoomError);
    let p: RoomErrorPayload = serde_json::from_value(out.to_caller[0].payload.clone()).unwrap();
    assert_eq!(p.code, RoomErrorCode::InvalidState);
}

#[tokio::test]
async fn signal_to_non_member_returns_not_joined() {
    let cfg = test_config();
    let db = Db::open(&cfg).await.expect("open db");
    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&cfg)));
    let clock = MockClock::new(1_000_000);
    let store: Arc<dyn locast_server::rooms::RoomStore> = Arc::new(DbRoomStore::new(db.clone()));
    let relay = SignalRelay::new();
    let ctx = DispatchContext {
        registry: &rooms,
        store: store.as_ref(),
        db: &db,
        clock: &clock,
        relay: &relay,
    };

    let (host_sk, host_uid, host_pk, room_id, _viewer_uid, _viewer_pk) =
        build_room_with_two_users(&db, &rooms, &clock).await;

    // Build a SIGNAL targeting a user_id that has NOT
    // joined the room. The recipient check must fail.
    let bogus_recipient = Uuid::now_v7();
    let payload = SignalPayload {
        to_user_id: bogus_recipient,
        kind: SignalKind::Offer,
        sdp: Some("v=0\r\n".into()),
        candidates: None,
    };
    let sig = sign_signal(&host_sk, &payload);
    let env = signal_envelope(
        host_uid,
        host_pk,
        sig,
        bogus_recipient,
        room_id,
        SignalKind::Offer,
        Some("v=0\r\n"),
    );

    let out = dispatch_room_message(env, &ctx, host_uid, host_pk).await;
    assert_eq!(out.to_caller.len(), 1);
    assert_eq!(out.to_caller[0].r#type, MessageKind::RoomError);
    let p: RoomErrorPayload = serde_json::from_value(out.to_caller[0].payload.clone()).unwrap();
    assert_eq!(p.code, RoomErrorCode::NotJoined);
}

#[tokio::test]
async fn signal_to_self_returns_invalid_state() {
    let cfg = test_config();
    let db = Db::open(&cfg).await.expect("open db");
    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&cfg)));
    let clock = MockClock::new(1_000_000);
    let store: Arc<dyn locast_server::rooms::RoomStore> = Arc::new(DbRoomStore::new(db.clone()));
    let relay = SignalRelay::new();
    let ctx = DispatchContext {
        registry: &rooms,
        store: store.as_ref(),
        db: &db,
        clock: &clock,
        relay: &relay,
    };

    let (host_sk, host_uid, host_pk, room_id, _viewer_uid, _viewer_pk) =
        build_room_with_two_users(&db, &rooms, &clock).await;

    let payload = SignalPayload {
        to_user_id: host_uid, // self
        kind: SignalKind::Offer,
        sdp: Some("v=0\r\n".into()),
        candidates: None,
    };
    let sig = sign_signal(&host_sk, &payload);
    let env = signal_envelope(
        host_uid,
        host_pk,
        sig,
        host_uid,
        room_id,
        SignalKind::Offer,
        Some("v=0\r\n"),
    );

    let out = dispatch_room_message(env, &ctx, host_uid, host_pk).await;
    assert_eq!(out.to_caller.len(), 1);
    let p: RoomErrorPayload = serde_json::from_value(out.to_caller[0].payload.clone()).unwrap();
    assert_eq!(p.code, RoomErrorCode::InvalidState);
}

#[tokio::test]
async fn signal_forwarded_to_recipient() {
    let cfg = test_config();
    let db = Db::open(&cfg).await.expect("open db");
    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&cfg)));
    let clock = MockClock::new(1_000_000);
    let store: Arc<dyn locast_server::rooms::RoomStore> = Arc::new(DbRoomStore::new(db.clone()));
    let relay = SignalRelay::new();
    let ctx = DispatchContext {
        registry: &rooms,
        store: store.as_ref(),
        db: &db,
        clock: &clock,
        relay: &relay,
    };

    let (host_sk, host_uid, host_pk, room_id, viewer_uid, _viewer_pk) =
        build_room_with_two_users(&db, &rooms, &clock).await;

    // Subscribe the viewer's outbound channel BEFORE
    // dispatching, so we can verify the relay delivered
    // the envelope.
    let (b_tx, mut b_rx) = tokio::sync::mpsc::unbounded_channel::<Envelope>();
    relay.register(viewer_uid, b_tx).await;
    assert!(relay.is_registered(viewer_uid).await);

    let payload = SignalPayload {
        to_user_id: viewer_uid,
        kind: SignalKind::Offer,
        sdp: Some("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n".into()),
        candidates: None,
    };
    let sig = sign_signal(&host_sk, &payload);
    let env = signal_envelope(
        host_uid,
        host_pk,
        sig,
        viewer_uid,
        room_id,
        SignalKind::Offer,
        Some("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n"),
    );

    let out = dispatch_room_message(env, &ctx, host_uid, host_pk).await;
    assert!(
        out.to_caller.is_empty(),
        "no caller-side envelope on success"
    );

    // B receives the envelope on its outbound channel.
    let received = tokio::time::timeout(std::time::Duration::from_secs(1), b_rx.recv())
        .await
        .expect("no timeout")
        .expect("channel closed");
    assert_eq!(received.r#type, MessageKind::Signal);
    assert_eq!(received.room_id, Some(room_id));
    let recv_payload: SignalPayload = serde_json::from_value(received.payload.clone()).unwrap();
    assert_eq!(recv_payload.to_user_id, viewer_uid);
    assert_eq!(recv_payload.kind, SignalKind::Offer);
}

#[tokio::test]
async fn signal_oversized_returns_invalid_state() {
    let cfg = test_config();
    let db = Db::open(&cfg).await.expect("open db");
    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&cfg)));
    let clock = MockClock::new(1_000_000);
    let store: Arc<dyn locast_server::rooms::RoomStore> = Arc::new(DbRoomStore::new(db.clone()));
    let relay = SignalRelay::new();
    let ctx = DispatchContext {
        registry: &rooms,
        store: store.as_ref(),
        db: &db,
        clock: &clock,
        relay: &relay,
    };

    let (host_sk, host_uid, host_pk, room_id, viewer_uid, _viewer_pk) =
        build_room_with_two_users(&db, &rooms, &clock).await;

    // Build an SDP body larger than 64 KiB. The server's
    // app-layer cap is 64 KiB per envelope (docs/ARCHITECTURE.md
    // §18.5.1). Even though the WS transport allows 1 MiB,
    // SIGNAL must be capped to 64 KiB.
    let huge_sdp: String = "a".repeat(65 * 1024);
    let payload = SignalPayload {
        to_user_id: viewer_uid,
        kind: SignalKind::Offer,
        sdp: Some(huge_sdp.clone()),
        candidates: None,
    };
    let sig = sign_signal(&host_sk, &payload);
    let env = signal_envelope(
        host_uid,
        host_pk,
        sig,
        viewer_uid,
        room_id,
        SignalKind::Offer,
        Some(&huge_sdp),
    );

    let out = dispatch_room_message(env, &ctx, host_uid, host_pk).await;
    assert_eq!(out.to_caller.len(), 1);
    let p: RoomErrorPayload = serde_json::from_value(out.to_caller[0].payload.clone()).unwrap();
    assert_eq!(p.code, RoomErrorCode::InvalidState);
}
