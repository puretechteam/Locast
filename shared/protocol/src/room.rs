//! Room-lifecycle payload structs per `docs/ARCHITECTURE.md` section 18.4.2
//! and the P2-T04 spec.
//!
//! P2-T04 adds the room create / join / leave / state / participant
//! / host-migration / presence envelopes. Playback (PLAY / PAUSE /
//! SEEK), drawing (DRAW / LASER), chat, and manifest publishing
//! remain out of scope here.
//!
//! P4-T01: PLAYBACK_CMD envelopes live at the bottom of this file.
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
    /// 12: P4-T01. A PLAYBACK_CMD arrived with a
    /// per-sender `monotonic_seq` outside the valid
    /// window (`<= last_acked_seq` or `> last_acked_seq + 1`).
    /// The wire reply is single-caller; the command is NOT
    /// broadcast. The client should reconnect / reset its
    /// sender-side sequence counter.
    StaleCommand,
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
    /// the `sdpMLineIndex` of the candidate. The server does
    /// not inspect this; it is forwarded verbatim.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sdp_m_line_index: Option<u32>,
}

// ===== P4-T01: PLAYBACK_CMD wire types =====
//
// The room playback surface (docs/ARCHITECTURE.md §13). PLAY /
// PAUSE / SEEK all ride on a single envelope kind (PLAYBACK_CMD)
// whose `action` field discriminates between the three. The
// server is the single arbiter: it validates host authority,
// per-room lifecycle state (PLAY requires Ready/Paused, PAUSE
// requires Playing, SEEK requires Playing/Paused), and the
// per-sender `monotonic_seq`; rejects are sent back as a
// single-caller ROOM_ERROR and are NOT broadcast.
//
// `server_seq` (per-room monotonic, assigned by the server) and
// `server_ts_ms` (server wall clock at acceptance) are stamped
// ONLY on the server-side broadcast payload; the client-supplied
// `monotonic_seq` is per-sender and is what the dedup logic
// (docs/ARCHITECTURE.md §13.2) uses to drop late commands.
//
// `media_position_ms` is u64 because media durations can exceed
// i64::MAX on very long content (the §13.4 encoding requirement is
// big-endian u64 on the wire). For v1 the server-side validator
// only checks the value is well-formed JSON; per-asset bounds
// checking against `media[].duration_ms` happens on the client.

/// PLAYBACK_CMD (C -> S, S -> all).
///
/// The single envelope kind for playback control. The
/// discriminant is `action`. The server stamps `server_seq` and
/// `server_ts_ms` on the rebroadcast payload; the original
/// sender-side payload carries only the per-sender
/// `monotonic_seq` and the playback fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct PlaybackCommandPayload {
    /// Discriminant: which playback action this command
    /// represents.
    #[serde(rename = "type")]
    pub action: PlaybackAction,
    /// Per-sender monotonic sequence. Must be exactly
    /// `last_acked_seq[sender_id] + 1` per docs/ARCHITECTURE.md
    /// §13.2. `monotonic_seq <= last_acked_seq[sender_id]` is
    /// dropped as a duplicate; `monotonic_seq >
    /// last_acked_seq[sender_id] + 1` is rejected as a gap.
    pub monotonic_seq: u64,
    /// Media position the command applies to (PLAY/SEEK).
    /// Always present; ignored for PAUSE (the room's last PLAY /
    /// SEEK position remains authoritative).
    pub media_position_ms: u64,
    /// Sender's wall clock at send (unix ms). Informational
    /// only; the server stamps its own `server_ts_ms` on
    /// acceptance.
    pub client_ts_ms: i64,
}

/// The discriminated playback action.
///
/// §13.4 defines `PLAY`, `PAUSE`, `SEEK` as the v1 action set.
/// We send the discriminant as `"play" | "pause" | "seek"` so
/// the wire shape stays stable if a future task adds more
/// actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
#[serde(rename_all = "lowercase")]
pub enum PlaybackAction {
    Play,
    Pause,
    Seek,
}

// ===== P4-T03: POSITION_REPORT wire type =====
//
// docs/ARCHITECTURE.md §13.1: POSITION_REPORT is a passive,
// non-authoritative snapshot of what a participant's local
// player is doing. It is NOT a command and the server MUST NOT
// interpret it as one (it does not mutate playback state,
// does not increment `server_seq`, and does not transition the
// room lifecycle). The server is a relay: it forwards the
// original payload verbatim to every other room participant
// (the originator is filtered out by the WS broadcast layer).
//
// The payload is intentionally minimal so the wire rate stays
// well within budget at 1 Hz / participant:
//
//   {
//     "media_position_ms": <u64>,  // local <video>.currentTime * 1000, rounded
//     "playing":           <bool>, // local <video>.paused (inverted)
//     "client_ts_ms":      <i64>,  // sender wall clock at send (informational)
//   }
//
// `user_id` is NOT on the wire: the server uses the bearer-derived
// identity to attribute the report to a participant and sets
// the broadcast item's `originator` so the WS layer can suppress
// echoes back to the sender. Receivers get the sender's identity
// from the `Envelope` they see on the wire (the server stamps
// the `sender` field on rebroadcast; for now we use the
// originator pattern from `MANIFEST_PUBLISHED`).
//
// Replay window: the architecture (§21.10) calls for a 5 s
// replay window on POSITION_REPORT vs. the standard 30 s. The
// server applies this drop at the WS layer (existing logic;
// P2-T04 sets the per-socket window).

/// POSITION_REPORT (C -> S -> other room members).
///
/// 1 Hz non-authoritative snapshot of the sender's local
/// playback state. The server is a pure relay: it does NOT
/// validate the playback position (no bounds check against
/// media duration; that is the host's job at PUBLISH time).
/// The server only validates that the sender is currently a
/// member of the named room and forwards the payload
/// verbatim. See module docs above for the architecture
/// references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct PositionReportPayload {
    /// Originating participant's `user_id`. Set by the
    /// server from the validated bearer so the receiver
    /// can attribute the report to a sender without
    /// trusting an unverified client field. The wire
    /// shape mirrors architecture §13.1 (the JSON
    /// example includes `user_id`).
    pub user_id: Uuid,
    /// Local `<video>` position in milliseconds (rounded from
    /// the floating-point `currentTime` seconds). The wire
    /// unit is integer ms so the server can re-emit the
    /// value verbatim without precision drift. `0` is a
    /// valid value (the very first frame); negative values
    /// are not produced by any conforming client.
    pub media_position_ms: u64,
    /// `true` when the local `<video>.paused === false` (i.e.
    /// the media element is actively playing); `false` when
    /// the element is paused. `playing` is observed locally
    /// and is not asserted by the server.
    pub playing: bool,
    /// Sender's wall clock at send (unix ms). Informational
    /// only; the server does not stamp `server_ts_ms` on the
    /// rebroadcast (the roadmap explicitly says "server
    /// forwards without modification"). Receivers may use
    /// this for display only.
    pub client_ts_ms: i64,
}

/// PLAYBACK_CMD (S -> all). The rebroadcast payload after the
/// server has accepted a command. The per-sender `monotonic_seq`
/// is preserved; the server stamps `server_seq` and
/// `server_ts_ms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct PlaybackAcceptedEvent {
    /// Original sender's user_id. Mirrors §13.1 PLAY's
    /// `sender_id` field; the per-room broadcast carries it so
    /// the receiving client can drop late commands
    /// (docs/ARCHITECTURE.md §13.2).
    pub sender_id: Uuid,
    /// The action that was accepted.
    pub action: PlaybackAction,
    /// Sender's monotonic_seq (verbatim from the command).
    pub monotonic_seq: u64,
    /// Media position the command applies to. Always present;
    /// ignored for PAUSE.
    pub media_position_ms: u64,
    /// Sender's wall clock at send (verbatim from the command).
    pub client_ts_ms: i64,
    /// Per-room server-assigned monotonic sequence. First
    /// accepted command after room creation is `1`; increments
    /// strictly per accepted command. NOT incremented for
    /// rejected commands.
    pub server_seq: u64,
    /// Server-stamped wall clock at acceptance, unix ms.
    pub server_ts_ms: i64,
}

/// SKEW_PROBE (C -> S). P4-T06 NTP-style clock skew
/// measurement (docs/ARCHITECTURE.md §13.3). The client
/// records its local wall clock at send time, then awaits
/// SKEW_REPLY. The reply carries the server's `now_ms()` and
/// echoes `client_send_ms` so the client can compute RTT,
/// offset, skew, and jitter from a single round trip.
///
/// The probe is fully authenticated (the per-connection
/// bearer is enforced by the WS layer before the room
/// dispatcher sees the envelope). There is no payload-level
/// `user_id` because the server attributes the probe to the
/// bearer; the client does not need a server-issued
/// identity to measure clock skew.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct SkewProbePayload {
    /// Client's local wall clock at send time (unix ms).
    /// The client captures this IMMEDIATELY before writing
    /// the envelope to the socket so the measurement
    /// tracks the actual wire-write moment rather than the
    /// envelope-construction moment. The server echoes this
    /// value back in `SkewReplyPayload.client_send_ms` so
    /// the client can compute RTT = `t3 - t0` and offset =
    /// `server_ts - (t0 + t_recv_client) / 2` after `t_recv_client`
    /// arrives.
    pub client_send_ms: i64,
}

/// SKEW_REPLY (S -> caller). P4-T06 server's response to a
/// SKEW_PROBE. Carries the server's wall clock at the moment
/// the request was accepted (the same `now_ms()` the
/// dispatcher captures for `Envelope.ts_ms` on every
/// `to_caller` reply) plus the echoed `client_send_ms` so the
/// client can pair the round trip deterministically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct SkewReplyPayload {
    /// Server's wall clock at the moment the probe was
    /// accepted, unix ms. Read from the dispatcher's
    /// injected `AppState::clock` (the same clock the rest
    /// of the room protocol uses for `Envelope.ts_ms`).
    pub server_ts_ms: i64,
    /// Echo of the request's `client_send_ms`. The server
    /// does not touch this value; the client uses it to
    /// pair the round trip with the original probe.
    pub client_send_ms: i64,
}

/// SKEW_REPLY-side NTP sample (architecture §13.3). The
/// client reconstructs the four-timestamp exchange from
/// its own local clock at send (`t0_local_ms`) and at
/// receive (`t3_local_ms`) plus the server's
/// `server_ts_ms` from the reply payload. The
/// `client_send_ms_echo` is included so the client can
/// assert the round trip was not reordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct SkewSample {
    pub t0_local_ms: i64,
    pub t3_local_ms: i64,
    pub server_ts_ms: i64,
    pub client_send_ms_echo: i64,
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
