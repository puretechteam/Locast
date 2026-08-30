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
    RoomErrorCode, RoomJoinRequestPayload, RoomLeavePayload, RoomStatePayload, RoomSummary,
};
use serde::Serialize;
use specta::Type;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
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
    /// P3-T03: emit `manifest://state` with a verified
    /// manifest descriptor. The default no-op impl is
    /// `()`; the Tauri-backed sink forwards to the
    /// webview.
    fn emit_manifest_state(&self, _ev: &ManifestStateEvent) {}
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
        fn emit_manifest_state(&self, ev: &ManifestStateEvent) {
            let _ = self.handle.emit(MANIFEST_STATE_EVENT, ev.clone());
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
        let payload = RoomJoinRequestPayload { code, display_name };
        let env = envelope(MessageKind::RoomJoinRequest, None, payload);
        let reply = self.request(env, MessageKind::RoomJoined).await?;
        let joined: locast_protocol::room::RoomJoinedPayload = decode_payload(&reply)?;
        let summary = RoomSummaryIpc::from(joined.room);
        *self.state.lock().await = Some(summary.clone());
        self.emit_state(&summary).await;
        self.spawn_presence_loop();
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
        self.abort_presence_loop().await;
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
                self.abort_presence_loop().await;
            }
            MessageKind::ManifestPublished => {
                self.handle_manifest_published(&env).await;
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
}
