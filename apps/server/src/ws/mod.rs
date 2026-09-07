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
    AuthBearer, AuthFailPayload, AuthFailReason, AuthOkPayload, AuthPayload, AuthResumePayload,
    ChallengePayload, HelloPayload, RateLimitPayload, WelcomeConfig, WelcomePayload, WelcomeRate,
};
use rand::RngCore;
use serde_json::json;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::auth::bearer;
use crate::auth::bearer::BearerBinding;
use crate::auth::state::ConnState;
use crate::auth::{verify, AuthError, AUTH_FAILURE_THRESHOLD, AUTH_FAILURE_WINDOW_MS};
use crate::ratelimit::{PerConnLimiter, RateLimitHit};
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
        // Use u64 arithmetic so a long-quiet connection
        // (large `delta_ms`) does not overflow when
        // multiplied by `refill_per_sec`.
        let now = now_ms();
        let delta_ms = (now - self.last_refill_ms).max(0) as u64;
        let refill_per_sec = self.refill_per_sec as u64;
        let refill = (delta_ms * refill_per_sec) / 1_000;
        if refill > 0 {
            let refill = refill.min(self.capacity as u64);
            self.tokens = (self.tokens as u64 + refill).min(self.capacity as u64) as u32;
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
        // Use u64 to avoid overflow when `delta` (ms since
        // last refill) and `refill_per_sec` (e.g. 1_000_000
        // for the bytes bucket) are both large.
        let now = now_ms();
        let delta_ms = (now - self.last_refill_ms).max(0) as u64;
        let refill_per_sec = self.refill_per_sec as u64;
        let refill = (delta_ms * refill_per_sec) / 1_000;
        if refill > 0 {
            let refill = refill.min(self.capacity as u64);
            self.tokens = (self.tokens as u64 + refill).min(self.capacity as u64) as u32;
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
    let limiter = Arc::new(Mutex::new(PerConnLimiter::new(
        state.config.rate_msgs_per_sec,
        state.config.rate_msg_burst,
        state.config.rate_bytes_per_sec,
        state.config.rate_bytes_burst,
    )));
    // Throttle deadline: while `now < throttle_until_ms`, inbound
    // frames are dropped silently per §20.8 ("throttled for 1 s,
    // do NOT disconnect"). Set to 0 to disable throttling.
    let throttle = Arc::new(Mutex::new(0i64));
    // Rolling window of recent bad-msg timestamps (§20.4.1).
    let bad_msgs: Arc<Mutex<VecDeque<i64>>> = Arc::new(Mutex::new(VecDeque::new()));
    // P7-T01: rolling window of AUTH-failure timestamps. Once
    // the window length crosses AUTH_FAILURE_THRESHOLD in
    // AUTH_FAILURE_WINDOW_MS milliseconds we emit
    // AUTH_FAIL(Rate) and start throttling this connection's
    // handshake path. Mirrors the bad-msg tracking shape so
    // the failure-handling code stays uniform.
    let auth_failures: Arc<Mutex<VecDeque<i64>>> = Arc::new(Mutex::new(VecDeque::new()));
    let mut authed: Option<(Uuid, [u8; 32])> = None;
    // The user's current room, for the broadcast forwarder
    // task spawned below. `None` if the user is not in a
    // room (or not yet authed).
    let current_room: Arc<tokio::sync::Mutex<Option<Uuid>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    // The connection's authenticated user_id, so the
    // forwarder can filter out events the user originated.
    let self_user_id: Arc<tokio::sync::Mutex<Option<Uuid>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    // mpsc::Sender the forwarder pushes outbound envelopes
    // into. The main loop drains it on every iteration.
    let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel::<Envelope>();
    // Notify the forwarder when the user's room changes,
    // so it can re-subscribe immediately rather than
    // waiting for its 20ms sleep to elapse.
    let room_changed = Arc::new(tokio::sync::Notify::new());

    debug!(request_id = %request_id, "ws connection open");

    // The handshake must complete within `handshake_timeout_ms` of
    // the TCP accept, per the architecture's connection lifecycle.
    let handshake_deadline = now_ms() + state.config.handshake_timeout_ms;

    // Spawn the broadcast forwarder. It watches
    // `current_room`, subscribes to the new room's broadcast
    // channel when the user joins, and forwards events to
    // the main loop via `outbound_tx`. The task exits when
    // the connection closes (`fwd_cancel` is notified).
    let fwd_cancel = Arc::new(tokio::sync::Notify::new());
    {
        let state = state.clone();
        let current_room = current_room.clone();
        let fwd_cancel = fwd_cancel.clone();
        let outbound_tx = outbound_tx.clone();
        let room_changed = room_changed.clone();
        let self_user_id = self_user_id.clone();
        tokio::spawn(async move {
            room_bcast_forwarder(
                state,
                current_room,
                outbound_tx,
                fwd_cancel,
                room_changed,
                self_user_id,
            )
            .await;
        });
    }

    loop {
        // Drain any pending outbound envelopes (broadcast
        // forwarder) before pulling the next inbound frame.
        // We bound the drain so a chatty forwarder cannot
        // starve the inbound read.
        let mut drained = 0;
        while drained < 32 {
            match outbound_rx.try_recv() {
                Ok(env) => {
                    if let Ok(msg) = encode_envelope_message(&env) {
                        if let Err(e) = sender.send(msg).await {
                            debug!(request_id = %request_id, error = %e, "ws send failed");
                            break;
                        }
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
            drained += 1;
        }
        // Select: wake on the next inbound frame, the
        // forwarder producing a new envelope, or a cancel
        // notification.
        let frame = tokio::select! {
            biased;
            _ = fwd_cancel.notified() => {
                debug!(request_id = %request_id, "ws forwarder cancelled");
                break;
            }
            res = receiver.next() => match res {
                Some(Ok(f)) => f,
                Some(Err(e)) => {
                    warn!(request_id = %request_id, error = %e, "ws recv error");
                    break;
                }
                None => break,
            },
            env = outbound_rx.recv() => {
                if let Some(env) = env {
                    if let Ok(msg) = encode_envelope_message(&env) {
                        if let Err(e) = sender.send(msg).await {
                            debug!(request_id = %request_id, error = %e, "ws send failed");
                            break;
                        }
                    }
                }
                continue;
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
        // The msg bucket miss short-circuits BEFORE we read
        // the full frame body, so a flooder cannot inflate
        // its bytes budget by sending big messages.
        {
            let mut l = limiter.lock().await;
            if l.check_msg().is_err() {
                // Throttle for 1s and notify the client. Do NOT
                // close the connection (§20.8). During the
                // handshake (no bearer yet) the server emits
                // AUTH_FAIL(Rate); post-handshake, the new
                // RATE_LIMIT envelope is the structured
                // equivalent.
                warn!(request_id = %request_id, "rate limited (msg/s); throttling");
                {
                    let mut t = throttle.lock().await;
                    *t = now_ms() + RATE_THROTTLE_MS;
                }
                if authed.is_some() {
                    send_rate_limit_envelope(
                        &mut sender,
                        RateLimitHit {
                            scope: locast_protocol::handshake::RateLimitScope::Conn,
                            observed: 1,
                            limit: state.config.rate_msgs_per_sec,
                            retry_after_ms: RATE_THROTTLE_MS as u32,
                        },
                        request_id,
                    )
                    .await;
                } else {
                    send_auth_fail_bytes(&mut sender, AuthFailReason::Rate, request_id).await;
                }
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

        // Bytes-per-second enforcement. Order note (security
        // finding #5): the bytes bucket is checked AFTER
        // the frame is fully read into memory, so a single
        // oversized frame can OOM before the throttle
        // fires. P2-T07 preserves the existing order;
        // future work should cap per-frame size at the
        // transport layer.
        {
            let mut l = limiter.lock().await;
            let n = bytes.len();
            if l.check_bytes(n).is_err() {
                warn!(
                    request_id = %request_id,
                    frame_bytes = n,
                    "rate limited (bytes/s); throttling"
                );
                {
                    let mut t = throttle.lock().await;
                    *t = now_ms() + RATE_THROTTLE_MS;
                }
                if authed.is_some() {
                    send_rate_limit_envelope(
                        &mut sender,
                        RateLimitHit {
                            scope: locast_protocol::handshake::RateLimitScope::Conn,
                            observed: u32::try_from(n).unwrap_or(u32::MAX),
                            limit: state.config.rate_bytes_per_sec,
                            retry_after_ms: RATE_THROTTLE_MS as u32,
                        },
                        request_id,
                    )
                    .await;
                } else {
                    send_auth_fail_bytes(&mut sender, AuthFailReason::Rate, request_id).await;
                }
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
            // P3-T14: a non-null `room_id` is only valid when
            // the authenticated user is currently a member of
            // that room. ROOM_CREATE / ROOM_JOIN_REQUEST carry
            // `room_id = None` (the server assigns / looks up
            // the room by code), so this check rejects every
            // room-scoped envelope from a caller who has not
            // successfully joined. The capability gate inside
            // dispatch still runs the per-type membership check
            // (e.g. "must be host" for ManifestPublish); this
            // is the front-line membership check that catches
            // stale sessions and cross-room relay attempts
            // before any handler runs.
            let caller_in_room = match (authed, envelope.room_id) {
                (Some((uid, _)), Some(rid)) => state.rooms.is_user_in_room(uid, rid).await,
                _ => false,
            };
            if !caller_in_room {
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
        }

        // Dispatch.
        let outcome = dispatch(
            envelope,
            &state,
            &conn_state,
            auth_failures.clone(),
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
                    {
                        let mut g = self_user_id.lock().await;
                        *g = Some(user_id);
                    }
                    // P3-T05: register the connection's
                    // outbound_tx with the SignalRelay so
                    // the SIGNAL dispatcher can deliver
                    // per-target SDP/ICE envelopes to this
                    // user. Done at AUTH_OK, after the
                    // bearer has been validated, so an
                    // unauthenticated transport never
                    // occupies a relay slot.
                    state
                        .signal_relay
                        .register(user_id, outbound_tx.clone())
                        .await;
                    // Security finding #2 (auth-order DoS):
                    // a successful AUTH earns a clean rate
                    // budget. Without this, a connection that
                    // burned its budget on HELLO/AUTH retries
                    // would start its post-handshake life
                    // already throttled.
                    {
                        let mut l = limiter.lock().await;
                        l.reset(
                            state.config.rate_msgs_per_sec,
                            state.config.rate_msg_burst,
                            state.config.rate_bytes_per_sec,
                            state.config.rate_bytes_burst,
                        );
                    }
                }
            }
        }
        if should_break {
            break;
        }

        // Refresh the forwarder's view of the user's current
        // room. Cheap (one read on the by_id map per authed
        // message); the forwarder compares against its own
        // last-known room and re-subscribes if it changed.
        if let Some((uid, _)) = authed {
            let r = state.rooms.get_user_room(uid).await;
            let mut g = current_room.lock().await;
            if *g != r {
                *g = r;
                drop(g);
                room_changed.notify_waiters();
            }
        }
    }

    // Stop the forwarder task. `outbound_tx` will also be
    // dropped when the function returns, which is what the
    // forwarder uses as the secondary exit signal.
    fwd_cancel.notify_waiters();
    drop(outbound_tx);

    // Mark the state machine closed.
    {
        let mut s = conn_state.lock().await;
        *s = s.clone().close();
    }
    // If the connection was authenticated, notify the room
    // registry. The registry decides whether the user was a
    // host (and starts the 30s grace) or just a viewer
    // (mark Disconnected and let the stale cleanup task
    // remove them after 5 min of silence).
    if let Some((user_id, _pubkey)) = authed {
        // Use the AppState clock so the deadline is on the
        // same timeline as the room ticker's `now_ms`.
        // The free `now_ms()` helper reads wall time and
        // would drift from the test `MockClock`.
        let now = state.clock.now_ms();
        let store: Arc<dyn crate::rooms::RoomStore> =
            Arc::new(crate::rooms::DbRoomStore::new(state.db.clone()));
        if let Err(e) = state
            .rooms
            .on_connection_lost(store.as_ref(), user_id, now)
            .await
        {
            debug!(request_id = %request_id, error = %e, "on_connection_lost noop");
        }
        // P3-T05: drop this user's SignalRelay slot so a
        // future SIGNAL targeting them surfaces as
        // RecipientNotInRoom (or no-ops if a fresh
        // connection registers before the send). The
        // outbound_tx has already been dropped above
        // (`drop(outbound_tx)`); the explicit unregister
        // also keeps the HashMap small.
        state.signal_relay.unregister(user_id).await;
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
pub struct DispatchOutcome {
    pub actions: Vec<Action>,
}

#[derive(Debug)]
pub enum Action {
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
    auth_failures: Arc<Mutex<VecDeque<i64>>>,
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
            handle_auth(
                envelope,
                state,
                conn_state,
                auth_failures,
                request_id,
                handshake_deadline,
            )
            .await
        }
        MessageKind::AuthResume => {
            handle_auth_resume(
                envelope,
                state,
                conn_state,
                auth_failures,
                request_id,
                handshake_deadline,
            )
            .await
        }
        _ => {
            // P7-T01: during the handshake, unknown
            // envelope kinds are LOGGED and SKIPPED, not
            // rejected. Per §18.11 a v1.1 client may speak
            // a kind the v1 server does not yet understand;
            // the server stays forward-compatible by
            // accepting-and-ignoring rather than dropping
            // the connection.
            warn!(
                request_id = %request_id,
                msg_type = %envelope.r#type.as_str(),
                "ignored unknown handshake message"
            );
            DispatchOutcome::default()
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
    // bearer table.
    let payload = &envelope.payload;
    let bearer_bytes: Option<Vec<u8>> =
        payload.get("bearer").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|n| n.as_u64().and_then(|x| u8::try_from(x).ok()))
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
    // P4-T06: SKEW_PROBE is a per-connection clock-skew
    // measurement (architecture §13.3). It is a one-shot
    // request/reply round trip; the reply is sent only to
    // the caller (no broadcast, no room context). The probe
    // is a single `MessageKind::SkewProbe` envelope whose
    // payload is `SkewProbePayload { client_send_ms: i64 }`;
    // the server stamps its own `clock.now_ms()` into a
    // `SkewReplyPayload` and echoes `client_send_ms` back
    // so the client can compute RTT, offset, and jitter
    // from the four-timestamp exchange.
    if envelope.r#type.is_skew_probe() {
        return handle_skew_probe(envelope, state.clock.as_ref(), request_id).await;
    }
    // The message passed bearer validation. Route ROOM_*
    // envelopes to the room dispatcher. Other envelope
    // types (future PLAY/PAUSE/SEEK/DRAW/LASER/CHAT/MANIFEST_*)
    // are not implemented in v1 / P2-T04 and are silently
    // accepted.
    if envelope.r#type.is_room_lifecycle()
        || envelope.r#type.is_manifest_lifecycle()
        || envelope.r#type.is_signal_lifecycle()
        || envelope.r#type.is_playback_lifecycle()
        || envelope.r#type.is_position_report()
    {
        let store: Arc<dyn crate::rooms::RoomStore> =
            Arc::new(crate::rooms::DbRoomStore::new(state.db.clone()));
        let ctx = crate::rooms::DispatchContext {
            registry: &state.rooms,
            store: store.as_ref(),
            db: &state.db,
            clock: state.clock.as_ref(),
            relay: &state.signal_relay,
        };
        // Capture the room_id before dispatching so we can
        // publish_room_events against it (manifest events
        // carry their own room_id; every other variant
        // gets the dispatch context's room_id). This is
        // only meaningful for envelopes that already
        // passed the membership check above (i.e. carry a
        // non-null room_id). ROOM_CREATE / ROOM_JOIN_REQUEST
        // carry room_id=None and the events they produce
        // carry their own room_id (ManifestPublished does;
        // others don't apply to ROOM_CREATE).
        if let Some(dispatch_room_id) = envelope.room_id {
            let outcome =
                crate::rooms::dispatch_room_message(envelope, &ctx, user_id, pubkey).await;
            // P3-T14: publish every `RoomEvent` in the
            // dispatch outcome to the room's broadcast
            // channel so the other participants' forwarders
            // see them (ROOM_STATE, PARTICIPANT_JOINED,
            // MANIFEST_PUBLISHED, etc.). Prior to P3-T14
            // the WS layer dropped these events silently --
            // the per-connection RPC replies in `to_caller`
            // were delivered, but every cross-participant
            // broadcast was lost, which silently broke
            // WebRTC negotiation and manifest delivery.
            // ManifestPublished carries its own room_id;
            // every other variant gets the dispatch
            // envelope's room_id.
            state
                .rooms
                .publish_events(&outcome.events, |_| dispatch_room_id);
            let mut actions = Vec::new();
            for env in outcome.to_caller {
                if let Ok(msg) = encode_envelope_message(&env) {
                    actions.push(Action::Send(msg));
                }
            }
            if outcome.close_caller {
                actions.push(Action::Close("room_close"));
            }
            return DispatchOutcome { actions };
        }
        // No room_id on the envelope: this is a ROOM_CREATE
        // or ROOM_JOIN_REQUEST. The dispatch handles both
        // without needing our event-broadcast helper (their
        // events are emitted by `registry.create`/`join`
        // into the per-room broadcast channel internally).
        let outcome = crate::rooms::dispatch_room_message(envelope, &ctx, user_id, pubkey).await;
        let mut actions = Vec::new();
        for env in outcome.to_caller {
            if let Ok(msg) = encode_envelope_message(&env) {
                actions.push(Action::Send(msg));
            }
        }
        if outcome.close_caller {
            actions.push(Action::Close("room_close"));
        }
        return DispatchOutcome { actions };
    }
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

    // P7-T01: peel off an optional `resume_token` field
    // from the HELLO payload. A valid resume token lets
    // the client follow up with AUTH_RESUME (instead of
    // AUTH); a bogus / unknown token is treated as a
    // plain HELLO so a corrupted envelope never blocks
    // the handshake.
    let mut resume_token_hash: Option<[u8; 32]> = None;
    if let Some(rt_val) = envelope.payload.get("resume_token") {
        if let Some(arr) = rt_val.as_array() {
            let bytes: Vec<u8> = arr
                .iter()
                .filter_map(|n| n.as_u64().and_then(|x| u8::try_from(x).ok()))
                .collect();
            if bytes.len() == 32 {
                let mut buf = [0u8; 32];
                buf.copy_from_slice(&bytes);
                let hash = bearer::hash_bearer(&buf);
                match state.db.validate_resume_token(&hash).await {
                    Ok(Some(_info)) => {
                        resume_token_hash = Some(hash);
                    }
                    Ok(None) => {
                        debug!(
                            request_id = %request_id,
                            "hello resume_token not found or expired; treating as fresh HELLO"
                        );
                    }
                    Err(e) => {
                        warn!(request_id = %request_id, error = %e, "validate_resume_token failed");
                    }
                }
            }
        }
    }
    // Mint a fresh connection_epoch for every HELLO so a
    // bearer stolen from connection N cannot be replayed
    // on connection N+1.
    let connection_epoch = {
        let mut g = state.epoch_counter.lock().expect("epoch_counter lock");
        g.next()
    };

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
        let after_challenge = match after_hello.transition_challenge(
            nonce,
            expires_ms,
            connection_epoch,
            resume_token_hash,
        ) {
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
    auth_failures: Arc<Mutex<VecDeque<i64>>>,
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

    // P7-T01: AUTH-failure rate limit. If this connection
    // has crossed AUTH_FAILURE_THRESHOLD bad AUTHs in the
    // last AUTH_FAILURE_WINDOW_MS, short-circuit to
    // AUTH_FAIL(Rate) before doing any verification work.
    if is_auth_throttled(&auth_failures).await {
        warn!(request_id = %request_id, "AUTH rate limited");
        return DispatchOutcome::auth_fail(AuthFailReason::Rate);
    }

    // P7-T01: banlist check (§21.3). Runs BEFORE signature
    // verification so a banned client cannot consume CPU
    // on repeated AUTH frames.
    match state.db.is_pubkey_banned(&pubkey).await {
        Ok(true) => {
            record_auth_failure(&auth_failures).await;
            warn!(request_id = %request_id, "AUTH rejected: banned pubkey");
            return DispatchOutcome::auth_fail(AuthFailReason::Banned);
        }
        Ok(false) => {}
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "is_pubkey_banned lookup failed");
        }
    }

    // Pull the challenge from state.
    let (nonce, expires_ms, connection_epoch) = {
        let s = conn_state.lock().await;
        match &*s {
            ConnState::ChallengeSent {
                nonce,
                expires_ms,
                connection_epoch,
                ..
            } => (*nonce, *expires_ms, *connection_epoch),
            _ => return DispatchOutcome::close("unexpected_auth"),
        }
    };

    if now_ms() > expires_ms {
        return DispatchOutcome::auth_fail(AuthFailReason::Expired);
    }

    if let Err(_e) = verify::verify_auth(&pubkey, &nonce, &sig) {
        record_auth_failure(&auth_failures).await;
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
        .insert_bearer(
            user_id,
            token_hash,
            expires_ms_token,
            connection_epoch,
            None,
        )
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

    // P7-T01: issue an opaque session_resume_token the
    // client can present on the next WS connection. The
    // server stores sha256(token) along with the
    // (user_id, connection_epoch, room_id) binding so a
    // stolen token cannot be replayed against a new
    // connection. The room_id is None at AUTH time (the
    // bearer is transport-only); a per-room token would
    // be minted after a successful ROOM_JOIN, out of
    // scope for this task.
    let mut resume_token = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut resume_token);
    let resume_token_hash = bearer::hash_bearer(&resume_token);
    let resume_expires_ms = now_ms() + state.config.bearer_ttl_seconds * 1000;
    if let Err(e) = state
        .db
        .insert_resume_token(
            &resume_token_hash,
            user_id,
            connection_epoch,
            None,
            resume_expires_ms,
        )
        .await
    {
        // Resume-token persistence is best-effort: a
        // failure does not block AUTH_OK (the client has a
        // working bearer for this session), it just means
        // the next reconnect will require a fresh HELLO +
        // CHALLENGE round trip.
        warn!(request_id = %request_id, error = %e, "insert_resume_token failed");
    }

    let ok = AuthOkPayload {
        user_id,
        bearer: AuthBearer {
            token: token.to_vec(),
            expires_ms: expires_ms_token,
        },
        pubkey: pubkey.to_vec(),
        resume_token: Some(resume_token.to_vec()),
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

    // P2-T04: a fresh authenticated transport for a user
    // who was the host with an active disconnect-grace
    // timer restores the host. The room registry's
    // `rejoin` returns the events; we both push the
    // events to the room's broadcast channel (so other
    // participants see HOST_RECONNECTED) and directly to
    // the new connection.
    {
        let store: Arc<dyn crate::rooms::RoomStore> =
            Arc::new(crate::rooms::DbRoomStore::new(state.db.clone()));
        if let Ok(Some(events)) = state
            .rooms
            .rejoin(store.as_ref(), user_id, pubkey, state.clock.now_ms())
            .await
        {
            for event in events {
                let (kind, payload) = match event {
                    crate::rooms::RoomEvent::HostReconnected(p) => (
                        MessageKind::HostReconnected,
                        serde_json::to_value(&p).unwrap_or(serde_json::json!({})),
                    ),
                    _ => continue,
                };
                if let Some(rid) = state.rooms.get_user_room(user_id).await {
                    let env = Envelope {
                        v: 1,
                        r#type: kind,
                        id: Uuid::now_v7(),
                        room_id: Some(rid),
                        sender: None,
                        ts_ms: now_ms(),
                        seq: 0,
                        payload,
                    };
                    if let Ok(msg) = encode_envelope_message(&env) {
                        out.actions.push(Action::Send(msg));
                    }
                }
            }
        }
    }
    out
}

/// P7-T01: AUTH_RESUME handler. The client presents a
/// bearer it received in a prior AUTH_OK together with a
/// fresh signature over the nonce from this connection's
/// CHALLENGE. The server:
///   1. Validates the bearer row exists and is not
///      expired.
///   2. Confirms the connection_epoch stamped onto the
///      bearer matches this connection's
///      `connection_epoch` (rejects replay against a
///      newer connection).
///   3. Verifies the Ed25519 signature against the
///      bearer-bound pubkey (rejects an attacker who stole
///      the bearer but does not have the matching private
///      key).
///   4. Re-issues a fresh bearer bound to the SAME
///      (user_id, connection_epoch, room_id) triple so the
///      resumed session keeps its capabilities.
///   5. Mints a fresh resume_token for the next cycle.
async fn handle_auth_resume(
    envelope: Envelope,
    state: &AppState,
    conn_state: &Arc<Mutex<ConnState>>,
    auth_failures: Arc<Mutex<VecDeque<i64>>>,
    request_id: Uuid,
    _handshake_deadline: i64,
) -> DispatchOutcome {
    let resume: AuthResumePayload = match serde_json::from_value(envelope.payload.clone()) {
        Ok(r) => r,
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "invalid AUTH_RESUME payload");
            return DispatchOutcome::auth_fail(AuthFailReason::BadSig);
        }
    };
    if envelope.sender.is_some() {
        return DispatchOutcome::auth_fail(AuthFailReason::BadSig);
    }
    if resume.bearer.len() != 32 || resume.signed_nonce.len() != 64 || resume.user_id.is_nil() {
        return DispatchOutcome::auth_fail(AuthFailReason::BadSig);
    }
    if is_auth_throttled(&auth_failures).await {
        warn!(request_id = %request_id, "AUTH_RESUME rate limited");
        return DispatchOutcome::auth_fail(AuthFailReason::Rate);
    }
    // Pull challenge + epoch from state.
    let (nonce, expires_ms, connection_epoch, resume_token_hash) = {
        let s = conn_state.lock().await;
        match &*s {
            ConnState::ChallengeSent {
                nonce,
                expires_ms,
                connection_epoch,
                resume_token_hash,
                ..
            } => (*nonce, *expires_ms, *connection_epoch, *resume_token_hash),
            _ => return DispatchOutcome::close("unexpected_resume"),
        }
    };
    if now_ms() > expires_ms {
        return DispatchOutcome::auth_fail(AuthFailReason::Expired);
    }
    // No resume_token_hash means the HELLO did not
    // advertise a valid one; AUTH_RESUME is invalid here.
    let _stored_hash = match resume_token_hash {
        Some(h) => h,
        None => {
            record_auth_failure(&auth_failures).await;
            warn!(request_id = %request_id, "AUTH_RESUME without prior HELLO resume_token");
            return DispatchOutcome::auth_fail(AuthFailReason::BadSig);
        }
    };
    // Look up the bearer.
    let mut bearer_bytes = [0u8; 32];
    bearer_bytes.copy_from_slice(&resume.bearer);
    let token_hash = bearer::hash_bearer(&bearer_bytes);
    let info = match state.db.validate_bearer(&token_hash).await {
        Ok(Some(i)) => i,
        Ok(None) => {
            record_auth_failure(&auth_failures).await;
            warn!(request_id = %request_id, "AUTH_RESUME bearer not found or expired");
            return DispatchOutcome::auth_fail(AuthFailReason::BadSig);
        }
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "bearer lookup failed");
            return DispatchOutcome::close("internal");
        }
    };
    if info.user_id != resume.user_id {
        record_auth_failure(&auth_failures).await;
        warn!(request_id = %request_id, "AUTH_RESUME bearer user_id mismatch");
        return DispatchOutcome::auth_fail(AuthFailReason::BadSig);
    }
    // P7-T01 review-fix: triple-bind assertion. The bearer
    // row carries the (user_id, connection_epoch, room_id)
    // it was minted for; reject any bearer presented against
    // a different connection_epoch (stolen-bearer replay
    // across a newer connection) or a different room_id.
    let binding = BearerBinding {
        user_id: info.user_id,
        connection_epoch: info.connection_epoch,
        room_id: info.room_id,
    };
    if !binding.matches(resume.user_id, connection_epoch, None) {
        record_auth_failure(&auth_failures).await;
        warn!(
            request_id = %request_id,
            bearer_epoch = info.connection_epoch,
            conn_epoch = connection_epoch,
            "AUTH_RESUME bearer epoch/room mismatch",
        );
        return DispatchOutcome::auth_fail(AuthFailReason::BadSig);
    }
    // Banlist check on the bearer-bound pubkey.
    match state.db.is_pubkey_banned(&info.pubkey).await {
        Ok(true) => {
            record_auth_failure(&auth_failures).await;
            warn!(request_id = %request_id, "AUTH_RESUME banned pubkey");
            return DispatchOutcome::auth_fail(AuthFailReason::Banned);
        }
        Ok(false) => {}
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "is_pubkey_banned lookup failed");
        }
    }
    // Verify signature against the bearer-bound pubkey.
    let mut sig = [0u8; 64];
    sig.copy_from_slice(&resume.signed_nonce);
    if verify::verify_auth(&info.pubkey, &nonce, &sig).is_err() {
        record_auth_failure(&auth_failures).await;
        warn!(request_id = %request_id, "AUTH_RESUME signature failed");
        return DispatchOutcome::auth_fail(AuthFailReason::BadSig);
    }
    let user_id = info.user_id;
    let pubkey = info.pubkey;

    // Mint a fresh bearer bound to the SAME (user_id,
    // connection_epoch, None) so the resumed session
    // keeps its transport-layer binding. (A per-room
    // resume is a future-task scope item; today every
    // AUTH_OK is transport-only at the time it is
    // minted.)
    let expires_ms_token = now_ms() + state.config.bearer_ttl_seconds * 1000;
    let mut new_token = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut new_token);
    let new_token_hash = bearer::hash_bearer(&new_token);
    if let Err(e) = state
        .db
        .insert_bearer(
            user_id,
            new_token_hash,
            expires_ms_token,
            connection_epoch,
            None,
        )
        .await
    {
        warn!(request_id = %request_id, error = %e, "db insert_bearer failed (resume)");
        return DispatchOutcome::close("internal");
    }

    {
        let mut guard = conn_state.lock().await;
        let new_state = match guard.clone().transition_authenticated(user_id, pubkey) {
            Ok(s) => s,
            Err(_) => return DispatchOutcome::close("illegal_state"),
        };
        *guard = new_state;
    }

    // Mint a new resume_token under the SAME epoch so the
    // client can resume again after a future disconnect.
    let mut resume_token = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut resume_token);
    let resume_token_hash_new = bearer::hash_bearer(&resume_token);
    let resume_expires_ms = now_ms() + state.config.bearer_ttl_seconds * 1000;
    if let Err(e) = state
        .db
        .insert_resume_token(
            &resume_token_hash_new,
            user_id,
            connection_epoch,
            None,
            resume_expires_ms,
        )
        .await
    {
        warn!(request_id = %request_id, error = %e, "insert_resume_token failed (resume)");
    }

    let ok = AuthOkPayload {
        user_id,
        bearer: AuthBearer {
            token: new_token.to_vec(),
            expires_ms: expires_ms_token,
        },
        pubkey: pubkey.to_vec(),
        resume_token: Some(resume_token.to_vec()),
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
            warn!(request_id = %request_id, error = %e, "encode AUTH_OK failed (resume)");
            return DispatchOutcome::close("internal");
        }
    };
    let mut out = DispatchOutcome::upgrade(user_id, pubkey);
    out.actions.push(Action::Send(msg));

    // Same host-rejoin path as the AUTH handler so a
    // resumed host's HOST_RECONNECTED event is delivered.
    {
        let store: Arc<dyn crate::rooms::RoomStore> =
            Arc::new(crate::rooms::DbRoomStore::new(state.db.clone()));
        if let Ok(Some(events)) = state
            .rooms
            .rejoin(store.as_ref(), user_id, pubkey, state.clock.now_ms())
            .await
        {
            for event in events {
                let (kind, payload) = match event {
                    crate::rooms::RoomEvent::HostReconnected(p) => (
                        MessageKind::HostReconnected,
                        serde_json::to_value(&p).unwrap_or(serde_json::json!({})),
                    ),
                    _ => continue,
                };
                if let Some(rid) = state.rooms.get_user_room(user_id).await {
                    let env = Envelope {
                        v: 1,
                        r#type: kind,
                        id: Uuid::now_v7(),
                        room_id: Some(rid),
                        sender: None,
                        ts_ms: now_ms(),
                        seq: 0,
                        payload,
                    };
                    if let Ok(msg) = encode_envelope_message(&env) {
                        out.actions.push(Action::Send(msg));
                    }
                }
            }
        }
    }
    out
}

/// P7-T01: AUTH-failure tracking. Mirrors
/// [`record_bad_msg`]: returns `true` once the rolling
/// window crosses the configured threshold so the caller
/// can emit AUTH_FAIL(Rate) and throttle.
async fn is_auth_throttled(failures: &Arc<Mutex<VecDeque<i64>>>) -> bool {
    let mut q = failures.lock().await;
    let now = now_ms();
    while let Some(&front) = q.front() {
        if now - front > AUTH_FAILURE_WINDOW_MS {
            q.pop_front();
        } else {
            break;
        }
    }
    q.len() >= AUTH_FAILURE_THRESHOLD
}

async fn record_auth_failure(failures: &Arc<Mutex<VecDeque<i64>>>) {
    let mut q = failures.lock().await;
    let now = now_ms();
    while let Some(&front) = q.front() {
        if now - front > AUTH_FAILURE_WINDOW_MS {
            q.pop_front();
        } else {
            break;
        }
    }
    q.push_back(now);
}

/// P4-T06: SKEW_PROBE reply handler. The probe is a
/// per-connection, one-shot, request/response exchange
/// (architecture §13.3). The caller has already been
/// authenticated (the WS layer's bearer check ran before
/// this function is reached). The handler:
///
///  1. Decodes the probe payload as `SkewProbePayload`.
///     A malformed payload closes the connection (the
///     probe is bearer-bearing, so the only way to send
///     one is to be an authenticated connection; a
///     malformed payload is a real bug).
///  2. Reads `state.clock.now_ms()` (the same
///     `AppState::clock` the rest of the server uses for
///     `Envelope.ts_ms` and room dispatch time) and
///     stamps it into `SkewReplyPayload.server_ts_ms`.
///  3. Echoes the request's `client_send_ms` back so
///     the client can pair the round trip deterministically.
///  4. Returns a single `Action::Send` carrying the
///     `SKEW_REPLY` envelope. There is no broadcast, no
///     room context, and no room state mutation.
///
/// The handler does not consult `usePlaybackStore` / room
/// membership / cap gates; SKEW_PROBE is the only bearer-
/// bearing envelope that is intentionally NOT room-scoped
/// because the client measures skew across the whole
/// connection, not per room.
async fn handle_skew_probe(
    envelope: Envelope,
    clock: &dyn crate::time::Clock,
    request_id: Uuid,
) -> DispatchOutcome {
    use locast_protocol::room::{SkewProbePayload, SkewReplyPayload};
    let probe: SkewProbePayload = match serde_json::from_value(envelope.payload.clone()) {
        Ok(p) => p,
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "invalid SKEW_PROBE payload");
            return DispatchOutcome::close("bad_msg");
        }
    };
    let reply = SkewReplyPayload {
        server_ts_ms: clock.now_ms(),
        client_send_ms: probe.client_send_ms,
    };
    let env = Envelope {
        v: 1,
        r#type: MessageKind::SkewReply,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: reply.server_ts_ms,
        seq: 0,
        payload: serde_json::to_value(&reply).unwrap_or(json!({})),
    };
    let msg = match encode_envelope_message(&env) {
        Ok(m) => m,
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "encode SKEW_REPLY failed");
            return DispatchOutcome::close("internal");
        }
    };
    debug!(
        request_id = %request_id,
        server_ts_ms = reply.server_ts_ms,
        echoed_client_send_ms = reply.client_send_ms,
        "sent SKEW_REPLY"
    );
    DispatchOutcome {
        actions: vec![Action::Send(msg)],
    }
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

/// Send a post-handshake RATE_LIMIT envelope. P2-T07
/// emit path for the new envelope. The `hit` carries the
/// scope, the observed rate, the configured limit, and the
/// retry hint.
async fn send_rate_limit_envelope(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    hit: crate::ratelimit::RateLimitHit,
    request_id: Uuid,
) {
    let payload = hit.to_payload();
    let env = Envelope {
        v: 1,
        r#type: MessageKind::RateLimit,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: now_ms(),
        seq: 0,
        payload: serde_json::to_value(&RateLimitPayload {
            scope: payload.scope,
            retry_after_ms: payload.retry_after_ms,
            observed: payload.observed,
            limit: payload.limit,
        })
        .unwrap_or(json!({})),
    };
    match encode_envelope_message(&env) {
        Ok(msg) => {
            let _ = sender.send(msg).await;
            debug!(
                request_id = %request_id,
                observed = hit.observed,
                limit = hit.limit,
                retry_after_ms = hit.retry_after_ms,
                "sent RATE_LIMIT"
            );
        }
        Err(e) => {
            warn!(request_id = %request_id, error = %e, "encode RATE_LIMIT failed");
        }
    }
}

fn encode_envelope_message(env: &Envelope) -> Result<Message, rmp_serde::encode::Error> {
    let bytes = rmp_serde::to_vec_named(env)?;
    Ok(Message::Binary(bytes))
}

/// Per-connection room-broadcast forwarder. Watches the
/// `current_room` cell; when the user joins a new room,
/// subscribes to its broadcast channel; forwards every
/// received item to the connection's `outbound_tx`. Exits
/// when `cancel` is notified (the connection is closing).
///
/// The `self_user_id` is the connection's authenticated
/// user_id; the forwarder uses it to filter the originator
/// field on broadcast items so the originating user does
/// not see their own event echoed back. (Most events are
/// originated by another user or by the server.)
async fn room_bcast_forwarder(
    state: AppState,
    current_room: Arc<tokio::sync::Mutex<Option<Uuid>>>,
    outbound_tx: tokio::sync::mpsc::UnboundedSender<Envelope>,
    cancel: Arc<tokio::sync::Notify>,
    room_changed: Arc<tokio::sync::Notify>,
    self_user_id: Arc<tokio::sync::Mutex<Option<Uuid>>>,
) {
    let mut subscribed: Option<(
        Uuid,
        tokio::sync::broadcast::Receiver<crate::rooms::registry::BroadcastItem>,
    )> = None;
    loop {
        // 1) Detect a room change. If the user moved, drop
        // the old subscription and grab a new one.
        let now_room = {
            let g = current_room.lock().await;
            *g
        };
        match (subscribed.as_ref(), now_room) {
            (Some((cur, _)), Some(now)) if *cur == now => {}
            (None, None) => {}
            _ => {
                subscribed = match now_room {
                    Some(rid) => state.rooms.subscribe(rid).await.map(|rx| (rid, rx)),
                    None => None,
                };
            }
        }
        // 2) Wait for a room change, or pull the next item
        // from the current subscription.
        let (room_id, item) = if let Some(s) = subscribed.as_mut() {
            let (rid, rx) = s;
            match rx.recv().await {
                Ok(item) => (*rid, item),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    subscribed = None;
                    continue;
                }
            }
        } else {
            // No subscription. Wait for either a room
            // change notification or a small sleep tick.
            tokio::select! {
                _ = cancel.notified() => return,
                _ = room_changed.notified() => continue,
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => continue,
            }
        };
        // Filter events: skip if the originator is the
        // current user (they already got the direct reply),
        // and skip if the user has been removed from the
        // room since the event was published.
        let self_uid = {
            let g = self_user_id.lock().await;
            *g
        };
        if let Some(uid) = self_uid {
            if item.originator == Some(uid) {
                continue;
            }
            if !state.rooms.is_user_in_room(uid, room_id).await {
                continue;
            }
        }
        let env = Envelope {
            v: 1,
            r#type: item.kind,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: None,
            ts_ms: now_ms(),
            seq: 0,
            payload: item.payload,
        };
        if outbound_tx.send(env).is_err() {
            return;
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
