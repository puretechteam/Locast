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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct HostMigratedPayload {
    pub previous_host_user_id: Uuid,
    pub new_host_user_id: Uuid,
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
