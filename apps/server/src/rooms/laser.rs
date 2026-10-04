//! P5-T04: LASER_MOVE / LASER_OFF relay (architecture §16,
//! §18.4.8).
//!
//! The laser is transient presence, not drawing state: the server
//! is a stateless relay. The dispatcher has already checked that
//! the caller is a member of the room named in `Envelope::room_id`
//! and holds `cap::LASER` there; this module only validates the
//! payload and turns it into a `RoomEvent` the WS layer publishes
//! to that room after dispatch (no room lock is taken, and the
//! drawing sequence is not touched, so DRAW_SYNC is unaffected).
//!
//! Every failure is a silent drop: a lost laser frame is superseded
//! by the next one, and a ROOM_ERROR would end the room on the
//! client. The sender is always the authenticated connection's
//! user id; nothing in the client payload can name another user.

use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::room::LaserMovePayload;
use serde::Deserialize;
use uuid::Uuid;

use super::registry::RoomEvent;
use super::validation::validate_unit_range;

/// Turn an authorized laser envelope from `user_id` into the event
/// to relay, or `None` to drop it (no room, wrong kind, malformed
/// payload or coordinates outside `[0, 1]`).
pub fn laser_event(envelope: &Envelope, user_id: Uuid) -> Option<RoomEvent> {
    let room_id = envelope.room_id?;
    match envelope.r#type {
        MessageKind::LaserMove => {
            // Decoding into the typed payload (by reference: no copy
            // of the bearer) drops every other key (the client
            // bearer, any claimed identity).
            let payload = LaserMovePayload::deserialize(&envelope.payload).ok()?;
            if !validate_unit_range(payload.x) || !validate_unit_range(payload.y) {
                return None;
            }
            Some(RoomEvent::LaserMove {
                room_id,
                sender_id: user_id,
                payload,
            })
        }
        MessageKind::LaserOff => Some(RoomEvent::LaserOff {
            room_id,
            sender_id: user_id,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(kind: MessageKind, room_id: Option<Uuid>, payload: serde_json::Value) -> Envelope {
        Envelope {
            v: 1,
            r#type: kind,
            id: Uuid::now_v7(),
            room_id,
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload,
        }
    }

    #[test]
    fn move_is_attributed_to_the_connection_not_the_payload() {
        let room = Uuid::now_v7();
        let me = Uuid::now_v7();
        let other = Uuid::now_v7();
        let env = envelope(
            MessageKind::LaserMove,
            Some(room),
            json!({ "x": 0.5, "y": 1.0, "user_id": other, "sender_id": other, "bearer": [1, 2] }),
        );
        match laser_event(&env, me) {
            Some(RoomEvent::LaserMove {
                room_id,
                sender_id,
                payload,
            }) => {
                assert_eq!(room_id, room);
                assert_eq!(sender_id, me);
                assert_eq!(payload, LaserMovePayload { x: 0.5, y: 1.0 });
            }
            other => panic!("expected LaserMove, got {other:?}"),
        }
    }

    #[test]
    fn off_is_attributed_to_the_connection() {
        let room = Uuid::now_v7();
        let me = Uuid::now_v7();
        let env = envelope(
            MessageKind::LaserOff,
            Some(room),
            json!({ "user_id": Uuid::nil() }),
        );
        assert!(matches!(
            laser_event(&env, me),
            Some(RoomEvent::LaserOff { room_id, sender_id }) if room_id == room && sender_id == me
        ));
    }

    #[test]
    fn malformed_or_out_of_range_moves_are_dropped() {
        let room = Some(Uuid::now_v7());
        let me = Uuid::now_v7();
        for payload in [
            json!({ "x": -0.01, "y": 0.5 }),
            json!({ "x": 0.5, "y": 1.01 }),
            json!({ "x": 0.5 }),
            json!({ "x": "0.5", "y": 0.5 }),
            json!({ "x": 1e39, "y": 0.5 }),
            json!(null),
        ] {
            let env = envelope(MessageKind::LaserMove, room, payload.clone());
            assert!(laser_event(&env, me).is_none(), "{payload} must be dropped");
        }
        // No room on the envelope: nothing to relay to.
        let env = envelope(MessageKind::LaserMove, None, json!({ "x": 0.5, "y": 0.5 }));
        assert!(laser_event(&env, me).is_none());
        // Not a laser kind.
        let env = envelope(
            MessageKind::StrokePoint,
            room,
            json!({ "x": 0.5, "y": 0.5 }),
        );
        assert!(laser_event(&env, me).is_none());
    }
}
