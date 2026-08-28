//! `net::signaling` - the native WebSocket client and connection
//! state machine.
//!
//! ## State machine
//!
//! ```text
//!                  start()
//!                    |
//!                    v
//!              +-----------+    TCP/TLS error
//!              | Disconnec |-----------------+
//!              |   ted     |                 v
//!              +-----------+         +-----------------+
//!                    ^               |   Reconnecting  |
//!                    |  shutdown()   +-----------------+
//!                    |                       |
//!                    |              backoff sleep, retry
//!                    |                       |
//!                    |                       v
//!              +-----------+         +-----------+
//!              | Shutting  |<------- | Connecting|
//!              |   Down    |  cancel +-----+-----+
//!              +-----------+                 |
//!                    ^                        | WS open
//!                    |                        v
//!                    |                +-------------+   HELLO sent
//!                    |   shutdown     | Handshaking |-----------+
//!                    +----------------+             |           |
//!                                     +------+------+           |
//!                                            |                  | AUTH_OK
//!                                            |  AUTH_FAIL       v
//!                                            v            +-------------+
//!                                     +-------------+     | Authenticated|
//!                                     | Reconnecting|     +-------+------+
//!                                     +-------------+             |
//!                                                                  | server close /
//!                                                                  | oversized frame
//!                                                                  v
//!                                                            (Reconnecting)
//! ```
//!
//! The connection loop is spawned by [`SignalingClient::start`]
//! and torn down by [`SignalingClient::shutdown`]. Cancellation
//! is delivered through a `tokio_util::sync::CancellationToken`.
//!
//! ## Security
//!
//! - The bearer token is stored ONLY in [`BearerRecord`], which
//!   is `pub` for the module but is `pub(crate)` for the
//!   `SignalingClient` API. The frontend never receives a
//!   `BearerRecord`; [`SignalingClient::snapshot`] returns a
//!   [`ConnectionState`] with no token, no signature, no nonce,
//!   and no private key material.
//! - `tracing` events NEVER carry the bearer, the signature,
//!   the nonce, or the private key. When the loop needs to
//!   reference the token it logs a truncated
//!   `sha256(token)[..6].hex()` only (see
//!   `redact_token`).
//! - The connection loop re-derives the bearer from the AUTH_OK
//!   frame; it does not re-use a previous bearer across
//!   reconnects (a new handshake always issues a new bearer).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::handshake::{
    AuthFailReason, AuthOkPayload, AuthPayload, HelloPayload, WelcomePayload,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::identity::keystore::IdentityService;

use super::config::SignalingConfig;
use super::reconnect::Backoff;
use super::state::{ConnPhase, ConnectionState, DisconnectReason};

/// The opaque `ws://...` -> `tokio_tungstenite` stream type
/// after a successful upgrade.
type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Internal error type for the signaling client. The Tauri
/// command surface maps each variant onto an `AppError`
/// variant; the connection loop converts every error into a
/// `tracing::warn!` + state transition and never panics.
#[derive(Debug, Error)]
pub enum SignalingError {
    /// The local identity has not been initialized. The user
    /// must call `identity_get` before they can connect.
    #[error("identity not initialized")]
    IdentityNotInitialized,

    /// The configured URL could not be parsed or the upgrade
    /// failed for transport reasons.
    #[error("transport error: {message}")]
    Transport { message: String },

    /// The handshake did not complete within the configured
    /// timeout.
    #[error("handshake timed out after {0:?}")]
    HandshakeTimeout(Duration),

    /// The server sent an AUTH_FAIL frame.
    #[error("auth failed: {reason:?}")]
    AuthFailed { reason: AuthFailReason },

    /// The server sent an envelope that the v1 protocol does
    /// not accept (bad version, room_id set during handshake,
    /// etc.).
    #[error("protocol error: {message}")]
    Protocol { message: String },

    /// An inbound frame was larger than the configured cap.
    #[error("oversized frame: {bytes} bytes (cap {cap})")]
    OversizedFrame { bytes: usize, cap: usize },
}

/// The authenticated session, held in native memory only.
///
/// This struct is `pub` so the connection loop can move it
/// between tasks, but it is NOT part of the IPC surface. There
/// is no `serde::Serialize` impl, no `specta::Type` impl, and
/// no Tauri command that returns one. The frontend can only
/// learn that the connection is `Authenticated` (a boolean
/// flag).
#[derive(Debug, Clone)]
pub struct BearerRecord {
    /// The server-assigned user id from AUTH_OK.
    pub user_id: Uuid,
    /// The 32-byte Ed25519 public key the server associated
    /// with this session. Echoed from AUTH_OK so the client
    /// can confirm the binding.
    pub pubkey: [u8; 32],
    /// The 32-byte bearer token. Held in memory only; the
    /// server stores `sha256(token)`.
    pub token: [u8; 32],
    /// Absolute expiry (unix ms). 15 minutes by default.
    pub expires_ms: i64,
}

/// The mutable runtime state of the signaling client. The
/// `bearer` field is intentionally NOT serialized and NOT
/// exposed through [`SignalingClient::snapshot`].
#[derive(Debug)]
pub struct SignalingInner {
    /// The safe view of the connection state. Updated under
    /// `inner`'s mutex.
    pub state: ConnectionState,
    /// The current bearer, or `None` if the client is not
    /// authenticated. The field is private to the module; no
    /// IPC consumer ever sees it.
    pub bearer: Option<BearerRecord>,
    /// Inbound subscribers. Every envelope the connection
    /// loop receives (post-handshake) is forwarded to each
    /// subscriber via an `mpsc::UnboundedSender`. The
    /// `RoomClient` subscribes to receive ROOM_* and
    /// PRESENCE envelopes.
    pub subscribers: Vec<mpsc::UnboundedSender<Envelope>>,
    /// Outbound envelope queue. Producers (the RoomClient
    /// and tests) push envelopes here; the connection loop
    /// pops them and writes them to the WS. The
    /// connection loop installs a fresh `UnboundedSender`
    /// on every start; producers see `None` when no
    /// connection is active.
    pub outbound_tx: Option<mpsc::UnboundedSender<Envelope>>,
    /// Notified when an outbound envelope is pushed. The
    /// connection loop's idle phase waits on this so an
    /// outbound send wakes the loop immediately rather
    /// than waiting for the next inbound frame.
    pub outbound_notify: Arc<tokio::sync::Notify>,
}

impl SignalingInner {
    fn new(config: &SignalingConfig) -> Self {
        Self {
            state: ConnectionState::for_url(&config.url),
            bearer: None,
            subscribers: Vec::new(),
            outbound_tx: None,
            outbound_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

/// The native signaling client. Held in `tauri::State`; the
/// `signaling_*` Tauri commands take `tauri::State<'_,
/// SignalingClient>`.
pub struct SignalingClient {
    config: Arc<SignalingConfig>,
    identity: Arc<IdentityService>,
    inner: Arc<Mutex<SignalingInner>>,
    /// Generation counter. Each call to `start` increments
    /// the generation and creates a fresh cancellation token
    /// for the new task; `shutdown` signals the most recent
    /// token.
    cancel: Mutex<CancellationToken>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl SignalingClient {
    /// Construct a new signaling client. Does not start the
    /// connection loop; call [`SignalingClient::start`] for
    /// that.
    pub fn new(config: SignalingConfig, identity: Arc<IdentityService>) -> Self {
        let config = Arc::new(config);
        let inner = Arc::new(Mutex::new(SignalingInner::new(&config)));
        Self {
            config,
            identity,
            inner,
            cancel: Mutex::new(CancellationToken::new()),
            task: Mutex::new(None),
        }
    }

    /// Spawn the connection loop. Idempotent: a second call
    /// while the loop is alive is a no-op. A new run after
    /// `shutdown` gets a fresh cancellation token and resets
    /// the observable phase to `Disconnected` so the UI does
    /// not briefly see `ShuttingDown` after a manual reconnect.
    pub async fn start(&self) -> Result<(), SignalingError> {
        let mut task_guard = self.task.lock().await;
        if let Some(handle) = task_guard.as_ref() {
            if !handle.is_finished() {
                return Ok(());
            }
            // Stale handle: replace.
            *task_guard = None;
        }
        // Fresh cancel token for the new task.
        let cancel = {
            let mut g = self.cancel.lock().await;
            *g = CancellationToken::new();
            g.clone()
        };
        // Reset observable state so a post-shutdown start does
        // not leave the UI looking at the terminal phase.
        {
            let mut g = self.inner.lock().await;
            g.state.phase = ConnPhase::Disconnected;
            g.state.connected = false;
            g.state.last_error = None;
            g.state.last_error_at_ms = None;
            g.state.attempt = 0;
            g.bearer = None;
            g.state.session_id = None;
            g.state.user_id = None;
            g.subscribers.clear();
        }

        let config = Arc::clone(&self.config);
        let identity = Arc::clone(&self.identity);
        let inner = Arc::clone(&self.inner);
        let handle = tokio::spawn(async move {
            connection_loop(config, identity, inner, cancel).await;
        });
        *task_guard = Some(handle);
        Ok(())
    }

    /// Signal cancellation and await the connection loop. Safe
    /// to call multiple times; the second call is a no-op
    /// once the loop has exited.
    pub async fn shutdown(&self) {
        {
            let g = self.cancel.lock().await;
            g.cancel();
        }
        let mut task_guard = self.task.lock().await;
        if let Some(handle) = task_guard.take() {
            let _ = handle.await;
        }
    }

    /// Read a redacted snapshot of the connection state. The
    /// bearer, the signature, the nonce, and any private key
    /// material are NEVER returned.
    pub async fn snapshot(&self) -> ConnectionState {
        let g = self.inner.lock().await;
        g.state.clone()
    }

    /// The WebSocket URL the client is configured for.
    pub fn server_url(&self) -> &str {
        &self.config.url
    }

    /// The cancellation token. Exposed for tests that want to
    /// trigger shutdown directly.
    #[cfg(test)]
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.blocking_lock().clone()
    }

    /// **Test-only.** Returns the bearer currently held in
    /// native state, if any. Production code paths MUST NOT
    /// call this method. The bearer is intentionally not
    /// exposed through [`SignalingClient::snapshot`] because
    /// the IPC boundary is the only place where secrets can
    /// leak, and this method is not wired into any
    /// `#[tauri::command]`. This method exists for the
    /// integration tests in `tests/signaling.rs`.
    pub async fn bearer_for_test(&self) -> Option<BearerRecord> {
        self.inner.lock().await.bearer.clone()
    }

    /// **Test-only.** Returns the current count of inbound
    /// subscribers registered on the signaling client. The
    /// P2-T05 spec asserts that `RoomClient::request` does
    /// NOT grow this list; the corresponding test in
    /// `net::room::tests` exercises 1000 sequential
    /// requests and asserts the count never exceeds 1.
    #[cfg(test)]
    pub async fn subscribers_count_for_test(&self) -> usize {
        self.inner.lock().await.subscribers.len()
    }

    /// Subscribe to the inbound envelope stream. Every
    /// envelope the connection loop receives (post-handshake)
    /// is forwarded to the returned [`mpsc::UnboundedReceiver`].
    /// The stream is closed (the receiver returns `None`)
    /// when the connection is shut down or the signaling
    /// client is dropped.
    ///
    /// Used by [`crate::net::room::RoomClient`] to receive
    /// the ROOM_* and PRESENCE envelopes.
    pub async fn subscribe(&self) -> mpsc::UnboundedReceiver<Envelope> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut g = self.inner.lock().await;
        g.subscribers.push(tx);
        rx
    }

    /// Send a single envelope over the signaling WS. The
    /// envelope's `payload` should NOT include a `bearer`
    /// field; the client attaches the current bearer
    /// automatically. Returns an error if the WS is not
    /// currently authenticated (the bearer is not held) or
    /// if the connection is not running.
    pub async fn send_envelope(&self, env: Envelope) -> Result<(), SignalingError> {
        let (bearer, tx) = {
            let g = self.inner.lock().await;
            let bearer = g
                .bearer
                .as_ref()
                .ok_or(SignalingError::IdentityNotInitialized)?;
            let tx = g.outbound_tx.clone().ok_or(SignalingError::Protocol {
                message: "signaling not connected".into(),
            })?;
            (bearer.clone(), tx)
        };
        // Inject the bearer into the payload.
        let mut payload = env.payload.as_object().cloned().unwrap_or_default();
        let bearer_arr: Vec<serde_json::Value> = bearer
            .token
            .iter()
            .map(|b| serde_json::Value::from(*b))
            .collect();
        payload.insert("bearer".into(), serde_json::Value::Array(bearer_arr));
        let env = Envelope {
            v: env.v,
            r#type: env.r#type,
            id: env.id,
            room_id: env.room_id,
            sender: env.sender,
            ts_ms: env.ts_ms,
            seq: env.seq,
            payload: serde_json::Value::Object(payload),
        };
        tx.send(env).map_err(|_| SignalingError::Protocol {
            message: "ws send queue closed".into(),
        })?;
        // Wake the connection loop so it picks the
        // envelope up promptly. We re-acquire the lock here
        // rather than holding it across the rest of the
        // function so other readers (subscribers, the
        // connection loop's idle check) are not starved.
        self.inner.lock().await.outbound_notify.notify_one();
        Ok(())
    }
}

/// Top-level connection loop. Drives the state machine: open
/// socket -> handshake -> idle -> (disconnect -> backoff ->
/// retry). Exits when the cancellation token fires.
async fn connection_loop(
    config: Arc<SignalingConfig>,
    identity: Arc<IdentityService>,
    inner: Arc<Mutex<SignalingInner>>,
    cancel: CancellationToken,
) {
    // Install an outbound queue and stash the sender in
    // `inner` so [`SignalingClient::send_envelope`] can push
    // envelopes from any task. The notify is shared with
    // `inner` so the idle phase can wake on outbound
    // activity.
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Envelope>();
    {
        let mut g = inner.lock().await;
        g.outbound_tx = Some(outbound_tx);
    }
    let outbound_notify = inner.lock().await.outbound_notify.clone();
    let mut backoff = Backoff::new();
    let mut last_disconnect: Option<DisconnectReason> = None;
    loop {
        if cancel.is_cancelled() {
            set_phase(&inner, ConnPhase::ShuttingDown, None).await;
            return;
        }
        // If we observed a disconnect on the previous
        // iteration, surface Reconnecting first so the UI
        // can show "trying again in Ns" before the
        // Connecting phase begins.
        if let Some(reason) = last_disconnect.take() {
            set_phase(&inner, ConnPhase::Reconnecting, Some(format!("{reason:?}"))).await;
            let delay = backoff.next_delay();
            if sleep_with_cancel(&cancel, delay, &inner).await {
                return;
            }
        }
        // Phase: Connecting.
        set_phase(&inner, ConnPhase::Connecting, None).await;

        let open = tokio::time::timeout(
            config.handshake_timeout,
            tokio_tungstenite::connect_async(&config.url),
        )
        .await;

        let mut socket = match open {
            Ok(Ok((s, _resp))) => s,
            Ok(Err(e)) => {
                warn!(error = %e, url = %redact_url(&config.url), "ws connect failed");
                record_failure(
                    &inner,
                    &mut backoff,
                    DisconnectReason::NetworkUnreachable,
                    &e.to_string(),
                )
                .await;
                if sleep_with_cancel(&cancel, backoff.next_delay(), &inner).await {
                    return;
                }
                continue;
            }
            Err(_) => {
                warn!(
                    timeout_ms = config.handshake_timeout.as_millis() as u64,
                    "ws connect timed out"
                );
                record_failure(
                    &inner,
                    &mut backoff,
                    DisconnectReason::HandshakeTimeout,
                    "connect timeout",
                )
                .await;
                if sleep_with_cancel(&cancel, backoff.next_delay(), &inner).await {
                    return;
                }
                continue;
            }
        };

        // Phase: Handshaking.
        set_phase(&inner, ConnPhase::Handshaking, None).await;

        // Send HELLO.
        let hello_id = Uuid::now_v7();
        let hello_payload = serde_json::to_value(HelloPayload {
            client_version: crate::VERSION.to_string(),
            platform: config.platform,
            device_id: hello_id.to_string(),
        })
        .unwrap_or(serde_json::json!({}));
        let hello_env = Envelope {
            v: 1,
            r#type: MessageKind::Hello,
            id: hello_id,
            room_id: None,
            sender: None,
            ts_ms: now_ms(),
            seq: 1,
            payload: hello_payload,
        };
        if let Err(e) = send_envelope(&mut socket, &hello_env).await {
            warn!(error = %e, "send HELLO failed");
            record_failure(
                &inner,
                &mut backoff,
                DisconnectReason::NetworkUnreachable,
                &e.to_string(),
            )
            .await;
            continue;
        }

        // Read WELCOME + CHALLENGE. The server may send them
        // in either order; the v1 spec is that WELCOME comes
        // first. We accept either ordering.
        let handshake_result = tokio::time::timeout(
            config.handshake_timeout,
            run_handshake(
                &mut socket,
                &config,
                &identity,
                &inner,
                &cancel,
                &mut backoff,
            ),
        )
        .await;

        match handshake_result {
            Ok(HandshakeResult::Authenticated) => {
                // Phase: Authenticated. The run_handshake
                // helper already populated the bearer and set
                // the phase.
                backoff.reset();
                // Now idle on the read side: drop any further
                // messages until disconnect or cancel.
                let outcome = idle_until_disconnect(
                    &mut socket,
                    &config,
                    &inner,
                    &mut outbound_rx,
                    &cancel,
                    outbound_notify.clone(),
                    &mut backoff,
                )
                .await;
                // Keep `outbound_tx` installed for the next
                // iteration of the connection loop; a new
                // `outbound_tx` is only installed when a
                // fresh `start()` is called. The current
                // `outbound_rx` is dropped here; the next
                // `idle_until_disconnect` re-borrows it from
                // the channel.
                last_disconnect = outcome;
            }
            Ok(HandshakeResult::AuthFailed { reason, message }) => {
                warn!(reason = ?reason, "AUTH_FAIL received");
                // After an AUTH_FAIL, skip to the cap so a
                // banned or rate-limited client does not
                // tight-loop on the early schedule. We do
                // this BEFORE record_failure so the state
                // surface reflects the post-cap attempt
                // counter.
                backoff.skip_to_cap();
                record_failure(
                    &inner,
                    &mut backoff,
                    DisconnectReason::AuthFailed,
                    &format!("auth_fail: {message}"),
                )
                .await;
                if sleep_with_cancel(&cancel, backoff.next_delay(), &inner).await {
                    return;
                }
            }
            Ok(HandshakeResult::ProtocolError(msg)) => {
                warn!(error = %msg, "protocol error during handshake");
                record_failure(&inner, &mut backoff, DisconnectReason::ProtocolError, &msg).await;
                if sleep_with_cancel(&cancel, backoff.next_delay(), &inner).await {
                    return;
                }
            }
            Ok(HandshakeResult::Oversized { bytes }) => {
                warn!(bytes, "oversized frame during handshake");
                record_failure(
                    &inner,
                    &mut backoff,
                    DisconnectReason::ProtocolError,
                    &format!("oversized frame: {bytes} bytes"),
                )
                .await;
                if sleep_with_cancel(&cancel, backoff.next_delay(), &inner).await {
                    return;
                }
            }
            Ok(HandshakeResult::Cancelled) => {
                set_phase(&inner, ConnPhase::ShuttingDown, None).await;
                return;
            }
            Err(_) => {
                warn!(
                    timeout_ms = config.handshake_timeout.as_millis() as u64,
                    "handshake timed out"
                );
                record_failure(
                    &inner,
                    &mut backoff,
                    DisconnectReason::HandshakeTimeout,
                    "handshake timeout",
                )
                .await;
                if sleep_with_cancel(&cancel, backoff.next_delay(), &inner).await {
                    return;
                }
            }
        }
    }
}

/// Outcomes of the in-handshake read loop.
enum HandshakeResult {
    Authenticated,
    AuthFailed {
        reason: AuthFailReason,
        message: String,
    },
    ProtocolError(String),
    Oversized {
        bytes: usize,
    },
    Cancelled,
}
/// Read WELCOME, CHALLENGE, and AUTH_OK/AUTH_FAIL off the
/// socket. Loads the keypair on demand and sends AUTH. The
/// helper updates `inner` (state + bearer) as it goes.
async fn run_handshake(
    socket: &mut Ws,
    config: &SignalingConfig,
    identity: &Arc<IdentityService>,
    inner: &Arc<Mutex<SignalingInner>>,
    cancel: &CancellationToken,
    _backoff: &mut Backoff,
) -> HandshakeResult {
    // Frame limit: 16 frames is well above the worst case
    // (WELCOME + CHALLENGE + AUTH_OK, plus possible
    // AUTH_FAIL). Anything beyond this is treated as
    // protocol noise.
    const FRAME_LIMIT: usize = 16;

    let mut welcome: Option<WelcomePayload> = None;
    let mut nonce: Option<Vec<u8>> = None;
    let mut auth_sent = false;

    for _ in 0..FRAME_LIMIT {
        if cancel.is_cancelled() {
            return HandshakeResult::Cancelled;
        }
        let frame = match read_frame(socket, config).await {
            Ok(Some(f)) => f,
            Ok(None) => {
                return HandshakeResult::ProtocolError(
                    "connection closed mid-handshake".to_string(),
                );
            }
            Err(FrameError::Oversized { bytes, cap: _ }) => {
                return HandshakeResult::Oversized { bytes };
            }
            Err(FrameError::Decode(e)) => {
                return HandshakeResult::ProtocolError(format!("msgpack decode: {e}"));
            }
            Err(FrameError::Text) => {
                return HandshakeResult::ProtocolError("text frame during handshake".to_string());
            }
            Err(FrameError::Ws(e)) => {
                return HandshakeResult::ProtocolError(format!("ws error: {e}"));
            }
        };

        // Envelope validation: v=1, room_id absent.
        if frame.v != 1 {
            return HandshakeResult::ProtocolError(format!("unsupported v: {}", frame.v));
        }
        if frame.room_id.is_some() {
            return HandshakeResult::ProtocolError("room_id set during handshake".to_string());
        }

        match frame.r#type {
            MessageKind::Welcome => {
                let w: WelcomePayload = match serde_json::from_value(frame.payload) {
                    Ok(v) => v,
                    Err(e) => {
                        return HandshakeResult::ProtocolError(format!("bad WELCOME: {e}"));
                    }
                };
                {
                    let mut g = inner.lock().await;
                    g.state.session_id = Some(w.session_id.to_string());
                }
                welcome = Some(w);
            }
            MessageKind::Challenge => {
                let c: locast_protocol::handshake::ChallengePayload =
                    match serde_json::from_value(frame.payload) {
                        Ok(v) => v,
                        Err(e) => {
                            return HandshakeResult::ProtocolError(format!("bad CHALLENGE: {e}"));
                        }
                    };
                if c.nonce.len() != 32 {
                    return HandshakeResult::ProtocolError(format!(
                        "bad nonce length: {}",
                        c.nonce.len()
                    ));
                }
                nonce = Some(c.nonce);
            }
            MessageKind::AuthOk => {
                if !auth_sent {
                    return HandshakeResult::ProtocolError(
                        "AUTH_OK without prior AUTH".to_string(),
                    );
                }
                let ok: AuthOkPayload = match serde_json::from_value(frame.payload) {
                    Ok(v) => v,
                    Err(e) => {
                        return HandshakeResult::ProtocolError(format!("bad AUTH_OK: {e}"));
                    }
                };
                if ok.bearer.token.len() != 32 {
                    return HandshakeResult::ProtocolError(format!(
                        "bad bearer length: {}",
                        ok.bearer.token.len()
                    ));
                }
                if ok.pubkey.len() != 32 {
                    return HandshakeResult::ProtocolError(format!(
                        "bad pubkey length: {}",
                        ok.pubkey.len()
                    ));
                }
                let mut token = [0u8; 32];
                token.copy_from_slice(&ok.bearer.token);
                let mut pubkey = [0u8; 32];
                pubkey.copy_from_slice(&ok.pubkey);
                {
                    let mut g = inner.lock().await;
                    g.state.user_id = Some(ok.user_id.to_string());
                    g.bearer = Some(BearerRecord {
                        user_id: ok.user_id,
                        pubkey,
                        token,
                        expires_ms: ok.bearer.expires_ms,
                    });
                    g.state.connected = true;
                    g.state.phase = ConnPhase::Authenticated;
                    g.state.last_error = None;
                    g.state.last_error_at_ms = None;
                }
                debug!(
                    user_id = %ok.user_id,
                    token_fpr = %redact_token(&token),
                    "AUTH_OK received"
                );
                return HandshakeResult::Authenticated;
            }
            MessageKind::AuthFail => {
                let f: locast_protocol::handshake::AuthFailPayload =
                    match serde_json::from_value(frame.payload) {
                        Ok(v) => v,
                        Err(e) => {
                            return HandshakeResult::ProtocolError(format!("bad AUTH_FAIL: {e}"));
                        }
                    };
                return HandshakeResult::AuthFailed {
                    reason: f.reason,
                    message: reason_to_string(f.reason),
                };
            }
            _ => {
                return HandshakeResult::ProtocolError(format!(
                    "unexpected type during handshake: {}",
                    frame.r#type.as_str()
                ));
            }
        }

        // Once we have both WELCOME and CHALLENGE, sign the
        // nonce and send AUTH.
        if let (Some(_w), Some(nonce_bytes)) = (&welcome, &nonce) {
            if !auth_sent {
                let keypair = match identity.load_keypair().await {
                    Ok(k) => k,
                    Err(_) => {
                        return HandshakeResult::ProtocolError(
                            "identity not initialized".to_string(),
                        );
                    }
                };
                let sig = keypair.sign_challenge(nonce_bytes);
                let pubkey = keypair.public_key_bytes();
                let auth_id = Uuid::now_v7();
                let auth_payload = serde_json::to_value(AuthPayload {
                    pubkey: pubkey.to_vec(),
                    sig: sig.to_vec(),
                })
                .unwrap_or(serde_json::json!({}));
                let auth_env = Envelope {
                    v: 1,
                    r#type: MessageKind::Auth,
                    id: auth_id,
                    room_id: None,
                    sender: None,
                    ts_ms: now_ms(),
                    seq: 2,
                    payload: auth_payload,
                };
                if let Err(e) = send_envelope(socket, &auth_env).await {
                    return HandshakeResult::ProtocolError(format!("send AUTH: {e}"));
                }
                auth_sent = true;
            }
        }
    }

    HandshakeResult::ProtocolError("handshake frame limit exceeded".to_string())
}

/// After AUTH_OK, idle on the read side. The v1 client has
/// nothing else to do; P3+ adds room lifecycle / playback
/// message handlers here. For P2-T03 we count frames and
/// treat any disconnect as a trigger to reconnect. Returns
/// the [`DisconnectReason`] that describes how the idle
/// ended (or `None` if the loop was cancelled).
async fn idle_until_disconnect(
    socket: &mut Ws,
    config: &SignalingConfig,
    inner: &Arc<Mutex<SignalingInner>>,
    outbound_rx: &mut mpsc::UnboundedReceiver<Envelope>,
    cancel: &CancellationToken,
    outbound_notify: Arc<tokio::sync::Notify>,
    _backoff: &mut Backoff,
) -> Option<DisconnectReason> {
    loop {
        if cancel.is_cancelled() {
            set_phase(inner, ConnPhase::ShuttingDown, None).await;
            let _ = socket.close(None).await;
            return Some(DisconnectReason::LocalShutdown);
        }
        let mut reason: Option<DisconnectReason> = None;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                set_phase(inner, ConnPhase::ShuttingDown, None).await;
                let _ = socket.close(None).await;
                reason = Some(DisconnectReason::LocalShutdown);
            }
            env = outbound_rx.recv() => {
                if let Some(env) = env {
                    if let Err(e) = send_envelope(socket, &env).await {
                        warn!(error = %e, "ws send from outbound queue failed");
                        reason = Some(DisconnectReason::NetworkUnreachable);
                    }
                } else {
                    reason = Some(DisconnectReason::LocalShutdown);
                }
            }
            _ = outbound_notify.notified() => {
                while let Ok(env) = outbound_rx.try_recv() {
                    if let Err(e) = send_envelope(socket, &env).await {
                        warn!(error = %e, "ws send from outbound queue failed");
                        reason = Some(DisconnectReason::NetworkUnreachable);
                        break;
                    }
                }
            }
            frame_res = read_frame_async(socket, config) => {
                let frame: Result<Option<Envelope>, FrameError> = frame_res;
                match frame {
                    Ok(Some(env)) => {
                        let subs: Vec<mpsc::UnboundedSender<Envelope>> = {
                            let g = inner.lock().await;
                            g.subscribers.clone()
                        };
                        for tx in subs {
                            let _ = tx.send(env.clone());
                        }
                    }
                    Ok(None) => reason = Some(DisconnectReason::ServerClose),
                    Err(FrameError::Oversized { bytes, cap: _ }) => {
                        warn!(bytes, "oversized frame in idle; closing");
                        let _ = socket.close(None).await;
                        reason = Some(DisconnectReason::ProtocolError);
                    }
                    Err(FrameError::Text) => {
                        warn!("text frame in idle; closing");
                        let _ = socket.close(None).await;
                        reason = Some(DisconnectReason::ProtocolError);
                    }
                    Err(FrameError::Decode(e)) => {
                        warn!(error = %e, "decode error in idle; closing");
                        let _ = socket.close(None).await;
                        reason = Some(DisconnectReason::ProtocolError);
                    }
                    Err(FrameError::Ws(e)) => {
                        debug!(error = %e, "ws error in idle");
                        reason = Some(DisconnectReason::NetworkUnreachable);
                    }
                }
            }
        }
        if let Some(r) = reason {
            return Some(r);
        }
    }
}

/// Async wrapper around the read_frame helper for use inside
/// `tokio::select!`. Same semantics as `read_frame` but
/// returns a `Result<Option<Envelope>, FrameError>`.
async fn read_frame_async(
    socket: &mut Ws,
    config: &SignalingConfig,
) -> Result<Option<Envelope>, FrameError> {
    loop {
        let frame = match socket.next().await {
            Some(Ok(f)) => f,
            Some(Err(e)) => return Err(FrameError::Ws(Box::new(e))),
            None => return Ok(None),
        };
        match frame {
            WsMessage::Binary(bytes) => {
                if bytes.len() > config.max_frame_bytes {
                    return Err(FrameError::Oversized {
                        bytes: bytes.len(),
                        cap: config.max_frame_bytes,
                    });
                }
                let env: Envelope = rmp_serde::from_slice(&bytes).map_err(FrameError::Decode)?;
                return Ok(Some(env));
            }
            WsMessage::Text(_) => return Err(FrameError::Text),
            WsMessage::Close(_) => return Ok(None),
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
        }
    }
}

// ---------------------------------------------------------------------------
// Frame I/O
// ---------------------------------------------------------------------------

enum FrameError {
    Oversized {
        bytes: usize,
        /// The configured cap. Held for the call site to log
        /// if it wants; the connection loop currently does
        /// not, hence the dead-code allow.
        #[allow(dead_code)]
        cap: usize,
    },
    Decode(rmp_serde::decode::Error),
    Text,
    /// The underlying tungstenite error. Boxed to keep the
    /// enum's `Result<_, FrameError>` small enough that
    /// `clippy::result_large_err` does not fire.
    Ws(Box<tokio_tungstenite::tungstenite::Error>),
}

async fn read_frame(
    socket: &mut Ws,
    config: &SignalingConfig,
) -> Result<Option<Envelope>, FrameError> {
    loop {
        let frame = match socket.next().await {
            Some(Ok(f)) => f,
            Some(Err(e)) => return Err(FrameError::Ws(Box::new(e))),
            None => return Ok(None),
        };
        match frame {
            WsMessage::Binary(bytes) => {
                if bytes.len() > config.max_frame_bytes {
                    return Err(FrameError::Oversized {
                        bytes: bytes.len(),
                        cap: config.max_frame_bytes,
                    });
                }
                let env: Envelope = rmp_serde::from_slice(&bytes).map_err(FrameError::Decode)?;
                return Ok(Some(env));
            }
            WsMessage::Text(_) => return Err(FrameError::Text),
            WsMessage::Close(_) => return Ok(None),
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => {
                // Skip control / extension frames and read the
                // next data frame.
                continue;
            }
        }
    }
}

async fn send_envelope(socket: &mut Ws, env: &Envelope) -> Result<(), String> {
    let bytes = rmp_serde::to_vec_named(env).map_err(|e| e.to_string())?;
    socket
        .send(WsMessage::Binary(bytes))
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// State helpers
// ---------------------------------------------------------------------------

async fn set_phase(inner: &Arc<Mutex<SignalingInner>>, phase: ConnPhase, err: Option<String>) {
    let mut g = inner.lock().await;
    g.state.phase = phase;
    g.state.connected = matches!(phase, ConnPhase::Authenticated);
    if let Some(msg) = err {
        g.state.last_error = Some(msg);
        g.state.last_error_at_ms = Some(now_ms());
    }
}

async fn record_failure(
    inner: &Arc<Mutex<SignalingInner>>,
    backoff: &mut Backoff,
    reason: DisconnectReason,
    err: &str,
) {
    // The backoff counter is advanced by the caller, AFTER
    // record_failure runs, when it calls `backoff.next_delay()`
    // to compute the sleep duration. We do NOT advance here,
    // otherwise the caller would double-advance past the cap.
    // The UI sees the upcoming attempt (current + 1) so a
    // single failure already surfaces attempt >= 1.
    let mut g = inner.lock().await;
    g.state.phase = ConnPhase::Reconnecting;
    g.state.connected = false;
    g.bearer = None;
    g.state.session_id = None;
    g.state.user_id = None;
    g.state.attempt = backoff.attempt() + 1;
    g.state.last_error = Some(format!("{reason:?}: {err}"));
    g.state.last_error_at_ms = Some(now_ms());
}

async fn sleep_with_cancel(
    cancel: &CancellationToken,
    dur: Duration,
    inner: &Arc<Mutex<SignalingInner>>,
) -> bool {
    let cancelled = tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = cancel.cancelled() => true,
    };
    if cancelled {
        // Mark ShuttingDown so observers can see the
        // connection is going away.
        set_phase(inner, ConnPhase::ShuttingDown, None).await;
    }
    cancelled
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn redact_token(token: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(token);
    let digest = h.finalize();
    hex::encode(&digest[..3])
}

fn redact_url(url: &str) -> String {
    // For ws://user:pass@host/path, replace the userinfo
    // block. v1 URLs do not carry credentials, so this is
    // defense-in-depth.
    if let Some(idx) = url.find("://") {
        if let Some(at_idx) = url[idx + 3..].find('@') {
            let abs = idx + 3 + at_idx;
            return format!("{}://***@{}", &url[..idx], &url[abs + 1..]);
        }
    }
    url.to_string()
}

// ---------------------------------------------------------------------------
// AuthFailReason -> human-readable string (for the UI). The wire enum does
// not carry a Display impl; we map it explicitly.
// ---------------------------------------------------------------------------

fn reason_to_string(reason: AuthFailReason) -> String {
    match reason {
        AuthFailReason::BadSig => "bad_sig".to_string(),
        AuthFailReason::Expired => "expired".to_string(),
        AuthFailReason::Banned => "banned".to_string(),
        AuthFailReason::Rate => "rate".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_token_is_truncated_sha256() {
        let t = [7u8; 32];
        let s = redact_token(&t);
        // 3 bytes => 6 hex chars.
        assert_eq!(s.len(), 6);
    }

    #[test]
    fn redact_url_handles_userinfo() {
        assert_eq!(redact_url("ws://h/p"), "ws://h/p");
        assert_eq!(redact_url("ws://u:p@h/p"), "ws://***@h/p");
    }

    #[test]
    fn auth_fail_reason_str_matches_wire() {
        assert_eq!(reason_to_string(AuthFailReason::BadSig), "bad_sig");
        assert_eq!(reason_to_string(AuthFailReason::Expired), "expired");
        assert_eq!(reason_to_string(AuthFailReason::Banned), "banned");
        assert_eq!(reason_to_string(AuthFailReason::Rate), "rate");
    }
}
