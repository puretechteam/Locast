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
    /// P3-T03: the action is host-only and the caller is
    /// not the current host. The wire-level equivalent is
    /// `RoomErrorCode::NotHost`.
    #[error("not the room host")]
    NotHost,
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
    /// P3-T03: the host-only `MANIFEST_PUBLISH` envelope.
    /// The capability check is two-fold:
    ///
    /// 1. The caller must be a participant of the room
    ///    named in `envelope.room_id` (consistent with
    ///    the other room-lifecycle commands).
    /// 2. The caller must be the room's CURRENT host
    ///    (their `ParticipantRecord::is_host` is true).
    ///    The host-grant on `cap_set` is automatic for
    ///    `RoomCreate` (the full bitfield including
    ///    `cap::PUBLISH_MANIFEST` is assigned at create
    ///    time) and is re-granted on host election; the
    ///    explicit host check is what the spec requires
    ///    so a demoted former host cannot publish.
    PublishManifest,
    /// P3-T04 prerequisite 3: the room-scoped
    /// `MANIFEST_REQUEST` envelope. The capability check
    /// is "caller is currently a participant of the room
    /// named in `envelope.room_id`". A viewer may fetch
    /// only the manifest of the room they are currently
    /// in; a non-member or a member of a different room
    /// is denied with `CapsError::NotMember`. There is
    /// no host-only check: any room member may fetch
    /// the room's current manifest.
    FetchManifest,
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
        // P3-T03: PublishManifest requires the caller to
        // be the current host. The check is "is the user
        // a participant AND marked as host in the CURRENT
        // room state?" The `get_user_room` + per-room
        // participant walk covers both branches; if the
        // user is not in any room, `is_host` defaults to
        // false. We do NOT trust `cap_set` for the
        // host-only check because the bitfield is
        // historical (it may include PUBLISH_MANIFEST
        // from a previous host election that was later
        // undone).
        Command::PublishManifest => {
            if let Some(rid) = registry.get_user_room(user_id).await {
                if registry.is_room_host(rid, user_id).await {
                    Ok(())
                } else {
                    Err(CapsError::NotHost)
                }
            } else {
                Err(CapsError::NotMember)
            }
        }
        // P3-T04 prerequisite 3: FetchManifest. The caller
        // must be a member of SOME room. The per-type
        // handler also checks that the caller's room is
        // the SAME as `envelope.room_id` (the caller's
        // membership in room X does not grant them the
        // right to fetch room Y's manifest).
        Command::FetchManifest => {
            if registry.get_user_room(user_id).await.is_some() {
                Ok(())
            } else {
                Err(CapsError::NotMember)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::registry::RoomRegistryConfig;
    use crate::time::{Clock, MockClock};

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

    #[tokio::test]
    async fn publish_manifest_is_denied_when_user_is_not_in_any_room() {
        let (reg, _clock) = fresh_registry();
        let err = check_capability(&reg, uid(1), Command::PublishManifest)
            .await
            .expect_err("expected NotMember");
        assert!(matches!(err, CapsError::NotMember));
    }

    #[tokio::test]
    async fn publish_manifest_is_denied_when_user_is_not_host() {
        // Set up: uid(1) creates a room, uid(2) joins it as
        // a viewer. PublishManifest must succeed for the
        // host and fail with NotHost for the viewer.
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room, _self_view) = reg
            .create(&s, "T".into(), uid(1), [1u8; 32], true, clock.now_ms())
            .await
            .expect("create room");
        // uid(2) joins as a viewer (not host).
        let (_joined, _evt) = reg
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
        let host_ok = check_capability(&reg, uid(1), Command::PublishManifest).await;
        assert!(host_ok.is_ok(), "host should be allowed to publish");
        let viewer_err = check_capability(&reg, uid(2), Command::PublishManifest)
            .await
            .expect_err("viewer must be denied");
        assert!(matches!(viewer_err, CapsError::NotHost));
    }

    /// P3-T04 prerequisite 3: a viewer can fetch the
    /// manifest (it is not host-only). A non-member
    /// cannot.
    #[tokio::test]
    async fn fetch_manifest_is_allowed_for_any_member_but_denied_for_non_member() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room, _self_view) = reg
            .create(&s, "T".into(), uid(1), [1u8; 32], true, clock.now_ms())
            .await
            .expect("create");
        let (_joined, _evt) = reg
            .join(
                &s,
                &room.code,
                uid(2),
                [2u8; 32],
                "viewer".into(),
                clock.now_ms(),
            )
            .await
            .expect("join");
        // Host and viewer can both fetch.
        assert!(check_capability(&reg, uid(1), Command::FetchManifest)
            .await
            .is_ok());
        assert!(check_capability(&reg, uid(2), Command::FetchManifest)
            .await
            .is_ok());
        // Non-member is denied.
        let err = check_capability(&reg, uid(3), Command::FetchManifest)
            .await
            .expect_err("non-member must be denied");
        assert!(matches!(err, CapsError::NotMember));
    }
}
