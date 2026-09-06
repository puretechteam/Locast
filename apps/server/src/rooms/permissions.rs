//! P6-T02: server-side handler for the PERMISSION_SET envelope.
//!
//! Wire shape: `shared/protocol/src/envelope.rs` defines
//! `MessageKind::PermissionSet` and `MessageKind::CapabilityUpdate`;
//! `shared/protocol/src/room.rs` defines the two payload structs.
//!
//! Authorization flow:
//!
//! 1. The dispatcher calls `check_capability(..., Command::PermissionSet)`
//!    which verifies the caller is a joined participant AND is the room host.
//! 2. This handler then validates:
//!    - Target is a current room participant.
//!    - Caller is not targeting themselves.
//! 3. The server computes `new_cap_set = (old & ~remove) | add` and
//!    persists it via `RoomStore::update_participant_cap_set`.
//! 4. A `RoomEvent::CapabilityUpdated` is broadcast to all participants.

#![forbid(unsafe_code)]

use locast_protocol::envelope::Envelope;
use locast_protocol::room::PermissionSetPayload;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use super::error::RoomError;
use super::registry::{CapabilityUpdated, RoomEvent, RoomRegistry};
use super::store::RoomStore;

fn decode_payload<T: DeserializeOwned>(value: &serde_json::Value) -> Result<T, RoomError> {
    serde_json::from_value(value.clone())
        .map_err(|e| RoomError::Internal(format!("permission_set payload: {}", e)))
}

/// Handle a PERMISSION_SET envelope from the host.
///
/// The dispatcher has already verified the caller is the room host
/// via `check_capability(..., Command::PermissionSet)`. This handler
/// applies the grant/revoke and broadcasts the update.
pub async fn handle_permission_set(
    envelope: Envelope,
    registry: &RoomRegistry,
    store: &dyn RoomStore,
    caller_user_id: Uuid,
    now_ms: i64,
) -> Result<Vec<RoomEvent>, RoomError> {
    let room_id = envelope.room_id.ok_or_else(|| {
        RoomError::Internal("PERMISSION_SET missing room_id".into())
    })?;

    let payload: PermissionSetPayload = decode_payload(&envelope.payload)?;

    handle_permission_set_payload(room_id, payload, registry, store, caller_user_id, now_ms)
        .await
}

async fn handle_permission_set_payload(
    room_id: Uuid,
    payload: PermissionSetPayload,
    registry: &RoomRegistry,
    store: &dyn RoomStore,
    caller_user_id: Uuid,
    now_ms: i64,
) -> Result<Vec<RoomEvent>, RoomError> {
    let target_user_id = payload.target_user_id;

    if target_user_id == caller_user_id {
        return Err(RoomError::Internal(
            "PERMISSION_SET: cannot modify own capabilities".into(),
        ));
    }

    let handle = registry
        .get_by_id(room_id)
        .await
        .ok_or(RoomError::RoomNotFound)?;

    let new_cap_set = {
        let state = handle.read().await;
        let participant = state
            .participants
            .iter()
            .find(|p| p.user_id == target_user_id)
            .ok_or_else(|| {
                RoomError::Internal("PERMISSION_SET: target is not a room participant".into())
            })?;

        (participant.cap_set & !payload.remove_cap_set) | payload.add_cap_set
    };

    store
        .update_participant_cap_set(room_id, target_user_id, new_cap_set)
        .await
        .map_err(RoomError::Internal)?;

    registry
        .update_participant_cap_set(room_id, target_user_id, new_cap_set, now_ms)
        .await?;

    tracing::info!(
        host_id = %caller_user_id,
        target_id = %target_user_id,
        new_cap_set = %new_cap_set,
        "capability updated"
    );

    Ok(vec![RoomEvent::CapabilityUpdated(
        CapabilityUpdated {
            target_user_id,
            cap_set: new_cap_set,
        },
    )])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::registry::RoomRegistryConfig;
    use crate::time::{Clock, MockClock};
    use locast_protocol::envelope::Envelope;
    use locast_protocol::room::cap;
    use locast_protocol::room::PermissionSetPayload;

    fn fresh_registry() -> (RoomRegistry, MockClock) {
        let clock = MockClock::new(1_000_000);
        let cfg = RoomRegistryConfig {
            max_participants: 8,
            host_disconnect_grace_ms: 200,
            participant_stale_after_ms: 300_000,
            participant_disconnect_after_ms: 15_000,
        };
        (RoomRegistry::new(cfg), clock)
    }

    fn uid(i: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[0] = i;
        b[15] = i;
        Uuid::from_bytes(b)
    }

    async fn setup_room_with_host_and_viewer(
        reg: &RoomRegistry,
        clock: &MockClock,
    ) -> (Uuid, Uuid, Uuid) {
        use super::super::store::NoopRoomStore;
        let s = NoopRoomStore;
        let (room, _self_view) = reg
            .create(&s, "T".into(), uid(1), [1u8; 32], true, clock.now_ms())
            .await
            .expect("create room");
        reg.join(
            &s,
            &room.code,
            uid(2),
            [2u8; 32],
            "viewer".into(),
            clock.now_ms(),
        )
        .await
        .expect("viewer joins");
        (room.id, uid(1), uid(2))
    }

    fn envelope(room_id: Uuid, payload: PermissionSetPayload) -> Envelope {
        Envelope {
            v: 1,
            r#type: locast_protocol::envelope::MessageKind::PermissionSet,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: None,
            ts_ms: 1_000_000,
            seq: 1,
            payload: serde_json::to_value(payload).unwrap(),
        }
    }

    #[tokio::test]
    async fn host_grants_draw_to_viewer() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, host_id, viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let payload = PermissionSetPayload {
            target_user_id: viewer_id,
            add_cap_set: cap::DRAW,
            remove_cap_set: 0,
        };
        let env = envelope(room_id, payload);

        let events = handle_permission_set(env, &reg, &s, host_id, clock.now_ms())
            .await
            .expect("handle_permission_set should succeed");

        let handle = reg.get_by_id(room_id).await.expect("room exists");
        let state = handle.read().await;
        let viewer = state
            .participants
            .iter()
            .find(|p| p.user_id == viewer_id)
            .expect("viewer should be in participants");
        assert_eq!(
            viewer.cap_set, cap::CHAT | cap::DRAW,
            "viewer should have DRAW added"
        );

        assert_eq!(events.len(), 1);
        let event = &events[0];
        match event {
            RoomEvent::CapabilityUpdated(updated) => {
                assert_eq!(updated.target_user_id, viewer_id);
                assert_eq!(updated.cap_set, cap::CHAT | cap::DRAW);
            }
            _ => panic!("expected CapabilityUpdated event"),
        }
    }

    #[tokio::test]
    async fn host_revokes_chat_from_viewer() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, host_id, viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let payload = PermissionSetPayload {
            target_user_id: viewer_id,
            add_cap_set: 0,
            remove_cap_set: cap::CHAT,
        };
        let env = envelope(room_id, payload);

        let events = handle_permission_set(env, &reg, &s, host_id, clock.now_ms())
            .await
            .expect("handle_permission_set should succeed");

        let handle = reg.get_by_id(room_id).await.expect("room exists");
        let state = handle.read().await;
        let viewer = state
            .participants
            .iter()
            .find(|p| p.user_id == viewer_id)
            .expect("viewer should be in participants");
        assert_eq!(viewer.cap_set, 0, "viewer should have CHAT revoked");

        assert_eq!(events.len(), 1);
        let event = &events[0];
        match event {
            RoomEvent::CapabilityUpdated(updated) => {
                assert_eq!(updated.target_user_id, viewer_id);
                assert_eq!(updated.cap_set, 0);
            }
            _ => panic!("expected CapabilityUpdated event"),
        }
    }

    #[tokio::test]
    async fn host_targets_themselves_returns_err() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, host_id, _) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let payload = PermissionSetPayload {
            target_user_id: host_id,
            add_cap_set: cap::DRAW,
            remove_cap_set: 0,
        };
        let env = envelope(room_id, payload);

        let err = handle_permission_set(env, &reg, &s, host_id, clock.now_ms())
            .await
            .expect_err("should return error when host targets themselves");
        assert!(matches!(err, RoomError::Internal(ref msg) if msg.contains("cannot modify own")));
    }

    #[tokio::test]
    async fn host_targets_unknown_user_returns_err() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, host_id, _) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let payload = PermissionSetPayload {
            target_user_id: uid(99),
            add_cap_set: cap::DRAW,
            remove_cap_set: 0,
        };
        let env = envelope(room_id, payload);

        let err = handle_permission_set(env, &reg, &s, host_id, clock.now_ms())
            .await
            .expect_err("should return error when targeting unknown user");
        assert!(matches!(err, RoomError::Internal(ref msg) if msg.contains("not a room participant")));
    }

    #[tokio::test]
    async fn capability_updated_event_has_correct_payload() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, host_id, viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let payload = PermissionSetPayload {
            target_user_id: viewer_id,
            add_cap_set: cap::DRAW | cap::MANAGE_ROOM,
            remove_cap_set: 0,
        };
        let env = envelope(room_id, payload);

        let events = handle_permission_set(env, &reg, &s, host_id, clock.now_ms())
            .await
            .expect("handle_permission_set should succeed");

        assert_eq!(events.len(), 1);
        match &events[0] {
            RoomEvent::CapabilityUpdated(updated) => {
                assert_eq!(updated.target_user_id, viewer_id);
                assert_eq!(updated.cap_set, cap::CHAT | cap::DRAW | cap::MANAGE_ROOM);
            }
            _ => panic!("expected CapabilityUpdated event"),
        }
    }

    #[tokio::test]
    async fn new_cap_set_is_persisted() {
        let (reg, clock) = fresh_registry();
        let (room_id, host_id, viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let payload = PermissionSetPayload {
            target_user_id: viewer_id,
            add_cap_set: cap::DRAW,
            remove_cap_set: 0,
        };
        let env = envelope(room_id, payload);

        let store = super::super::store::NoopRoomStore;
        handle_permission_set(env, &reg, &store, host_id, clock.now_ms())
            .await
            .expect("handle_permission_set should succeed");

        let handle = reg.get_by_id(room_id).await.expect("room exists");
        let state = handle.read().await;
        let viewer = state
            .participants
            .iter()
            .find(|p| p.user_id == viewer_id)
            .expect("viewer should be in participants");
        assert_eq!(
            viewer.cap_set, cap::CHAT | cap::DRAW,
            "viewer cap_set should be updated in registry"
        );
    }
}
