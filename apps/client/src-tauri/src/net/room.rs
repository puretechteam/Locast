//! `net::room` - the room-lifecycle client built on top of
//! [`SignalingClient`].
//!
//! The `SignalingClient` owns the WebSocket and the
//! connection state; the `RoomClient` adds a typed
//! `room_create` / `room_join` / `room_leave` / `room_get_state`
//! API plus a `mpsc` receiver for inbound `ROOM_*` and
//! `PRESENCE` envelopes.
//!
//! P2-T04: the room lifecycle.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::room::{
    HostMigratedPayload, Participant, ParticipantStatus, PresencePayload, RoomCreatePayload,
    RoomErrorCode, RoomErrorPayload, RoomJoinRequestPayload, RoomLeavePayload, RoomStatePayload,
    RoomSummary,
};
use serde::Serialize;
use specta::Type;
use tokio::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

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

/// Room-error code returned across the IPC boundary. The
/// set is closed; the wire enum is `RoomErrorCode`.
#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(tag = "code", rename_all = "PascalCase")]
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

/// The room-lifecycle client. Holds a reference to the
/// underlying `SignalingClient` and an inbound `mpsc`
/// receiver for the ROOM_* and PRESENCE envelopes.
pub struct RoomClient {
    signaling: Arc<SignalingClient>,
    /// The most recent full room summary the client received.
    /// `None` if the user has not joined any room yet.
    state: Mutex<Option<RoomSummaryIpc>>,
    inbound: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Envelope>>>,
}

impl RoomClient {
    /// Build a new room client and register an inbound
    /// subscription with the signaling client.
    pub fn new(signaling: Arc<SignalingClient>) -> Self {
        // We can't `await` in this constructor because the
        // underlying `SignalingClient::subscribe` is async.
        // Callers that need an inbound receiver should use
        // [`RoomClient::init`].
        Self {
            signaling,
            state: Mutex::new(None),
            inbound: Mutex::new(None),
        }
    }

    /// Subscribe to the signaling client's inbound envelope
    /// stream. Call this once after constructing the client.
    pub async fn init(&self) {
        let rx = self.signaling.subscribe().await;
        let mut g = self.inbound.lock().await;
        *g = Some(rx);
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
    /// channel and update the cached state. Returns when
    /// the channel closes (the signaling client has shut
    /// down) or the future is cancelled.
    pub async fn run_inbound(&self) {
        let mut rx = {
            let mut g = self.inbound.lock().await;
            match g.take() {
                Some(rx) => rx,
                None => return,
            }
        };
        while let Some(env) = rx.recv().await {
            match env.r#type {
                MessageKind::RoomState => {
                    if let Ok(state) = decode_payload::<RoomStatePayload>(&env) {
                        let summary = RoomSummaryIpc::from(state.room);
                        *self.state.lock().await = Some(summary);
                    }
                }
                MessageKind::HostMigrated => {
                    if let Ok(m) = decode_payload::<HostMigratedPayload>(&env) {
                        // Update the cached `host_user_id` and
                        // clear the grace-window state. The
                        // HOST_MIGRATED payload carries the
                        // authoritative new host; the
                        // participant list is unchanged so
                        // no further refresh is required.
                        let mut g = self.state.lock().await;
                        if let Some(s) = g.as_mut() {
                            s.host_user_id = m.new_host_user_id.to_string();
                            s.host_disconnected = false;
                            s.host_disconnect_deadline_ms = None;
                        }
                    }
                }
                MessageKind::RoomClosed | MessageKind::RoomError => {
                    *self.state.lock().await = None;
                }
                _ => {}
            }
        }
    }

    /// Send an envelope and wait for a specific reply
    /// message kind. Times out after 10 seconds.
    async fn request(
        &self,
        env: Envelope,
        expected: MessageKind,
    ) -> Result<Envelope, RoomClientError> {
        // Subscribe to the inbound stream BEFORE sending
        // so we don't miss the reply.
        let mut rx = self.signaling.subscribe().await;
        self.signaling
            .send_envelope(env)
            .await
            .map_err(|e| RoomClientError::Signaling(e.to_string()))?;
        let deadline = std::time::Duration::from_secs(10);
        let res = tokio::time::timeout(deadline, async {
            while let Some(env) = rx.recv().await {
                if env.r#type == MessageKind::RoomError {
                    let p: RoomErrorPayload = match serde_json::from_value(env.payload) {
                        Ok(p) => p,
                        Err(e) => {
                            return Err(RoomClientError::Unexpected(format!(
                                "bad RoomErrorPayload: {e}"
                            )));
                        }
                    };
                    return Err(RoomClientError::Server {
                        code: p.code.into(),
                        message: p.message,
                    });
                }
                if env.r#type == expected {
                    return Ok(env);
                }
            }
            Err(RoomClientError::NotConnected)
        })
        .await;
        match res {
            Ok(r) => r,
            Err(_) => Err(RoomClientError::Unexpected("request timeout".into())),
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
        // Best-effort: the inbound receiver will be
        // dropped when this struct is dropped, which
        // closes the channel on the sender side. The
        // signaling client can then move on.
        warn!("RoomClient dropped");
    }
}
