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
use super::state::ParticipantRecord;

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
    Media,
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
    KeepTempFile,
    DeleteTempFile,
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
            (Scope::Media, Action::KeepTempFile) => cap_bits::MEDIA,
            (Scope::Media, Action::DeleteTempFile) => cap_bits::MEDIA,
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
    state
        .participants
        .iter()
        // The live record: a user who left and rejoined has an
        // old `Left` record ahead of the current one, and its
        // capabilities must not count.
        .find(|p| {
            p.user_id == user_id && p.status != locast_protocol::room::ParticipantStatus::Left
        })
        .is_some_and(|p| participant_can(p, scope, action))
}

/// The per-participant rule behind [`can`]: the host may do
/// everything; any other participant needs the action's
/// capability bit (granted by the host via PERMISSION_SET).
/// Exposed so handlers that already hold the room lock can
/// re-check with the same rule instead of re-locking.
pub fn participant_can(participant: &ParticipantRecord, scope: Scope, action: Action) -> bool {
    participant.is_host || participant.cap_set & scope.action_bit(action) != 0
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
    /// check is "caller is a member of the room named in
    /// `envelope.room_id`". The per-type handler
    /// additionally checks that `to_user_id` is a member
    /// of the same room (defense against cross-room relay
    /// and stale peer sessions). Non-members are denied
    /// with `CapsError::NotMember`.
    Signal,
    /// P4-T01: PLAYBACK_CMD envelope (PLAY / PAUSE / SEEK per
    /// docs/ARCHITECTURE.md §13). The caller must be a
    /// participant of the room named in `envelope.room_id`
    /// and either its host or holding PLAYBACK_CONTROL there
    /// (`can()`). The per-type
    /// handler additionally validates the room lifecycle
    /// state (PLAY requires Ready/Paused, PAUSE requires
    /// Playing, SEEK requires Playing/Paused) and the
    /// per-sender `monotonic_seq` (must equal
    /// `last_acked_seq[sender] + 1`).
    PlaybackControl,
    /// P4-T03: non-authoritative POSITION_REPORT envelope
    /// (1 Hz local-playback snapshot per
    /// docs/ARCHITECTURE.md §13.1). The capability check is
    /// "caller is a member of the room named in
    /// `envelope.room_id`" (mirrors `FetchManifest` /
    /// `Signal`), so cross-room injection is denied; the
    /// per-type handler re-checks it. Non-members are denied with
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
    /// P6-T02: host-only PERMISSION_SET envelope. The
    /// capability check is two-fold:
    ///
    /// 1. The caller must be a participant of the room
    ///    named in `envelope.room_id`.
    /// 2. The caller must be the room's CURRENT host
    ///    (`ParticipantRecord::is_host` is true).
    ///
    /// Unlike `PublishManifest` / `PlaybackControl` which
    /// also check `is_room_host`, the PERMISSION_SET
    /// handler does NOT call `can()` — it performs the
    /// `is_host` check directly and then applies the
    /// mutation. The `can()` function is read-only.
    PermissionSet,
    /// P6-T03: CHAT_MESSAGE envelope. Any room member with
    /// the CHAT capability can send. The capability check
    /// is "caller is a member of the room named in
    /// `envelope.room_id` AND has the CHAT bit set".
    ChatMessage,
}

impl Command {
    fn to_scope_action(self) -> Option<(Scope, Action)> {
        match self {
            Command::PlaybackControl => Some((Scope::Playback, Action::IssuePlaybackCommand)),
            Command::Draw => Some((Scope::Drawing, Action::DrawBegin)),
            Command::PublishManifest => Some((Scope::Manifest, Action::PublishManifest)),
            Command::ChatMessage => Some((Scope::Chat, Action::SendChat)),
            // PermissionSet is checked via is_room_host directly in check_capability;
            // it does not use can() since can() is read-only.
            Command::PermissionSet => None,
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
    room_id: Option<Uuid>,
    command: Command,
) -> Result<(), CapsError> {
    // Room lifecycle entry points carry no room to authorize
    // against (ROOM_CREATE / ROOM_JOIN_REQUEST address a room by
    // code; ROOM_LEAVE is handled by the registry).
    match command {
        Command::RoomCreate | Command::RoomJoinRequest | Command::RoomLeave => return Ok(()),
        // PRESENCE is a per-user keepalive (the client sends it
        // without a room_id) and grants nothing; it only needs
        // the caller to be in some room. A room_id, if present,
        // must be one the caller is in.
        Command::Presence => {
            let member = match room_id {
                Some(rid) => registry.is_user_in_room(user_id, rid).await,
                None => registry.get_user_room(user_id).await.is_some(),
            };
            return if member {
                Ok(())
            } else {
                Err(CapsError::NotMember)
            };
        }
        _ => {}
    }

    // Every other command acts on ONE room: the room named by the
    // envelope. Membership and capabilities are checked against
    // that room only. A user may be in several rooms; the gate
    // never picks "a room the user is in" on its own, because a
    // privilege held in one room must not authorize an action in
    // another.
    let Some(rid) = room_id else {
        return Err(CapsError::NotMember);
    };
    if !registry.is_user_in_room(user_id, rid).await {
        return Err(CapsError::NotMember);
    }
    match command {
        // Host-only.
        Command::PublishManifest | Command::PermissionSet => {
            if registry.is_room_host(rid, user_id).await {
                Ok(())
            } else {
                Err(CapsError::NotHost)
            }
        }
        // Capability-bit gated (host, or a participant the host
        // granted the bit to).
        Command::PlaybackControl | Command::Draw | Command::ChatMessage => {
            let (scope, action) = command
                .to_scope_action()
                .expect("capability-gated command has a scope/action");
            if can(registry, user_id, rid, scope, action).await {
                Ok(())
            } else {
                Err(CapsError::NotHost)
            }
        }
        // Membership only.
        Command::FetchManifest | Command::Signal | Command::PositionReport => Ok(()),
        Command::RoomCreate | Command::RoomJoinRequest | Command::RoomLeave | Command::Presence => {
            Ok(())
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
        assert!(check_capability(&reg, uid(1), None, Command::RoomCreate)
            .await
            .is_ok());
        assert!(check_capability(&reg, uid(99), None, Command::RoomCreate)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn room_join_request_is_allowed_for_anyone() {
        let (reg, _clock) = fresh_registry();
        assert!(
            check_capability(&reg, uid(1), None, Command::RoomJoinRequest)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn room_leave_is_allowed_in_v1() {
        let (reg, _clock) = fresh_registry();
        assert!(check_capability(&reg, uid(1), None, Command::RoomLeave)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn presence_is_denied_when_user_is_not_in_a_room() {
        let (reg, _clock) = fresh_registry();
        let err = check_capability(&reg, uid(1), None, Command::Presence)
            .await
            .expect_err("expected NotMember");
        assert!(matches!(err, CapsError::NotMember));
    }

    #[tokio::test]
    async fn signal_is_denied_when_user_is_not_in_any_room() {
        let (reg, _clock) = fresh_registry();
        let err = check_capability(&reg, uid(1), None, Command::Signal)
            .await
            .expect_err("expected NotMember");
        assert!(matches!(err, CapsError::NotMember));
    }

    #[tokio::test]
    async fn publish_manifest_is_denied_when_user_is_not_in_any_room() {
        let (reg, _clock) = fresh_registry();
        let err = check_capability(&reg, uid(1), None, Command::PublishManifest)
            .await
            .expect_err("expected NotMember");
        assert!(matches!(err, CapsError::NotMember));
    }

    #[tokio::test]
    async fn publish_manifest_is_denied_when_user_is_not_host() {
        let (reg, clock) = fresh_registry();
        let (room_id, _) = setup_room_with_host_and_viewer(&reg, &clock).await;
        let host_ok = check_capability(&reg, uid(1), Some(room_id), Command::PublishManifest).await;
        assert!(host_ok.is_ok(), "host should be allowed to publish");
        let viewer_err = check_capability(&reg, uid(2), Some(room_id), Command::PublishManifest)
            .await
            .expect_err("viewer must be denied");
        assert!(matches!(viewer_err, CapsError::NotHost));
        let _ = room_id;
    }

    #[tokio::test]
    async fn fetch_manifest_is_allowed_for_any_member_but_denied_for_non_member() {
        let (reg, clock) = fresh_registry();
        let (room_id, _) = setup_room_with_host_and_viewer(&reg, &clock).await;
        assert!(
            check_capability(&reg, uid(1), Some(room_id), Command::FetchManifest)
                .await
                .is_ok()
        );
        assert!(
            check_capability(&reg, uid(2), Some(room_id), Command::FetchManifest)
                .await
                .is_ok()
        );
        let err = check_capability(&reg, uid(3), Some(room_id), Command::FetchManifest)
            .await
            .expect_err("non-member must be denied");
        assert!(matches!(err, CapsError::NotMember));
        let _ = room_id;
    }

    #[tokio::test]
    async fn playback_control_is_denied_when_user_is_not_in_any_room() {
        let (reg, _clock) = fresh_registry();
        let err = check_capability(&reg, uid(1), None, Command::PlaybackControl)
            .await
            .expect_err("expected NotMember");
        assert!(matches!(err, CapsError::NotMember));
    }

    #[tokio::test]
    async fn playback_control_is_denied_when_user_is_not_host() {
        let (reg, clock) = fresh_registry();
        let (room_id, _) = setup_room_with_host_and_viewer(&reg, &clock).await;
        let host_ok = check_capability(&reg, uid(1), Some(room_id), Command::PlaybackControl).await;
        assert!(
            host_ok.is_ok(),
            "host should be allowed to playback-control"
        );
        let viewer_err = check_capability(&reg, uid(2), Some(room_id), Command::PlaybackControl)
            .await
            .expect_err("viewer must be denied");
        assert!(matches!(viewer_err, CapsError::NotHost));
        let _ = room_id;
    }

    #[tokio::test]
    async fn playback_control_allowed_for_cohost_with_playback_bit() {
        let (reg, clock) = fresh_registry();
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
            "cohost".into(),
            clock.now_ms(),
        )
        .await
        .expect("cohost joins");
        let cohost_id = uid(2);
        let room_id = room.id;
        reg.update_participant_cap_set(
            room_id,
            cohost_id,
            cap_bits::PLAYBACK_CONTROL,
            clock.now_ms(),
        )
        .await
        .expect("grant playback control to cohost");
        let result =
            check_capability(&reg, cohost_id, Some(room_id), Command::PlaybackControl).await;
        assert!(
            result.is_ok(),
            "co-host with PLAYBACK_CONTROL bit should be allowed"
        );
    }

    #[tokio::test]
    async fn playback_control_denied_for_cohost_without_playback_bit() {
        let (reg, clock) = fresh_registry();
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
            "cohost".into(),
            clock.now_ms(),
        )
        .await
        .expect("cohost joins");
        let cohost_id = uid(2);
        let room_id = room.id;
        reg.update_participant_cap_set(room_id, cohost_id, cap_bits::DRAW, clock.now_ms())
            .await
            .expect("grant draw (not playback) to cohost");
        let err = check_capability(&reg, cohost_id, Some(room_id), Command::PlaybackControl)
            .await
            .expect_err("co-host without PLAYBACK_CONTROL bit must be denied");
        assert!(matches!(err, CapsError::NotHost));
        let _ = room_id;
    }

    /// uid(1) hosts room A; uid(3) hosts room B, which uid(1)
    /// joins as a plain viewer. Every privilege uid(1) holds in A
    /// must be evaluated against the room each request names.
    #[tokio::test]
    async fn privileges_are_checked_against_the_named_room_only() {
        use super::super::store::NoopRoomStore;
        let (reg, clock) = fresh_registry();
        let s = NoopRoomStore;
        let (a, _) = reg
            .create(&s, "A".into(), uid(1), [1u8; 32], true, clock.now_ms())
            .await
            .expect("create A");
        let (b, _) = reg
            .create(&s, "B".into(), uid(3), [3u8; 32], true, clock.now_ms())
            .await
            .expect("create B");
        reg.join(
            &s,
            &b.code,
            uid(1),
            [1u8; 32],
            "a-host".into(),
            clock.now_ms(),
        )
        .await
        .expect("uid(1) joins B as a viewer");
        let gated = [
            Command::PublishManifest,
            Command::PermissionSet,
            Command::PlaybackControl,
            Command::Draw,
        ];
        // The old gate authorized against whichever of the user's
        // rooms `get_user_room` returned. Asserting success in A
        // AND refusal in B fails for either choice.
        {
            for cmd in gated {
                assert!(
                    check_capability(&reg, uid(1), Some(a.id), cmd)
                        .await
                        .is_ok(),
                    "{cmd:?} in the room uid(1) hosts"
                );
                assert!(
                    matches!(
                        check_capability(&reg, uid(1), Some(b.id), cmd).await,
                        Err(CapsError::NotHost)
                    ),
                    "{cmd:?} in the room uid(1) only views"
                );
            }
            // Membership-level commands still work in both rooms.
            for rid in [a.id, b.id] {
                for cmd in [
                    Command::ChatMessage,
                    Command::FetchManifest,
                    Command::Signal,
                ] {
                    assert!(check_capability(&reg, uid(1), Some(rid), cmd).await.is_ok());
                }
            }
        }
        // A room the user is not in, and a missing room id.
        let not_in = Uuid::now_v7();
        for cmd in [
            Command::ChatMessage,
            Command::PublishManifest,
            Command::PositionReport,
        ] {
            assert!(matches!(
                check_capability(&reg, uid(1), Some(not_in), cmd).await,
                Err(CapsError::NotMember)
            ));
            assert!(matches!(
                check_capability(&reg, uid(1), None, cmd).await,
                Err(CapsError::NotMember)
            ));
        }
    }

    /// A participant granted DRAW who leaves and rejoins gets a
    /// fresh record with the default capabilities; the old `Left`
    /// record (still first in the list) must not authorize them.
    #[tokio::test]
    async fn capabilities_of_a_left_record_do_not_survive_a_rejoin() {
        use super::super::store::NoopRoomStore;
        let (reg, clock) = fresh_registry();
        let s = NoopRoomStore;
        let (room_id, _host) = setup_room_with_host_and_viewer(&reg, &clock).await;
        let viewer = uid(2);
        reg.update_participant_cap_set(room_id, viewer, cap_bits::DRAW, clock.now_ms())
            .await
            .expect("grant DRAW");
        assert!(can(&reg, viewer, room_id, Scope::Drawing, Action::DrawBegin).await);

        reg.leave_room(&s, viewer, Some(room_id), true, clock.now_ms())
            .await
            .expect("viewer leaves");
        let code = reg
            .get_by_id(room_id)
            .await
            .expect("room")
            .read()
            .await
            .code
            .clone();
        reg.join(&s, &code, viewer, [2u8; 32], "again".into(), clock.now_ms())
            .await
            .expect("viewer rejoins");

        assert!(!can(&reg, viewer, room_id, Scope::Drawing, Action::DrawBegin).await);
        assert!(matches!(
            check_capability(&reg, viewer, Some(room_id), Command::Draw).await,
            Err(CapsError::NotHost)
        ));
        // A new grant lands on the live record.
        reg.update_participant_cap_set(room_id, viewer, cap_bits::DRAW, clock.now_ms())
            .await
            .expect("grant again");
        assert!(can(&reg, viewer, room_id, Scope::Drawing, Action::DrawBegin).await);
    }
}
