//! Common envelope wrapping every wire message.
//!
//! The shape is the v1 envelope from `docs/ARCHITECTURE.md` section
//! 18.3. The payload field is held as a [`serde_json::Value`] so the
//! envelope is schema-agnostic; the per-type payload structs live in
//! the [`crate::handshake`] module and the future room / playback /
//! drawing modules.
//!
//! The envelope `type` field is a [`MessageKind`] enum. Unknown
//! message types (forward compatibility per §18.11) are surfaced
//! as the `Other(String)` variant; the server still rejects
//! unknown types today but the wire decoder is permissive so that
//! a v1.1 client can talk to a v1 server without panicking on
//! decode.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

/// The wire envelope for every message. See §18.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct Envelope {
    /// Protocol version. Always `1` for v1; receivers reject
    /// anything else (§18.11).
    pub v: u8,
    /// The message type. See [`MessageKind`]. Wire field is
    /// `type`; we use a Rust field name that does not collide
    /// with the reserved keyword.
    #[serde(rename = "type")]
    pub r#type: MessageKind,
    /// Unique per-message UUID v7. The idempotency key for
    /// server-side dedup (§18.7).
    pub id: Uuid,
    /// `None` for handshake messages; `Some(room_id)` otherwise.
    pub room_id: Option<Uuid>,
    /// `None` for handshake and server-originated messages.
    pub sender: Option<Sender>,
    /// Sender wall clock at send (unix ms). Informational;
    /// replay protection is by `seq` (§18.7).
    pub ts_ms: i64,
    /// Monotonic per-sender sequence. First message after
    /// connect is `1`; never reused.
    pub seq: u64,
    /// Type-specific payload. Held as a generic JSON value so
    /// the envelope is schema-agnostic.
    #[ts(type = "unknown")]
    pub payload: serde_json::Value,
}

/// The message type tag. Wire field is `type`. The v1 spec only
/// defines a handful of types; future types (P3+ room lifecycle,
/// playback, drawing, etc.) will be added as new variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub enum MessageKind {
    #[serde(rename = "HELLO")]
    Hello,
    #[serde(rename = "WELCOME")]
    Welcome,
    #[serde(rename = "CHALLENGE")]
    Challenge,
    #[serde(rename = "AUTH")]
    Auth,
    #[serde(rename = "AUTH_OK")]
    AuthOk,
    #[serde(rename = "AUTH_FAIL")]
    AuthFail,
    // ----- P2-T04: room lifecycle (create / join / leave / state / host migration) -----
    #[serde(rename = "ROOM_CREATE")]
    RoomCreate,
    #[serde(rename = "ROOM_CREATED")]
    RoomCreated,
    #[serde(rename = "ROOM_JOIN_REQUEST")]
    RoomJoinRequest,
    #[serde(rename = "ROOM_JOINED")]
    RoomJoined,
    #[serde(rename = "ROOM_LEAVE")]
    RoomLeave,
    #[serde(rename = "ROOM_STATE")]
    RoomState,
    #[serde(rename = "PARTICIPANT_JOINED")]
    ParticipantJoined,
    #[serde(rename = "PARTICIPANT_LEFT")]
    ParticipantLeft,
    #[serde(rename = "HOST_DISCONNECTED")]
    HostDisconnected,
    #[serde(rename = "HOST_RECONNECTED")]
    HostReconnected,
    #[serde(rename = "HOST_MIGRATED")]
    HostMigrated,
    #[serde(rename = "ROOM_CLOSED")]
    RoomClosed,
    #[serde(rename = "ROOM_ERROR")]
    RoomError,
    #[serde(rename = "PRESENCE")]
    Presence,
    /// Forward-compat: an unknown type string. The v1 server
    /// rejects anything in this variant; future versions may
    /// learn to handle some of them.
    #[serde(untagged)]
    #[ts(skip)]
    Other(String),
}

impl MessageKind {
    /// The wire string for this type tag.
    pub fn as_str(&self) -> &str {
        match self {
            MessageKind::Hello => "HELLO",
            MessageKind::Welcome => "WELCOME",
            MessageKind::Challenge => "CHALLENGE",
            MessageKind::Auth => "AUTH",
            MessageKind::AuthOk => "AUTH_OK",
            MessageKind::AuthFail => "AUTH_FAIL",
            MessageKind::RoomCreate => "ROOM_CREATE",
            MessageKind::RoomCreated => "ROOM_CREATED",
            MessageKind::RoomJoinRequest => "ROOM_JOIN_REQUEST",
            MessageKind::RoomJoined => "ROOM_JOINED",
            MessageKind::RoomLeave => "ROOM_LEAVE",
            MessageKind::RoomState => "ROOM_STATE",
            MessageKind::ParticipantJoined => "PARTICIPANT_JOINED",
            MessageKind::ParticipantLeft => "PARTICIPANT_LEFT",
            MessageKind::HostDisconnected => "HOST_DISCONNECTED",
            MessageKind::HostReconnected => "HOST_RECONNECTED",
            MessageKind::HostMigrated => "HOST_MIGRATED",
            MessageKind::RoomClosed => "ROOM_CLOSED",
            MessageKind::RoomError => "ROOM_ERROR",
            MessageKind::Presence => "PRESENCE",
            MessageKind::Other(s) => s,
        }
    }

    /// `true` if this is a room-lifecycle message (server
    /// routes it through `room_dispatch` after bearer
    /// validation).
    pub fn is_room_lifecycle(&self) -> bool {
        matches!(
            self,
            MessageKind::RoomCreate
                | MessageKind::RoomJoinRequest
                | MessageKind::RoomLeave
                | MessageKind::Presence
        )
    }
}

/// The per-envelope sender identity. The signature is over the
/// canonicalized payload bytes (§18.9). The sender is `None` for
/// handshake messages and server-originated broadcasts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct Sender {
    /// Server-assigned UUID v7 (architecture §20.4.4). The
    /// client-side `user_id` (sha256 hex) is NOT used here.
    pub user_id: Uuid,
    /// 32-byte Ed25519 public key.
    pub pubkey: Vec<u8>,
    /// 64-byte Ed25519 signature over the canonical payload
    /// (or, for AUTH, over the raw 32-byte nonce).
    pub sig: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_json_roundtrip() {
        let env = Envelope {
            v: 1,
            r#type: MessageKind::Hello,
            id: Uuid::nil(),
            room_id: None,
            sender: None,
            ts_ms: 1_700_000_000_000,
            seq: 1,
            payload: json!({"client_version": "0.0.0", "platform": "win", "device_id": "abc"}),
        };
        let s = serde_json::to_string(&env).expect("serialize");
        let back: Envelope = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, env);
    }

    #[test]
    fn unknown_type_round_trips_as_other() {
        let s = r#"{"v":1,"type":"FOOBAR","id":"00000000-0000-0000-0000-000000000000","room_id":null,"sender":null,"ts_ms":1,"seq":1,"payload":{}}"#;
        let env: Envelope = serde_json::from_str(s).expect("decode unknown");
        assert!(matches!(env.r#type, MessageKind::Other(ref t) if t == "FOOBAR"));
    }
}
