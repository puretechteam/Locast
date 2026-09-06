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
//!
//! P6-T01 introduces the [`Scope`] / [`Action`] / [`can()`]
//! API as the canonical capability check. The legacy
//! [`Command`] enum and [`check_capability()`] are retained
//! for the dispatch layer mapping from `MessageKind`;
//! `check_capability()` delegates to `can()` internally.

#![forbid(unsafe_code)]

use uuid::Uuid;

use super::registry::RoomRegistry;

pub use locast_protocol::room::cap as cap_bits;

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

/// The set of capability scopes. Each scope groups related
/// actions that share a common capability bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Playback,
    Drawing,
    Chat,
    Room,
    Manifest,
}

/// Actions within a capability scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    IssuePlaybackCommand,
    DrawBegin,
    DrawPoint,
    DrawEnd,
    UndoStroke,
    ClearAll,
    SendChat,
    ManageRoom,
    Kick,
    PublishManifest,
    Invite,
}

impl Scope {
    pub fn action_bit(self, action: Action) -> u32 {
        match (self, action) {
            (Scope::Playback, Action::IssuePlaybackCommand) => cap_bits::PLAYBACK_CONTROL,
            (Scope::Drawing, Action::DrawBegin) => cap_bits::DRAW,
            (Scope::Drawing, Action::DrawPoint) => cap_bits::DRAW,
            (Scope::Drawing, Action::DrawEnd) => cap_bits::DRAW,
            (Scope::Drawing, Action::UndoStroke) => cap_bits::DRAW,
            (Scope::Drawing, Action::ClearAll) => cap_bits::DRAW,
            (Scope::Chat, Action::SendChat) => cap_bits::CHAT,
            (Scope::Room, Action::ManageRoom) => cap_bits::MANAGE_ROOM,
            (Scope::Room, Action::Kick) => cap_bits::KICK,
            (Scope::Manifest, Action::PublishManifest) => cap_bits::PUBLISH_MANIFEST,
            (Scope::Manifest, Action::Invite) => cap_bits::INVITE,
            _ => 0,
        }
    }
}

/// Authoritative capability check using the (scope, action) API.
/// Returns `true` if the given `user_id` is permitted to perform
/// `action` in `scope` within the room identified by `room_id`.
///
/// The host always returns `true` for every action. Non-host
/// members are checked against their `cap_set` bitfield.
///
/// # Arguments
/// * `registry` - The room registry
/// * `user_id` - The user attempting the action
/// * `room_id` - The room in which the action is attempted
/// * `scope` - The capability scope
/// * `action` - The specific action within the scope
pub async fn can(
    registry: &RoomRegistry,
    user_id: Uuid,
    room_id: Uuid,
    scope: Scope,
    action: Action,
) -> bool {
    let Some(handle) = registry.get_by_id(room_id).await else {
        return false;
    };
    let state = handle.read().await;
    if let Some(participant) = state.participants.iter().find(|p| p.user_id == user_id) {
        if participant.is_host {
            return true;
        }
        let bit = scope.action_bit(action);
        participant.cap_set & bit != 0
    } else {
        false
    }
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
    /// P3-T05: the per-target WebRTC `SIGNAL` envelope
    /// (SDP offer/answer, ICE candidates). The capability
    /// check is "caller is a member of SOME room". The
    /// per-type handler additionally checks that
    /// `envelope.room_id` matches the caller's current
    /// room AND that `to_user_id` is a member of the
    /// same room (defense against cross-room relay and
    /// stale peer sessions). Non-members are denied with
    /// `CapsError::NotMember`.
    Signal,
    /// P4-T01: host-only PLAYBACK_CMD envelope (PLAY / PAUSE /
    /// SEEK per docs/ARCHITECTURE.md §13). The capability
    /// check is identical in shape to `PublishManifest`: the
    /// caller must be a participant of the room named in
    /// `envelope.room_id` AND must be marked as host in the
    /// CURRENT room state. We do not trust `cap_set` for the
    /// host check (the bitfield is historical). The per-type
    /// handler additionally validates the room lifecycle
    /// state (PLAY requires Ready/Paused, PAUSE requires
    /// Playing, SEEK requires Playing/Paused) and the
    /// per-sender `monotonic_seq` (must equal
    /// `last_acked_seq[sender] + 1`).
    PlaybackControl,
    /// P4-T03: non-authoritative POSITION_REPORT envelope
    /// (1 Hz local-playback snapshot per
    /// docs/ARCHITECTURE.md §13.1). The capability check is
    /// "caller is a member of some room" (mirrors
    /// `FetchManifest` / `Signal` shape). The per-type
    /// handler additionally checks that `envelope.room_id`
    /// matches the caller's current room so cross-room
    /// injection is denied. Non-members are denied with
    /// `CapsError::NotMember`. There is no host-only check:
    /// every participant (host or viewer) reports its own
    /// local state at 1 Hz.
    PositionReport,
    /// P5-T02: the per-stroke DRAWING protocol (DRAW_BEGIN
    /// / DRAW_POINT / DRAW_END). The capability check is
    /// "caller is a member of the room named in
    /// `envelope.room_id`" (mirrors `Signal` /
    /// `PositionReport`). The per-type handler
    /// additionally verifies the DRAW_BEGIN signature
    /// and binds the stroke id to the bearer so cross-
    /// sender injections are denied at the per-stroke
    /// level (any stroke begin a non-member could not
    /// have started is rejected).
    Draw,
}

impl Command {
    fn to_scope_action(self) -> Option<(Scope, Action)> {
        match self {
            Command::PlaybackControl => Some((Scope::Playback, Action::IssuePlaybackCommand)),
            Command::Draw => Some((Scope::Drawing, Action::DrawBegin)),
            Command::PublishManifest => Some((Scope::Manifest, Action::PublishManifest)),
            _ => None,
        }
    }
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
    if let Some((scope, action)) = command.to_scope_action() {
        if let Some(rid) = registry.get_user_room(user_id).await {
            if !can(registry, user_id, rid, scope, action).await {
                return Err(CapsError::NotHost);
            }
        }
    }
    match command {
        Command::RoomCreate => Ok(()),
        Command::RoomJoinRequest => Ok(()),
        Command::RoomLeave => Ok(()),
        Command::Presence => {
            if registry.get_user_room(user_id).await.is_none() {
                Err(CapsError::NotMember)
            } else {
                Ok(())
            }
        }
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
        Command::FetchManifest => {
            if registry.get_user_room(user_id).await.is_some() {
                Ok(())
            } else {
                Err(CapsError::NotMember)
            }
        }
        Command::Signal => {
            if registry.get_user_room(user_id).await.is_some() {
                Ok(())
            } else {
                Err(CapsError::NotMember)
            }
        }
        Command::PlaybackControl => {
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
        Command::PositionReport => {
            if registry.get_user_room(user_id).await.is_some() {
                Ok(())
            } else {
                Err(CapsError::NotMember)
            }
        }
        Command::Draw => {
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
    ) -> (Uuid, Uuid) {
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
        (room.id, uid(1))
    }

    // --- can() tests ---

    #[tokio::test]
    async fn can_returns_false_for_non_member() {
        let (reg, clock) = fresh_registry();
        let (room_id, _) = setup_room_with_host_and_viewer(&reg, &clock).await;
        assert!(
            !can(
                &reg,
                uid(99),
                room_id,
                Scope::Playback,
                Action::IssuePlaybackCommand
            )
            .await
        );
        assert!(!can(&reg, uid(99), room_id, Scope::Drawing, Action::DrawBegin).await);
        assert!(!can(&reg, uid(99), room_id, Scope::Chat, Action::SendChat).await);
        assert!(!can(&reg, uid(99), room_id, Scope::Room, Action::ManageRoom).await);
        assert!(
            !can(
                &reg,
                uid(99),
                room_id,
                Scope::Manifest,
                Action::PublishManifest
            )
            .await
        );
    }

    #[tokio::test]
    async fn can_returns_false_for_wrong_room() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room1, _self_view) = reg
            .create(&s, "T".into(), uid(1), [1u8; 32], true, clock.now_ms())
            .await
            .expect("create room1");
        let (room2, _self_view) = reg
            .create(&s, "T".into(), uid(3), [3u8; 32], true, clock.now_ms())
            .await
            .expect("create room2");
        reg.join(
            &s,
            &room2.code,
            uid(2),
            [2u8; 32],
            "viewer".into(),
            clock.now_ms(),
        )
        .await
        .expect("viewer joins room2");
        assert!(
            !can(
                &reg,
                uid(2),
                room1.id,
                Scope::Playback,
                Action::IssuePlaybackCommand
            )
            .await
        );
    }

    #[tokio::test]
    async fn can_host_has_all_capabilities() {
        let (reg, clock) = fresh_registry();
        let (room_id, host_uid) = setup_room_with_host_and_viewer(&reg, &clock).await;
        assert!(
            can(
                &reg,
                host_uid,
                room_id,
                Scope::Playback,
                Action::IssuePlaybackCommand
            )
            .await
        );
        assert!(can(&reg, host_uid, room_id, Scope::Drawing, Action::DrawBegin).await);
        assert!(can(&reg, host_uid, room_id, Scope::Drawing, Action::DrawPoint).await);
        assert!(can(&reg, host_uid, room_id, Scope::Drawing, Action::DrawEnd).await);
        assert!(can(&reg, host_uid, room_id, Scope::Drawing, Action::UndoStroke).await);
        assert!(can(&reg, host_uid, room_id, Scope::Drawing, Action::ClearAll).await);
        assert!(can(&reg, host_uid, room_id, Scope::Chat, Action::SendChat).await);
        assert!(can(&reg, host_uid, room_id, Scope::Room, Action::ManageRoom).await);
        assert!(can(&reg, host_uid, room_id, Scope::Room, Action::Kick).await);
        assert!(
            can(
                &reg,
                host_uid,
                room_id,
                Scope::Manifest,
                Action::PublishManifest
            )
            .await
        );
        assert!(can(&reg, host_uid, room_id, Scope::Manifest, Action::Invite).await);
    }

    #[tokio::test]
    async fn can_viewer_with_default_caps() {
        let (reg, clock) = fresh_registry();
        let (room_id, _) = setup_room_with_host_and_viewer(&reg, &clock).await;
        let viewer = uid(2);
        assert!(
            !can(
                &reg,
                viewer,
                room_id,
                Scope::Playback,
                Action::IssuePlaybackCommand
            )
            .await
        );
        assert!(!can(&reg, viewer, room_id, Scope::Drawing, Action::DrawBegin).await);
        assert!(!can(&reg, viewer, room_id, Scope::Drawing, Action::DrawPoint).await);
        assert!(!can(&reg, viewer, room_id, Scope::Drawing, Action::DrawEnd).await);
        assert!(!can(&reg, viewer, room_id, Scope::Drawing, Action::UndoStroke).await);
        assert!(!can(&reg, viewer, room_id, Scope::Drawing, Action::ClearAll).await);
        assert!(can(&reg, viewer, room_id, Scope::Chat, Action::SendChat).await);
        assert!(!can(&reg, viewer, room_id, Scope::Room, Action::ManageRoom).await);
        assert!(!can(&reg, viewer, room_id, Scope::Room, Action::Kick).await);
        assert!(
            !can(
                &reg,
                viewer,
                room_id,
                Scope::Manifest,
                Action::PublishManifest
            )
            .await
        );
        assert!(!can(&reg, viewer, room_id, Scope::Manifest, Action::Invite).await);
    }

    // --- check_capability() legacy tests ---

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
        let (reg, _clock) = fresh_registry();
        assert!(check_capability(&reg, uid(1), Command::RoomLeave)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn presence_is_denied_when_user_is_not_in_a_room() {
        let (reg, _clock) = fresh_registry();
        let err = check_capability(&reg, uid(1), Command::Presence)
            .await
            .expect_err("expected NotMember");
        assert!(matches!(err, CapsError::NotMember));
    }

    #[tokio::test]
    async fn signal_is_denied_when_user_is_not_in_any_room() {
        let (reg, _clock) = fresh_registry();
        let err = check_capability(&reg, uid(1), Command::Signal)
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
        let (reg, clock) = fresh_registry();
        let (room_id, _) = setup_room_with_host_and_viewer(&reg, &clock).await;
        let host_ok = check_capability(&reg, uid(1), Command::PublishManifest).await;
        assert!(host_ok.is_ok(), "host should be allowed to publish");
        let viewer_err = check_capability(&reg, uid(2), Command::PublishManifest)
            .await
            .expect_err("viewer must be denied");
        assert!(matches!(viewer_err, CapsError::NotHost));
        let _ = room_id;
    }

    #[tokio::test]
    async fn fetch_manifest_is_allowed_for_any_member_but_denied_for_non_member() {
        let (reg, clock) = fresh_registry();
        let (room_id, _) = setup_room_with_host_and_viewer(&reg, &clock).await;
        assert!(check_capability(&reg, uid(1), Command::FetchManifest)
            .await
            .is_ok());
        assert!(check_capability(&reg, uid(2), Command::FetchManifest)
            .await
            .is_ok());
        let err = check_capability(&reg, uid(3), Command::FetchManifest)
            .await
            .expect_err("non-member must be denied");
        assert!(matches!(err, CapsError::NotMember));
        let _ = room_id;
    }

    #[tokio::test]
    async fn playback_control_is_denied_when_user_is_not_in_any_room() {
        let (reg, _clock) = fresh_registry();
        let err = check_capability(&reg, uid(1), Command::PlaybackControl)
            .await
            .expect_err("expected NotMember");
        assert!(matches!(err, CapsError::NotMember));
    }

    #[tokio::test]
    async fn playback_control_is_denied_when_user_is_not_host() {
        let (reg, clock) = fresh_registry();
        let (room_id, _) = setup_room_with_host_and_viewer(&reg, &clock).await;
        let host_ok = check_capability(&reg, uid(1), Command::PlaybackControl).await;
        assert!(
            host_ok.is_ok(),
            "host should be allowed to playback-control"
        );
        let viewer_err = check_capability(&reg, uid(2), Command::PlaybackControl)
            .await
            .expect_err("viewer must be denied");
        assert!(matches!(viewer_err, CapsError::NotHost));
        let _ = room_id;
    }
}
