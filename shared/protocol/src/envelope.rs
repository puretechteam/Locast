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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
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
    // ----- P2-T07: per-connection rate-limit envelope -----
    // Server -> caller only. Emitted when a post-handshake
    // connection trips the per-conn token bucket (msg/s or
    // bytes/s). The handshake path continues to use
    // AUTH_FAIL(Rate) per §20.8; RATE_LIMIT is reserved for
    // the post-handshake case where the caller has a bearer
    // and needs a structured retry hint.
    #[serde(rename = "RATE_LIMIT")]
    RateLimit,
    // ----- P3-T03: signed manifest publication -----
    // MANIFEST_PUBLISH is host -> server; MANIFEST_PUBLISHED is
    // server -> all participants. The server is the relay; it
    // does NOT validate beyond the bearer + the
    // `PublishManifest` capability check (host-only at the time
    // of publish).
    #[serde(rename = "MANIFEST_PUBLISH")]
    ManifestPublish,
    #[serde(rename = "MANIFEST_PUBLISHED")]
    ManifestPublished,
    // MANIFEST_REQUEST is any room member -> server; the
    // server replies with MANIFEST_RESPONSE carrying the
    // room's currently-authoritative manifest. This is the
    // late-joiner catch-up path (architecture §18.4.3).
    #[serde(rename = "MANIFEST_REQUEST")]
    ManifestRequest,
    #[serde(rename = "MANIFEST_RESPONSE")]
    ManifestResponse,
    // ----- P3-T05: WebRTC SDP/ICE relay -----
    // SIGNAL envelopes carry per-target SDP (offer/answer) or ICE
    // candidate material. The server is a pure relay: it does not
    // inspect or rewrite SDP or candidate bodies; it only enforces
    // the per-envelope Ed25519 signature over the canonicalized
    // payload, sender/recipient room-membership, and a 64 KiB
    // application-layer size cap (docs/ARCHITECTURE.md §18.5.1).
    #[serde(rename = "SIGNAL")]
    Signal,
    // ----- P4-T01: playback commands (PLAY / PAUSE / SEEK) -----
    // Single envelope kind carrying a discriminated `PlaybackAction`
    // payload. The server is the authority: it validates host
    // authority, room lifecycle, and per-sender monotonic
    // sequencing, assigns a per-room `server_seq` and a server
    // wall-clock `server_ts_ms`, then rebroadcasts the accepted
    // command to every room participant. Non-host senders, stale
    // commands, and out-of-state commands get a single-caller
    // `ROOM_ERROR` reply and are NOT broadcast.
    #[serde(rename = "PLAYBACK_CMD")]
    PlaybackCmd,
    // ----- P4-T03: non-authoritative 1 Hz position telemetry -----
    // POSITION_REPORT is sent by every participant (host or
    // viewer) at ~1 Hz while connected. It is the local
    // `<video>` element's current state. The server is a
    // pure relay: it does NOT validate playback timing, it
    // does NOT stamp `server_ts` on the rebroadcast
    // (architecture §12.8 + roadmap P4-T03: "server
    // forwards without modification"), and the report does
    // NOT alter `server_seq`, `last_position_ms`, or the
    // room lifecycle. The WS forwarder's originator filter
    // suppresses the report for the sender so a client does
    // not see its own position echoed back.
    #[serde(rename = "POSITION_REPORT")]
    PositionReport,
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
            MessageKind::RateLimit => "RATE_LIMIT",
            MessageKind::ManifestPublish => "MANIFEST_PUBLISH",
            MessageKind::ManifestPublished => "MANIFEST_PUBLISHED",
            MessageKind::ManifestRequest => "MANIFEST_REQUEST",
            MessageKind::ManifestResponse => "MANIFEST_RESPONSE",
            MessageKind::Signal => "SIGNAL",
            MessageKind::PlaybackCmd => "PLAYBACK_CMD",
            MessageKind::PositionReport => "POSITION_REPORT",
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

    /// `true` if this is a manifest-lifecycle message (host
    /// publishes; server rebroadcasts). The WS layer routes
    /// these to the room dispatcher the same way
    /// `is_room_lifecycle` messages are routed, but kept
    /// separately so a future per-type handler can branch
    /// without re-plumbing the `is_room_lifecycle` check.
    pub fn is_manifest_lifecycle(&self) -> bool {
        matches!(
            self,
            MessageKind::ManifestPublish
                | MessageKind::ManifestPublished
                | MessageKind::ManifestRequest
                | MessageKind::ManifestResponse
        )
    }

    /// `true` for the per-target WebRTC SDP/ICE relay envelope.
    /// Routed by the WS layer to the room dispatcher the same way
    /// `is_room_lifecycle` / `is_manifest_lifecycle` are routed,
    /// but kept separate so the routing predicate does not have to
    /// grow a long match arm.
    pub fn is_signal_lifecycle(&self) -> bool {
        matches!(self, MessageKind::Signal)
    }

    /// `true` for host-only PLAYBACK_CMD envelopes (PLAY / PAUSE /
    /// SEEK). Routed through the same WS-layer entry point as
    /// `is_signal_lifecycle`, but kept separate so the host-only
    // capability check + per-sender monotonic-seq check stay
    // localized to the playback dispatcher (P4-T01).
    pub fn is_playback_lifecycle(&self) -> bool {
        matches!(self, MessageKind::PlaybackCmd)
    }

    /// `true` for the 1 Hz non-authoritative POSITION_REPORT
    /// envelopes (P4-T03). Routed through the same WS-layer
    /// entry point as the lifecycle predicates so the
    /// per-connection bearer check + the per-room membership
    /// gate both apply; the per-type handler then forwards
    /// the payload verbatim without mutation.
    pub fn is_position_report(&self) -> bool {
        matches!(self, MessageKind::PositionReport)
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
