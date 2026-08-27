//! WebSocket transport and per-connection state machine.
//!
//! The per-connection task drives the auth handshake
//! ([`crate::auth::state::ConnState`]) and the post-handshake
//! message handling. v1 only validates that subsequent messages
//! carry a bearer token; the room lifecycle / playback / drawing
//! message types land in P3+.
//!
//! Sensitive values are NEVER logged:
//!
//! - bearer tokens (plaintext or hashed)
//! - signed nonce contents (the nonce itself is fine)
//! - private key material
//! - the AUTH signature bytes
//!
//! See `docs/ARCHITECTURE.md` section 21.14.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::handshake::{
    AuthBearer, AuthFailPayload, AuthFailReason, AuthOkPayload, AuthPayload, ChallengePayload,
    HelloPayload, WelcomeConfig, WelcomePayload, WelcomeRate,
};
use rand::RngCore;
use serde_json::json;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::auth::bearer;
use crate::auth::state::ConnState;
use crate::auth::{verify, AuthError};
use crate::AppState;

/// Per-connection rate limit. Defaults to 100 msg/s sustained, 200
/// burst (§18.6). The bucket is checked once per inbound message.
/// The values are sourced from [`crate::Config`] so tests can pin a
/// small rate and exercise the throttle logic deterministically.
/// The `DEFAULT_RATE_*` constants here are documentation only; the
/// authoritative defaults live in [`crate::config`].
#[allow(dead_code)]
const DEFAULT_RATE_SUSTAINED_PER_SEC: u32 = 100;
#[allow(dead_code)]
const DEFAULT_RATE_BURST: u32 = 200;

/// Per-connection bytes-per-second sustained rate and burst
/// (advertised in WELCOME per §18.6 / §20.6). Enforced on every
/// inbound frame; exceeding it triggers an AUTH_FAIL(Rate) message
/// but does NOT close the connection (§20.8).
#[allow(dead_code)]
const DEFAULT_RATE_BYTES_SUSTAINED_PER_SEC: u32 = 1_000_000;
#[allow(dead_code)]
const DEFAULT_RATE_BYTES_BURST: u32 = 2_000_000;

/// After this many decode / framing / version-mismatch failures on a
/// single connection in a 60 s rolling window, the server closes the
/// connection (§20.4.1). Below the threshold the server sends an
/// AUTH_FAIL(Rate)-style close and continues serving.
const BAD_MSG_THRESHOLD: usize = 3;
const BAD_MSG_WINDOW_MS: i64 = 60_000;

/// How long to throttle inbound frames after a rate-limit hit
/// (§20.8: "throttled for 1 s", do NOT disconnect).
const RATE_THROTTLE_MS: i64 = 1_000;

/// Rate-bucket state for a single connection.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: u32,
    refill_per_sec: u32,
    tokens: u32,
    last_refill_ms: i64,
}

impl Default for TokenBucket {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenBucket {
    pub fn new() -> Self {
        Self::new_with(DEFAULT_RATE_BURST, DEFAULT_RATE_SUSTAINED_PER_SEC)
    }

    pub fn new_with(capacity: u32, refill_per_sec: u32) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity,
            last_refill_ms: now_ms(),
        }
    }

    /// Try to consume one token. Returns `true` if the
    /// connection is under the rate limit, `false` otherwise.
    pub fn try_consume(&mut self) -> bool {
        let now = now_ms();
        let delta = (now - self.last_refill_ms).max(0) as u32;
        let refill = (delta * self.refill_per_sec) / 1000;
        if refill > 0 {
            self.tokens = (self.tokens + refill).min(self.capacity);
            self.last_refill_ms = now;
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }

    /// Try to consume `n` tokens at once. Returns `true` if the
    /// bucket had `n` tokens available (and consumes them),
    /// `false` otherwise.
    pub fn try_consume_n(&mut self, n: u32) -> bool {
        // Refill first so a long-quiet connection can burst.
        let now = now_ms();
        let delta = (now - self.last_refill_ms).max(0) as u32;
        let refill = (delta * self.refill_per_sec) / 1000;
        if refill > 0 {
            self.tokens = (self.tokens + refill).min(self.capacity);
            self.last_refill_ms = now;
        }
        if self.tokens < n {
            return false;
        }
        self.tokens -= n;
        true
    }
}

/// WsError - the closed set the per-connection task raises.
#[derive(Debug, Error)]
pub enum WsError {
    #[error("auth error: {0}")]
    Auth(#[from] AuthError),

    #[error("encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),

    #[error("decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),

    #[error("ws error: {0}")]
    Ws(#[from] axum::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("db error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("invalid frame: {0}")]
    InvalidFrame(String),

    #[error("invalid envelope: {0}")]
    InvalidEnvelope(String),

    #[error("internal: {0}")]
    Internal(String),
}

impl WsError {
    /// Log-safe description. Never includes the bearer token,
    /// the AUTH signature bytes, the signed nonce, or the
    /// private key. Used as the `error = %...` field in tracing
    /// events.
    pub fn log_redacted(&self) -> String {
        match self {
            WsError::Auth(e) => format!("auth: {}", auth_err_kind(e)),
            WsError::Encode(_) => "encode".to_string(),
            WsError::Decode(_) => "decode".to_string(),
            WsError::Ws(_) => "ws".to_string(),
            WsError::Io(_) => "io".to_string(),
            WsError::Db(_) => "db".to_string(),
            WsError::InvalidFrame(_) => "invalid_frame".to_string(),
            WsError::InvalidEnvelope(_) => "invalid_envelope".to_string(),
            WsError::Internal(_) => "internal".to_string(),
        }
    }
}

fn auth_err_kind(e: &AuthError) -> &'static str {
    match e {
        AuthError::BadSig => "bad_sig",
        AuthError::Expired => "expired",
        AuthError::Banned => "banned",
        AuthError::RateLimited => "rate",
        AuthError::Internal(_) => "internal",
    }
}

/// Handle a WebSocket upgrade request.
pub async fn handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.max_message_size(state.config.max_frame_bytes)
        .max_frame_size(state.config.max_frame_bytes)
        .on_upgrade(move |socket| connection_loop(socket, state))
}

async fn connection_loop(socket: WebSocket, state: AppState) {
    let request_id = Uuid::now_v7();
    let started = Instant::now();
    let (mut sender, mut receiver) = socket.split();
    let conn_state = Arc::new(Mutex::new(ConnState::New));
    let msg_bucket = Arc::new(Mutex::new(TokenBucket::new_with(
        state.config.rate_msg_burst,
        state.config.rate_msgs_per_sec,
    )));
    let byte_bucket = Arc::new(Mutex::new(TokenBucket::new_with(
        state.config.rate_bytes_burst,
        state.config.rate_bytes_per_sec,
    )));
    // Throttle deadline: while `now < throttle_until_ms`, inbound
    // frames are dropped silently per §20.8 ("throttled for 1 s,
    // do NOT disconnect"). Set to 0 to disable throttling.
    let throttle = Arc::new(Mutex::new(0i64));
    // Rolling window of recent bad-msg timestamps (§20.4.1).
    let bad_msgs: Arc<Mutex<VecDeque<i64>>> = Arc::new(Mutex::new(VecDeque::new()));
    let mut authed: Option<(Uuid, [u8; 32])> = None;

    debug!(request_id = %request_id, "ws connection open");

    // The handshake must complete within `handshake_timeout_ms` of
    // the TCP accept, per the architecture's connection lifecycle.
    let handshake_deadline = now_ms() + state.config.handshake_timeout_ms;

    while let Some(frame_res) = receiver.next().await {
        let frame = match frame_res {
            Ok(f) => f,
            Err(e) => {
                warn!(request_id = %request_id, error = %e, "ws recv error");
                break;
            }
        };

        // Throttle check: drop silently if we are in the post-rate-limit
        // cooldown window. (§20.8 does not disconnect.)
        {
            let t = throttle.lock().await;
            if *t > now_ms() {
                continue;
            }
        }

        // Rate limit: count one message token per inbound frame.
        {
            let mut b = msg_bucket.lock().await;
            if !b.try_consume() {
                // Throttle for 1s and notify the client. Do NOT
                // close the connection (§20.8).
                warn!(request_id = %request_id, "rate limited (msg/s); throttling");
                {
                    let mut t = throttle.lock().await;
                    *t = now_ms() + RATE_THROTTLE_MS;
                }
                send_auth_fail_bytes(&mut sender, AuthFailReason::Rate, request_id).await;
                continue;
            }
        }

        let bytes = match frame {
            Message::Binary(b) => b,
            Message::Text(_) => {
                warn!(request_id = %request_id, "rejected text frame");
                if record_bad_msg(&bad_msgs, started).await {
                    let _ = sender
                        .send(Message::Close(Some(CloseFrame {
                            code: 1008,
                            reason: "bad_msg".into(),
                        })))
                        .await;
                    break;
                }
                continue;
            }
            Message::Close(_) => break,
            Message::Ping(p) => {
                if let Err(e) = sender.send(Message::Pong(p)).await {
                    debug!(request_id = %request_id, error = %e, "pong send failed");
                }
                continue;
            }
            Message::Pong(_) => continue,
        };

        // Bytes-per-second enforcement: the bytes bucket is
        // refilled at RATE_BYTES_SUSTAINED_PER_SEC and capped at
        // RATE_BYTES_BURST (§20.6). Each inbound frame consumes
        // `bytes.len()` tokens.
        {
            let mut b = byte_bucket.lock().await;
            let n = bytes.len() as u32;
            if !b.try_consume_n(n) {
                warn!(
                    request_id = %request_id,
                    frame_bytes = bytes.len(),
                    "rate limited (bytes/s); throttling"
                );
                {
                    let mut t = throttle.lock().await;
                    *t = now_ms() + RATE_THROTTLE_MS;
                }
                send_auth_fail_bytes(&mut sender, AuthFailReason::Rate, request_id).await;
                continue;
            }
        }

        let envelope: Envelope = match rmp_serde::from_slice(&bytes) {
            Ok(e) => e,
            Err(err) => {
                warn!(request_id = %request_id, error = %err, "msgpack decode failed");
                if record_bad_msg(&bad_msgs, started).await {
                    let _ = sender
                        .send(Message::Close(Some(CloseFrame {
                            code: 1008,
                            reason: "bad_msg".into(),
                        })))
                        .await;
                    break;
                }
                continue;
            }
        };

        if envelope.v != 1 {
            warn!(
                request_id = %request_id,
                v = envelope.v,
                "rejected message with v != 1"
            );
            if record_bad_msg(&bad_msgs, started).await {
                let _ = sender
                    .send(Message::Close(Some(CloseFrame {
                        code: 1008,
                        reason: "bad_msg".into(),
                    })))
                    .await;
                break;
            }
            continue;
        }

        if envelope.room_id.is_some() {
            warn!(
                request_id = %request_id,
                kind = %envelope.r#type.as_str(),
                "rejected non-null room_id outside a room"
            );
            if record_bad_msg(&bad_msgs, started).await {
                let _ = sender
                    .send(Message::Close(Some(CloseFrame {
                        code: 1008,
                        reason: "bad_msg".into(),
                    })))
                    .await;
                break;
            }
            continue;
        }

        // Dispatch.
        let outcome = dispatch(
            envelope,
            &state,
            &conn_state,
            authed,
            request_id,
            handshake_deadline,
        )
        .await;

        let mut should_break = false;
        for action in outcome.actions {
            match action {
                Action::Send(msg) => {
                    if let Err(e) = sender.send(msg).await {
                        debug!(request_id = %request_id, error = %e, "ws send failed");
                        should_break = true;
                        break;
                    }
                }
                Action::Close(reason) => {
                    let _ = sender
                        .send(Message::Close(Some(CloseFrame {
                            code: 1000,
                            reason: reason.into(),
                        })))
                        .await;
                    should_break = true;
                    break;
                }
                Action::Upgrade { user_id, pubkey } => {
                    authed = Some((user_id, pubkey));
                }
            }
        }
        if should_break {
            break;
        }
    }

    // Mark the state machine closed.
    {
        let mut s = conn_state.lock().await;
        *s = s.clone().close();
    }
    debug!(request_id = %request_id, "ws connection closed");
}

/// Record a bad-msg event. Returns `true` if the rolling-window
/// count has crossed [`BAD_MSG_THRESHOLD`] in the last
/// [`BAD_MSG_WINDOW_MS`] milliseconds (§20.4.1), in which case the
/// caller should close the connection.
async fn record_bad_msg(bad_msgs: &Arc<Mutex<VecDeque<i64>>>, _started: Instant) -> bool {
    let mut q = bad_msgs.lock().await;
    let now = now_ms();
    while let Some(&front) = q.front() {
        if now - front > BAD_MSG_WINDOW_MS {
            q.pop_front();
        } else {
            break;
        }
    }
    q.push_back(now);
    q.len() >= BAD_MSG_THRESHOLD
}

/// A single dispatch decision. Holds a list of actions to apply
/// in order, so the WELCOME+CHALLENGE two-frame send is expressed
/// naturally.
#[derive(Debug, Default)]
struct DispatchOutcome {
    actions: Vec<Action>,
}

#[derive(Debug)]
enum Action {
    Send(Message),
    Close(&'static str),
    Upgrade { user_id: Uuid, pubkey: [u8; 32] },
}

impl DispatchOutcome {
    fn close(reason: &'static str) -> Self {
        Self {
            actions: vec![Action::Close(reason)],
        }
    }
    fn auth_fail(reason: AuthFailReason) -> Self {
        let env = Envelope {
            v: 1,
            r#type: MessageKind::AuthFail,
            id: Uuid::now_v7(),
            room_id: None,
            sender: None,
            ts_ms: now_ms(),
            seq: 0,
            payload: serde_json::to_value(&AuthFailPayload { reason }).unwrap_or(json!({})),
        };
        let mut out = Self::close("auth_fail");
        if let Ok(msg) = encode_envelope_message(&env) {
            out.actions.insert(0, Action::Send(msg));
        }
        out
    }
    fn upgrade(user_id: Uuid, pubkey: [u8; 32]) -> Self {
        Self {
            actions: vec![Action::Upgrade { user_id, pubkey }],
        }
    }
}

async fn dispatch(
    envelope: Envelope,
    state: &AppState,
    conn_state: &Arc<Mutex<ConnState>>,
    authed: Option<(Uuid, [u8; 32])>,
    request_id: Uuid,
    handshake_deadline: i64,
) -> DispatchOutcome {
    if let Some((_user_id, _pubkey)) = authed {
        // Post-handshake path.
        return dispatch_authed(envelope, state, authed, request_id).await;
    }

    match envelope.r#type {
        MessageKind::Hello => {
            handle_hello(envelope, state, conn_state, request_id, handshake_deadline).await
        }
        MessageKind::Auth => {
            handle_auth(envelope, state, conn_state, request_id, handshake_deadline).await
        }
        _ => {
            // During the handshake, only HELLO and AUTH are valid.
            warn!(
                request_id = %request_id,
                msg_type = %envelope.r#type.as_str(),
                "rejected unexpected handshake message"
            );
            DispatchOutcome::close("unexpected_type")
        }
    }
}

async fn dispatch_authed(
    envelope: Envelope,
    state: &AppState,
    authed: Option<(Uuid, [u8; 32])>,
    request_id: Uuid,
) -> DispatchOutcome {
    // Post-handshake: every message must carry a bearer field
    // in its payload. The bearer is validated against the
    // bearer table. (The v1 wire shape for post-handshake
    // messages is reserved; P3+ room lifecycle etc. will
    // define their own payload schemas and the bearer field
    // becomes top-level.)
    let payload = &envelope.payload;
    let bearer_bytes: Option<Vec<u8>> =
        payload.get("bearer").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|n| n.as_u64().map(|x| x as u8))
                .collect()
        });

    let (user_id, pubkey) = match authed {
        Some(pair) => pair,
        None => return DispatchOutcome::close("internal"),
    };

    let bearer_bytes = match bearer_bytes {
        Some(b) if b.len() == 32 => b,
        _ => {
            debug!(request_id = %request_id, "post-handshake message without bearer");
            return DispatchOutcome::close("auth_required");
        }
    };
    let token_hash = locast_crypto::sha256::sha256(&bearer_bytes);
    let info = match state.db.validate_bearer(&token_hash).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            warn!(request_id = %request_id, "bearer not found or expired");
            return DispatchOutcome::close("auth_required");
        }
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "bearer lookup failed");
            return DispatchOutcome::close("internal");
        }
    };
    if info.user_id != user_id {
        warn!(request_id = %request_id, "bearer user_id mismatch");
        return DispatchOutcome::close("bearer_mismatch");
    }
    if info.pubkey != pubkey {
        warn!(request_id = %request_id, "bearer pubkey mismatch");
        return DispatchOutcome::close("bearer_mismatch");
    }
    // The message passed bearer validation. v1 has nothing else
    // to do here; the room lifecycle lands in P3+.
    let _ = envelope;
    let _ = request_id;
    DispatchOutcome::default()
}

async fn handle_hello(
    envelope: Envelope,
    state: &AppState,
    conn_state: &Arc<Mutex<ConnState>>,
    request_id: Uuid,
    handshake_deadline: i64,
) -> DispatchOutcome {
    // Decode payload.
    let hello: HelloPayload = match serde_json::from_value(envelope.payload.clone()) {
        Ok(h) => h,
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "invalid HELLO payload");
            return DispatchOutcome::close("bad_msg");
        }
    };
    if envelope.sender.is_some() {
        return DispatchOutcome::close("handshake_no_sender");
    }
    debug!(
        request_id = %request_id,
        platform = ?hello.platform,
        client_version = %hello.client_version,
        "received HELLO"
    );

    let session_id = Uuid::now_v7();
    let server_ts_ms = now_ms();

    if now_ms() > handshake_deadline {
        return DispatchOutcome::close("handshake_timeout");
    }

    let mut nonce = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let expires_ms = server_ts_ms + state.config.challenge_ttl_ms;

    // Move New -> HelloReceived -> ChallengeSent atomically.
    {
        let mut s = conn_state.lock().await;
        let after_hello = match s.clone().transition_hello(session_id, server_ts_ms) {
            Ok(s) => s,
            Err(current) => {
                warn!(
                    request_id = %request_id,
                    state = current.name(),
                    "rejected duplicate HELLO"
                );
                return DispatchOutcome::close("duplicate_hello");
            }
        };
        let after_challenge = match after_hello.transition_challenge(nonce, expires_ms) {
            Ok(s) => s,
            Err(current) => {
                warn!(
                    request_id = %request_id,
                    state = current.name(),
                    "rejected illegal state transition"
                );
                return DispatchOutcome::close("illegal_state");
            }
        };
        *s = after_challenge;
    }

    let welcome = WelcomePayload {
        session_id,
        server_ts_ms,
        config: WelcomeConfig {
            max_room_size: 8,
            rate: WelcomeRate {
                msgs_per_sec: state.config.rate_msgs_per_sec as u16,
                bytes_per_sec: state.config.rate_bytes_per_sec,
            },
        },
    };
    let challenge = ChallengePayload {
        nonce: nonce.to_vec(),
        expires_ms,
    };
    let w_env = Envelope {
        v: 1,
        r#type: MessageKind::Welcome,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: server_ts_ms,
        seq: 0,
        payload: serde_json::to_value(&welcome).unwrap_or(json!({})),
    };
    let c_env = Envelope {
        v: 1,
        r#type: MessageKind::Challenge,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: server_ts_ms,
        seq: 0,
        payload: serde_json::to_value(&challenge).unwrap_or(json!({})),
    };

    let w_msg = match encode_envelope_message(&w_env) {
        Ok(m) => m,
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "encode WELCOME failed");
            return DispatchOutcome::close("internal");
        }
    };
    let c_msg = match encode_envelope_message(&c_env) {
        Ok(m) => m,
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "encode CHALLENGE failed");
            return DispatchOutcome::close("internal");
        }
    };

    DispatchOutcome {
        actions: vec![Action::Send(w_msg), Action::Send(c_msg)],
    }
}

async fn handle_auth(
    envelope: Envelope,
    state: &AppState,
    conn_state: &Arc<Mutex<ConnState>>,
    request_id: Uuid,
    handshake_deadline: i64,
) -> DispatchOutcome {
    let auth: AuthPayload = match serde_json::from_value(envelope.payload.clone()) {
        Ok(a) => a,
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "invalid AUTH payload");
            return DispatchOutcome::auth_fail(AuthFailReason::BadSig);
        }
    };
    if envelope.sender.is_some() {
        return DispatchOutcome::auth_fail(AuthFailReason::BadSig);
    }
    if auth.pubkey.len() != 32 || auth.sig.len() != 64 {
        return DispatchOutcome::auth_fail(AuthFailReason::BadSig);
    }
    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(&auth.pubkey);
    let mut sig = [0u8; 64];
    sig.copy_from_slice(&auth.sig);

    if now_ms() > handshake_deadline {
        return DispatchOutcome::auth_fail(AuthFailReason::Expired);
    }

    // Pull the challenge from state.
    let (nonce, expires_ms) = {
        let s = conn_state.lock().await;
        match &*s {
            ConnState::ChallengeSent {
                nonce, expires_ms, ..
            } => (*nonce, *expires_ms),
            _ => return DispatchOutcome::close("unexpected_auth"),
        }
    };

    if now_ms() > expires_ms {
        return DispatchOutcome::auth_fail(AuthFailReason::Expired);
    }

    if let Err(_e) = verify::verify_auth(&pubkey, &nonce, &sig) {
        debug!(request_id = %request_id, "AUTH verify failed");
        return DispatchOutcome::auth_fail(AuthFailReason::BadSig);
    }

    let user_id = match state.db.upsert_user(&pubkey).await {
        Ok(id) => id,
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "db upsert_user failed");
            return DispatchOutcome::close("internal");
        }
    };

    let expires_ms_token = now_ms() + state.config.bearer_ttl_seconds * 1000;
    let mut token = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut token);
    let token_hash = bearer::hash_bearer(&token);
    if let Err(e) = state
        .db
        .insert_bearer(user_id, token_hash, expires_ms_token)
        .await
    {
        warn!(request_id = %request_id, error = %e, "db insert_bearer failed");
        return DispatchOutcome::close("internal");
    }

    // Move to Authenticated.
    {
        let mut guard = conn_state.lock().await;
        let new_state = match guard.clone().transition_authenticated(user_id, pubkey) {
            Ok(s) => s,
            Err(_) => return DispatchOutcome::close("illegal_state"),
        };
        *guard = new_state;
    }

    let ok = AuthOkPayload {
        user_id,
        bearer: AuthBearer {
            token: token.to_vec(),
            expires_ms: expires_ms_token,
        },
        pubkey: pubkey.to_vec(),
    };
    let env = Envelope {
        v: 1,
        r#type: MessageKind::AuthOk,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: now_ms(),
        seq: 0,
        payload: serde_json::to_value(&ok).unwrap_or(json!({})),
    };
    let msg = match encode_envelope_message(&env) {
        Ok(m) => m,
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "encode AUTH_OK failed");
            return DispatchOutcome::close("internal");
        }
    };
    let mut out = DispatchOutcome::upgrade(user_id, pubkey);
    out.actions.push(Action::Send(msg));
    out
}

async fn send_auth_fail_bytes(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    reason: AuthFailReason,
    request_id: Uuid,
) {
    let env = Envelope {
        v: 1,
        r#type: MessageKind::AuthFail,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: now_ms(),
        seq: 0,
        payload: serde_json::to_value(&AuthFailPayload { reason }).unwrap_or(json!({})),
    };
    match encode_envelope_message(&env) {
        Ok(msg) => {
            let _ = sender.send(msg).await;
            debug!(request_id = %request_id, "sent AUTH_FAIL");
        }
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "encode AUTH_FAIL failed");
        }
    }
}

fn encode_envelope_message(env: &Envelope) -> Result<Message, rmp_serde::encode::Error> {
    let bytes = rmp_serde::to_vec_named(env)?;
    Ok(Message::Binary(bytes))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
