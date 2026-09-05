//! P4-T03: server-side POSITION_REPORT relay.
//!
//! The room dispatcher's per-message handler for
//! `MessageKind::PositionReport`. The server is a pure relay
//! per the roadmap's "server forwards without modification"
//! requirement and per `docs/ARCHITECTURE.md` §12.8. This
//! module does NOT:
//!
//! - mutate `RoomState::playback` (`server_seq`,
//!   `last_position_ms`, `last_acked_seq`, lifecycle).
//! - validate the playback position against media duration
//!   or the host's `media_position_ms` (that is the host's
//!   job at MANIFEST_PUBLISH / PLAY time).
//! - stamp `server_ts_ms` on the rebroadcast payload (the
//!   roadmap explicitly says "forwards without
//!   modification").
//! - persist the report (architecture §12.3: "Not persisted
//!   in room state").
//!
//! This module DOES:
//!
//! 1. Decode the payload as a [`PositionReportPayload`].
//!    A malformed payload produces a single-caller
//!    `ROOM_ERROR(InvalidState)` (mirroring the
//!    MANIFEST_PUBLISH handler's behavior); the report is
//!    NOT relayed.
//! 2. Verify the sender is currently a participant in the
//!    room named in `envelope.room_id`. A non-member or a
//!    member of a different room gets `ROOM_ERROR(NotJoined)`
//!    and the report is NOT relayed. This is the cross-room
//!    injection defense.
//! 3. On success, build a [`RoomEvent::PositionReport`]
//!    carrying the original payload verbatim (cloned, not
//!    transformed) and the room id derived from the
//!    envelope. The dispatch layer publishes the event to
//!    the room's broadcast channel; the WS forwarder fans
//!    it out to every other participant. The originator
//!    filter suppresses the report for the sender so the
//!    client does not see its own position echoed back.
//!
//! v1 simplicity: the report is NOT relayed back to the host
//! as a "host's own position through the same path". The
//! phrase "host's UI shows the viewer's position" in the
//! roadmap is the remote telemetry surface; the host's own
//! position is shown via `usePlaybackStore.lastApplied` (the
//! server-authoritative state from PLAYBACK_CMD). The
//! reporter on the host still runs (every participant
//! reports, per architecture §12.3), but the host does not
//! consume its own forwarded report. The receiver-side
//! store keys viewer positions by `sender_id` so the
//! multi-viewer case is naturally distinguished.
//!
//! Presence/heartbeat interaction (P4-T08) is intentionally
//! out of scope here; `tick_stale_participants` already
//! drops viewers who have not refreshed `last_seen_ms` in
//! `participant_stale_after_ms`, so forwarded reports from a
//! stale-but-still-connected viewer will simply stop
//! arriving (the upstream sender's TCP/WebSocket has
//! already gone away or been cleaned up). No additional
//! TTL bookkeeping is needed at this layer.

#![forbid(unsafe_code)]

use locast_protocol::envelope::Envelope;
use locast_protocol::room::PositionReportPayload;
use uuid::Uuid;

use super::error::RoomError;
use super::registry::{RoomEvent, RoomRegistry};

/// P4-T03: the typed handler. Returns the [`RoomEvent`] the
/// dispatch layer should publish on success, or a typed
/// [`PresenceError`] on reject.
///
/// The handler is intentionally narrow: it never touches
/// `RoomState` (no lock acquisition, no state read or write).
/// The membership check is done against `RoomRegistry`'s
/// read-only snapshots (`get_user_room` and
/// `is_user_in_room`) so a flood of 1 Hz reports does not
/// contend with playback / manifest / room-lifecycle
/// mutations.
///
/// `envelope.room_id` is the room the caller claims to be
/// reporting on. The handler independently verifies this
/// matches the caller's current room via
/// `registry.get_user_room`; a mismatch is treated as
/// `NotJoined` (cross-room injection is denied, mirroring
/// MANIFEST_REQUEST's strict per-room check at
/// `apps/server/src/rooms/dispatch.rs:878`).
pub async fn handle_position_report(
    envelope: &Envelope,
    registry: &RoomRegistry,
    user_id: Uuid,
) -> Result<RoomEvent, PresenceError> {
    // 1. Decode the payload. Malformed JSON is a per-caller
    //    ROOM_ERROR(InvalidState); the report is NOT
    //    relayed (the bearer has already passed the WS
    //    layer's auth gate, so we know who the caller is).
    let payload: PositionReportPayload = serde_json::from_value(envelope.payload.clone())
        .map_err(|e| PresenceError::BadPayload(e.to_string()))?;

    // 2. Membership gate. The caller's bearer has already
    //    been validated by the WS layer; the only thing
    //    left to check is "is this user currently in the
    //    room they claim to be reporting on".
    //
    //    Two sub-checks:
    //
    //    a) The envelope MUST carry a room_id. POSITION_REPORT
    //       is a per-room envelope (it is only meaningful
    //       in the context of a room the caller is in); a
    //       missing room_id is a protocol error.
    //    b) The caller MUST currently be a participant in
    //       the named room. We compare against the
    //       registry's view of the caller's current room
    //       (NOT the envelope's room_id) so a cross-room
    //       injection (caller is in room X, sends a report
    //       claiming room Y) is denied.
    let room_id = envelope.room_id.ok_or(PresenceError::BadPayload(
        "POSITION_REPORT requires room_id".into(),
    ))?;
    let user_room = registry.get_user_room(user_id).await;
    let Some(user_room) = user_room else {
        return Err(PresenceError::NotJoined);
    };
    if user_room != room_id {
        // Cross-room injection: caller is in a different
        // room than the envelope claims. Deny.
        return Err(PresenceError::NotJoined);
    }
    if !registry.is_user_in_room(user_id, room_id).await {
        // Belt-and-suspenders: the participant may have
        // just left between the get_user_room snapshot
        // and this call. Deny.
        return Err(PresenceError::NotJoined);
    }

    // 3. Accept. Build a PositionReport RoomEvent with the
    //    ORIGINAL payload (preserving `media_position_ms`,
    //    `playing`, `client_ts_ms`) plus the verified
    //    `sender_id` (set from the validated bearer per
    //    architecture §13.1 point 2: "Not signed (server
    //    uses session token to attribute)"). The WS
    //    forwarder's originator filter (set on the
    //    BroadcastItem below) prevents the sender from
    //    seeing its own report echoed back.
    Ok(RoomEvent::PositionReport {
        room_id,
        sender_id: user_id,
        payload: PositionReportPayload {
            user_id,
            media_position_ms: payload.media_position_ms,
            playing: payload.playing,
            client_ts_ms: payload.client_ts_ms,
        },
    })
}

/// The closed set of reject reasons from
/// [`handle_position_report`]. Mapped onto the wire-level
/// `RoomError` by the dispatch layer.
#[derive(Debug, thiserror::Error)]
pub enum PresenceError {
    #[error("caller is not a participant of the named room")]
    NotJoined,
    #[error("malformed POSITION_REPORT payload: {0}")]
    BadPayload(String),
}

impl From<PresenceError> for RoomError {
    fn from(e: PresenceError) -> Self {
        match e {
            PresenceError::NotJoined => RoomError::NotJoined,
            PresenceError::BadPayload(_msg) => RoomError::InvalidState,
        }
    }
}

#[cfg(test)]
mod tests {
    //! P4-T03 focused tests for the POSITION_REPORT relay.
    //!
    //! The cap-gate + cross-room tests live in
    //! `dispatch.rs::tests` (mirroring P4-T01's split). This
    //! module covers:
    //!
    //! 1. happy path: an in-room sender's report is
    //!    accepted and produces a `RoomEvent::PositionReport`
    //!    carrying the original payload verbatim.
    //! 2. malformed payload is rejected with `BadPayload`
    //!    (no event produced).
    //! 3. payload missing `room_id` is rejected with
    //!    `BadPayload`.
    //! 4. a caller in a different room is rejected with
    //!    `NotJoined` (cross-room injection defense).
    //! 5. the room state (`server_seq`, `last_position_ms`,
    //!    lifecycle) is NOT mutated by an accepted report.
    //!
    //! These tests use the same MockClock + fresh registry
    //! pattern as `apps/server/src/rooms/playback.rs::tests`
    //! so a future refactor that consolidates the test
    //! helpers stays consistent.

    use super::*;
    use crate::time::{Clock, MockClock};
    use serde_json::json;

    fn fresh_clock() -> MockClock {
        MockClock::new(1_000_000)
    }

    fn fresh_registry() -> RoomRegistry {
        let cfg = crate::rooms::registry::RoomRegistryConfig {
            max_participants: 8,
            host_disconnect_grace_ms: 200,
            participant_stale_after_ms: 300_000,
            participant_disconnect_after_ms: 15_000,
        };
        RoomRegistry::new(cfg)
    }

    fn uid(i: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[0] = i;
        b[15] = i;
        Uuid::from_bytes(b)
    }

    async fn make_room_with_host_viewer(reg: &RoomRegistry, clock: &MockClock) -> (Uuid, Uuid) {
        let s = crate::rooms::store::NoopRoomStore;
        let host_uid = uid(1);
        let host_pk = [1u8; 32];
        let (room, _self_view) = reg
            .create(&s, "P4-T03".into(), host_uid, host_pk, true, clock.now_ms())
            .await
            .expect("create room");
        let _ = reg
            .join(
                &s,
                &room.code,
                uid(2),
                [2u8; 32],
                "viewer".into(),
                clock.now_ms(),
            )
            .await
            .expect("viewer joins");
        (room.id, host_uid)
    }

    fn envelope_for(room_id: Uuid, payload: serde_json::Value) -> Envelope {
        Envelope {
            v: 1,
            r#type: locast_protocol::envelope::MessageKind::PositionReport,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload,
        }
    }

    #[tokio::test]
    async fn in_room_sender_report_is_accepted_and_payload_is_verbatim() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, _host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        let viewer_uid = uid(2);

        let input = PositionReportPayload {
            user_id: viewer_uid,
            media_position_ms: 12_345,
            playing: true,
            client_ts_ms: 1_700_000_000_000,
        };
        let env = envelope_for(room_id, serde_json::to_value(&input).unwrap());
        let evt = handle_position_report(&env, &reg, viewer_uid)
            .await
            .expect("accept");
        match evt {
            RoomEvent::PositionReport {
                room_id: rid,
                sender_id,
                payload,
            } => {
                assert_eq!(rid, room_id, "event must carry the verified room id");
                assert_eq!(sender_id, viewer_uid, "event must carry the sender id");
                // The server stamps the sender's user_id
                // from the bearer (the wire's user_id field
                // is informational); the position +
                // playing + client_ts_ms are forwarded
                // verbatim.
                assert_eq!(payload.user_id, viewer_uid);
                assert_eq!(payload.media_position_ms, 12_345);
                assert!(payload.playing);
                assert_eq!(payload.client_ts_ms, 1_700_000_000_000);
            }
            other => panic!("expected PositionReport event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_payload_is_rejected() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, _host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        let viewer_uid = uid(2);

        let env = envelope_for(room_id, json!({"not": "a position report"}));
        let err = handle_position_report(&env, &reg, viewer_uid)
            .await
            .expect_err("expected reject");
        assert!(matches!(err, PresenceError::BadPayload(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn missing_room_id_is_rejected() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (_room_id, _host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        let viewer_uid = uid(2);

        let payload = PositionReportPayload {
            user_id: Uuid::nil(),
            media_position_ms: 0,
            playing: false,
            client_ts_ms: 0,
        };
        let mut env = envelope_for(Uuid::now_v7(), serde_json::to_value(&payload).unwrap());
        env.room_id = None;
        let err = handle_position_report(&env, &reg, viewer_uid)
            .await
            .expect_err("expected reject");
        assert!(matches!(err, PresenceError::BadPayload(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn non_member_is_rejected() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, _host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        let outsider_uid = uid(99);

        let payload = PositionReportPayload {
            user_id: outsider_uid,
            media_position_ms: 0,
            playing: false,
            client_ts_ms: 0,
        };
        let env = envelope_for(room_id, serde_json::to_value(&payload).unwrap());
        let err = handle_position_report(&env, &reg, outsider_uid)
            .await
            .expect_err("expected reject");
        assert!(matches!(err, PresenceError::NotJoined), "got {err:?}");
    }

    #[tokio::test]
    async fn accepted_report_does_not_mutate_playback_state() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        let viewer_uid = uid(2);

        // Snapshot the playback bookkeeping BEFORE the report.
        let handle = reg.get_by_id(room_id).await.expect("room");
        let (server_seq_before, last_pos_before, lifecycle_before, last_acked_before) = {
            let s = handle.read().await;
            (
                s.playback.server_seq,
                s.playback.last_position_ms,
                s.state,
                s.playback.last_acked_seq.get(&viewer_uid).copied(),
            )
        };

        let payload = PositionReportPayload {
            user_id: viewer_uid,
            media_position_ms: 999_999,
            playing: true,
            client_ts_ms: clock.now_ms(),
        };
        let env = envelope_for(room_id, serde_json::to_value(&payload).unwrap());
        let _ = handle_position_report(&env, &reg, viewer_uid)
            .await
            .expect("accept");

        // Re-snapshot AFTER and assert nothing moved.
        let (server_seq_after, last_pos_after, lifecycle_after, last_acked_after) = {
            let s = handle.read().await;
            (
                s.playback.server_seq,
                s.playback.last_position_ms,
                s.state,
                s.playback.last_acked_seq.get(&viewer_uid).copied(),
            )
        };
        assert_eq!(
            server_seq_before, server_seq_after,
            "server_seq must NOT advance on a position report"
        );
        assert_eq!(
            last_pos_before, last_pos_after,
            "last_position_ms must NOT be mutated by a position report"
        );
        assert_eq!(
            lifecycle_before, lifecycle_after,
            "room lifecycle must NOT be mutated by a position report"
        );
        assert_eq!(
            last_acked_before, last_acked_after,
            "last_acked_seq must NOT be mutated by a position report"
        );

        // The host's row was also untouched (only the viewer's
        // monotonic_seq would matter; reports carry no seq).
        let _ = host_uid;
    }
}
