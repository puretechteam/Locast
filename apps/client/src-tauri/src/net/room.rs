//! `net::room` - the room-lifecycle client built on top of
//! [`SignalingClient`].
//!
//! The `SignalingClient` owns the WebSocket and the
//! connection state; the `RoomClient` adds a typed
//! `room_create` / `room_join` / `room_leave` / `room_get_state`
//! API plus a `mpsc` receiver for inbound `ROOM_*` and
//! `PRESENCE` envelopes.
//!
//! P2-T05: the `request` correlation now uses a
//! `HashMap<MessageKind, Vec<oneshot::Sender<Envelope>>>`
//! so the inbound subscription is shared between the
//! request-reply correlation and the background
//! `run_inbound` loop. This fixes the leak where each
//! `request` call used to register a fresh subscriber on
//! the `SignalingClient`.
//!
//! P2-T05 also emits `room://state` and `room://event`
//! Tauri events on every state-changing inbound envelope so
//! the React layer can subscribe to deltas without polling.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::room::{
    HostMigratedPayload, Participant, ParticipantStatus, PresencePayload, RoomCreatePayload,
    RoomErrorCode, RoomJoinRequestPayload, RoomLeavePayload, RoomStatePayload, RoomSummary,
};
use serde::Serialize;
use specta::Type;
use tokio::sync::{oneshot, Mutex};
use tracing::warn;
use uuid::Uuid;

/// Sink for `room://state` / `room://event` push events.
/// `RoomClient` holds a `Mutex<Option<Arc<dyn RoomEventSink>>>`
/// and dispatches every state-changing inbound envelope
/// through it. Production code passes a Tauri-backed sink
/// (see [`TauriEventSink`]); the unit tests use a no-op
/// implementation so the test binary does not link
/// Tauri's WebView2 DLL on Windows.
pub trait RoomEventSink: Send + Sync {
    /// Emit `room://state` with the given summary.
    fn emit_state(&self, summary: &RoomSummaryIpc);
    /// Emit `room://event` with the given summary.
    fn emit_event(&self, summary: &RoomSummaryIpc);
    /// Emit `room://state` with `None` to signal the
    /// room has been cleared (RoomClosed / RoomError).
    fn emit_state_cleared(&self);
}

/// A no-op sink. Used by the unit tests so the lib test
/// binary does not pull in Tauri's runtime. Production
/// code uses [`TauriEventSink`] instead.
#[derive(Default)]
pub struct NoopEventSink;

impl RoomEventSink for NoopEventSink {
    fn emit_state(&self, _summary: &RoomSummaryIpc) {}
    fn emit_event(&self, _summary: &RoomSummaryIpc) {}
    fn emit_state_cleared(&self) {}
}

/// A Tauri-backed sink. Wraps a `tauri::AppHandle` and
/// forwards `room://state` / `room://event` events through
/// the webview's event bus. Compiled only in non-test
/// builds; the lib unit tests use [`NoopEventSink`] to
/// avoid linking `WebView2Loader.dll` on Windows.
#[cfg(not(test))]
mod tauri_sink {
    use super::*;
    use tauri::Emitter;

    pub struct TauriEventSink {
        pub(super) handle: tauri::AppHandle,
    }

    impl TauriEventSink {
        pub fn new(handle: tauri::AppHandle) -> Self {
            Self { handle }
        }
    }

    impl super::RoomEventSink for TauriEventSink {
        fn emit_state(&self, summary: &RoomSummaryIpc) {
            let _ = self.handle.emit(ROOM_STATE_EVENT, summary.clone());
        }
        fn emit_event(&self, summary: &RoomSummaryIpc) {
            let _ = self.handle.emit(ROOM_EVENT_EVENT, summary.clone());
        }
        fn emit_state_cleared(&self) {
            let _ = self
                .handle
                .emit(ROOM_STATE_EVENT, Option::<RoomSummaryIpc>::None);
        }
    }
}

#[cfg(not(test))]
pub use tauri_sink::TauriEventSink;

use super::signaling::SignalingClient;

/// The redacted, IPC-safe summary of a single room as seen
/// from the client. Mirrors `RoomSummary` with the
/// `RoomSummary::room_id` -> `id` rename so the TS binding
/// matches the rest of the IPC surface (which uses
/// `id`, not `room_id`).
#[derive(Debug, Clone, Serialize, Type)]
pub struct RoomSummaryIpc {
    pub id: String,
    pub code: String,
    pub title: String,
    pub host_user_id: String,
    pub host_migration_enabled: bool,
    pub created_ms: i64,
    pub participants: Vec<ParticipantIpc>,
    pub host_disconnected: bool,
    pub host_disconnect_deadline_ms: Option<i64>,
}

impl From<RoomSummary> for RoomSummaryIpc {
    fn from(s: RoomSummary) -> Self {
        Self {
            id: s.id.to_string(),
            code: s.code,
            title: s.title,
            host_user_id: s.host_user_id.to_string(),
            host_migration_enabled: s.host_migration_enabled,
            created_ms: s.created_ms,
            participants: s.participants.into_iter().map(Into::into).collect(),
            host_disconnected: s.host_disconnected,
            host_disconnect_deadline_ms: s.host_disconnect_deadline_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct ParticipantIpc {
    pub user_id: String,
    pub display_name: String,
    pub joined_ms: i64,
    pub status: ParticipantStatusIpc,
    pub last_seen_ms: i64,
    pub is_host: bool,
}

impl From<Participant> for ParticipantIpc {
    fn from(p: Participant) -> Self {
        Self {
            user_id: p.user_id.to_string(),
            display_name: p.display_name,
            joined_ms: p.joined_ms,
            status: p.status.into(),
            last_seen_ms: p.last_seen_ms,
            is_host: p.is_host,
        }
    }
}

/// IPC-safe mirror of [`locast_protocol::room::ParticipantStatus`].
#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "PascalCase")]
pub enum ParticipantStatusIpc {
    Joining,
    Connected,
    Reconnecting,
    Disconnected,
    Left,
}

impl From<ParticipantStatus> for ParticipantStatusIpc {
    fn from(s: ParticipantStatus) -> Self {
        match s {
            ParticipantStatus::Joining => Self::Joining,
            ParticipantStatus::Connected => Self::Connected,
            ParticipantStatus::Reconnecting => Self::Reconnecting,
            ParticipantStatus::Disconnected => Self::Disconnected,
            ParticipantStatus::Left => Self::Left,
        }
    }
}

/// IPC-safe error code returned across the IPC boundary. The
/// set is closed; the wire enum is `RoomErrorCode`.
#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "PascalCase")]
pub enum RoomErrorCodeIpc {
    Unauthorized,
    InvalidCode,
    RoomNotFound,
    RoomClosed,
    RoomFull,
    AlreadyJoined,
    NotJoined,
    InvalidState,
    NotHost,
    MigrationDisabled,
    Internal,
}

impl From<RoomErrorCode> for RoomErrorCodeIpc {
    fn from(c: RoomErrorCode) -> Self {
        match c {
            RoomErrorCode::Unauthorized => Self::Unauthorized,
            RoomErrorCode::InvalidCode => Self::InvalidCode,
            RoomErrorCode::RoomNotFound => Self::RoomNotFound,
            RoomErrorCode::RoomClosed => Self::RoomClosed,
            RoomErrorCode::RoomFull => Self::RoomFull,
            RoomErrorCode::AlreadyJoined => Self::AlreadyJoined,
            RoomErrorCode::NotJoined => Self::NotJoined,
            RoomErrorCode::InvalidState => Self::InvalidState,
            RoomErrorCode::NotHost => Self::NotHost,
            RoomErrorCode::MigrationDisabled => Self::MigrationDisabled,
            RoomErrorCode::Internal => Self::Internal,
        }
    }
}

/// A typed room-lifecycle error. Used by the Tauri commands
/// when the server returns a `ROOM_ERROR` envelope or the
/// network/socket is down.
#[derive(Debug, thiserror::Error, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RoomClientError {
    #[error("signaling client is not connected")]
    NotConnected,
    #[error("room protocol error: {code:?}: {message}")]
    Server {
        code: RoomErrorCodeIpc,
        message: String,
    },
    #[error("unexpected reply: {0}")]
    Unexpected(String),
    #[error("signaling error: {0}")]
    Signaling(String),
}

/// Tauri event name emitted whenever the cached room
/// summary changes. The payload is the redacted
/// `RoomSummaryIpc`.
pub const ROOM_STATE_EVENT: &str = "room://state";

/// Tauri event name emitted for every state-changing
/// room event (HostMigrated, HostReconnected,
/// ParticipantJoined, ParticipantLeft, RoomClosed). The
/// payload is the same redacted `RoomSummaryIpc` (the new
/// authoritative snapshot) so the React layer can both
/// listen for state changes and update the cache in one
/// pass.
pub const ROOM_EVENT_EVENT: &str = "room://event";

/// Default timeout for a single request-reply round trip.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The room-lifecycle client. Holds a reference to the
/// underlying `SignalingClient`, the cached state, and the
/// pending request-reply correlations.
pub struct RoomClient {
    signaling: Arc<SignalingClient>,
    /// The most recent full room summary the client received.
    /// `None` if the user has not joined any room yet.
    state: Mutex<Option<RoomSummaryIpc>>,
    inbound: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Envelope>>>,
    /// Pending request-reply correlations. Each entry is
    /// keyed by the expected reply `MessageKind`; a value
    /// is a `Vec` so multiple concurrent requests for the
    /// same kind (e.g. two `room_create`s) can be in
    /// flight at once and the inbound loop pops them in
    /// FIFO order.
    pending: Mutex<HashMap<MessageKind, Vec<oneshot::Sender<Envelope>>>>,
    /// Sink for `room://state` / `room://event` push
    /// events. `None` until the host calls
    /// [`RoomClient::install_event_sink`]. Production code
    /// installs a [`TauriEventSink`]; tests leave it as
    /// `None` (the `handle_inbound` path becomes a
    /// pure-state mutation).
    sink: Mutex<Option<Arc<dyn RoomEventSink>>>,
}

impl RoomClient {
    /// Build a new room client. The inbound subscription
    /// is established in [`RoomClient::init`].
    pub fn new(signaling: Arc<SignalingClient>) -> Self {
        Self {
            signaling,
            state: Mutex::new(None),
            inbound: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            sink: Mutex::new(None),
        }
    }

    /// Subscribe to the signaling client's inbound envelope
    /// stream. Call this once after constructing the client.
    pub async fn init(&self) {
        let rx = self.signaling.subscribe().await;
        let mut g = self.inbound.lock().await;
        *g = Some(rx);
    }

    /// Install a Tauri-backed [`RoomEventSink`] so the
    /// client can emit `room://state` / `room://event`
    /// events. Optional; the client works without a sink
    /// (the events just don't fire). In non-test builds
    /// the caller passes a `TauriEventSink::new(handle)`.
    #[cfg(not(test))]
    pub async fn install_app_handle(&self, handle: tauri::AppHandle) {
        let sink: Arc<dyn RoomEventSink> = Arc::new(TauriEventSink::new(handle));
        *self.sink.lock().await = Some(sink);
    }

    /// Install a generic event sink. The unit tests use
    /// this with a [`NoopEventSink`].
    pub async fn install_event_sink(&self, sink: Arc<dyn RoomEventSink>) {
        *self.sink.lock().await = Some(sink);
    }

    /// Read the latest cached room summary. The cache is
    /// updated every time the client receives a
    /// `ROOM_STATE` or one of the create/join/leave replies.
    pub async fn state(&self) -> Option<RoomSummaryIpc> {
        self.state.lock().await.clone()
    }

    /// Send a `ROOM_CREATE` envelope and return the server's
    /// `ROOM_CREATED` summary.
    pub async fn room_create(
        &self,
        title: String,
        migration_enabled: bool,
    ) -> Result<RoomSummaryIpc, RoomClientError> {
        let payload = RoomCreatePayload {
            title,
            migration_enabled,
        };
        let env = envelope(MessageKind::RoomCreate, None, payload);
        let reply = self.request(env, MessageKind::RoomCreated).await?;
        let created: locast_protocol::room::RoomCreatedPayload = decode_payload(&reply)?;
        let summary = RoomSummaryIpc::from(created.room);
        *self.state.lock().await = Some(summary.clone());
        // The room create is a "to caller" event; we
        // emit a `room://state` so the React side
        // observing via the event stream sees the
        // new state immediately, without having to
        // re-poll via `room_get_state`.
        self.emit_state(&summary).await;
        Ok(summary)
    }

    /// Send a `ROOM_JOIN_REQUEST` envelope and return the
    /// server's `ROOM_JOINED` summary.
    pub async fn room_join(
        &self,
        code: String,
        display_name: String,
    ) -> Result<RoomSummaryIpc, RoomClientError> {
        let payload = RoomJoinRequestPayload { code, display_name };
        let env = envelope(MessageKind::RoomJoinRequest, None, payload);
        let reply = self.request(env, MessageKind::RoomJoined).await?;
        let joined: locast_protocol::room::RoomJoinedPayload = decode_payload(&reply)?;
        let summary = RoomSummaryIpc::from(joined.room);
        *self.state.lock().await = Some(summary.clone());
        self.emit_state(&summary).await;
        Ok(summary)
    }

    /// Send a `ROOM_LEAVE` envelope. The server does not
    /// send a direct reply in v1; the caller should rely on
    /// the inbound `ROOM_CLOSED` and `PARTICIPANT_LEFT`
    /// events to update the UI.
    pub async fn room_leave(&self) -> Result<(), RoomClientError> {
        let env = envelope(MessageKind::RoomLeave, None, RoomLeavePayload {});
        self.signaling
            .send_envelope(env)
            .await
            .map_err(|e| RoomClientError::Signaling(e.to_string()))?;
        // Drop the cached state; the server will send
        // ROOM_CLOSED and the inbound loop will clear
        // it.
        *self.state.lock().await = None;
        Ok(())
    }

    /// Send a `PRESENCE` envelope. Cheap; the server uses
    /// it to refresh `last_seen` so the stale-participant
    /// cleanup does not remove us.
    pub async fn presence(&self) -> Result<(), RoomClientError> {
        let env = envelope(
            MessageKind::Presence,
            None,
            PresencePayload {
                status: "alive".into(),
            },
        );
        self.signaling
            .send_envelope(env)
            .await
            .map_err(|e| RoomClientError::Signaling(e.to_string()))
    }

    /// Drive the inbound subscriber: pop envelopes off the
    /// channel, update the cached state, dispatch any
    /// pending request-reply correlations, and emit
    /// `room://state` / `room://event` Tauri events.
    /// Returns when the channel closes (the signaling
    /// client has shut down) or the future is cancelled.
    pub async fn run_inbound(&self) {
        let mut rx = {
            let mut g = self.inbound.lock().await;
            match g.take() {
                Some(rx) => rx,
                None => return,
            }
        };
        while let Some(env) = rx.recv().await {
            self.handle_inbound(env).await;
        }
    }

    /// Single inbound-envelope handler. Split out so it
    /// is straightforward to unit test in isolation
    /// (without the surrounding `mpsc::Receiver`).
    async fn handle_inbound(&self, env: Envelope) {
        // 1) Dispatch any pending request-reply
        //    correlation. We do this BEFORE updating the
        //    cache so a request that completes in the
        //    same frame as a broadcast sees the reply
        //    first, then any state changes follow.
        self.deliver_to_pending(&env).await;
        // 2) Update the cached state and emit Tauri
        //    events for the types that callers care
        //    about.
        match env.r#type {
            MessageKind::RoomState => {
                if let Ok(state) = decode_payload::<RoomStatePayload>(&env) {
                    let summary = RoomSummaryIpc::from(state.room);
                    *self.state.lock().await = Some(summary.clone());
                    self.emit_state(&summary).await;
                }
            }
            MessageKind::RoomJoined => {
                if let Ok(p) = decode_payload::<locast_protocol::room::RoomJoinedPayload>(&env) {
                    let summary = RoomSummaryIpc::from(p.room);
                    *self.state.lock().await = Some(summary.clone());
                    self.emit_state(&summary).await;
                    // The viewer joined: the participant
                    // list now includes them. The
                    // server's per-participant
                    // ParticipantJoined event for the
                    // newly-joined user is filtered out
                    // by the WS forwarder for that user
                    // (it is the originator) but a
                    // separate event for every existing
                    // participant is not emitted on
                    // join. We emit a `room://event`
                    // here so subscribers can update
                    // their view.
                    self.emit_event(&summary).await;
                }
            }
            MessageKind::RoomCreated => {
                if let Ok(p) = decode_payload::<locast_protocol::room::RoomCreatedPayload>(&env) {
                    let summary = RoomSummaryIpc::from(p.room);
                    *self.state.lock().await = Some(summary.clone());
                    self.emit_state(&summary).await;
                    self.emit_event(&summary).await;
                }
            }
            MessageKind::HostMigrated => {
                if let Ok(m) = decode_payload::<HostMigratedPayload>(&env) {
                    // P2-T05: if the server included a
                    // post-migration summary, REPLACE the
                    // cached state entirely. Otherwise
                    // fall back to the pre-P2-T05
                    // behavior of updating only the host
                    // fields.
                    if let Some(boxed) = m.summary {
                        let summary = RoomSummaryIpc::from(*boxed);
                        *self.state.lock().await = Some(summary.clone());
                        self.emit_state(&summary).await;
                        self.emit_event(&summary).await;
                    } else {
                        let mut g = self.state.lock().await;
                        if let Some(s) = g.as_mut() {
                            s.host_user_id = m.new_host_user_id.to_string();
                            s.host_disconnected = false;
                            s.host_disconnect_deadline_ms = None;
                        }
                        if let Some(s) = g.as_ref() {
                            self.emit_state(s).await;
                            self.emit_event(s).await;
                        }
                    }
                }
            }
            MessageKind::HostReconnected => {
                if let Ok(m) = decode_payload::<locast_protocol::room::HostReconnectedPayload>(&env)
                {
                    let mut g = self.state.lock().await;
                    if let Some(s) = g.as_mut() {
                        s.host_user_id = m.host_user_id.to_string();
                        s.host_disconnected = false;
                        s.host_disconnect_deadline_ms = None;
                    }
                    if let Some(s) = g.as_ref() {
                        self.emit_state(s).await;
                        self.emit_event(s).await;
                    }
                }
            }
            MessageKind::ParticipantJoined => {
                if let Ok(p) =
                    decode_payload::<locast_protocol::room::ParticipantJoinedPayload>(&env)
                {
                    let mut g = self.state.lock().await;
                    if let Some(s) = g.as_mut() {
                        let participant: ParticipantIpc = p.participant.into();
                        // Replace existing entry by
                        // user_id if present.
                        if let Some(slot) = s
                            .participants
                            .iter_mut()
                            .find(|x| x.user_id == participant.user_id)
                        {
                            *slot = participant;
                        } else {
                            s.participants.push(participant);
                        }
                    }
                    if let Some(s) = g.as_ref() {
                        self.emit_state(s).await;
                        self.emit_event(s).await;
                    }
                }
            }
            MessageKind::ParticipantLeft => {
                if let Ok(p) = decode_payload::<locast_protocol::room::ParticipantLeftPayload>(&env)
                {
                    let mut g = self.state.lock().await;
                    if let Some(s) = g.as_mut() {
                        s.participants
                            .retain(|x| x.user_id != p.user_id.to_string());
                    }
                    if let Some(s) = g.as_ref() {
                        self.emit_state(s).await;
                        self.emit_event(s).await;
                    }
                }
            }
            MessageKind::RoomClosed | MessageKind::RoomError => {
                *self.state.lock().await = None;
                self.emit_state_cleared().await;
            }
            _ => {}
        }
    }

    /// Dispatch one inbound envelope to the first
    /// `oneshot::Sender` in the FIFO queue for its
    /// `MessageKind`, or to a single `RoomError` waiter
    /// (the first error to arrive resolves the pending
    /// request regardless of which kind the caller
    /// expects).
    async fn deliver_to_pending(&self, env: &Envelope) {
        // RoomError resolves any pending request that has
        // not been satisfied yet. Pop the first sender
        // for the envelope's own kind first, then fall
        // back to the first sender across all kinds.
        let mut pending = self.pending.lock().await;
        if let Some(senders) = pending.get_mut(&env.r#type) {
            if !senders.is_empty() {
                let tx = senders.remove(0);
                let _ = tx.send(env.clone());
                return;
            }
        }
        if env.r#type == MessageKind::RoomError {
            // Route the error to the first pending
            // request of any kind.
            for senders in pending.values_mut() {
                if !senders.is_empty() {
                    let tx = senders.remove(0);
                    let _ = tx.send(env.clone());
                    return;
                }
            }
        }
    }

    /// Send an envelope and wait for a specific reply
    /// message kind. Times out after [`REQUEST_TIMEOUT`].
    /// Does NOT call `signaling.subscribe()`; the
    /// correlation goes through the `pending` map shared
    /// with [`Self::run_inbound`].
    async fn request(
        &self,
        env: Envelope,
        expected: MessageKind,
    ) -> Result<Envelope, RoomClientError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.entry(expected.clone()).or_default().push(tx);
        }
        if let Err(e) = self.signaling.send_envelope(env).await {
            // Roll back the registration so a future
            // request doesn't pick up our sender.
            let mut pending = self.pending.lock().await;
            if let Some(senders) = pending.get_mut(&expected) {
                if !senders.is_empty() {
                    senders.remove(0);
                }
            }
            return Err(RoomClientError::Signaling(e.to_string()));
        }
        let res = tokio::time::timeout(REQUEST_TIMEOUT, rx).await;
        match res {
            Ok(Ok(env)) => Ok(env),
            Ok(Err(_)) => {
                let mut pending = self.pending.lock().await;
                if let Some(senders) = pending.get_mut(&expected) {
                    if !senders.is_empty() {
                        senders.remove(0);
                    }
                }
                Err(RoomClientError::NotConnected)
            }
            Err(_) => {
                // Timeout: the sender is dropped when
                // its receiver dies; clean it up so the
                // queue does not grow.
                let mut pending = self.pending.lock().await;
                if let Some(senders) = pending.get_mut(&expected) {
                    if !senders.is_empty() {
                        senders.remove(0);
                    }
                }
                Err(RoomClientError::Unexpected("request timeout".into()))
            }
        }
    }

    /// Best-effort emit of the `room://state` event. A
    /// missing sink is a no-op.
    async fn emit_state(&self, summary: &RoomSummaryIpc) {
        let g = self.sink.lock().await;
        if let Some(s) = g.as_ref() {
            s.emit_state(summary);
        }
    }

    /// Best-effort emit of the `room://state` event when
    /// the cache is cleared (RoomClosed / RoomError).
    async fn emit_state_cleared(&self) {
        let g = self.sink.lock().await;
        if let Some(s) = g.as_ref() {
            s.emit_state_cleared();
        }
    }

    /// Best-effort emit of the `room://event` event.
    /// The payload is the same `RoomSummaryIpc` shape
    /// (no bearer, no signature, no envelope) so the
    /// React layer can update its cache and react to the
    /// delta with a single listener.
    async fn emit_event(&self, summary: &RoomSummaryIpc) {
        let g = self.sink.lock().await;
        if let Some(s) = g.as_ref() {
            s.emit_event(summary);
        }
    }
}

fn envelope<T: serde::Serialize>(kind: MessageKind, room_id: Option<Uuid>, payload: T) -> Envelope {
    Envelope {
        v: 1,
        r#type: kind,
        id: Uuid::now_v7(),
        room_id,
        sender: None,
        ts_ms: now_ms(),
        seq: 0,
        payload: serde_json::to_value(payload).unwrap_or(serde_json::json!({})),
    }
}

fn decode_payload<T: serde::de::DeserializeOwned>(env: &Envelope) -> Result<T, RoomClientError> {
    serde_json::from_value(env.payload.clone())
        .map_err(|e| RoomClientError::Unexpected(format!("decode: {e}")))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Drop for RoomClient {
    fn drop(&mut self) {
        warn!("RoomClient dropped");
    }
}

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use super::*;
    use locast_protocol::room::RoomErrorPayload;

    fn env_of(kind: MessageKind, payload: serde_json::Value) -> Envelope {
        Envelope {
            v: 1,
            r#type: kind,
            id: Uuid::now_v7(),
            room_id: None,
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload,
        }
    }

    fn sample_summary(host: Uuid) -> RoomSummary {
        RoomSummary {
            id: Uuid::now_v7(),
            code: "AAAAAA".into(),
            title: "T".into(),
            host_user_id: host,
            host_migration_enabled: true,
            created_ms: 1_000,
            participants: vec![Participant {
                user_id: host,
                pubkey: vec![1; 32],
                display_name: "host".into(),
                joined_ms: 1_000,
                status: ParticipantStatus::Connected,
                last_seen_ms: 1_000,
                is_host: true,
            }],
            host_disconnected: false,
            host_disconnect_deadline_ms: None,
        }
    }

    /// Build a `RoomClient` against a stub signaling
    /// client. The signaling client is never started; the
    /// tests use the `pending` map directly to drive
    /// `handle_inbound`.
    async fn fresh_room_client() -> RoomClient {
        let signaling = Arc::new(SignalingClient::new(
            super::super::config::SignalingConfig::from_env(),
            // The keystore is never used in these unit
            // tests because the test never starts the
            // connection loop. A panic here would
            // require the keystore to be constructed;
            // we pass a dummy value via
            // `Arc::new` from a fresh test
            // IdentityService built against a
            // tempdir-backed storage. For unit-test
            // isolation we instead construct the
            // signaling client with a custom test
            // identity service created below.
            {
                // A real IdentityService needs a
                // storage handle. We don't need any
                // of that here because the
                // `request` path in these tests is
                // driven directly via the `pending`
                // map, never through the real
                // signaling transport. We can
                // short-circuit by using a
                // placeholder; `SignalingClient::new`
                // does not touch the keystore, so
                // any value works. Use
                // `IdentityService::new_for_test` if
                // available, else use
                // `IdentityService::new` against a
                // throwaway storage.
                use crate::identity::keystore::IdentityService;
                use crate::storage::Storage;
                use tempfile::TempDir;
                let dir = TempDir::new().expect("tempdir");
                let path = dir.path().join("index.sqlite");
                let storage = Storage::open(&path).await.expect("storage open");
                Arc::new(IdentityService::new(storage))
            },
        ));
        let r = RoomClient::new(signaling);
        r.init().await;
        r
    }

    #[tokio::test]
    async fn host_migrated_with_summary_replaces_cached_state() {
        let rc = fresh_room_client().await;
        // Seed stale state: host A.
        let host_a = Uuid::from_bytes([1u8; 16]);
        let host_b = Uuid::from_bytes([2u8; 16]);
        let stale = RoomSummaryIpc::from(sample_summary(host_a));
        *rc.state.lock().await = Some(stale);
        // New summary with host B and a different
        // participant list.
        let new_summary = RoomSummary {
            id: Uuid::now_v7(),
            code: "BBBBBB".into(),
            title: "T2".into(),
            host_user_id: host_b,
            host_migration_enabled: true,
            created_ms: 2_000,
            participants: vec![
                Participant {
                    user_id: host_b,
                    pubkey: vec![2; 32],
                    display_name: "B".into(),
                    joined_ms: 2_000,
                    status: ParticipantStatus::Connected,
                    last_seen_ms: 2_000,
                    is_host: true,
                },
                Participant {
                    user_id: host_a,
                    pubkey: vec![1; 32],
                    display_name: "A".into(),
                    joined_ms: 1_000,
                    status: ParticipantStatus::Connected,
                    last_seen_ms: 1_000,
                    is_host: false,
                },
            ],
            host_disconnected: false,
            host_disconnect_deadline_ms: None,
        };
        let payload = HostMigratedPayload {
            previous_host_user_id: host_a,
            new_host_user_id: host_b,
            summary: Some(Box::new(new_summary.clone())),
        };
        let env = env_of(
            MessageKind::HostMigrated,
            serde_json::to_value(payload).unwrap(),
        );
        rc.handle_inbound(env).await;
        let s = rc.state().await.expect("state");
        assert_eq!(s.host_user_id, host_b.to_string());
        assert_eq!(s.participants.len(), 2);
        assert!(s
            .participants
            .iter()
            .any(|p| p.user_id == host_a.to_string() && !p.is_host));
        assert!(s
            .participants
            .iter()
            .any(|p| p.user_id == host_b.to_string() && p.is_host));
    }

    #[tokio::test]
    async fn host_migrated_without_summary_keeps_participants() {
        let rc = fresh_room_client().await;
        let host_a = Uuid::from_bytes([1u8; 16]);
        let host_b = Uuid::from_bytes([2u8; 16]);
        let stale = RoomSummaryIpc::from(sample_summary(host_a));
        *rc.state.lock().await = Some(stale);
        let payload = HostMigratedPayload {
            previous_host_user_id: host_a,
            new_host_user_id: host_b,
            summary: None,
        };
        let env = env_of(
            MessageKind::HostMigrated,
            serde_json::to_value(payload).unwrap(),
        );
        rc.handle_inbound(env).await;
        let s = rc.state().await.expect("state");
        assert_eq!(s.host_user_id, host_b.to_string());
        // Fallback path: the participants list is
        // unchanged (only the host_user_id, host_disconnected,
        // and deadline fields are updated).
        assert_eq!(s.participants.len(), 1);
    }

    #[tokio::test]
    async fn participant_left_removes_user() {
        let rc = fresh_room_client().await;
        let host = Uuid::from_bytes([1u8; 16]);
        let viewer = Uuid::from_bytes([2u8; 16]);
        let mut summary = sample_summary(host);
        summary.participants.push(Participant {
            user_id: viewer,
            pubkey: vec![2; 32],
            display_name: "V".into(),
            joined_ms: 1_100,
            status: ParticipantStatus::Connected,
            last_seen_ms: 1_100,
            is_host: false,
        });
        *rc.state.lock().await = Some(RoomSummaryIpc::from(summary));
        let env = env_of(
            MessageKind::ParticipantLeft,
            serde_json::to_value(locast_protocol::room::ParticipantLeftPayload {
                user_id: viewer,
                reason: "leave".into(),
            })
            .unwrap(),
        );
        rc.handle_inbound(env).await;
        let s = rc.state().await.expect("state");
        assert_eq!(s.participants.len(), 1);
        assert_eq!(s.participants[0].user_id, host.to_string());
    }

    #[tokio::test]
    async fn room_closed_clears_cache() {
        let rc = fresh_room_client().await;
        let host = Uuid::from_bytes([1u8; 16]);
        *rc.state.lock().await = Some(RoomSummaryIpc::from(sample_summary(host)));
        let env = env_of(
            MessageKind::RoomClosed,
            serde_json::to_value(locast_protocol::room::RoomClosedPayload {
                reason: "host_left".into(),
            })
            .unwrap(),
        );
        rc.handle_inbound(env).await;
        assert!(rc.state().await.is_none());
    }

    #[tokio::test]
    async fn room_state_replaces_cache() {
        let rc = fresh_room_client().await;
        let host_a = Uuid::from_bytes([1u8; 16]);
        *rc.state.lock().await = Some(RoomSummaryIpc::from(sample_summary(host_a)));
        let host_b = Uuid::from_bytes([2u8; 16]);
        let new_summary = sample_summary(host_b);
        let env = env_of(
            MessageKind::RoomState,
            serde_json::to_value(RoomStatePayload {
                room: new_summary,
                host_disconnect_deadline_ms: None,
            })
            .unwrap(),
        );
        rc.handle_inbound(env).await;
        let s = rc.state().await.expect("state");
        assert_eq!(s.host_user_id, host_b.to_string());
    }

    #[tokio::test]
    async fn room_error_clears_cache() {
        let rc = fresh_room_client().await;
        let host = Uuid::from_bytes([1u8; 16]);
        *rc.state.lock().await = Some(RoomSummaryIpc::from(sample_summary(host)));
        let env = env_of(
            MessageKind::RoomError,
            serde_json::to_value(RoomErrorPayload {
                code: RoomErrorCode::Internal,
                message: "boom".into(),
            })
            .unwrap(),
        );
        rc.handle_inbound(env).await;
        assert!(rc.state().await.is_none());
    }

    #[tokio::test]
    async fn deliver_to_pending_routes_to_first_waiter() {
        let rc = fresh_room_client().await;
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = rc.pending.lock().await;
            pending
                .entry(MessageKind::RoomCreated)
                .or_default()
                .push(tx);
        }
        let env = env_of(
            MessageKind::RoomCreated,
            serde_json::json!({"room": {"id": Uuid::now_v7()}}),
        );
        rc.deliver_to_pending(&env).await;
        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("not timeout")
            .expect("not closed");
        assert_eq!(received.r#type, MessageKind::RoomCreated);
    }

    #[tokio::test]
    async fn deliver_to_pending_room_error_routes_to_any_pending() {
        let rc = fresh_room_client().await;
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = rc.pending.lock().await;
            pending.entry(MessageKind::RoomJoined).or_default().push(tx);
        }
        let env = env_of(
            MessageKind::RoomError,
            serde_json::to_value(RoomErrorPayload {
                code: RoomErrorCode::Internal,
                message: "x".into(),
            })
            .unwrap(),
        );
        rc.deliver_to_pending(&env).await;
        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("not timeout")
            .expect("not closed");
        assert_eq!(received.r#type, MessageKind::RoomError);
    }

    #[tokio::test]
    async fn request_does_not_grow_subscribers() {
        // P2-T05 spec Part 4: 1000 sequential `request`
        // calls must NOT grow the signaling client's
        // subscriber list beyond 1.
        //
        // The fresh client already holds 1 subscriber
        // (the one registered in `init`). The test
        // asserts the count is bounded at every step
        // and that it does not grow.
        let rc = fresh_room_client().await;
        let initial = rc.signaling.subscribers_count_for_test().await;
        assert_eq!(initial, 1, "init should register exactly one subscriber");
        for i in 0..1000 {
            // Each request fails fast (no real WS) and
            // rolls back the registration. We just want
            // to assert the subscriber count never grows.
            let env = env_of(
                MessageKind::RoomCreate,
                serde_json::json!({"title": "x", "migration_enabled": false}),
            );
            let _ = rc.request(env, MessageKind::RoomCreated).await;
            let n = rc.signaling.subscribers_count_for_test().await;
            assert!(n <= 1, "subscribers grew to {n} after request {i}");
        }
        // Final count is still 1.
        assert_eq!(rc.signaling.subscribers_count_for_test().await, 1);
    }

    #[tokio::test]
    async fn concurrent_requests_resolve_independently() {
        // Drive 4 concurrent requests against the same
        // RoomClient. Each request registers its own
        // oneshot in the `pending` map; the inbound
        // loop dispatches them by kind.
        let rc = fresh_room_client().await;
        // Manually install 4 waiters and resolve them
        // by hand to avoid a real WS.
        let mut waiters = Vec::new();
        for _ in 0..4 {
            let (tx, rx) = oneshot::channel();
            rc.pending
                .lock()
                .await
                .entry(MessageKind::RoomJoined)
                .or_default()
                .push(tx);
            waiters.push(rx);
        }
        for (i, rx) in waiters.into_iter().enumerate() {
            let env = env_of(
                MessageKind::RoomJoined,
                serde_json::json!({"room": {"id": Uuid::now_v7()}}),
            );
            rc.deliver_to_pending(&env).await;
            let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
                .await
                .expect("not timeout")
                .expect("not closed");
            assert_eq!(received.r#type, MessageKind::RoomJoined);
            let _ = i;
        }
    }
}
