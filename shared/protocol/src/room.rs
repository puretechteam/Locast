//! Room-lifecycle payload structs per `docs/ARCHITECTURE.md` section 18.4.2
//! and the P2-T04 spec.
//!
//! P2-T04 adds the room create / join / leave / state / participant
//! / host-migration / presence envelopes. Playback (PLAY / PAUSE /
//! SEEK), drawing (DRAW / LASER), chat, and manifest publishing
//! remain out of scope here.
//!
//! The room code uses the 32-character unambiguous alphabet defined
//! in `docs/ARCHITECTURE.md` §21.2: `ABCDEFGHJKLMNPQRSTUVWXYZ23456789`
//! (no `0`, `O`, `1`, `I`, `L`). Codes are 6 characters; the
//! generator, the alphabet constant, and the validation helpers
//! live in `apps/server/src/rooms/codes.rs`.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

/// v1 capability bitfield. The host gets all bits set; default
/// participants get only `CHAT`. Future tasks may set per-room
/// capability grants.
pub mod cap {
    pub const PLAYBACK_CONTROL: u32 = 0x01;
    pub const DRAW: u32 = 0x02;
    pub const LASER: u32 = 0x04;
    pub const MANAGE_ROOM: u32 = 0x08;
    pub const KICK: u32 = 0x10;
    pub const PUBLISH_MANIFEST: u32 = 0x20;
    pub const INVITE: u32 = 0x40;
    pub const CHAT: u32 = 0x80;
}

/// ROOM_CREATE (C -> S). The creator chooses the migration
/// setting at creation; the v1 protocol does not let it be
/// changed later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct RoomCreatePayload {
    pub title: String,
    pub migration_enabled: bool,
}

/// ROOM_CREATED (S -> C). Server's response to a successful
/// ROOM_CREATE. Includes the room summary and the caller's own
/// participant record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct RoomCreatedPayload {
    pub room: RoomSummary,
    pub you: ParticipantSelf,
}

/// ROOM_JOIN_REQUEST (C -> S). The 6-char code is uppercase; the
/// display name is 1-32 chars and validated by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct RoomJoinRequestPayload {
    pub code: String,
    pub display_name: String,
}

/// ROOM_JOINED (S -> C). Server's response to a successful
/// ROOM_JOIN_REQUEST.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct RoomJoinedPayload {
    pub room: RoomSummary,
    pub you: ParticipantSelf,
}

/// ROOM_LEAVE (C -> S). Intentional leave. The payload is
/// empty in v1; the server reads the authenticated user_id off
/// the bearer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[ts(export_to = "ts/index.ts")]
pub struct RoomLeavePayload {}

/// ROOM_STATE (S -> C). Full snapshot of a room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct RoomStatePayload {
    pub room: RoomSummary,
    /// When the host transport has been lost and the room has
    /// migration enabled, this is the unix-ms deadline after
    /// which the server will elect a new host. `None` when the
    /// host is connected.
    pub host_disconnect_deadline_ms: Option<i64>,
}

/// The caller's own view of themselves in a room. Echoed in
/// `ROOM_CREATED` / `ROOM_JOINED` so the client can show the
/// right `joined_ms` / `cap_set` without recomputing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct ParticipantSelf {
    pub user_id: Uuid,
    pub cap_set: u32,
    pub joined_ms: i64,
}

/// The full summary of a room that the server can publish. The
/// `participants` list includes the host; `host_disconnected`
/// and `host_disconnect_deadline_ms` describe the host's
/// transport state when migration is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct RoomSummary {
    pub id: Uuid,
    pub code: String,
    pub title: String,
    pub host_user_id: Uuid,
    pub host_migration_enabled: bool,
    pub created_ms: i64,
    pub participants: Vec<Participant>,
    pub host_disconnected: bool,
    pub host_disconnect_deadline_ms: Option<i64>,
}

/// The view of a single participant published to other
/// participants. Carries the pubkey so other clients can verify
/// signed messages from that participant (P3+ playback
/// commands).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct Participant {
    pub user_id: Uuid,
    pub pubkey: Vec<u8>,
    pub display_name: String,
    pub joined_ms: i64,
    pub status: ParticipantStatus,
    pub last_seen_ms: i64,
    pub is_host: bool,
}

/// Participant transport status. `Joining` is the brief moment
/// after ROOM_JOIN_REQUEST before the first inbound message;
/// `Connected` is the steady state; `Reconnecting` is set when
/// the participant's transport was lost and the server is
/// waiting for a re-handshake; `Disconnected` is the terminal
/// state for a transport loss that did not recover; `Left` is
/// the terminal state for an intentional ROOM_LEAVE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub enum ParticipantStatus {
    Joining,
    Connected,
    Reconnecting,
    Disconnected,
    Left,
}

/// PARTICIPANT_JOINED (S -> C). Broadcast when a new participant
/// enters the room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct ParticipantJoinedPayload {
    pub participant: Participant,
}

/// PARTICIPANT_LEFT (S -> C). Broadcast when a participant
/// leaves for any reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct ParticipantLeftPayload {
    pub user_id: Uuid,
    /// `"leave"` | `"timeout"` | `"kick"` | `"host_migrated_away"`.
    /// The v1 protocol does not define a `kick`; the value is
    /// reserved for a future admin command. `host_migrated_away`
    /// is broadcast when the host's transport was lost and the
    /// new host was elected, so clients that had marked the old
    /// host as `Reconnecting` can update the participant list.
    pub reason: String,
}

/// HOST_DISCONNECTED (S -> C). Sent to all participants when
/// the host's transport is lost. `new_host_user_id` is `Some`
/// only when migration just happened (i.e. the grace period
/// elapsed); during the grace it is `None` and the deadline
/// tells the client when the migration will occur.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct HostDisconnectedPayload {
    pub previous_host_user_id: Uuid,
    pub reconnect_deadline_ms: i64,
    pub new_host_user_id: Option<Uuid>,
}

/// HOST_RECONNECTED (S -> C). Sent when the host re-auths
/// before the grace period elapses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct HostReconnectedPayload {
    pub host_user_id: Uuid,
}

/// HOST_MIGRATED (S -> C). Sent when a new host was elected.
/// The old host is now a normal viewer.
///
/// `summary` carries the authoritative post-migration room
/// snapshot. Clients should replace their cached state with
/// it on receipt; if it is `None` (older server build) the
/// client falls back to the pre-P2-T05 behavior of just
/// updating `host_user_id` and clearing the grace flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct HostMigratedPayload {
    pub previous_host_user_id: Uuid,
    pub new_host_user_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Box<RoomSummary>>,
}

/// ROOM_CLOSED (S -> C). The room has ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct RoomClosedPayload {
    /// `"host_left"` | `"host_disconnected_no_migration"` |
    /// `"admin"` | `"empty"`. v1 only emits the first two;
    /// the others are reserved for future tasks.
    pub reason: String,
}

/// ROOM_ERROR (S -> C). Sent in response to a malformed or
/// out-of-state ROOM_* envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct RoomErrorPayload {
    #[ts(inline)]
    pub code: RoomErrorCode,
    pub message: String,
}

/// The closed set of room-error codes. The wire representation
/// is the discriminator name; the integer in the comment is the
/// v1 stable identifier (kept here for spec cross-referencing
/// only; the protocol itself does not transmit it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub enum RoomErrorCode {
    /// 1: The bearer is missing, malformed, or unknown.
    Unauthorized,
    /// 2: The supplied room code is the wrong length or
    /// contains characters outside the 32-char alphabet.
    InvalidCode,
    /// 3: No room with that code exists.
    RoomNotFound,
    /// 4: The room has already ended.
    RoomClosed,
    /// 5: The room is at its max-participants cap.
    RoomFull,
    /// 6: The user is already a participant in the room.
    AlreadyJoined,
    /// 7: The user is not a participant in the room.
    NotJoined,
    /// 8: The room is in a state that does not accept the
    /// envelope (e.g. ROOM_LEAVE when no migration is possible
    /// because the room is in the grace period).
    InvalidState,
    /// 9: The action is host-only and the caller is not the
    /// host. v1 does not yet use this; reserved for P3+.
    NotHost,
    /// 10: A migration request was made but the room has
    /// migration disabled.
    MigrationDisabled,
    /// 11: An internal error. The client should retry.
    Internal,
}

/// PRESENCE (C -> S). The client periodically re-sends a
/// PRESENCE envelope to refresh its `last_seen` timestamp. The
/// payload is a single `status: "alive"` field in v1; the
/// status string is reserved for a future "afk" / "do not
/// disturb" extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct PresencePayload {
    pub status: String,
}

/// MANIFEST_PUBLISH (C -> S, host -> server). The host signs
/// a fresh [`locast_manifest::MediaManifest`] and submits it
/// to the server for broadcast. The server is the relay; it
/// does NOT validate beyond the bearer + the `PublishManifest`
/// capability check (host-only at the time of publish).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct ManifestPublishPayload {
    /// The signed manifest. The server-side defense-in-depth
    /// check is a `locast_manifest::verify_manifest` call, but
    /// it is NOT the trust boundary: the viewer's TOFU check
    /// against the invite's `h=` parameter is.
    pub manifest: locast_manifest::MediaManifest,
}

/// MANIFEST_PUBLISHED (S -> C, server -> all participants).
/// Broadcast to every participant in the room after a
/// successful MANIFEST_PUBLISH. Viewers store the verified
/// manifest locally and kick off the P3 download flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct ManifestPublishedPayload {
    /// The signed manifest, echoed from the host's
    /// MANIFEST_PUBLISH. Viewers verify the signature against
    /// the host's pubkey (the manifest's
    /// `host_signature.public_key`) and TOFU-compare against
    /// the invite's `h=` parameter.
    pub manifest: locast_manifest::MediaManifest,
    /// Server-assigned per-room monotonic version. `1` on
    /// the first publish of a room; incremented on each
    /// subsequent publish. Viewers persist this in
    /// `room_manifests.version` and use it to ignore
    /// out-of-order `MANIFEST_PUBLISHED` envelopes.
    pub version: i64,
    /// Server-stamped publication time, unix ms. The viewer
    /// uses this as the row's `created_at` for the local
    /// `room_manifests` table.
    pub published_at_ms: i64,
}

/// MANIFEST_REQUEST (C -> S, any room member -> server).
/// A late-joiner catch-up request: the server replies with
/// `MANIFEST_RESPONSE` carrying the room's currently-
/// authoritative manifest (highest `version` in the
/// `room_manifests` table). The request is a no-op if no
/// manifest has been published yet.
///
/// The `media_id` field is informational (architecture
/// §18.4.3). P3-T04's planner will filter the returned
/// `manifest.media[]` by `media_id` if it is present; in
/// v1 the request asks for the full manifest and the
/// client does the filtering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct ManifestRequestPayload {
    pub media_id: Uuid,
}

/// MANIFEST_RESPONSE (S -> C). The server's reply to a
/// MANIFEST_REQUEST. The payload mirrors the MANIFEST_PUBLISHED
/// payload (the manifest, the version, the publish time).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct ManifestResponsePayload {
    /// The signed manifest, identical to what a
    /// MANIFEST_PUBLISHED would have carried at this
    /// `version`.
    pub manifest: locast_manifest::MediaManifest,
    /// Server-assigned per-room monotonic version.
    pub version: i64,
    /// Server-stamped publication time, unix ms.
    pub published_at_ms: i64,
}

/// SIGNAL (C -> S -> C). WebRTC SDP/ICE relay between two room
/// members. The server is a pure relay: it routes the envelope
/// from `sender` to `to_user_id` without inspecting the `sdp` or
/// `candidates` bodies (docs/ARCHITECTURE.md §19.3.3, §18.5.1).
///
/// The wire carries three discriminated payloads via the
/// `kind` field:
/// - `Offer`: an SDP offer (`sdp` is set, `candidates` is None
///   at offer time; trickle candidates come in later `Ice`
///   envelopes).
/// - `Answer`: an SDP answer (`sdp` is set).
/// - `Ice`: a single ICE candidate (`candidates` carries one
///   entry); the server forwards it unchanged. A `candidates`
///   list of length 1 with an empty-string `candidate` field
///   signals `end-of-candidates` per §19.3.3.
///
/// All three variants carry an Ed25519 signature in
/// `envelope.sender.sig` over the canonicalized payload bytes
/// (domain-separated by `locast/v1/SIGNAL`); the server
/// verifies the signature against the sender's pubkey and
/// rejects envelopes whose `sender` is missing or whose
/// `sender.user_id` / `sender.pubkey` do not match the
/// bearer-derived identity (P3-T05 security model).
///
/// The `sdp` blob is base64 NOT encoded: it is the raw SDP
/// text. The application-layer cap is 64 KiB per envelope
/// (enforced server-side, not here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct SignalPayload {
    /// Target peer (the receiving user_id).
    pub to_user_id: Uuid,
    /// Which kind of signal this is.
    pub kind: SignalKind,
    /// SDP body for Offer / Answer; None for Ice.
    /// Raw SDP text (not base64).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sdp: Option<String>,
    /// Single ICE candidate for Ice; None for Offer / Answer.
    /// The candidate string is the SDP `candidate:` attribute
    /// value (without the leading `candidate:` prefix is also
    /// acceptable; the server is a pure relay and does not
    /// inspect the body). An empty `candidate` signals
    /// `end-of-candidates`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub candidates: Option<Vec<SignalCandidate>>,
}

/// The discriminated kind inside a SIGNAL envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    Offer,
    Answer,
    Ice,
}

/// A single ICE candidate entry inside a SIGNAL Ice envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct SignalCandidate {
    /// The candidate string (SDP `candidate:<...>` value, or
    /// the full line including the `candidate:` prefix). The
    /// server does not parse this; it is forwarded verbatim.
    pub candidate: String,
    /// The `sdpMid` of the candidate (typically `"0"`). The
    /// server does not inspect this; it is forwarded verbatim.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sdp_mid: Option<String>,
    /// The `sdpMLineIndex` of the candidate. The server does
    /// not inspect this; it is forwarded verbatim.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sdp_m_line_index: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn room_create_serde_roundtrip() {
        let p = RoomCreatePayload {
            title: "Movie night".into(),
            migration_enabled: true,
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: RoomCreatePayload = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn room_error_payload_includes_code() {
        let p = RoomErrorPayload {
            code: RoomErrorCode::RoomNotFound,
            message: "no room with code XXXXXX".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&p).unwrap();
        assert_eq!(v["code"], json!("RoomNotFound"));
    }

    #[test]
    fn participant_status_serializes() {
        let s = serde_json::to_string(&ParticipantStatus::Connected).unwrap();
        assert_eq!(s, "\"Connected\"");
    }
}
