//! v1 capability gate for the four initial room envelopes.
//!
//! P2-T07 adds the chokepoint; the gate is permissive in
//! v1 (all four commands pass for any joined participant).
//! Future P3+ / P6 work will add the actual denial cases
//! (e.g. host-only, co-host, viewer-choke). The gate is
//! called by [`super::dispatch::dispatch_room_message`]
//! BEFORE the per-type handler so the same plumbing can
//! carry both v1's permissive rules and P3+'s
//! per-capability rules without re-plumbing the dispatch
//! layer.

#![forbid(unsafe_code)]

use uuid::Uuid;

use super::registry::RoomRegistry;

/// The closed set of reasons a capability gate may
/// refuse a command. v1 only emits `NotMember` (for
/// PRESENCE when the user is not in any room). Future
/// variants (`NotHost`, `NotCoHost`, etc.) are added by
/// P3-P6; P2-T07 only delivers the chokepoint.
#[derive(Debug, thiserror::Error)]
pub enum CapsError {
    #[error("not a member of the room")]
    NotMember,
}

/// The four initial room envelopes the capability gate
/// guards. New commands (PLAY, PAUSE, DRAW, CHAT, etc.)
/// are added here by P4/P5/P6 as they land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    RoomCreate,
    RoomJoinRequest,
    RoomLeave,
    Presence,
}

/// Authoritative capability check for the v1 initial
/// command set. v1 is permissive: every command passes
/// for any joined participant; PRESENCE additionally
/// requires the user to be in a room.
///
/// Callers (the room dispatcher) MUST treat `Ok(())` as
/// "proceed with the per-type handler" and `Err(CapsError)`
/// as "do not call the handler; the user's next
/// authoritative call (e.g. ROOM_LEAVE) will surface the
/// real error."
pub async fn check_capability(
    registry: &RoomRegistry,
    user_id: Uuid,
    command: Command,
) -> Result<(), CapsError> {
    match command {
        // Anyone authed may create a room; the registry
        // will assign them as host with the full cap set.
        Command::RoomCreate => Ok(()),
        // Anyone authed may request to join a room; the
        // registry's `join` handles "already joined",
        // "room full", etc.
        Command::RoomJoinRequest => Ok(()),
        // ROOM_LEAVE requires being a member of a room.
        // The registry's `leave` returns `NotJoined` for
        // non-members, so we trust that path.
        Command::RoomLeave => Ok(()),
        // PRESENCE requires being a member of a room.
        Command::Presence => {
            if registry.get_user_room(user_id).await.is_none() {
                Err(CapsError::NotMember)
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::registry::RoomRegistryConfig;
    use crate::time::MockClock;

    fn fresh_registry() -> (RoomRegistry, MockClock) {
        let clock = MockClock::new(1_000_000);
        let cfg = RoomRegistryConfig {
            max_participants: 8,
            host_disconnect_grace_ms: 200,
            participant_stale_after_ms: 300_000,
        };
        (RoomRegistry::new(cfg), clock)
    }

    fn uid(i: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[0] = i;
        b[15] = i;
        Uuid::from_bytes(b)
    }

    #[tokio::test]
    async fn room_create_is_allowed_for_anyone() {
        let (reg, _clock) = fresh_registry();
        assert!(check_capability(&reg, uid(1), Command::RoomCreate)
            .await
            .is_ok());
        assert!(check_capability(&reg, uid(99), Command::RoomCreate)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn room_join_request_is_allowed_for_anyone() {
        let (reg, _clock) = fresh_registry();
        assert!(check_capability(&reg, uid(1), Command::RoomJoinRequest)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn room_leave_is_allowed_in_v1() {
        // v1 lets ROOM_LEAVE through unconditionally; the
        // registry's `leave` returns NotJoined for
        // non-members, which the per-type handler
        // translates into ROOM_ERROR(NotJoined).
        let (reg, _clock) = fresh_registry();
        assert!(check_capability(&reg, uid(1), Command::RoomLeave)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn presence_is_denied_when_user_is_not_in_a_room() {
        let (reg, _clock) = fresh_registry();
        // No room membership -> PRESENCE is rejected.
        let err = check_capability(&reg, uid(1), Command::Presence)
            .await
            .expect_err("expected NotMember");
        assert!(matches!(err, CapsError::NotMember));
    }
}
