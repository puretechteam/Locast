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
//!
//! P3-T03: handles `MANIFEST_PUBLISHED` inbound envelopes
//! by verifying the signed manifest, persisting it to the
//! local `room_manifests` table, and emitting a
//! `manifest://state` Tauri event. The Tauri event carries
//! `{ room_id, manifest_hash, version }` (a small payload;
//! the full manifest stays in the Rust cache for the
//! download planner).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::room::{
    HostMigratedPayload, Participant, ParticipantStatus, PresencePayload, RoomCreatePayload,
    RoomErrorCode, RoomErrorPayload, RoomJoinRequestPayload, RoomLeavePayload, RoomStatePayload,
    RoomSummary,
};
use serde::Serialize;
use specta::Type;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
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
    /// P3-T03: emit `manifest://state` with a verified
    /// manifest descriptor. The default no-op impl is
    /// `()`; the Tauri-backed sink forwards to the
    /// webview.
    fn emit_manifest_state(&self, _ev: &ManifestStateEvent) {}
    /// P4-T02: emit `playback://state` with the
    /// authoritative room playback state. The default
    /// no-op impl is `()`; the Tauri-backed sink
    /// forwards to the webview.
    fn emit_playback_state(&self, _ev: &PlaybackStateEvent) {}
    /// P4-T03: emit `position://report` with a viewer's
    /// POSITION_REPORT observation (the server forwards
    /// the original payload verbatim). The default no-op
    /// impl is `()`; the Tauri-backed sink forwards to the
    /// webview. The receiving client (typically the host)
    /// uses this to render the per-viewer position
    /// indicator.
    fn emit_position_report(&self, _ev: &PositionReportEvent) {}
    /// P5-T03: emit `drawing://begin` when a remote
    /// DRAW_BEGIN is accepted and rebroadcast by the server.
    fn emit_stroke_begin(&self, _ev: &StrokeBeginEvent) {}
    /// P5-T03: emit `drawing://point` when a remote
    /// DRAW_POINT is accepted and rebroadcast by the server.
    fn emit_stroke_point(&self, _ev: &StrokePointEvent) {}
    /// P5-T03: emit `drawing://end` when a remote
    /// DRAW_END is accepted and rebroadcast by the server.
    fn emit_stroke_end(&self, _ev: &StrokeEndEvent) {}
    /// P5-T03: emit `drawing://undo` when a DRAW_UNDO is accepted
    /// and rebroadcast by the server. Delivered to every
    /// participant, the actor included: the stroke is removed from
    /// the canvas only on this event.
    fn emit_stroke_undo(&self, _ev: &StrokeUndoEvent) {}
    /// P5-T03: emit `drawing://clear` when a DRAW_CLEAR is accepted
    /// and rebroadcast by the server (actor included).
    fn emit_stroke_clear(&self, _ev: &StrokeClearEvent) {}
    /// Emit `drawing://sync` when the server sends a DRAW_SYNC.
    fn emit_stroke_sync(&self, _ev: &StrokeSyncEvent) {}
    /// P5-T04: emit `laser://move` when the server relays another
    /// participant's LASER_MOVE.
    fn emit_laser_move(&self, _ev: &LaserMoveEvent) {}
    /// P5-T04: emit `laser://off` when the server relays another
    /// participant's LASER_OFF.
    fn emit_laser_off(&self, _ev: &LaserOffEvent) {}
    /// P6-T03: emit `chat://message` when the server broadcasts a
    /// CHAT_MESSAGE (the local user's own message included).
    fn emit_chat_message(&self, _ev: &ChatMessageEvent) {}
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
    fn emit_stroke_begin(&self, _ev: &StrokeBeginEvent) {}
    fn emit_stroke_point(&self, _ev: &StrokePointEvent) {}
    fn emit_stroke_end(&self, _ev: &StrokeEndEvent) {}
    fn emit_stroke_undo(&self, _ev: &StrokeUndoEvent) {}
    fn emit_stroke_clear(&self, _ev: &StrokeClearEvent) {}
    fn emit_stroke_sync(&self, _ev: &StrokeSyncEvent) {}
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
        fn emit_manifest_state(&self, ev: &ManifestStateEvent) {
            let _ = self.handle.emit(MANIFEST_STATE_EVENT, ev.clone());
        }
        fn emit_playback_state(&self, ev: &PlaybackStateEvent) {
            let _ = self.handle.emit(PLAYBACK_STATE_EVENT, ev.clone());
        }
        fn emit_position_report(&self, ev: &PositionReportEvent) {
            let _ = self.handle.emit(POSITION_REPORT_EVENT, ev.clone());
        }
        fn emit_stroke_begin(&self, ev: &StrokeBeginEvent) {
            let _ = self.handle.emit(STROKE_BEGIN_EVENT, ev.clone());
        }
        fn emit_stroke_point(&self, ev: &StrokePointEvent) {
            let _ = self.handle.emit(STROKE_POINT_EVENT, ev.clone());
        }
        fn emit_stroke_end(&self, ev: &StrokeEndEvent) {
            let _ = self.handle.emit(STROKE_END_EVENT, ev.clone());
        }
        fn emit_stroke_undo(&self, ev: &StrokeUndoEvent) {
            let _ = self.handle.emit(STROKE_UNDO_EVENT, ev.clone());
        }
        fn emit_stroke_clear(&self, ev: &StrokeClearEvent) {
            let _ = self.handle.emit(STROKE_CLEAR_EVENT, ev.clone());
        }
        fn emit_stroke_sync(&self, ev: &StrokeSyncEvent) {
            let _ = self.handle.emit(STROKE_SYNC_EVENT, ev.clone());
        }
        fn emit_laser_move(&self, ev: &LaserMoveEvent) {
            let _ = self.handle.emit(LASER_MOVE_EVENT, ev.clone());
        }
        fn emit_laser_off(&self, ev: &LaserOffEvent) {
            let _ = self.handle.emit(LASER_OFF_EVENT, ev.clone());
        }
        fn emit_chat_message(&self, ev: &ChatMessageEvent) {
            let _ = self.handle.emit(CHAT_MESSAGE_EVENT, ev.clone());
        }
    }
}

#[cfg(not(test))]
pub use tauri_sink::TauriEventSink;

use super::signaling::SignalingClient;
use super::state::ConnPhase;

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
    /// The local user's capability bitfield from `you.cap_set`
    /// in `ROOM_CREATED` / `ROOM_JOINED`. `None` until the
    /// room summary is loaded from a create/join response.
    pub you_cap_set: Option<u32>,
    /// The local user's server-assigned `user_id`, filled from
    /// the `RoomClient`'s `local_user_id` whenever the summary
    /// is read (`room_get_state`) or emitted (`room://state`,
    /// `room://event`). Lets the webview tell host from viewer
    /// (`you_user_id == host_user_id`). Display only: the
    /// server enforces every host-only action.
    pub you_user_id: Option<String>,
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
            you_cap_set: None,
            you_user_id: None,
        }
    }
}

/// Whether a ROOM_ERROR means the local user is no longer
/// in the room. A payload that does not decode counts as
/// ending it (the conservative, pre-existing behavior).
fn room_error_ends_membership(env: &Envelope) -> bool {
    match decode_payload::<RoomErrorPayload>(env) {
        Ok(p) => matches!(
            p.code,
            RoomErrorCode::Unauthorized
                | RoomErrorCode::RoomNotFound
                | RoomErrorCode::RoomClosed
                | RoomErrorCode::NotJoined
        ),
        Err(_) => true,
    }
}

/// The local user's cap set after a HOST_MIGRATED: the
/// promoted participant gets `cap::HOST`, the demoted host
/// `cap::CHAT` (as `rooms::host::elect_new_host` on the
/// server sets them); anyone else keeps `previous`.
fn migrated_you_cap_set(
    previous: Option<u32>,
    local: Option<Uuid>,
    m: &HostMigratedPayload,
) -> Option<u32> {
    match local {
        Some(me) if me == m.new_host_user_id => Some(locast_protocol::room::cap::HOST),
        Some(me) if me == m.previous_host_user_id => Some(locast_protocol::room::cap::CHAT),
        _ => previous,
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
    pub cap_set: u32,
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
            cap_set: 0,
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
    /// P4-T01: server rejected a playback command
    /// because the per-sender `monotonic_seq` was
    /// outside the valid window (duplicate replay or
    /// gap). The client should reconnect and re-sync
    /// its sender-side sequence counter before
    /// issuing further playback commands.
    StaleCommand,
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
            RoomErrorCode::StaleCommand => Self::StaleCommand,
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
    #[error("manifest rejected: {0}")]
    ManifestRejected(String),
}

/// Closed set of reasons the verifier pipeline rejects a
/// manifest. Used by `RoomClient::accept_manifest` and
/// surfaced (in display form) on the `manifest_fetch`
/// late-join path.
#[derive(Debug, thiserror::Error)]
pub enum ManifestAcceptError {
    #[error("signature verification failed: {0}")]
    BadSignature(String),
    #[error("malformed manifest room_id: {0}")]
    BadRoomId(String),
    #[error("host_signature.public_key wrong length: {0}")]
    BadPubkeyLength(usize),
    #[error("host_signature.public_key not valid base64")]
    BadPubkeyFormat,
    #[error("manifest has no host_signature")]
    NoHostSignature,
    #[error("no trust anchor installed (set_expected_host_pubkey)")]
    NoTrustAnchor,
    #[error("host_signature.public_key does not match invite h=")]
    TrustAnchorMismatch,
    #[error("canonical serialization failed: {0}")]
    Canonicalize(String),
    #[error("stale version: incoming {incoming} < cached {cached}")]
    StaleVersion { incoming: i64, cached: i64 },
}

impl From<ManifestAcceptError> for RoomClientError {
    fn from(e: ManifestAcceptError) -> Self {
        RoomClientError::ManifestRejected(e.to_string())
    }
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

/// P3-T03: Tauri event name emitted when a verified
/// manifest has been accepted into the local cache. The
/// payload is a small descriptor
/// (`{ room_id, manifest_hash, version }`); the full
/// manifest is held in the Rust
/// [`RoomClient::verified_manifests`] cache for the
/// download planner (P3-T04) to read. The event is only
/// emitted for manifests that pass the
/// `locast_manifest::verify_manifest` check.
pub const MANIFEST_STATE_EVENT: &str = "manifest://state";

/// P4-T02: Tauri event name emitted whenever the server
/// accepts a host `PLAYBACK_CMD` and rebroadcasts it as
/// a [`PlaybackAcceptedEvent`]. The payload is the
/// authoritative room playback state: the server's
/// `server_seq`, the server-stamped `server_ts_ms`, the
/// original sender's `monotonic_seq`, the action, the
/// `media_position_ms`, and the originator's
/// `sender_id`. The React client drives its local
/// `<video>` element from this event; the originating
/// host's local UI should ignore the rebroadcast for
/// its own commands (the host applied the change
/// locally before sending the command).
pub const PLAYBACK_STATE_EVENT: &str = "playback://state";

/// P4-T03: Tauri event name emitted whenever the server
/// forwards a non-authoritative POSITION_REPORT from a
/// participant. The payload is a small IPC-safe struct
/// with the report's `media_position_ms` + `playing`
/// flag + the originator's `user_id`. The server forwards
/// the original payload verbatim; the Rust side just
/// decorates it with the room id and the originating
/// `user_id` so the React layer can key positions by
/// sender (multi-viewer distinction).
pub const POSITION_REPORT_EVENT: &str = "position://report";

/// P5-T03: Tauri event name emitted when a remote
/// DRAW_BEGIN is accepted and rebroadcast by the server.
/// The payload carries the sender_id, room_id, and the
/// verified begin payload so the React layer can create
/// a remote stroke.
pub const STROKE_BEGIN_EVENT: &str = "drawing://begin";

/// P5-T03: Tauri event name emitted when a remote
/// DRAW_POINT is accepted and rebroadcast by the server.
pub const STROKE_POINT_EVENT: &str = "drawing://point";

/// P7-T01 review-fix: maximum number of envelopes the
/// room client will buffer in `pending_outbound` while the
/// signaling connection is offline. Beyond this cap, the
/// oldest envelope is dropped (with a WARN) so an extended
/// outage combined with a UI emit-storm cannot grow the
/// buffer without bound and OOM the native side.
pub const PENDING_OUTBOUND_CAP: usize = 256;

/// P5-T03: Tauri event name emitted when a remote
/// DRAW_END is accepted and rebroadcast by the server.
pub const STROKE_END_EVENT: &str = "drawing://end";

/// P5-T03: Tauri event name emitted when a DRAW_UNDO is accepted
/// and rebroadcast by the server (to the actor too).
pub const STROKE_UNDO_EVENT: &str = "drawing://undo";

/// P5-T03: Tauri event name emitted when a DRAW_CLEAR is accepted
/// and rebroadcast by the server (to the actor too).
pub const STROKE_CLEAR_EVENT: &str = "drawing://clear";

/// Tauri event name emitted when the server sends a DRAW_SYNC: the
/// room's authoritative drawing state, replacing the webview's.
pub const STROKE_SYNC_EVENT: &str = "drawing://sync";

/// P5-T04: Tauri event name emitted when the server relays another
/// participant's LASER_MOVE.
pub const LASER_MOVE_EVENT: &str = "laser://move";

/// P5-T04: Tauri event name emitted when the server relays another
/// participant's LASER_OFF.
pub const LASER_OFF_EVENT: &str = "laser://off";

/// P6-T03: Tauri event name emitted when the server broadcasts a
/// CHAT_MESSAGE.
pub const CHAT_MESSAGE_EVENT: &str = "chat://message";

/// P4-T02: the IPC-safe playback event payload. Mirrors
/// `locast_protocol::room::PlaybackAcceptedEvent` with
/// the same field names; the wire field `action`
/// becomes `kind` here so the TypeScript event payload
/// has a stable name across the two protocol types
/// (PLAYBACK_CMD uses `action` because that matches the
/// PLAY/PAUSE/SEEK wire spec; this IPC event uses
/// `kind` because `playback://state.action` would be
/// ambiguous in the React layer).
///
/// `media_id` is intentionally absent: the v1 wire has
/// no per-asset `media_id` on the playback command (the
/// room has a single shared cursor per the §11 design).
/// When P5+ adds per-asset playback, the wire will gain
/// a `media_id` field and this struct will mirror it.
#[derive(Debug, Clone, Serialize, Type)]
pub struct PlaybackStateEvent {
    /// The room id, set as `Envelope::room_id` when the
    /// event is delivered. Subscribers MUST verify this
    /// matches the user's current room before applying.
    pub room_id: String,
    /// Server-assigned per-room monotonic sequence.
    /// Strictly increasing across all accepted
    /// PLAYBACK_CMD broadcasts for the lifetime of the
    /// room. Subscribers drop events with
    /// `server_seq <= last_applied_server_seq` for the
    /// same room.
    pub server_seq: u64,
    /// Server-stamped wall-clock at acceptance, unix ms.
    /// The client does NOT use this for ordering
    /// (`server_seq` is authoritative) but the React
    /// store surfaces it for UI display.
    pub server_ts_ms: i64,
    /// The original sender's `user_id` (UUID as string).
    /// After host migration the originator changes; the
    /// React store uses this only to decide whether
    /// the event is an echo of the local user's own
    /// command (and should be ignored).
    pub sender_id: String,
    /// Per-sender monotonic sequence, preserved from the
    /// original `PlaybackCommandPayload`. The client
    /// may use this for its own per-sender dedup; the
    /// server has already validated this number.
    pub monotonic_seq: u64,
    /// The action that was accepted. Wire: `play` |
    /// `pause` | `seek`.
    pub kind: String,
    /// Media position the command applies to. For
    /// `pause` this is the room's last PLAY/SEEK
    /// position (preserved by the server, not
    /// modified by PAUSE).
    pub media_position_ms: u64,
}

impl From<(Uuid, &locast_protocol::room::PlaybackAcceptedEvent)> for PlaybackStateEvent {
    fn from((room_id, evt): (Uuid, &locast_protocol::room::PlaybackAcceptedEvent)) -> Self {
        Self {
            room_id: room_id.to_string(),
            server_seq: evt.server_seq,
            server_ts_ms: evt.server_ts_ms,
            sender_id: evt.sender_id.to_string(),
            monotonic_seq: evt.monotonic_seq,
            // The protocol's `PlaybackAction` is
            // `#[serde(rename_all = "lowercase")]`, so
            // the wire string is already lowercase.
            // We re-derive it here to keep this struct
            // free of the protocol type in the wire
            // shape (and to survive a future rename
            // without breaking the IPC surface).
            kind: serde_json::to_value(evt.action)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "play".to_string()),
            media_position_ms: evt.media_position_ms,
        }
    }
}

/// P3-T03: the small, IPC-safe descriptor emitted with
/// the `manifest://state` event. `manifest_hash` is the
/// 64-char lowercase BLAKE3 of the canonical manifest
/// bytes. `version` is the server's per-room monotonic
/// counter (1 on the first publish).
#[derive(Debug, Clone, Serialize, Type)]
pub struct ManifestStateEvent {
    pub room_id: String,
    pub manifest_hash: String,
    pub version: i64,
}

/// P4-T03: the IPC-safe position report payload emitted
/// with the `position://report` event. Carries the
/// viewer's reported local state (verbatim from the wire
/// payload, per the roadmap's "server forwards without
/// modification" requirement) plus the originator's
/// `user_id` so the React layer can key positions by
/// sender and keep multiple viewers distinct.
///
/// The struct deliberately does NOT carry a server
/// `server_seq` / `server_ts_ms` -- the server does NOT
/// stamp these on the rebroadcast (architecture §12.8:
/// "Relays POSITION_REPORT messages without
/// modification"). The host's UI uses the report only
/// for display, not for ordering; the position's freshness
/// is inferred from `client_ts_ms` (the sender's local
/// wall clock at send time).
#[derive(Debug, Clone, Serialize, Type)]
pub struct PositionReportEvent {
    /// The room id, set as `Envelope::room_id` when the
    /// event is delivered. Subscribers MUST verify this
    /// matches the user's current room before applying.
    pub room_id: String,
    /// The originating participant's `user_id` (UUID as
    /// string). The React layer keys its position map by
    /// this so multiple viewers' positions remain
    /// distinguishable.
    pub sender_id: String,
    /// The viewer's local `<video>` position in integer
    /// milliseconds. Verbatim from the wire payload.
    pub media_position_ms: u64,
    /// `true` when the viewer's local `<video>.paused
    /// === false`. Verbatim from the wire payload.
    pub playing: bool,
    /// The sender's wall clock at send (unix ms).
    /// Verbatim from the wire payload (the server does
    /// NOT stamp `server_ts_ms`). The React layer uses
    /// this only for display (e.g. "last seen 3 s ago");
    /// ordering is by `sender_id` + arrival time on the
    /// client.
    pub client_ts_ms: i64,
}

impl From<(Uuid, Uuid, &locast_protocol::room::PositionReportPayload)> for PositionReportEvent {
    fn from(
        (room_id, sender_id, payload): (Uuid, Uuid, &locast_protocol::room::PositionReportPayload),
    ) -> Self {
        Self {
            room_id: room_id.to_string(),
            sender_id: sender_id.to_string(),
            media_position_ms: payload.media_position_ms,
            playing: payload.playing,
            client_ts_ms: payload.client_ts_ms,
        }
    }
}

/// P5-T03: IPC-safe stroke begin event payload.
/// Emitted as `drawing://begin` when a remote DRAW_BEGIN
/// is accepted and rebroadcast by the server. The sender_id
/// is the server-authoritative originator (from the bearer).
#[derive(Debug, Clone, Serialize, Type)]
pub struct StrokeBeginEvent {
    pub room_id: String,
    pub sender_id: String,
    pub stroke_id: String,
    pub tool: String,
    pub color: String,
    pub width: f32,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub ts_ms: i64,
    /// The room's drawing sequence number (`Envelope::seq`). The
    /// webview ignores a drawing event at or below the last one it
    /// applied (a duplicate, or already covered by a DRAW_SYNC).
    pub seq: u64,
}

impl From<(Uuid, Uuid, &locast_protocol::room::StrokeBeginPayload)> for StrokeBeginEvent {
    fn from(
        (room_id, sender_id, payload): (Uuid, Uuid, &locast_protocol::room::StrokeBeginPayload),
    ) -> Self {
        Self {
            room_id: room_id.to_string(),
            sender_id: sender_id.to_string(),
            stroke_id: payload.stroke_id.to_string(),
            tool: serde_json::to_value(payload.tool)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "pen".to_string()),
            color: payload.color.clone(),
            width: payload.width,
            x: payload.x,
            y: payload.y,
            pressure: payload.pressure,
            ts_ms: payload.ts_ms,
            seq: 0,
        }
    }
}

/// P5-T03: IPC-safe stroke point event payload.
/// Emitted as `drawing://point` when a remote DRAW_POINT
/// is accepted and rebroadcast by the server.
#[derive(Debug, Clone, Serialize, Type)]
pub struct StrokePointEvent {
    pub room_id: String,
    pub sender_id: String,
    pub stroke_id: String,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub ts_ms: i64,
    /// The room's drawing sequence number (`Envelope::seq`). The
    /// webview ignores a drawing event at or below the last one it
    /// applied (a duplicate, or already covered by a DRAW_SYNC).
    pub seq: u64,
}

impl From<(Uuid, Uuid, &locast_protocol::room::StrokePointPayload)> for StrokePointEvent {
    fn from(
        (room_id, sender_id, payload): (Uuid, Uuid, &locast_protocol::room::StrokePointPayload),
    ) -> Self {
        Self {
            room_id: room_id.to_string(),
            sender_id: sender_id.to_string(),
            stroke_id: payload.stroke_id.to_string(),
            x: payload.x,
            y: payload.y,
            pressure: payload.pressure,
            ts_ms: payload.ts_ms,
            seq: 0,
        }
    }
}

/// P5-T03: IPC-safe stroke end event payload.
/// Emitted as `drawing://end` when a remote DRAW_END
/// is accepted and rebroadcast by the server.
#[derive(Debug, Clone, Serialize, Type)]
pub struct StrokeEndEvent {
    pub room_id: String,
    pub sender_id: String,
    pub stroke_id: String,
    pub ts_ms: i64,
    /// The room's drawing sequence number (`Envelope::seq`). The
    /// webview ignores a drawing event at or below the last one it
    /// applied (a duplicate, or already covered by a DRAW_SYNC).
    pub seq: u64,
}

impl From<(Uuid, Uuid, &locast_protocol::room::StrokeEndPayload)> for StrokeEndEvent {
    fn from(
        (room_id, sender_id, payload): (Uuid, Uuid, &locast_protocol::room::StrokeEndPayload),
    ) -> Self {
        Self {
            room_id: room_id.to_string(),
            sender_id: sender_id.to_string(),
            stroke_id: payload.stroke_id.to_string(),
            ts_ms: payload.ts_ms,
            seq: 0,
        }
    }
}

/// P5-T03: IPC-safe undo event payload. Emitted as `drawing://undo`
/// when a DRAW_UNDO is accepted. `sender_id` is the server-stamped
/// actor (the connection that issued the undo), not the stroke owner.
#[derive(Debug, Clone, Serialize, Type)]
pub struct StrokeUndoEvent {
    pub room_id: String,
    pub sender_id: String,
    pub stroke_id: String,
    /// The room's drawing sequence number (`Envelope::seq`). The
    /// webview ignores a drawing event at or below the last one it
    /// applied (a duplicate, or already covered by a DRAW_SYNC).
    pub seq: u64,
}

impl From<(Uuid, Uuid, &locast_protocol::room::StrokeUndoPayload)> for StrokeUndoEvent {
    fn from(
        (room_id, sender_id, payload): (Uuid, Uuid, &locast_protocol::room::StrokeUndoPayload),
    ) -> Self {
        Self {
            room_id: room_id.to_string(),
            sender_id: sender_id.to_string(),
            stroke_id: payload.stroke_id.to_string(),
            seq: 0,
        }
    }
}

/// One point of a stroke in a [`StrokeSyncEvent`].
#[derive(Debug, Clone, Serialize, Type)]
pub struct StrokeSyncPoint {
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub ts_ms: i64,
}

/// A stroke's DRAW_BEGIN fields in a [`StrokeSyncEvent`].
#[derive(Debug, Clone, Serialize, Type)]
pub struct StrokeSyncBegin {
    pub tool: String,
    pub color: String,
    pub width: f32,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub ts_ms: i64,
}

/// One stroke in a [`StrokeSyncEvent`].
#[derive(Debug, Clone, Serialize, Type)]
pub struct StrokeSyncStrokeEvent {
    pub stroke_id: String,
    pub owner_id: String,
    /// `None` when the server no longer holds this stroke's content;
    /// the stroke is still on the canvas and a webview that has it
    /// keeps its own copy.
    pub begin: Option<StrokeSyncBegin>,
    pub points: Vec<StrokeSyncPoint>,
    /// Set once the stroke has ended; `None` while in progress.
    pub end_ts_ms: Option<i64>,
}

/// Emitted as `drawing://sync` for a DRAW_SYNC: the room's whole
/// drawing state as of drawing sequence `seq`, in drawing order.
#[derive(Debug, Clone, Serialize, Type)]
pub struct StrokeSyncEvent {
    pub room_id: String,
    pub seq: u64,
    pub strokes: Vec<StrokeSyncStrokeEvent>,
}

impl From<(Uuid, &locast_protocol::room::StrokeSyncPayload)> for StrokeSyncEvent {
    fn from((room_id, payload): (Uuid, &locast_protocol::room::StrokeSyncPayload)) -> Self {
        let strokes = payload
            .strokes
            .iter()
            .map(|st| StrokeSyncStrokeEvent {
                stroke_id: st.stroke_id.to_string(),
                owner_id: st.owner_id.to_string(),
                begin: st.begin.as_ref().map(|b| StrokeSyncBegin {
                    tool: serde_json::to_value(b.tool)
                        .ok()
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                        .unwrap_or_else(|| "pen".to_string()),
                    color: b.color.clone(),
                    width: b.width,
                    x: b.x,
                    y: b.y,
                    pressure: b.pressure,
                    ts_ms: b.ts_ms,
                }),
                points: st
                    .points
                    .iter()
                    .map(|p| StrokeSyncPoint {
                        x: p.x,
                        y: p.y,
                        pressure: p.pressure,
                        ts_ms: p.ts_ms,
                    })
                    .collect(),
                end_ts_ms: st.end_ts_ms,
            })
            .collect();
        Self {
            room_id: room_id.to_string(),
            seq: payload.seq,
            strokes,
        }
    }
}

/// P5-T03: IPC-safe clear event payload. Emitted as `drawing://clear`
/// when a DRAW_CLEAR is accepted. `sender_id` is the server-stamped
/// actor.
#[derive(Debug, Clone, Serialize, Type)]
pub struct StrokeClearEvent {
    pub room_id: String,
    pub sender_id: String,
    /// The room's drawing sequence number (`Envelope::seq`). The
    /// webview ignores a drawing event at or below the last one it
    /// applied (a duplicate, or already covered by a DRAW_SYNC).
    pub seq: u64,
}

impl From<(Uuid, Uuid)> for StrokeClearEvent {
    fn from((room_id, sender_id): (Uuid, Uuid)) -> Self {
        Self {
            room_id: room_id.to_string(),
            sender_id: sender_id.to_string(),
            seq: 0,
        }
    }
}

/// P5-T04: IPC-safe laser position event payload. Emitted as
/// `laser://move` when the server relays another participant's
/// LASER_MOVE. `sender_id` is the server-stamped sender (the
/// payload carries no identity). Unsequenced: each move supersedes
/// the previous one.
#[derive(Serialize, Type, Clone, Debug)]
pub struct LaserMoveEvent {
    pub room_id: String,
    pub sender_id: String,
    pub x: f32,
    pub y: f32,
}

/// P6-T03: IPC-safe chat message payload, emitted as `chat://message`.
/// `sender_id` is the server-stamped originator; `sender_name` is
/// resolved from the room roster (the wire payload carries no name).
#[derive(Serialize, Type, Clone, Debug)]
pub struct ChatMessageEvent {
    pub room_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub text: String,
    pub reply_to: Option<String>,
    pub ts_ms: i64,
}

/// P5-T04: IPC-safe laser release event payload. Emitted as
/// `laser://off` when the server relays another participant's
/// LASER_OFF; the webview fades that sender's trail out.
#[derive(Serialize, Type, Clone, Debug)]
pub struct LaserOffEvent {
    pub room_id: String,
    pub sender_id: String,
}

/// Default timeout for a single request-reply round trip.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How often the background presence loop sends a
/// `PRESENCE` envelope while the user is in a room. The
/// server uses this to refresh `last_seen` so the
/// stale-participant cleanup does not remove us.
const PRESENCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// Background task that sends a `PRESENCE` envelope
    /// every [`PRESENCE_INTERVAL`] while the user is in a
    /// room. Spawned on a successful `room_join`, aborted
    /// on `room_leave` and on inbound `RoomClosed` /
    /// `RoomError` envelopes. Aborting (vs awaiting) is
    /// sufficient because the next iteration would just
    /// re-send the same `PRESENCE` envelope.
    ///
    /// Held in a `std::sync::Mutex` (not `tokio::sync::Mutex`)
    /// so [`Drop`] can take it without blocking on a runtime
    /// thread; the lock is only held briefly (take-or-insert
    /// of the `Option<JoinHandle>`) and never across an
    /// `.await`.
    presence_task: StdMutex<Option<JoinHandle<()>>>,
    /// P3-T03: per-room verified-manifest cache. The
    /// download planner (P3-T04) reads from this. The
    /// cache is populated by the `MANIFEST_PUBLISHED`
    /// inbound handler after a successful
    /// `locast_manifest::verify_manifest`. The map is
    /// keyed by `room_id` (Uuid). The
    /// `verified_at_ms` is the server's `published_at_ms`
    /// from the broadcast envelope.
    verified_manifests: StdMutex<HashMap<Uuid, locast_manifest::MediaManifest>>,
    /// P3-T04 prerequisite 2: the trusted host public
    /// key, set by [`Self::set_expected_host_pubkey`] from
    /// the parsed invite URL. `None` until the invite
    /// is parsed; the `MANIFEST_PUBLISHED` handler
    /// refuses to accept any manifest while this is
    /// `None` (no trust anchor = no manifest accepted).
    /// This is a `StdMutex<Option<[u8;32]>>` because the
    /// inbound handler reads it from a sync context.
    expected_host_pubkey: StdMutex<Option<[u8; 32]>>,
    /// P3-T04 prerequisite 4: the local SQLite pool,
    /// used by the `MANIFEST_PUBLISHED` handler to
    /// persist verified manifests to the local
    /// `room_manifests` table. `None` in unit tests
    /// that do not set up storage.
    pool: StdMutex<Option<sqlx::SqlitePool>>,
    /// P3-T04: highest server-assigned `version` per room
    /// accepted into the in-memory cache. Used by the
    /// `MANIFEST_PUBLISHED` handler to reject stale
    /// (out-of-order / replayed) envelopes so a newer
    /// cached/persisted manifest cannot be downgraded by
    /// a previously-buffered older one.
    current_versions: StdMutex<HashMap<Uuid, i64>>,
    /// P6-T02: the local user's server-assigned `user_id`
    /// (Uuid v7). Set on successful `room_create` /
    /// `room_join` and cleared on `room_leave`. Used
    /// to identify whether an inbound `CAPABILITY_UPDATE`
    /// is for the local user.
    local_user_id: Mutex<Option<Uuid>>,
    /// P7-T01: outbound envelopes produced while the
    /// signaling WS was in Reconnecting / Handshaking /
    /// Connecting. The signaling client cannot accept
    /// sends during those phases (its `outbound_tx` is
    /// `None` until AUTH_OK), so the RoomClient buffers
    /// them here. After AUTH_OK the connection-loop
    /// callback drains the buffer by re-issuing each
    /// envelope through the live `signaling.send_envelope`
    /// path.
    pending_outbound: StdMutex<Vec<Envelope>>,
    /// P7-T01: the (code, display_name) of the most
    /// recent room the user was in. Used by
    /// `rejoin_active_room` to re-issue ROOM_JOIN_REQUEST
    /// after a WS reconnect.
    active_room_code: StdMutex<Option<(String, String)>>,
    /// The library item ids the host explicitly chose to share
    /// with `manifest_publish`. `None` means "all permanent
    /// items" (the pre-selection behaviour). Kept so the
    /// post-reconnect auto-republish re-shares the same media
    /// instead of silently widening the selection.
    host_media_selection: StdMutex<Option<Vec<String>>>,
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
            presence_task: StdMutex::new(None),
            verified_manifests: StdMutex::new(HashMap::new()),
            expected_host_pubkey: StdMutex::new(None),
            pool: StdMutex::new(None),
            current_versions: StdMutex::new(HashMap::new()),
            local_user_id: Mutex::new(None),
            pending_outbound: StdMutex::new(Vec::new()),
            active_room_code: StdMutex::new(None),
            host_media_selection: StdMutex::new(None),
        }
    }

    /// P3-T04 prerequisite 4: install the local SQLite
    /// pool so the inbound `MANIFEST_PUBLISHED` handler
    /// can persist verified manifests. Called by
    /// `lib.rs` after the storage is open.
    pub fn set_storage_pool(&self, pool: sqlx::SqlitePool) {
        *self.pool.lock().expect("pool lock") = Some(pool);
    }

    /// P3-T04 prerequisite 2: install the trusted host
    /// public key from the parsed invite URL. After this
    /// call, the `MANIFEST_PUBLISHED` handler will reject
    /// any manifest whose `host_signature.public_key`
    /// (decoded to raw 32 bytes) does NOT match this
    /// pubkey. A manifest passes the cryptographic
    /// signature check but FAILS the trust check is
    /// treated as a hard rejection: the manifest is
    /// dropped, no cache update, no `manifest://state`
    /// event, no local `room_manifests` row.
    ///
    /// Calling this more than once with a different
    /// pubkey is allowed (the new value replaces the
    /// old) but the room lifecycle (re-join) is the
    /// natural time to do it. The v1 trust model has
    /// no host rotation; a new host after migration is
    /// a new invite.
    pub fn set_expected_host_pubkey(&self, pubkey: [u8; 32]) {
        *self
            .expected_host_pubkey
            .lock()
            .expect("expected_host_pubkey lock") = Some(pubkey);
    }

    /// Drop the trust anchor. Called before a join that has no
    /// invite `h=` key so an anchor from a previous room can
    /// never vouch for this room's manifests; with no anchor
    /// every manifest is rejected with `NoTrustAnchor`.
    pub fn clear_expected_host_pubkey(&self) {
        *self
            .expected_host_pubkey
            .lock()
            .expect("expected_host_pubkey lock") = None;
    }

    /// Remember the media ids the host chose to share (see
    /// [`RoomClient::host_media_selection`]).
    pub fn set_host_media_selection(&self, selection: Option<Vec<String>>) {
        *self
            .host_media_selection
            .lock()
            .expect("host_media_selection lock") = selection;
    }

    /// The media ids the host last chose to share, if any.
    pub fn host_media_selection(&self) -> Option<Vec<String>> {
        self.host_media_selection
            .lock()
            .expect("host_media_selection lock")
            .clone()
    }

    /// The server-assigned version of the verified manifest
    /// cached for `room_id`, if any.
    pub fn verified_manifest_version(&self, room_id: Uuid) -> Option<i64> {
        self.current_versions
            .lock()
            .expect("current_versions lock")
            .get(&room_id)
            .copied()
    }

    /// Mirror a server room snapshot into the local `rooms` /
    /// `room_participants` / `user_identities` tables so the
    /// download path can resolve the room and its peers. No-op
    /// when no storage pool is installed (unit tests).
    async fn mirror_room_snapshot(&self, room: &RoomSummary, local_user_id: Option<Uuid>) {
        let pool = self.pool.lock().ok().and_then(|g| g.clone());
        if let Some(pool) = pool {
            crate::storage::room_snapshot::persist_room_snapshot_best_effort(
                &pool,
                room,
                local_user_id,
            )
            .await;
        }
    }

    /// Read the current trust anchor, if any.
    pub fn expected_host_pubkey(&self) -> Option<[u8; 32]> {
        *self
            .expected_host_pubkey
            .lock()
            .expect("expected_host_pubkey lock")
    }

    /// P3-T03: read the verified manifest for a given
    /// room, if one has been accepted. The download
    /// planner (P3-T04) is the primary consumer.
    pub fn verified_manifest(&self, room_id: Uuid) -> Option<locast_manifest::MediaManifest> {
        self.verified_manifests
            .lock()
            .expect("verified_manifests lock")
            .get(&room_id)
            .cloned()
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
        let summary = self.state.lock().await.clone();
        match summary {
            Some(s) => Some(self.with_you(&s).await),
            None => None,
        }
    }

    /// Copy of `summary` with `you_user_id` set from the
    /// current `local_user_id`.
    async fn with_you(&self, summary: &RoomSummaryIpc) -> RoomSummaryIpc {
        let mut out = summary.clone();
        out.you_user_id = self.local_user_id.lock().await.map(|u| u.to_string());
        out
    }

    /// Read the local user's server-assigned `user_id` (Uuid v7).
    /// Set on successful `room_create` / `room_join` and
    /// cleared on `room_leave`. Returns `None` if the user
    /// is not currently in a room.
    pub async fn local_user_id(&self) -> Option<Uuid> {
        *self.local_user_id.lock().await
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
        let mut summary = RoomSummaryIpc::from(created.room);
        summary.you_cap_set = Some(created.you.cap_set);
        *self.state.lock().await = Some(summary.clone());
        // The room create is a "to caller" event; we
        // emit a `room://state` so the React side
        // observing via the event stream sees the
        // new state immediately, without having to
        // re-poll via `room_get_state`.
        self.emit_state(&summary).await;
        // The host is a participant of the room they
        // just created; without a presence loop the
        // server's stale-participant cleanup would
        // reap the host within the stale window.
        self.spawn_presence_loop();
        Ok(summary)
    }

    /// Send a `ROOM_JOIN_REQUEST` envelope and return the
    /// server's `ROOM_JOINED` summary.
    pub async fn room_join(
        &self,
        code: String,
        display_name: String,
    ) -> Result<RoomSummaryIpc, RoomClientError> {
        let payload = RoomJoinRequestPayload {
            code: code.clone(),
            display_name: display_name.clone(),
        };
        let env = envelope(MessageKind::RoomJoinRequest, None, payload);
        let reply = self.request(env, MessageKind::RoomJoined).await?;
        let joined: locast_protocol::room::RoomJoinedPayload = decode_payload(&reply)?;
        let mut summary = RoomSummaryIpc::from(joined.room);
        summary.you_cap_set = Some(joined.you.cap_set);
        *self.state.lock().await = Some(summary.clone());
        self.emit_state(&summary).await;
        self.spawn_presence_loop();
        // P7-T01: remember the join credentials so a
        // post-AUTH_OK rejoin can re-issue ROOM_JOIN_REQUEST
        // if the WS reconnects.
        if let Ok(mut g) = self.active_room_code.lock() {
            *g = Some((code, display_name));
        } else {
            // Lock poisoned; fall through silently.
        }
        Ok(summary)
    }

    /// P7-T01: re-issue ROOM_JOIN_REQUEST for the room
    /// the user was in before the WS reconnect. Called
    /// from the signaling client's post-AUTH_OK hook.
    /// Returns `Ok(())` if the user is not in a room
    /// (no-op). Any error is propagated so the caller
    /// can log it.
    pub async fn rejoin_active_room(&self) -> Result<(), RoomClientError> {
        let creds = {
            let g = self.active_room_code.lock().expect("active_room_code lock");
            g.clone()
        };
        let (code, display_name) = match creds {
            Some(c) => c,
            None => return Ok(()),
        };
        let env = envelope(
            MessageKind::RoomJoinRequest,
            None,
            RoomJoinRequestPayload { code, display_name },
        );
        let reply = self.request(env, MessageKind::RoomJoined).await?;
        let joined: locast_protocol::room::RoomJoinedPayload = decode_payload(&reply)?;
        let mut summary = RoomSummaryIpc::from(joined.room);
        summary.you_cap_set = Some(joined.you.cap_set);
        *self.state.lock().await = Some(summary.clone());
        self.emit_state(&summary).await;
        // Drain any pending outbound envelopes that
        // accumulated during the WS outage.
        let pending: Vec<Envelope> = {
            let mut g = self.pending_outbound.lock().expect("pending_outbound lock");
            std::mem::take(&mut *g)
        };
        for env in pending {
            if let Err(e) = self.signaling.send_envelope(env).await {
                warn!(error = %e, "drain pending_outbound send failed");
            }
        }
        Ok(())
    }

    /// Send a `ROOM_LEAVE` envelope. The server does not
    /// send a direct reply in v1; the caller should rely on
    /// the inbound `ROOM_CLOSED` and `PARTICIPANT_LEFT`
    /// events to update the UI.
    pub async fn room_leave(&self) -> Result<(), RoomClientError> {
        let env = envelope(MessageKind::RoomLeave, None, RoomLeavePayload {});
        self.send_or_buffer(env).await?;
        // Drop the cached state; the server will send
        // ROOM_CLOSED and the inbound loop will clear
        // it.
        *self.state.lock().await = None;
        *self.local_user_id.lock().await = None;
        self.forget_room_session();
        self.abort_presence_loop().await;
        Ok(())
    }

    /// Drop what a post-reconnect rejoin or a republish would use: the
    /// join credentials, the host's media selection and any envelopes
    /// buffered while the WS was down. Called whenever the user is no
    /// longer in the room, whether they left or the server ended it, so
    /// a later WS reconnect does not re-join a dead room. The buffer is
    /// room-scoped (chat, drawing, and the `ROOM_LEAVE` that `room_leave`
    /// just queued); left in place it would be flushed into whichever
    /// room is joined next, where a stale `ROOM_LEAVE` removes the user
    /// or ends the room for a host.
    fn forget_room_session(&self) {
        if let Ok(mut g) = self.active_room_code.lock() {
            *g = None;
        }
        if let Ok(mut g) = self.pending_outbound.lock() {
            g.clear();
        }
        self.set_host_media_selection(None);
    }

    /// P7-T01: send an envelope, buffering it if the
    /// signaling WS is not currently in the
    /// `Authenticated` phase (Connecting / Handshaking /
    /// Reconnecting / ShuttingDown). The buffer is
    /// drained after a successful reconnect by
    /// `rejoin_active_room`.
    pub async fn send_or_buffer(&self, env: Envelope) -> Result<(), RoomClientError> {
        // If the signaling snapshot says
        // `connected == true`, the bearer is live and
        // we send through the live path. Otherwise the
        // WS is mid-reconnect; buffer the envelope so
        // it can be flushed after the next AUTH_OK.
        //
        // P7-T01 review-fix: only buffer when we
        // actually had a connection that just dropped.
        // A fresh client that has never authenticated
        // (e.g. unit tests, or a user that joined a
        // room before `start`) must still error out
        // fast via `send_envelope`, the same as the
        // pre-P7-T01 behavior. Buffering without a
        // prior connection would silently hold
        // envelopes indefinitely and turn a 0-cost
        // error into a 10s+ per-call timeout storm.
        let snapshot = self.signaling.snapshot().await;
        let has_bearer = self.signaling.bearer_for_test().await.is_some();
        if snapshot.connected && matches!(snapshot.phase, ConnPhase::Authenticated) && has_bearer {
            self.signaling
                .send_envelope(env)
                .await
                .map_err(|e| RoomClientError::Signaling(e.to_string()))?;
        } else if !has_bearer {
            self.signaling
                .send_envelope(env)
                .await
                .map_err(|e| RoomClientError::Signaling(e.to_string()))?;
        } else {
            let mut g = self.pending_outbound.lock().expect("pending_outbound lock");
            // P7-T01 review-fix: cap the buffer at
            // `PENDING_OUTBOUND_CAP` envelopes. The pending
            // queue is only ever populated while the
            // signaling WS is mid-reconnect; under normal
            // operation the queue stays near-empty and the
            // cap is never hit. An extended outage combined
            // with a UI emit-storm (chat, drawing, playback)
            // could otherwise grow the buffer without bound
            // and OOM the native side.
            while g.len() >= PENDING_OUTBOUND_CAP {
                let dropped = g.remove(0);
                warn!(
                    kind = ?dropped.r#type,
                    pending = g.len(),
                    cap = PENDING_OUTBOUND_CAP,
                    "pending_outbound cap exceeded; dropped oldest envelope",
                );
            }
            g.push(env);
        }
        Ok(())
    }

    /// P7-T01: drain any buffered outbound envelopes
    /// produced while the signaling WS was offline.
    /// Called by `rejoin_active_room` after the
    /// post-AUTH_OK ROOM_JOIN succeeds. Each buffered
    /// envelope is re-sent; failures are logged but do
    /// not abort the drain.
    pub async fn drain_pending_outbound(&self) {
        let pending: Vec<Envelope> = {
            let mut g = self.pending_outbound.lock().expect("pending_outbound lock");
            std::mem::take(&mut *g)
        };
        for env in pending {
            if let Err(e) = self.signaling.send_envelope(env).await {
                warn!(error = %e, "drain pending_outbound send failed");
            }
        }
    }

    /// P6-T02: send a `PERMISSION_SET` envelope to grant or
    /// revoke capabilities for a participant. This is a
    /// fire-and-forget from the client's perspective: the
    /// server validates the caller is the host, applies the
    /// mutation, and broadcasts a `CAPABILITY_UPDATE` to
    /// all participants. The client that sent `PERMISSION_SET`
    /// receives the broadcast via `handle_inbound` and updates
    /// its local state.
    pub async fn permission_set(
        &self,
        room_id: Uuid,
        target_user_id: Uuid,
        add_cap_set: u32,
        remove_cap_set: u32,
    ) -> Result<(), RoomClientError> {
        let payload = locast_protocol::room::PermissionSetPayload {
            target_user_id,
            add_cap_set,
            remove_cap_set,
        };
        let env = envelope(MessageKind::PermissionSet, Some(room_id), payload);
        self.signaling
            .send_envelope(env)
            .await
            .map_err(|e| RoomClientError::Signaling(e.to_string()))?;
        Ok(())
    }

    /// P6-T03: send a chat message. The server validates the
    /// caller's CHAT capability, broadcasts a `CHAT_MESSAGE`
    /// envelope to all participants, and sets `sent_ms` from
    /// the server clock.
    pub async fn chat_message(
        &self,
        room_id: Uuid,
        text: String,
        reply_to: Option<Uuid>,
    ) -> Result<(), RoomClientError> {
        let payload = locast_protocol::room::ChatPayload {
            sender_id: Uuid::nil(),
            text,
            reply_to,
            sent_ms: 0,
        };
        let env = envelope(MessageKind::ChatMessage, Some(room_id), payload);
        self.signaling
            .send_envelope(env)
            .await
            .map_err(|e| RoomClientError::Signaling(e.to_string()))?;
        Ok(())
    }

    /// P3-T04 prerequisite 3: ask the server for the
    /// room's currently-authoritative manifest. Used by
    /// late-joiners to catch up on a manifest that was
    /// published before the viewer joined.
    ///
    /// The response goes through the SAME verify + TOFU +
    /// stale-version guard + persist + emit pipeline as
    /// the `MANIFEST_PUBLISHED` broadcast path, so a late
    /// joiner is never exposed to a manifest whose host
    /// signature does not match the invite's `h=` anchor,
    /// and a hostile server cannot downgrade the cached
    /// version. On rejection, `Err` is returned and the
    /// caller surfaces a typed error.
    pub async fn manifest_fetch(
        &self,
        room_id: Uuid,
        media_id: Uuid,
    ) -> Result<locast_protocol::room::ManifestResponsePayload, RoomClientError> {
        let payload = locast_protocol::room::ManifestRequestPayload { media_id };
        let env = envelope(MessageKind::ManifestRequest, Some(room_id), payload);
        let reply = self.request(env, MessageKind::ManifestResponse).await?;
        let response: locast_protocol::room::ManifestResponsePayload = decode_payload(&reply)?;
        self.accept_manifest(
            response.manifest.clone(),
            response.version,
            response.published_at_ms,
            "MANIFEST_RESPONSE",
        )
        .await?;
        Ok(response)
    }

    /// P3-T04 (P3-T03 prerequisite): the verified-manifest
    /// acceptance pipeline. Called from BOTH the
    /// `MANIFEST_PUBLISHED` broadcast handler and the
    /// `MANIFEST_RESPONSE` late-join fetch path so the
    /// trust boundary is identical on every entry point.
    ///
    /// Steps (return `Err` on any failure; the caller
    /// logs at WARN with the supplied `source`):
    /// 1. `verify_manifest` (cryptographic signature).
    /// 2. Parse `manifest.room_id` -> Uuid.
    /// 3. Decode `host_signature.public_key` to raw 32
    ///    bytes; compare against the installed
    ///    `expected_host_pubkey` (TOFU trust anchor).
    /// 4. Compute the BLAKE3 of canonical bytes for the
    ///    small event payload.
    /// 5. Reject stale manifests (`incoming_version <
    ///    cached_version`).
    /// 6. Insert into the in-memory cache.
    /// 7. Persist to `room_manifests` (best-effort; does
    ///    not roll back the cache on failure).
    /// 8. Emit `manifest://state`.
    ///
    /// Returns the room UUID on success so the broadcast
    /// path can use it without re-parsing.
    pub async fn accept_manifest(
        &self,
        manifest: locast_manifest::MediaManifest,
        incoming_version: i64,
        published_at_ms: i64,
        source: &'static str,
    ) -> Result<Uuid, ManifestAcceptError> {
        // Step 1: cryptographic signature.
        if let Err(e) = locast_manifest::verify_manifest(&manifest) {
            return Err(ManifestAcceptError::BadSignature(e.to_string()));
        }
        // Step 2: parse room_id.
        let room_uuid = Uuid::parse_str(&manifest.room_id)
            .map_err(|e| ManifestAcceptError::BadRoomId(e.to_string()))?;
        // Step 3: TOFU against the invite h= anchor.
        let manifest_pubkey_bytes = match manifest.host_signature.as_ref() {
            Some(hs) => match locast_crypto::ed25519::from_base64(&hs.public_key) {
                Ok(b) if b.len() == 32 => {
                    let mut out = [0u8; 32];
                    out.copy_from_slice(&b);
                    out
                }
                Ok(b) => {
                    return Err(ManifestAcceptError::BadPubkeyLength(b.len()));
                }
                Err(_) => {
                    return Err(ManifestAcceptError::BadPubkeyFormat);
                }
            },
            None => {
                return Err(ManifestAcceptError::NoHostSignature);
            }
        };
        let expected = {
            let g = self
                .expected_host_pubkey
                .lock()
                .expect("expected_host_pubkey lock");
            *g
        };
        let expected = expected.ok_or(ManifestAcceptError::NoTrustAnchor)?;
        if manifest_pubkey_bytes != expected {
            return Err(ManifestAcceptError::TrustAnchorMismatch);
        }
        // Step 4: BLAKE3 of canonical bytes.
        let manifest_hash = locast_manifest::serialize(&manifest)
            .map(|bytes| locast_crypto::blake3::blake3_hex(&bytes))
            .map_err(|e| ManifestAcceptError::Canonicalize(e.to_string()))?;
        // Step 5: stale-version guard.
        {
            let mut versions = self.current_versions.lock().expect("current_versions lock");
            if let Some(prev) = versions.get(&room_uuid).copied() {
                if incoming_version < prev {
                    return Err(ManifestAcceptError::StaleVersion {
                        incoming: incoming_version,
                        cached: prev,
                    });
                }
            }
            versions.insert(room_uuid, incoming_version);
        }
        // Step 6: in-memory cache.
        {
            let mut cache = self
                .verified_manifests
                .lock()
                .expect("verified_manifests lock");
            cache.insert(room_uuid, manifest.clone());
        }
        // Step 7: persist.
        let _ = source;
        {
            let pool_opt = self.pool.lock().expect("pool lock").clone();
            if let Some(pool) = pool_opt {
                let store = crate::storage::manifests::ManifestStore::new(&pool);
                let row_id = Uuid::now_v7();
                if let Err(e) = store
                    .upsert(
                        row_id,
                        room_uuid,
                        published_at_ms,
                        &manifest,
                        incoming_version,
                    )
                    .await
                {
                    warn!(
                        error = %e,
                        source = source,
                        "manifest accept: ManifestStore::upsert failed; in-memory cache is authoritative for this session"
                    );
                }
            }
        }
        // Step 8: emit.
        let ev = ManifestStateEvent {
            room_id: room_uuid.to_string(),
            manifest_hash,
            version: incoming_version,
        };
        self.emit_manifest_state(&ev).await;
        Ok(room_uuid)
    }

    async fn handle_manifest_published(&self, env: &Envelope) {
        let payload: locast_protocol::room::ManifestPublishedPayload = match decode_payload(env) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "ignoring MANIFEST_PUBLISHED: bad payload");
                return;
            }
        };
        let manifest = payload.manifest;
        match self
            .accept_manifest(
                manifest,
                payload.version,
                payload.published_at_ms,
                "MANIFEST_PUBLISHED",
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                warn!(source = "MANIFEST_PUBLISHED", error = %e, "ignoring MANIFEST_PUBLISHED");
            }
        }
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

    /// P4-T06: NTP-style clock skew measurement. Captures
    /// the local wall clock at send time, sends a
    /// `SKEW_PROBE` envelope, awaits the `SKEW_REPLY`,
    /// and returns the four-timestamp sample for the
    /// `room::skew::compute_skew_jitter` reducer. The
    /// actual NTP math (median / stddev / RTT filter)
    /// lives in `apps/client/src-tauri/src/room/skew.rs`
    /// and is exercised in isolation there; this method
    /// is a thin transport.
    pub async fn clock_skew_probe(
        &self,
    ) -> Result<locast_protocol::room::SkewSample, RoomClientError> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let t0_local_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let env = envelope(
            MessageKind::SkewProbe,
            None,
            locast_protocol::room::SkewProbePayload {
                client_send_ms: t0_local_ms,
            },
        );
        let reply = self.request(env, MessageKind::SkewReply).await?;
        let payload: locast_protocol::room::SkewReplyPayload = decode_payload(&reply)?;
        let t3_local_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Ok(locast_protocol::room::SkewSample {
            t0_local_ms,
            t3_local_ms,
            server_ts_ms: payload.server_ts_ms,
            client_send_ms_echo: payload.client_send_ms,
        })
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

    /// P5-T04: the room and server-stamped sender of a relayed
    /// LASER_MOVE / LASER_OFF, or `None` when the frame must be
    /// dropped: it names no room or a room other than the one this
    /// client is in, it has no usable server-stamped sender (see
    /// [`stroke_sender`]; the payload is never consulted), or the
    /// sender is the local user (the server never echoes a laser
    /// back to its sender; this is a defensive second check).
    async fn laser_origin(&self, env: &Envelope) -> Option<(Uuid, Uuid)> {
        let room_id = env.room_id?;
        let current_room = self.state.lock().await.as_ref().map(|s| s.id.clone());
        if current_room.as_deref() != Some(room_id.to_string().as_str()) {
            return None;
        }
        let sender_id = stroke_sender(env)?;
        if *self.local_user_id.lock().await == Some(sender_id) {
            debug!("dropping an echo of the local user's own laser");
            return None;
        }
        Some((room_id, sender_id))
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
        let resolved = self.deliver_to_pending(&env).await;
        // 2) Update the cached state and emit Tauri
        //    events for the types that callers care
        //    about.
        match env.r#type {
            MessageKind::RoomState => {
                if let Ok(state) = decode_payload::<RoomStatePayload>(&env) {
                    let local = *self.local_user_id.lock().await;
                    self.mirror_room_snapshot(&state.room, local).await;
                    let mut summary = RoomSummaryIpc::from(state.room);
                    // ROOM_STATE carries no `you` record: keep the
                    // cap set the last create / join / update set.
                    let mut g = self.state.lock().await;
                    summary.you_cap_set = g.as_ref().and_then(|s| s.you_cap_set);
                    *g = Some(summary.clone());
                    drop(g);
                    self.emit_state(&summary).await;
                }
            }
            MessageKind::RoomJoined => {
                if let Ok(p) = decode_payload::<locast_protocol::room::RoomJoinedPayload>(&env) {
                    self.mirror_room_snapshot(&p.room, Some(p.you.user_id))
                        .await;
                    let mut summary = RoomSummaryIpc::from(p.room);
                    summary.you_cap_set = Some(p.you.cap_set);
                    *self.state.lock().await = Some(summary.clone());
                    *self.local_user_id.lock().await = Some(p.you.user_id);
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
                    self.mirror_room_snapshot(&p.room, Some(p.you.user_id))
                        .await;
                    let mut summary = RoomSummaryIpc::from(p.room);
                    summary.you_cap_set = Some(p.you.cap_set);
                    *self.state.lock().await = Some(summary.clone());
                    *self.local_user_id.lock().await = Some(p.you.user_id);
                    self.emit_state(&summary).await;
                    self.emit_event(&summary).await;
                }
            }
            MessageKind::HostMigrated => {
                if let Ok(mut m) = decode_payload::<HostMigratedPayload>(&env) {
                    // P2-T05: if the server included a
                    // post-migration summary, REPLACE the
                    // cached state entirely. Otherwise
                    // fall back to the pre-P2-T05
                    // behavior of updating only the host
                    // fields.
                    //
                    // HOST_MIGRATED carries no `you` record and no
                    // CAPABILITY_UPDATE follows it, so the local
                    // cap set is carried over from the cached
                    // summary (a DRAW / LASER grant must not vanish
                    // from the UI) and only rewritten when the
                    // migration changed the local user's role, the
                    // same way the server does (`cap::HOST` for the
                    // promoted participant, `cap::CHAT` for the
                    // demoted host).
                    let local = *self.local_user_id.lock().await;
                    if let Some(boxed) = m.summary.take() {
                        self.mirror_room_snapshot(&boxed, local).await;
                        let mut summary = RoomSummaryIpc::from(*boxed);
                        let mut g = self.state.lock().await;
                        let previous = g.as_ref().and_then(|s| s.you_cap_set);
                        summary.you_cap_set = migrated_you_cap_set(previous, local, &m);
                        *g = Some(summary.clone());
                        drop(g);
                        self.emit_state(&summary).await;
                        self.emit_event(&summary).await;
                    } else {
                        let mut g = self.state.lock().await;
                        if let Some(s) = g.as_mut() {
                            s.host_user_id = m.new_host_user_id.to_string();
                            s.host_disconnected = false;
                            s.host_disconnect_deadline_ms = None;
                            s.you_cap_set = migrated_you_cap_set(s.you_cap_set, local, &m);
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
            MessageKind::HostDisconnected => {
                if let Ok(m) =
                    decode_payload::<locast_protocol::room::HostDisconnectedPayload>(&env)
                {
                    let mut g = self.state.lock().await;
                    if let Some(s) = g.as_mut() {
                        s.host_disconnected = true;
                        s.host_disconnect_deadline_ms = Some(m.reconnect_deadline_ms);
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
            // A ROOM_ERROR answering a manifest fetch (e.g. the
            // late-join fetch before the host has published
            // anything) fails that request only; it does not
            // end the room.
            MessageKind::RoomError if resolved == Some(MessageKind::ManifestResponse) => {
                tracing::debug!("ROOM_ERROR answered a manifest fetch; room state kept");
            }
            // An unsolicited ROOM_ERROR that only rejects one
            // fire-and-forget command (PLAYBACK_CMD, drawing,
            // laser, PERMISSION_SET) does not end the room;
            // only a code saying the membership is gone does.
            MessageKind::RoomError if resolved.is_none() && !room_error_ends_membership(&env) => {
                tracing::warn!("ROOM_ERROR rejected a command; room state kept");
            }
            MessageKind::RoomClosed | MessageKind::RoomError => {
                *self.state.lock().await = None;
                *self.local_user_id.lock().await = None;
                self.forget_room_session();
                self.emit_state_cleared().await;
                self.abort_presence_loop().await;
            }
            MessageKind::ManifestPublished => {
                self.handle_manifest_published(&env).await;
            }
            // P4-T02: a server-accepted playback event
            // arrived. Decode the payload, scope-check
            // against the user's current room, and
            // forward the authoritative state to the
            // React layer via `playback://state`. The
            // React store applies `server_seq` ordering
            // + media-readiness buffering; the Rust
            // side does NOT maintain playback state
            // (the server is the only authority) and
            // does NOT cache a per-room `last_seq` (the
            // store + `<video>` element own that).
            //
            // If `env.room_id` is None or does not
            // match the user's current room, the event
            // is dropped (the server should not be
            // sending us a different room's playback,
            // but we defend in depth).
            MessageKind::PlaybackCmd => {
                if let Some(room_id) = env.room_id {
                    let current_room = self.state.lock().await.as_ref().map(|s| s.id.clone());
                    let room_id_str = room_id.to_string();
                    // Only forward if the event belongs
                    // to the user's current room. If the
                    // cache is empty (room just closed /
                    // we just left), the event is from
                    // a stale subscription; drop it.
                    if current_room.as_deref() == Some(room_id_str.as_str()) {
                        if let Ok(evt) =
                            decode_payload::<locast_protocol::room::PlaybackAcceptedEvent>(&env)
                        {
                            let ipc = PlaybackStateEvent::from((room_id, &evt));
                            let g = self.sink.lock().await;
                            if let Some(s) = g.as_ref() {
                                s.emit_playback_state(&ipc);
                            }
                        }
                    }
                }
            }
            // P4-T03: a non-authoritative POSITION_REPORT
            // arrived. The server forwarded the payload
            // verbatim (per the roadmap's "forwards without
            // modification" requirement); the wire's
            // `user_id` is set by the server from the
            // validated bearer. Decode, scope-check against
            // the user's current room, and emit
            // `position://report` so the React layer can
            // update its per-viewer position map.
            //
            // This handler does NOT mutate any local state
            // -- POSITION_REPORT is telemetry, not a
            // command. The local `<video>` element is NOT
            // seeked / paused / played from a position
            // report (architecture §12.3: "Affects local
            // playback: No").
            //
            // The originator filter on the WS layer
            // suppresses the sender's own report so the
            // sender does not see its own report echoed
            // back at itself; receivers never have to
            // filter on their side.
            MessageKind::PositionReport => {
                if let Some(room_id) = env.room_id {
                    let current_room = self.state.lock().await.as_ref().map(|s| s.id.clone());
                    let room_id_str = room_id.to_string();
                    if current_room.as_deref() == Some(room_id_str.as_str()) {
                        if let Ok(payload) =
                            decode_payload::<locast_protocol::room::PositionReportPayload>(&env)
                        {
                            let ipc =
                                PositionReportEvent::from((room_id, payload.user_id, &payload));
                            let g = self.sink.lock().await;
                            if let Some(s) = g.as_ref() {
                                s.emit_position_report(&ipc);
                            }
                        }
                    }
                }
            }
            // P5-T03: a remote DRAW_BEGIN was accepted and
            // rebroadcast by the server. The sender_id is
            // the server-authoritative originator (from the
            // validated bearer). Emit `drawing://begin` so
            // the React layer can create a remote stroke.
            MessageKind::StrokeBegin => {
                if let Some(room_id) = env.room_id {
                    let current_room = self.state.lock().await.as_ref().map(|s| s.id.clone());
                    let room_id_str = room_id.to_string();
                    if current_room.as_deref() == Some(room_id_str.as_str()) {
                        if let Ok(payload) =
                            decode_payload::<locast_protocol::room::StrokeBeginPayload>(&env)
                        {
                            if let Some(sender_id) = stroke_sender(&env) {
                                let mut ipc =
                                    StrokeBeginEvent::from((room_id, sender_id, &payload));
                                ipc.seq = env.seq;
                                let g = self.sink.lock().await;
                                if let Some(s) = g.as_ref() {
                                    s.emit_stroke_begin(&ipc);
                                }
                            }
                        }
                    }
                }
            }
            // P5-T03: a remote DRAW_POINT was accepted and
            // rebroadcast by the server. The sender_id comes
            // from the server-stamped envelope sender (not the
            // payload). Emit `drawing://point` so the React
            // layer can append to the remote stroke.
            MessageKind::StrokePoint => {
                if let Some(room_id) = env.room_id {
                    let current_room = self.state.lock().await.as_ref().map(|s| s.id.clone());
                    let room_id_str = room_id.to_string();
                    if current_room.as_deref() == Some(room_id_str.as_str()) {
                        if let Ok(payload) =
                            decode_payload::<locast_protocol::room::StrokePointPayload>(&env)
                        {
                            if let Some(sender_id) = stroke_sender(&env) {
                                let mut ipc =
                                    StrokePointEvent::from((room_id, sender_id, &payload));
                                ipc.seq = env.seq;
                                let g = self.sink.lock().await;
                                if let Some(s) = g.as_ref() {
                                    s.emit_stroke_point(&ipc);
                                }
                            }
                        }
                    }
                }
            }
            // P5-T03: a remote DRAW_END was accepted and
            // rebroadcast by the server. The sender_id comes
            // from the server-stamped envelope sender (not the
            // payload). Emit `drawing://end` so the React
            // layer can finalize the remote stroke.
            MessageKind::StrokeEnd => {
                if let Some(room_id) = env.room_id {
                    let current_room = self.state.lock().await.as_ref().map(|s| s.id.clone());
                    let room_id_str = room_id.to_string();
                    if current_room.as_deref() == Some(room_id_str.as_str()) {
                        if let Ok(payload) =
                            decode_payload::<locast_protocol::room::StrokeEndPayload>(&env)
                        {
                            if let Some(sender_id) = stroke_sender(&env) {
                                let mut ipc = StrokeEndEvent::from((room_id, sender_id, &payload));
                                ipc.seq = env.seq;
                                let g = self.sink.lock().await;
                                if let Some(s) = g.as_ref() {
                                    s.emit_stroke_end(&ipc);
                                }
                            }
                        }
                    }
                }
            }
            // P5-T03: a DRAW_UNDO was accepted by the server. It is
            // delivered to every participant, the local user
            // included: the stroke leaves the canvas only on this
            // event. `sender_id` is the server-stamped actor.
            MessageKind::StrokeUndo => {
                if let Some(room_id) = env.room_id {
                    let current_room = self.state.lock().await.as_ref().map(|s| s.id.clone());
                    if current_room.as_deref() == Some(room_id.to_string().as_str()) {
                        if let Ok(payload) =
                            decode_payload::<locast_protocol::room::StrokeUndoPayload>(&env)
                        {
                            if let Some(sender_id) = stroke_sender(&env) {
                                let mut ipc = StrokeUndoEvent::from((room_id, sender_id, &payload));
                                ipc.seq = env.seq;
                                let g = self.sink.lock().await;
                                if let Some(s) = g.as_ref() {
                                    s.emit_stroke_undo(&ipc);
                                }
                            }
                        }
                    }
                }
            }
            // P5-T03: a DRAW_CLEAR was accepted (actor included).
            MessageKind::StrokeClear => {
                if let Some(room_id) = env.room_id {
                    let current_room = self.state.lock().await.as_ref().map(|s| s.id.clone());
                    if current_room.as_deref() == Some(room_id.to_string().as_str()) {
                        if let Some(sender_id) = stroke_sender(&env) {
                            let mut ipc = StrokeClearEvent::from((room_id, sender_id));
                            ipc.seq = env.seq;
                            let g = self.sink.lock().await;
                            if let Some(s) = g.as_ref() {
                                s.emit_stroke_clear(&ipc);
                            }
                        }
                    }
                }
            }
            // The server's drawing snapshot after this client's room
            // subscription dropped events: hand it to the webview,
            // which replaces its drawing state with it.
            MessageKind::StrokeSync => {
                if let Some(room_id) = env.room_id {
                    let current_room = self.state.lock().await.as_ref().map(|s| s.id.clone());
                    if current_room.as_deref() == Some(room_id.to_string().as_str()) {
                        if let Ok(payload) =
                            decode_payload::<locast_protocol::room::StrokeSyncPayload>(&env)
                        {
                            let ipc = StrokeSyncEvent::from((room_id, &payload));
                            let g = self.sink.lock().await;
                            if let Some(s) = g.as_ref() {
                                s.emit_stroke_sync(&ipc);
                            }
                        }
                    }
                }
            }
            // P5-T04: another participant's laser moved. The server
            // relays it to everyone but the sender, stamped with the
            // authenticated sender; the payload has no identity.
            // P6-T03: a chat message the server broadcast. The
            // sender's own message arrives here too: it is how the
            // sender's panel shows it.
            MessageKind::ChatMessage => {
                let current = self.state.lock().await.clone();
                if let (Some(room_id), Some(summary)) = (env.room_id, current) {
                    if summary.id == room_id.to_string() {
                        if let Ok(p) = decode_payload::<locast_protocol::room::ChatPayload>(&env) {
                            let sender_id = p.sender_id.to_string();
                            let sender_name = summary
                                .participants
                                .iter()
                                .find(|u| u.user_id == sender_id)
                                .map(|u| u.display_name.clone())
                                .unwrap_or_else(|| sender_id.chars().take(8).collect());
                            let ipc = ChatMessageEvent {
                                room_id: room_id.to_string(),
                                sender_id,
                                sender_name,
                                text: p.text,
                                reply_to: p.reply_to.map(|u| u.to_string()),
                                ts_ms: p.sent_ms,
                            };
                            let g = self.sink.lock().await;
                            if let Some(s) = g.as_ref() {
                                s.emit_chat_message(&ipc);
                            }
                        }
                    }
                }
            }
            MessageKind::LaserMove => {
                if let Some((room_id, sender_id)) = self.laser_origin(&env).await {
                    if let Ok(payload) =
                        decode_payload::<locast_protocol::room::LaserMovePayload>(&env)
                    {
                        if laser_unit_ok(payload.x) && laser_unit_ok(payload.y) {
                            let ipc = LaserMoveEvent {
                                room_id: room_id.to_string(),
                                sender_id: sender_id.to_string(),
                                x: payload.x,
                                y: payload.y,
                            };
                            let g = self.sink.lock().await;
                            if let Some(s) = g.as_ref() {
                                s.emit_laser_move(&ipc);
                            }
                        } else {
                            debug!("dropping LASER_MOVE with out-of-range coordinates");
                        }
                    }
                }
            }
            // P5-T04: another participant released its laser.
            MessageKind::LaserOff => {
                if let Some((room_id, sender_id)) = self.laser_origin(&env).await {
                    if decode_payload::<locast_protocol::room::LaserOffPayload>(&env).is_ok() {
                        let ipc = LaserOffEvent {
                            room_id: room_id.to_string(),
                            sender_id: sender_id.to_string(),
                        };
                        let g = self.sink.lock().await;
                        if let Some(s) = g.as_ref() {
                            s.emit_laser_off(&ipc);
                        }
                    }
                }
            }
            // P6-T02: a participant's cap_set was updated
            // by the host. If the target is the local user,
            // update `you_cap_set`. Otherwise update the
            // participant's entry in the participants list.
            MessageKind::CapabilityUpdate => {
                if let Ok(payload) =
                    decode_payload::<locast_protocol::room::CapabilityUpdatePayload>(&env)
                {
                    let mut g = self.state.lock().await;
                    // Copy, do not hold: `emit_state` re-locks
                    // `local_user_id` to fill `you_user_id`.
                    let local_user_id = *self.local_user_id.lock().await;
                    if let Some(s) = g.as_mut() {
                        if Some(payload.target_user_id) == local_user_id {
                            s.you_cap_set = Some(payload.cap_set);
                        } else if let Some(participant) = s
                            .participants
                            .iter_mut()
                            .find(|p| p.user_id == payload.target_user_id.to_string())
                        {
                            participant.cap_set = payload.cap_set;
                        }
                    }
                    if let Some(s) = g.as_ref() {
                        self.emit_state(s).await;
                        self.emit_event(s).await;
                    }
                }
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
    ///
    /// Returns the reply kind of the request the envelope
    /// resolved, if any.
    async fn deliver_to_pending(&self, env: &Envelope) -> Option<MessageKind> {
        // RoomError resolves any pending request that has
        // not been satisfied yet. Pop the first sender
        // for the envelope's own kind first, then fall
        // back to the first sender across all kinds.
        let mut pending = self.pending.lock().await;
        if let Some(senders) = pending.get_mut(&env.r#type) {
            if !senders.is_empty() {
                let tx = senders.remove(0);
                let _ = tx.send(env.clone());
                return Some(env.r#type.clone());
            }
        }
        if env.r#type == MessageKind::RoomError {
            // Route the error to the first pending
            // request of any kind.
            for (kind, senders) in pending.iter_mut() {
                if !senders.is_empty() {
                    let tx = senders.remove(0);
                    let _ = tx.send(env.clone());
                    return Some(kind.clone());
                }
            }
        }
        None
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
        // P7-T01: route through the buffering
        // wrapper so a request made while the WS is
        // reconnecting is held (and re-sent after the
        // next AUTH_OK) instead of failing fast.
        if let Err(e) = self.send_or_buffer(env).await {
            // Roll back the registration so a future
            // request doesn't pick up our sender.
            let mut pending = self.pending.lock().await;
            if let Some(senders) = pending.get_mut(&expected) {
                if !senders.is_empty() {
                    senders.remove(0);
                }
            }
            return Err(e);
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
        let summary = self.with_you(summary).await;
        let g = self.sink.lock().await;
        if let Some(s) = g.as_ref() {
            s.emit_state(&summary);
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
        let summary = self.with_you(summary).await;
        let g = self.sink.lock().await;
        if let Some(s) = g.as_ref() {
            s.emit_event(&summary);
        }
    }

    /// P3-T03: best-effort emit of the `manifest://state`
    /// event. The payload is the small `ManifestStateEvent`
    /// descriptor; the full verified manifest stays in
    /// the Rust cache.
    async fn emit_manifest_state(&self, ev: &ManifestStateEvent) {
        let g = self.sink.lock().await;
        if let Some(s) = g.as_ref() {
            s.emit_manifest_state(ev);
        }
    }

    /// Spawn the background presence loop. Aborts any
    /// previously running loop first so a re-join
    /// (after a leave) does not leak a stale task.
    fn spawn_presence_loop(&self) {
        let signaling = Arc::clone(&self.signaling);
        let mut g = self.presence_task.lock().expect("presence_task lock");
        if let Some(prev) = g.take() {
            prev.abort();
        }
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(PRESENCE_INTERVAL).await;
                if let Err(e) = signaling
                    .send_envelope(envelope(
                        MessageKind::Presence,
                        None,
                        PresencePayload {
                            status: "alive".into(),
                        },
                    ))
                    .await
                {
                    warn!(error = %e, "presence send failed; ending loop");
                    return;
                }
            }
        });
        *g = Some(handle);
    }

    /// Abort the background presence loop if one is
    /// running. Idempotent: a no-op when no loop is
    /// active.
    async fn abort_presence_loop(&self) {
        let mut g = self.presence_task.lock().expect("presence_task lock");
        if let Some(handle) = g.take() {
            handle.abort();
        }
    }

    /// Test-only: report whether a background presence
    /// loop is currently scheduled. Used by the
    /// `presence_loop_propagates_participant_joins_and_leaves`
    /// integration test to confirm the loop is
    /// actually spawned on join/create and aborted on
    /// leave/closed.
    #[doc(hidden)]
    pub fn presence_task_active(&self) -> bool {
        self.presence_task
            .lock()
            .expect("presence_task lock")
            .is_some()
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

/// The owner of a rebroadcast DRAW_BEGIN / DRAW_POINT / DRAW_END, or
/// the actor of a DRAW_UNDO / DRAW_CLEAR: the server-assigned user id
/// the server stamps on `Envelope::sender`. The drawing payloads do not
/// carry it. A frame without a usable id (no sender, or the nil UUID)
/// is dropped rather than attributed to the nil user, which would
/// break the per-owner checks that undo and clear rely on.
fn stroke_sender(env: &Envelope) -> Option<Uuid> {
    match env.sender.as_ref().map(|s| s.user_id) {
        Some(id) if !id.is_nil() => Some(id),
        _ => {
            debug!(kind = ?env.r#type, "dropping drawing frame without a sender id");
            None
        }
    }
}

/// P5-T04: a relayed laser coordinate must be finite and within
/// `[0, 1]`. The server checks this too; the client re-checks
/// rather than trusting the relay.
fn laser_unit_ok(n: f32) -> bool {
    n.is_finite() && (0.0..=1.0).contains(&n)
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
        let mut g = self.presence_task.lock().expect("presence_task lock");
        if let Some(handle) = g.take() {
            handle.abort();
        }
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

    #[test]
    fn stroke_sender_requires_a_non_nil_server_stamped_owner() {
        let owner = Uuid::now_v7();
        let mut env = env_of(MessageKind::StrokePoint, serde_json::json!({}));
        assert_eq!(stroke_sender(&env), None, "no sender: dropped");
        env.sender = Some(locast_protocol::envelope::Sender {
            user_id: Uuid::nil(),
            pubkey: Vec::new(),
            sig: Vec::new(),
        });
        assert_eq!(stroke_sender(&env), None, "nil sender: dropped");
        env.sender = Some(locast_protocol::envelope::Sender {
            user_id: owner,
            pubkey: Vec::new(),
            sig: Vec::new(),
        });
        assert_eq!(stroke_sender(&env), Some(owner));
    }

    /// P5-T04: records the laser events `handle_inbound` emits.
    #[derive(Default)]
    struct LaserSink {
        moves: std::sync::Mutex<Vec<LaserMoveEvent>>,
        offs: std::sync::Mutex<Vec<LaserOffEvent>>,
    }
    impl RoomEventSink for LaserSink {
        fn emit_state(&self, _summary: &RoomSummaryIpc) {}
        fn emit_event(&self, _summary: &RoomSummaryIpc) {}
        fn emit_state_cleared(&self) {}
        fn emit_laser_move(&self, ev: &LaserMoveEvent) {
            self.moves.lock().unwrap().push(ev.clone());
        }
        fn emit_laser_off(&self, ev: &LaserOffEvent) {
            self.offs.lock().unwrap().push(ev.clone());
        }
    }

    /// A room client in a room (returned id) as user `me`, with a
    /// [`LaserSink`] installed.
    async fn laser_client(me: Uuid) -> (RoomClient, Uuid, Arc<LaserSink>) {
        let rc = fresh_room_client().await;
        let summary = RoomSummaryIpc::from(sample_summary(Uuid::now_v7()));
        let room_id = Uuid::parse_str(&summary.id).unwrap();
        *rc.state.lock().await = Some(summary);
        *rc.local_user_id.lock().await = Some(me);
        let sink = Arc::new(LaserSink::default());
        rc.install_event_sink(sink.clone()).await;
        (rc, room_id, sink)
    }

    fn laser_env(
        kind: MessageKind,
        room_id: Uuid,
        sender: Option<Uuid>,
        payload: serde_json::Value,
    ) -> Envelope {
        let mut env = env_of(kind, payload);
        env.room_id = Some(room_id);
        env.sender = sender.map(|user_id| locast_protocol::envelope::Sender {
            user_id,
            pubkey: Vec::new(),
            sig: Vec::new(),
        });
        env
    }

    /// P6-T03: records the chat events `handle_inbound` emits.
    #[derive(Default)]
    struct ChatSink {
        msgs: std::sync::Mutex<Vec<ChatMessageEvent>>,
    }
    impl RoomEventSink for ChatSink {
        fn emit_state(&self, _summary: &RoomSummaryIpc) {}
        fn emit_event(&self, _summary: &RoomSummaryIpc) {}
        fn emit_state_cleared(&self) {}
        fn emit_chat_message(&self, ev: &ChatMessageEvent) {
            self.msgs.lock().unwrap().push(ev.clone());
        }
    }

    #[tokio::test]
    async fn inbound_chat_message_reaches_the_sink_with_the_roster_name() {
        let me = Uuid::now_v7();
        let rc = fresh_room_client().await;
        // `me` is the host (named "host" in the sample roster).
        let summary = RoomSummaryIpc::from(sample_summary(me));
        let room_id = Uuid::parse_str(&summary.id).unwrap();
        *rc.state.lock().await = Some(summary);
        *rc.local_user_id.lock().await = Some(me);
        let sink = Arc::new(ChatSink::default());
        rc.install_event_sink(sink.clone()).await;

        let stranger = Uuid::now_v7();
        let reply = Uuid::now_v7();
        let chat = |sender: Uuid, text: &str, room: Uuid| {
            let mut env = env_of(
                MessageKind::ChatMessage,
                serde_json::json!({
                    "sender_id": sender, "text": text, "reply_to": reply, "sent_ms": 42
                }),
            );
            env.room_id = Some(room);
            env
        };
        // The local user's own echo is delivered: it is how the
        // sender's panel shows the message.
        rc.handle_inbound(chat(me, "mine", room_id)).await;
        rc.handle_inbound(chat(stranger, "theirs", room_id)).await;
        // A message for some other room is dropped.
        rc.handle_inbound(chat(stranger, "elsewhere", Uuid::now_v7()))
            .await;

        let msgs = sink.msgs.lock().unwrap().clone();
        assert_eq!(msgs.len(), 2, "got {msgs:?}");
        assert_eq!(msgs[0].sender_name, "host");
        assert_eq!(msgs[0].text, "mine");
        assert_eq!(msgs[0].room_id, room_id.to_string());
        assert_eq!(msgs[0].reply_to, Some(reply.to_string()));
        assert_eq!(msgs[0].ts_ms, 42);
        assert_eq!(msgs[1].sender_id, stranger.to_string());
        assert_eq!(msgs[1].sender_name, stranger.to_string()[..8]);
    }

    #[tokio::test]
    async fn laser_move_takes_the_sender_from_the_envelope_not_the_payload() {
        let me = Uuid::now_v7();
        let remote = Uuid::now_v7();
        let spoofed = Uuid::now_v7();
        let (rc, room_id, sink) = laser_client(me).await;
        // A hostile payload naming someone else (or us) is ignored:
        // only the server-stamped `Envelope::sender` counts.
        rc.handle_inbound(laser_env(
            MessageKind::LaserMove,
            room_id,
            Some(remote),
            serde_json::json!({ "x": 0.25, "y": 0.75, "user_id": spoofed, "sender_id": me }),
        ))
        .await;
        let moves = sink.moves.lock().unwrap().clone();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].sender_id, remote.to_string());
        assert_eq!(moves[0].room_id, room_id.to_string());
        assert_eq!((moves[0].x, moves[0].y), (0.25, 0.75));

        rc.handle_inbound(laser_env(
            MessageKind::LaserOff,
            room_id,
            Some(remote),
            serde_json::json!({ "user_id": spoofed }),
        ))
        .await;
        let offs = sink.offs.lock().unwrap().clone();
        assert_eq!(offs.len(), 1);
        assert_eq!(offs[0].sender_id, remote.to_string());
        assert_eq!(offs[0].room_id, room_id.to_string());
    }

    #[tokio::test]
    async fn laser_without_a_usable_sender_is_dropped() {
        let (rc, room_id, sink) = laser_client(Uuid::now_v7()).await;
        for kind in [MessageKind::LaserMove, MessageKind::LaserOff] {
            for sender in [None, Some(Uuid::nil())] {
                rc.handle_inbound(laser_env(
                    kind.clone(),
                    room_id,
                    sender,
                    serde_json::json!({ "x": 0.5, "y": 0.5, "user_id": Uuid::now_v7() }),
                ))
                .await;
            }
        }
        assert!(sink.moves.lock().unwrap().is_empty());
        assert!(sink.offs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn laser_for_another_room_or_no_room_is_dropped() {
        let (rc, _room_id, sink) = laser_client(Uuid::now_v7()).await;
        let remote = Uuid::now_v7();
        for kind in [MessageKind::LaserMove, MessageKind::LaserOff] {
            rc.handle_inbound(laser_env(
                kind.clone(),
                Uuid::now_v7(),
                Some(remote),
                serde_json::json!({ "x": 0.5, "y": 0.5 }),
            ))
            .await;
            let mut no_room = laser_env(
                kind,
                Uuid::now_v7(),
                Some(remote),
                serde_json::json!({ "x": 0.5, "y": 0.5 }),
            );
            no_room.room_id = None;
            rc.handle_inbound(no_room).await;
        }
        assert!(sink.moves.lock().unwrap().is_empty());
        assert!(sink.offs.lock().unwrap().is_empty());

        // Not in any room at all.
        *rc.state.lock().await = None;
        rc.handle_inbound(laser_env(
            MessageKind::LaserMove,
            Uuid::now_v7(),
            Some(remote),
            serde_json::json!({ "x": 0.5, "y": 0.5 }),
        ))
        .await;
        assert!(sink.moves.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn laser_echo_of_the_local_user_is_dropped() {
        let me = Uuid::now_v7();
        let (rc, room_id, sink) = laser_client(me).await;
        for kind in [MessageKind::LaserMove, MessageKind::LaserOff] {
            rc.handle_inbound(laser_env(
                kind,
                room_id,
                Some(me),
                serde_json::json!({ "x": 0.5, "y": 0.5 }),
            ))
            .await;
        }
        assert!(sink.moves.lock().unwrap().is_empty());
        assert!(sink.offs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn laser_move_with_bad_coordinates_is_dropped() {
        let (rc, room_id, sink) = laser_client(Uuid::now_v7()).await;
        let remote = Uuid::now_v7();
        for payload in [
            serde_json::json!({ "x": -0.1, "y": 0.5 }),
            serde_json::json!({ "x": 0.5, "y": 1.5 }),
            // JSON has no NaN / infinity; a huge value overflows f32
            // to infinity or fails to decode, either way dropped.
            serde_json::json!({ "x": 1e300, "y": 0.5 }),
            serde_json::json!({ "x": "0.5", "y": 0.5 }),
            serde_json::json!({ "y": 0.5 }),
        ] {
            rc.handle_inbound(laser_env(
                MessageKind::LaserMove,
                room_id,
                Some(remote),
                payload,
            ))
            .await;
        }
        assert!(sink.moves.lock().unwrap().is_empty());
        // The edges of the frame are valid.
        rc.handle_inbound(laser_env(
            MessageKind::LaserMove,
            room_id,
            Some(remote),
            serde_json::json!({ "x": 0.0, "y": 1.0 }),
        ))
        .await;
        assert_eq!(sink.moves.lock().unwrap().len(), 1);
    }

    #[test]
    fn laser_unit_range_rejects_non_finite_values() {
        assert!(laser_unit_ok(0.0) && laser_unit_ok(1.0) && laser_unit_ok(0.5));
        for bad in [-0.0001, 1.0001, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(!laser_unit_ok(bad), "{bad} must be rejected");
        }
    }

    #[test]
    fn undo_and_clear_events_carry_the_server_stamped_actor_not_the_payload() {
        let room = Uuid::now_v7();
        let actor = Uuid::now_v7();
        let stroke = Uuid::now_v7();
        let undo = StrokeUndoEvent::from((
            room,
            actor,
            &locast_protocol::room::StrokeUndoPayload { stroke_id: stroke },
        ));
        assert_eq!(undo.room_id, room.to_string());
        assert_eq!(undo.sender_id, actor.to_string());
        assert_eq!(undo.stroke_id, stroke.to_string());
        let clear = StrokeClearEvent::from((room, actor));
        assert_eq!(clear.room_id, room.to_string());
        assert_eq!(clear.sender_id, actor.to_string());
        // The wire payload has no identity field to read.
        let wire =
            serde_json::to_value(locast_protocol::room::StrokeUndoPayload { stroke_id: stroke })
                .unwrap();
        assert_eq!(wire, serde_json::json!({ "stroke_id": stroke }));
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

    /// Run one HOST_MIGRATED (A -> B, with or without the
    /// post-migration summary) against a client whose local
    /// user is `me` and whose cached `you_cap_set` is `caps`;
    /// return the resulting `you_cap_set`.
    async fn you_cap_set_after_migration(me: Uuid, caps: u32, with_summary: bool) -> Option<u32> {
        let rc = fresh_room_client().await;
        let host_a = Uuid::from_bytes([1u8; 16]);
        let host_b = Uuid::from_bytes([2u8; 16]);
        let mut cached = RoomSummaryIpc::from(sample_summary(host_a));
        cached.you_cap_set = Some(caps);
        *rc.state.lock().await = Some(cached);
        *rc.local_user_id.lock().await = Some(me);
        let payload = HostMigratedPayload {
            previous_host_user_id: host_a,
            new_host_user_id: host_b,
            summary: with_summary.then(|| Box::new(sample_summary(host_b))),
        };
        let env = env_of(
            MessageKind::HostMigrated,
            serde_json::to_value(payload).unwrap(),
        );
        rc.handle_inbound(env).await;
        rc.state().await.expect("state").you_cap_set
    }

    #[tokio::test]
    async fn host_migrated_keeps_a_bystanders_granted_caps() {
        use locast_protocol::room::cap;
        let bystander = Uuid::from_bytes([3u8; 16]);
        let granted = cap::CHAT | cap::DRAW | cap::LASER;
        for with_summary in [true, false] {
            assert_eq!(
                you_cap_set_after_migration(bystander, granted, with_summary).await,
                Some(granted),
                "with_summary = {with_summary}"
            );
        }
    }

    #[tokio::test]
    async fn host_migrated_gives_the_promoted_local_user_host_caps() {
        use locast_protocol::room::cap;
        let host_b = Uuid::from_bytes([2u8; 16]);
        for with_summary in [true, false] {
            assert_eq!(
                you_cap_set_after_migration(host_b, cap::CHAT | cap::DRAW, with_summary).await,
                Some(cap::HOST),
                "with_summary = {with_summary}"
            );
        }
    }

    #[tokio::test]
    async fn host_migrated_drops_the_demoted_local_host_to_chat() {
        use locast_protocol::room::cap;
        let host_a = Uuid::from_bytes([1u8; 16]);
        for with_summary in [true, false] {
            assert_eq!(
                you_cap_set_after_migration(host_a, cap::HOST, with_summary).await,
                Some(cap::CHAT),
                "with_summary = {with_summary}"
            );
        }
    }

    #[tokio::test]
    async fn room_state_keeps_the_cached_you_cap_set() {
        use locast_protocol::room::cap;
        let rc = fresh_room_client().await;
        let host = Uuid::from_bytes([1u8; 16]);
        let mut cached = RoomSummaryIpc::from(sample_summary(host));
        cached.you_cap_set = Some(cap::CHAT | cap::DRAW);
        *rc.state.lock().await = Some(cached);
        let env = env_of(
            MessageKind::RoomState,
            serde_json::to_value(RoomStatePayload {
                room: sample_summary(host),
                host_disconnect_deadline_ms: None,
            })
            .unwrap(),
        );
        rc.handle_inbound(env).await;
        let s = rc.state().await.expect("state");
        assert_eq!(s.you_cap_set, Some(cap::CHAT | cap::DRAW));
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

    /// After the server ends the room, a later WS reconnect must not
    /// re-join it (the post-AUTH_OK hook reads these credentials), and
    /// the host's media selection must not leak into the next room.
    #[tokio::test]
    async fn room_closed_forgets_the_rejoin_credentials_and_media_selection() {
        let rc = fresh_room_client().await;
        let host = Uuid::from_bytes([1u8; 16]);
        *rc.state.lock().await = Some(RoomSummaryIpc::from(sample_summary(host)));
        *rc.active_room_code.lock().expect("lock") = Some(("ABC123".into(), "viewer".into()));
        rc.set_host_media_selection(Some(vec!["m1".into()]));

        let env = env_of(
            MessageKind::RoomClosed,
            serde_json::to_value(locast_protocol::room::RoomClosedPayload {
                reason: "host_left".into(),
            })
            .unwrap(),
        );
        rc.handle_inbound(env).await;

        assert!(rc.active_room_code.lock().expect("lock").is_none());
        assert!(rc.host_media_selection().is_none());
        assert!(
            rc.rejoin_active_room().await.is_ok(),
            "with no credentials the rejoin is a no-op and sends nothing"
        );
    }

    /// Envelopes buffered while the WS was down belong to the session
    /// that just ended. If they survive, the next reconnect after a
    /// later join flushes them into the new room (a stale `ROOM_LEAVE`
    /// would remove the user from it).
    #[tokio::test]
    async fn ending_the_room_session_discards_buffered_envelopes() {
        let stale = || envelope(MessageKind::RoomLeave, None, RoomLeavePayload {});

        // Server ends the room.
        let rc = fresh_room_client().await;
        *rc.active_room_code.lock().expect("lock") = Some(("ABC123".into(), "viewer".into()));
        rc.pending_outbound.lock().expect("lock").push(stale());
        let env = env_of(
            MessageKind::RoomClosed,
            serde_json::to_value(locast_protocol::room::RoomClosedPayload {
                reason: "host_left".into(),
            })
            .unwrap(),
        );
        rc.handle_inbound(env).await;
        assert!(rc.pending_outbound.lock().expect("lock").is_empty());

        // The user leaves (room_leave ends in the same call).
        let rc = fresh_room_client().await;
        rc.pending_outbound.lock().expect("lock").push(stale());
        rc.pending_outbound.lock().expect("lock").push(stale());
        rc.forget_room_session();
        assert!(rc.pending_outbound.lock().expect("lock").is_empty());
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
        for code in [
            RoomErrorCode::Unauthorized,
            RoomErrorCode::RoomNotFound,
            RoomErrorCode::RoomClosed,
            RoomErrorCode::NotJoined,
        ] {
            let rc = fresh_room_client().await;
            let host = Uuid::from_bytes([1u8; 16]);
            *rc.state.lock().await = Some(RoomSummaryIpc::from(sample_summary(host)));
            let env = env_of(
                MessageKind::RoomError,
                serde_json::to_value(RoomErrorPayload {
                    code,
                    message: "gone".into(),
                })
                .unwrap(),
            );
            rc.handle_inbound(env).await;
            assert!(rc.state().await.is_none(), "{code:?}");
        }
    }

    /// A rejected fire-and-forget command (e.g. a host
    /// PLAYBACK_CMD the server refused) must not drop the
    /// local user out of the room.
    #[tokio::test]
    async fn room_error_rejecting_a_command_keeps_the_room() {
        for code in [
            RoomErrorCode::InvalidState,
            RoomErrorCode::NotHost,
            RoomErrorCode::StaleCommand,
            RoomErrorCode::Internal,
        ] {
            let rc = fresh_room_client().await;
            let host = Uuid::from_bytes([1u8; 16]);
            *rc.state.lock().await = Some(RoomSummaryIpc::from(sample_summary(host)));
            let env = env_of(
                MessageKind::RoomError,
                serde_json::to_value(RoomErrorPayload {
                    code,
                    message: "playback rejected".into(),
                })
                .unwrap(),
            );
            rc.handle_inbound(env).await;
            assert!(rc.state().await.is_some(), "{code:?}");
        }
    }

    #[tokio::test]
    async fn room_error_for_a_manifest_fetch_keeps_the_room() {
        let rc = fresh_room_client().await;
        let host = Uuid::from_bytes([1u8; 16]);
        *rc.state.lock().await = Some(RoomSummaryIpc::from(sample_summary(host)));
        let (tx, rx) = oneshot::channel();
        rc.pending
            .lock()
            .await
            .entry(MessageKind::ManifestResponse)
            .or_default()
            .push(tx);
        let env = env_of(
            MessageKind::RoomError,
            serde_json::to_value(RoomErrorPayload {
                code: RoomErrorCode::InvalidState,
                message: "no manifest".into(),
            })
            .unwrap(),
        );
        rc.handle_inbound(env).await;
        assert_eq!(rx.await.expect("waiter").r#type, MessageKind::RoomError);
        assert!(
            rc.state().await.is_some(),
            "a failed fetch must not end the room"
        );
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

    /// A recording event sink that captures every
    /// `emit_*` call so the test can assert the
    /// `manifest://state` event fires.
    struct RecordingSink {
        manifests: std::sync::Mutex<Vec<ManifestStateEvent>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                manifests: std::sync::Mutex::new(Vec::new()),
            }
        }
    }
    impl RoomEventSink for RecordingSink {
        fn emit_state(&self, _summary: &RoomSummaryIpc) {}
        fn emit_event(&self, _summary: &RoomSummaryIpc) {}
        fn emit_state_cleared(&self) {}
        fn emit_manifest_state(&self, ev: &ManifestStateEvent) {
            self.manifests.lock().unwrap().push(ev.clone());
        }
    }

    #[tokio::test]
    async fn manifest_published_verifies_and_caches() {
        // Build a host-side signed manifest, then drive
        // the MANIFEST_PUBLISHED inbound handler and
        // assert the cache + the Tauri event fire.
        let mut m = locast_manifest::MediaManifest {
            manifest_version: 1,
            room_id: Uuid::now_v7().to_string(),
            media: vec![],
            subtitles: vec![],
            created_at: 1_700_000_000_000,
            host_signature: None,
        };
        // RFC 8032 §7.1 test 1 vector.
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        m = locast_manifest::sign_manifest(&seed, &m).expect("sign");
        let room_uuid = Uuid::parse_str(&m.room_id).expect("uuid");

        let rc = fresh_room_client().await;
        let recorder = Arc::new(RecordingSink::new());
        rc.install_event_sink(recorder.clone()).await;
        // P3-T04 prerequisite 2: install the trust anchor
        // so the TOFU check passes. The manifest's
        // pubkey is the RFC 8032 test 1 verifying key,
        // derived from the seed.
        let expected_pubkey: [u8; 32] = [
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
            0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
            0xf7, 0x07, 0x51, 0x1a,
        ];
        rc.set_expected_host_pubkey(expected_pubkey);

        let payload = locast_protocol::room::ManifestPublishedPayload {
            manifest: m.clone(),
            version: 1,
            published_at_ms: 1_700_000_000_000,
        };
        let env = Envelope {
            v: 1,
            r#type: MessageKind::ManifestPublished,
            id: Uuid::now_v7(),
            room_id: Some(room_uuid),
            sender: None,
            ts_ms: 1_700_000_000_000,
            seq: 0,
            payload: serde_json::to_value(payload).expect("payload json"),
        };
        rc.handle_inbound(env).await;

        // The verified manifest is in the cache.
        let cached = rc
            .verified_manifest(room_uuid)
            .expect("manifest must be cached after verified publish");
        assert_eq!(cached.room_id, m.room_id);
        assert!(cached.host_signature.is_some());
        // The Tauri event fired exactly once.
        let fired = recorder.manifests.lock().unwrap();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].room_id, room_uuid.to_string());
        assert_eq!(fired[0].version, 1);
        assert_eq!(fired[0].manifest_hash.len(), 64); // 32-byte BLAKE3 hex
    }

    #[tokio::test]
    async fn manifest_published_without_trust_anchor_is_dropped() {
        // P3-T04 prerequisite 2: when no trust anchor has
        // been installed, the handler must drop the
        // manifest even if the cryptographic signature
        // is valid. Defense in depth: a signaling server
        // that has a valid signed manifest for a room
        // we did not join through the invite must NOT
        // be able to push the manifest.
        let mut m = locast_manifest::MediaManifest {
            manifest_version: 1,
            room_id: Uuid::now_v7().to_string(),
            media: vec![],
            subtitles: vec![],
            created_at: 1_700_000_000_000,
            host_signature: None,
        };
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        m = locast_manifest::sign_manifest(&seed, &m).expect("sign");
        let room_uuid = Uuid::parse_str(&m.room_id).expect("uuid");

        let rc = fresh_room_client().await;
        let recorder = Arc::new(RecordingSink::new());
        rc.install_event_sink(recorder.clone()).await;
        // No set_expected_host_pubkey call.

        let payload = locast_protocol::room::ManifestPublishedPayload {
            manifest: m,
            version: 1,
            published_at_ms: 0,
        };
        let env = Envelope {
            v: 1,
            r#type: MessageKind::ManifestPublished,
            id: Uuid::now_v7(),
            room_id: Some(room_uuid),
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload: serde_json::to_value(payload).expect("payload json"),
        };
        rc.handle_inbound(env).await;
        // The cache stays empty.
        assert!(rc.verified_manifest(room_uuid).is_none());
        // No Tauri event fired.
        assert!(recorder.manifests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn manifest_published_with_mismatched_pubkey_is_dropped() {
        // The manifest is correctly signed by the RFC
        // 8032 test 1 key, but the trust anchor is set
        // to a DIFFERENT pubkey. The handler must drop.
        let mut m = locast_manifest::MediaManifest {
            manifest_version: 1,
            room_id: Uuid::now_v7().to_string(),
            media: vec![],
            subtitles: vec![],
            created_at: 1_700_000_000_000,
            host_signature: None,
        };
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        m = locast_manifest::sign_manifest(&seed, &m).expect("sign");
        let room_uuid = Uuid::parse_str(&m.room_id).expect("uuid");

        let rc = fresh_room_client().await;
        let recorder = Arc::new(RecordingSink::new());
        rc.install_event_sink(recorder.clone()).await;
        // Set a DIFFERENT pubkey as the trust anchor.
        let mut wrong: [u8; 32] = [0u8; 32];
        wrong[0] = 0xAA;
        wrong[31] = 0xBB;
        rc.set_expected_host_pubkey(wrong);

        let payload = locast_protocol::room::ManifestPublishedPayload {
            manifest: m,
            version: 1,
            published_at_ms: 0,
        };
        let env = Envelope {
            v: 1,
            r#type: MessageKind::ManifestPublished,
            id: Uuid::now_v7(),
            room_id: Some(room_uuid),
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload: serde_json::to_value(payload).expect("payload json"),
        };
        rc.handle_inbound(env).await;
        assert!(rc.verified_manifest(room_uuid).is_none());
        assert!(recorder.manifests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn manifest_published_persists_to_local_sqlite() {
        // P3-T04 prerequisite 4: a verified manifest is
        // written to the local room_manifests table. A
        // fresh RoomClient on the same pool (simulating a
        // restart) can read it back via ManifestStore.
        use sqlx::sqlite::SqlitePoolOptions;

        let mut m = locast_manifest::MediaManifest {
            manifest_version: 1,
            room_id: Uuid::now_v7().to_string(),
            media: vec![],
            subtitles: vec![],
            created_at: 1_700_000_000_000,
            host_signature: None,
        };
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        m = locast_manifest::sign_manifest(&seed, &m).expect("sign");
        let room_uuid = Uuid::parse_str(&m.room_id).expect("uuid");

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        sqlx::query(
            "CREATE TABLE room_manifests (
                id TEXT PRIMARY KEY,
                room_id TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                media TEXT NOT NULL,
                subtitles TEXT NOT NULL DEFAULT '[]',
                version INTEGER NOT NULL,
                UNIQUE (room_id, version)
            )",
        )
        .execute(&pool)
        .await
        .expect("create table");

        let rc = fresh_room_client().await;
        rc.set_storage_pool(pool.clone());
        let recorder = Arc::new(RecordingSink::new());
        rc.install_event_sink(recorder.clone()).await;
        let expected_pubkey: [u8; 32] = [
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
            0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
            0xf7, 0x07, 0x51, 0x1a,
        ];
        rc.set_expected_host_pubkey(expected_pubkey);

        let payload = locast_protocol::room::ManifestPublishedPayload {
            manifest: m,
            version: 1,
            published_at_ms: 1_700_000_000_000,
        };
        let env = Envelope {
            v: 1,
            r#type: MessageKind::ManifestPublished,
            id: Uuid::now_v7(),
            room_id: Some(room_uuid),
            sender: None,
            ts_ms: 1_700_000_000_000,
            seq: 0,
            payload: serde_json::to_value(payload).expect("payload json"),
        };
        rc.handle_inbound(env).await;

        // Read back from the same pool. The handler
        // called ManifestStore::upsert with the verified
        // manifest.
        let store = crate::storage::manifests::ManifestStore::new(&pool);
        let got = store
            .get_latest(room_uuid)
            .await
            .expect("get_latest")
            .expect("must be persisted");
        assert_eq!(got.room_id, room_uuid.to_string());
        assert_eq!(got.version, 1);
        let media: Vec<locast_manifest::MediaEntry> =
            serde_json::from_str(&got.media_json).expect("media json");
        assert!(media.is_empty());
    }

    #[tokio::test]
    async fn manifest_published_with_tampered_signature_is_dropped() {
        // Same setup, but tamper with one byte of the
        // signature. The handler must drop the event
        // without populating the cache.
        let mut m = locast_manifest::MediaManifest {
            manifest_version: 1,
            room_id: Uuid::now_v7().to_string(),
            media: vec![],
            subtitles: vec![],
            created_at: 1_700_000_000_000,
            host_signature: None,
        };
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        m = locast_manifest::sign_manifest(&seed, &m).expect("sign");
        // Tamper with the signature.
        if let Some(hs) = m.host_signature.as_mut() {
            let mut bytes = locast_crypto::ed25519::from_base64(&hs.value).unwrap();
            bytes[0] ^= 0x01;
            hs.value = locast_crypto::ed25519::to_base64(&bytes);
        }
        let room_uuid = Uuid::parse_str(&m.room_id).expect("uuid");

        let rc = fresh_room_client().await;
        let recorder = Arc::new(RecordingSink::new());
        rc.install_event_sink(recorder.clone()).await;

        let payload = locast_protocol::room::ManifestPublishedPayload {
            manifest: m,
            version: 1,
            published_at_ms: 0,
        };
        let env = Envelope {
            v: 1,
            r#type: MessageKind::ManifestPublished,
            id: Uuid::now_v7(),
            room_id: Some(room_uuid),
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload: serde_json::to_value(payload).expect("payload json"),
        };
        rc.handle_inbound(env).await;
        // The cache stays empty.
        assert!(rc.verified_manifest(room_uuid).is_none());
        // No Tauri event fired.
        assert!(recorder.manifests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_manifest_version_is_rejected() {
        // P3-T04 prerequisite: an older-version MANIFEST_PUBLISHED
        // (or MANIFEST_RESPONSE arriving on the late-join path)
        // cannot downgrade the cached manifest. The reject
        // surfaces as `ManifestAcceptError::StaleVersion`.
        let mut m = locast_manifest::MediaManifest {
            manifest_version: 1,
            room_id: Uuid::now_v7().to_string(),
            media: vec![],
            subtitles: vec![],
            created_at: 1_700_000_000_000,
            host_signature: None,
        };
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        m = locast_manifest::sign_manifest(&seed, &m).expect("sign");
        let room_uuid = Uuid::parse_str(&m.room_id).expect("uuid");
        let expected_pubkey: [u8; 32] = [
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
            0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
            0xf7, 0x07, 0x51, 0x1a,
        ];

        let rc = fresh_room_client().await;
        rc.set_expected_host_pubkey(expected_pubkey);

        // Accept version 2 first.
        rc.accept_manifest(m.clone(), 2, 0, "test")
            .await
            .expect("v2 accepted");

        // Now try version 1 (stale) — must be rejected.
        let err = rc
            .accept_manifest(m.clone(), 1, 0, "test")
            .await
            .expect_err("v1 stale must be rejected");
        assert!(matches!(
            err,
            super::ManifestAcceptError::StaleVersion {
                incoming: 1,
                cached: 2
            }
        ));
        // Cache still holds the v2 manifest.
        let cached = rc.verified_manifest(room_uuid).expect("cached");
        assert_eq!(cached.room_id, m.room_id);

        // Version 3 (strictly newer) is accepted.
        rc.accept_manifest(m.clone(), 3, 0, "test")
            .await
            .expect("v3 accepted");
    }

    #[tokio::test]
    async fn accept_manifest_direct_tofu_mismatch() {
        // P3-T04 late-join TOFU: the unified `accept_manifest`
        // helper must reject a manifest whose host_signature
        // does not match the installed trust anchor, even when
        // called directly (i.e. outside the MANIFEST_PUBLISHED
        // broadcast handler). This is the closure of the audit
        // finding that MANIFEST_RESPONSE previously bypassed
        // TOFU.
        let mut m = locast_manifest::MediaManifest {
            manifest_version: 1,
            room_id: Uuid::now_v7().to_string(),
            media: vec![],
            subtitles: vec![],
            created_at: 1_700_000_000_000,
            host_signature: None,
        };
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        m = locast_manifest::sign_manifest(&seed, &m).expect("sign");
        let room_uuid = Uuid::parse_str(&m.room_id).expect("uuid");

        let rc = fresh_room_client().await;
        // Install a WRONG trust anchor (every byte zero'd, not
        // the RFC 8032 §7.1 test 1 verifying key).
        rc.set_expected_host_pubkey([0u8; 32]);

        let err = rc
            .accept_manifest(m.clone(), 1, 0, "MANIFEST_RESPONSE")
            .await
            .expect_err("mismatched anchor must reject");
        assert!(matches!(
            err,
            super::ManifestAcceptError::TrustAnchorMismatch
        ));
        // No manifest cached.
        assert!(rc.verified_manifest(room_uuid).is_none());
    }

    #[tokio::test]
    async fn cleared_trust_anchor_rejects_a_previously_trusted_host() {
        // A code-only join clears the anchor; an anchor from an
        // earlier invite must not vouch for the new room.
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        let sign = |room: Uuid| {
            locast_manifest::sign_manifest(
                &seed,
                &locast_manifest::MediaManifest {
                    manifest_version: 1,
                    room_id: room.to_string(),
                    media: vec![],
                    subtitles: vec![],
                    created_at: 1_700_000_000_000,
                    host_signature: None,
                },
            )
            .expect("sign")
        };
        let first = sign(Uuid::now_v7());
        let pk_b64 = first
            .host_signature
            .as_ref()
            .expect("signed")
            .public_key
            .clone();
        let pk: [u8; 32] = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(pk_b64)
                .expect("b64")
                .try_into()
                .expect("32 bytes")
        };

        let rc = fresh_room_client().await;
        rc.set_expected_host_pubkey(pk);
        rc.accept_manifest(first, 1, 0, "MANIFEST_RESPONSE")
            .await
            .expect("trusted host accepted");

        rc.clear_expected_host_pubkey();
        assert!(rc.expected_host_pubkey().is_none());
        let second = sign(Uuid::now_v7());
        let second_room = Uuid::parse_str(&second.room_id).expect("uuid");
        let err = rc
            .accept_manifest(second, 1, 0, "MANIFEST_PUBLISHED")
            .await
            .expect_err("no anchor after clear");
        assert!(matches!(err, super::ManifestAcceptError::NoTrustAnchor));
        assert!(rc.verified_manifest(second_room).is_none());
    }

    #[tokio::test]
    async fn accept_manifest_without_trust_anchor_rejects() {
        // P3-T04 late-join TOFU: a `MANIFEST_RESPONSE` arriving
        // before the invite has been parsed (no trust anchor
        // installed) must be rejected. The audit's gap was
        // that the late-join path returned the typed payload
        // without this check.
        let mut m = locast_manifest::MediaManifest {
            manifest_version: 1,
            room_id: Uuid::now_v7().to_string(),
            media: vec![],
            subtitles: vec![],
            created_at: 1_700_000_000_000,
            host_signature: None,
        };
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        m = locast_manifest::sign_manifest(&seed, &m).expect("sign");

        let rc = fresh_room_client().await;
        // No `set_expected_host_pubkey` call.
        let err = rc
            .accept_manifest(m, 1, 0, "MANIFEST_RESPONSE")
            .await
            .expect_err("no anchor must reject");
        assert!(matches!(err, super::ManifestAcceptError::NoTrustAnchor));
    }

    /// Captures the drawing events the room client emits.
    #[derive(Default)]
    struct DrawingSink {
        undos: std::sync::Mutex<Vec<StrokeUndoEvent>>,
        syncs: std::sync::Mutex<Vec<StrokeSyncEvent>>,
    }
    impl RoomEventSink for DrawingSink {
        fn emit_state(&self, _summary: &RoomSummaryIpc) {}
        fn emit_event(&self, _summary: &RoomSummaryIpc) {}
        fn emit_state_cleared(&self) {}
        fn emit_stroke_undo(&self, ev: &StrokeUndoEvent) {
            self.undos.lock().unwrap().push(ev.clone());
        }
        fn emit_stroke_sync(&self, ev: &StrokeSyncEvent) {
            self.syncs.lock().unwrap().push(ev.clone());
        }
    }

    #[tokio::test]
    async fn drawing_events_carry_seq_and_draw_sync_reaches_the_webview() {
        let rc = fresh_room_client().await;
        let summary = sample_summary(Uuid::from_bytes([1u8; 16]));
        let room = summary.id;
        *rc.state.lock().await = Some(RoomSummaryIpc::from(summary));
        let sink = Arc::new(DrawingSink::default());
        rc.install_event_sink(sink.clone()).await;
        let actor = Uuid::now_v7();
        let stroke = Uuid::now_v7();

        let mut undo = env_of(
            MessageKind::StrokeUndo,
            serde_json::json!({ "stroke_id": stroke }),
        );
        undo.room_id = Some(room);
        undo.seq = 7;
        undo.sender = Some(locast_protocol::envelope::Sender {
            user_id: actor,
            pubkey: Vec::new(),
            sig: Vec::new(),
        });
        rc.handle_inbound(undo).await;
        let undos = sink.undos.lock().unwrap().clone();
        assert_eq!(undos.len(), 1);
        assert_eq!(
            undos[0].seq, 7,
            "the server's drawing seq reaches the webview"
        );

        let snapshot = locast_protocol::room::StrokeSyncPayload {
            seq: 9,
            strokes: vec![locast_protocol::room::StrokeSyncStroke {
                stroke_id: stroke,
                owner_id: actor,
                begin: Some(locast_protocol::room::StrokeBeginPayload {
                    stroke_id: stroke,
                    tool: locast_protocol::room::StrokeTool::Pen,
                    color: "#123456".into(),
                    width: 3.0,
                    x: 0.1,
                    y: 0.2,
                    pressure: 0.5,
                    ts_ms: 1,
                }),
                points: vec![locast_protocol::room::StrokeSyncPoint {
                    x: 0.3,
                    y: 0.4,
                    pressure: 0.5,
                    ts_ms: 2,
                }],
                end_ts_ms: Some(3),
            }],
        };
        let mut sync = env_of(
            MessageKind::StrokeSync,
            serde_json::to_value(&snapshot).unwrap(),
        );
        sync.room_id = Some(room);
        rc.handle_inbound(sync.clone()).await;
        // A snapshot for another room is not this room's state.
        sync.room_id = Some(Uuid::now_v7());
        rc.handle_inbound(sync).await;

        let syncs = sink.syncs.lock().unwrap().clone();
        assert_eq!(syncs.len(), 1);
        assert_eq!(syncs[0].room_id, room.to_string());
        assert_eq!(syncs[0].seq, 9);
        let st = &syncs[0].strokes[0];
        assert_eq!(st.stroke_id, stroke.to_string());
        assert_eq!(st.owner_id, actor.to_string());
        assert_eq!(st.begin.as_ref().unwrap().tool, "pen");
        assert_eq!(st.points.len(), 1);
        assert_eq!(st.end_ts_ms, Some(3));
    }
}
