//! P4-T01: server-side PLAYBACK_CMD validation + ordering.
//!
//! The single arbiter for playback commands (PLAY / PAUSE /
//! SEEK per docs/ARCHITECTURE.md section 13). The dispatcher
//! routes a `MessageKind::PlaybackCmd` envelope here AFTER the
//! WS layer has confirmed the caller is a member of the room
//! named in `envelope.room_id` and the capability gate
//! (`caps::check_capability(Command::PlaybackControl)`) has
//! confirmed the caller may issue playback commands: the host,
//! or a co-host the host granted `PLAYBACK_CONTROL` via
//! PERMISSION_SET. The same rule (`caps::participant_can`) is
//! re-checked here inside the room lock, together with the
//! caller's authenticated pubkey. This module does
// the rest:
//!
//! 1. Decode the payload into a typed [`PlaybackCommandPayload`].
// 2. Look up the room state via `RoomRegistry::get_user_room` + a
//    handle helper, and check the room lifecycle against the
//    requested action (PLAY requires Open/Paused, PAUSE requires
//    Playing, SEEK requires Playing/Paused). See `state.rs:43-58`
//    for the lifecycle variants.
// 3. Check the per-sender `monotonic_seq` against the room's
//    per-sender `last_acked_seq` table (drops duplicates; rejects
//    gaps with a single-caller ROOM_ERROR(StaleCommand)).
// 4. On accept, mutate `RoomState`:
//    - increment `RoomState::playback.server_seq`
//    - update `RoomState::playback.last_acked_seq[sender_id]`
//    - update `RoomState::playback.last_position_ms`
//    - transition `RoomState::state` (Open/Paused -> Playing on
//      PLAY, Playing -> Paused on PAUSE, Playing/Paused -> same
//      on SEEK)
// 5. Publish a `RoomEvent::PlaybackCommand` carrying the
//    server-stamped `PlaybackAcceptedEvent` to the room's
//    broadcast channel WHILE STILL HOLDING the room write lock
//    that assigned `server_seq`, so broadcast order always equals
//    `server_seq` order even when the host and a co-host send
//    concurrently on separate connections. The WS forwarders turn
//    it into a PLAYBACK_CMD envelope for every other participant.
//!
//! Rejected commands produce `Err(PlaybackError)` which the
//! dispatch layer surfaces as a single-caller ROOM_ERROR. They
//! do NOT update server_seq / last_acked_seq / last_position_ms
//! and they do NOT produce a broadcast event.
//!
//! Migration interaction: `last_acked_seq` is keyed by `user_id`
//! (not `pubkey`). After a host migration the new host's
//! `last_acked_seq` entry starts at 0 (the default for an absent
//! key); the old host's entry persists, so any stale PLAYBACK_CMD
//! from the former host continues to be rejected as a duplicate
//! (or, more rarely, as a stale-seq entry > last_acked_seq + 1
//! if the new host accepted commands in the meantime). A demoted
//! host's PLAYBACK_CMD cannot poison the new host's authoritative
//! playback ordering.
//!
//! v1 simplicity: `server_seq` and `last_position_ms` are in-memory
//! only and are reset on room creation; they are not persisted to
//! the database. Restarting the server ends any in-flight
//! playback ordering. This is acceptable for v1 because:
//!
//! 1. There is no "resume" semantic for playback (every room
//!    starts in Open state on creation; a re-host after server
//!    restart starts fresh).
//! 2. The roadmap does not require durable playback ordering.
//! 3. P5+ may add an `room_events` table if a resume semantic
//    becomes a requirement.

#![forbid(unsafe_code)]

use locast_protocol::envelope::Envelope;
use locast_protocol::room::{PlaybackAcceptedEvent, PlaybackAction, PlaybackCommandPayload};
use uuid::Uuid;

use super::caps::{participant_can, Action, Scope};
use super::error::RoomError;
use super::registry::{RoomEvent, RoomRegistry};
use super::state::RoomLifecycle;
use crate::time::Clock;

/// P4-T01: the in-memory domain error for playback validation.
/// The dispatch layer maps this onto the wire-level
/// `RoomError` (which itself maps onto `RoomErrorCode`). Kept
/// as a thin wrapper around `RoomError` for two reasons:
///
/// 1. The capability gate runs BEFORE this module and may have
///    already raised a generic `CapsError::NotHost` / `NotMember`;
///    we don't want to silently re-classify those as
///    playback-specific errors.
/// 2. The new `StaleCommand` / `DuplicateCommand` variants on
///    `RoomError` are playback-specific and never raised by any
///    other per-type handler.
#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    #[error("envelope has no room_id")]
    NoRoomId,
    #[error("caller is not a participant of the named room")]
    NotJoined,
    #[error("room is not open for playback commands")]
    RoomClosed,
    #[error("caller may not issue playback commands")]
    NotHost,
    #[error("playback monotonic_seq gap (got {got}, expected {expected})")]
    StaleCommand { got: u64, expected: u64 },
    #[error("playback command is stale (seq {got} <= last_acked_seq {last})")]
    DuplicateCommand { got: u64, last: u64 },
    #[error("malformed playback payload: {0}")]
    BadPayload(String),
}

/// Validate + accept a playback command. On accept the
/// `RoomEvent::PlaybackCommand` is ALREADY PUBLISHED to the
/// room's broadcast channel (under the room lock, see step 5
/// above) and is returned for the caller's information only;
/// callers must not publish it again. On reject returns a
/// typed `PlaybackError` and publishes nothing. The dispatch
/// layer converts `PlaybackError` into a wire-level ROOM_ERROR
/// via the shared `RoomError` -> `RoomErrorCode` mapping; see
/// the per-error mapping below.
///
/// `pubkey` is the authenticated session's pubkey; it must be
/// the pubkey on the caller's participant record.
pub async fn handle_playback_cmd(
    envelope: &Envelope,
    registry: &RoomRegistry,
    clock: &dyn Clock,
    user_id: Uuid,
    pubkey: [u8; 32],
) -> Result<RoomEvent, PlaybackError> {
    // 1. The room id must be present (PLAYBACK_CMD is a
    //    per-room envelope, never a server-broadcast
    //    envelope).
    let room_id = envelope.room_id.ok_or(PlaybackError::NoRoomId)?;

    // 2. Decode the payload. We treat malformed JSON as a
    //    per-caller ROOM_ERROR(InvalidState), never a
    //    broadcast. The capability gate has already passed
    //    so the caller is at least authed; we don't need to
    //    re-validate the bearer here.
    let payload_value = envelope.payload.clone();
    let payload: PlaybackCommandPayload = serde_json::from_value(payload_value)
        .map_err(|e| PlaybackError::BadPayload(e.to_string()))?;

    // 3. Look up the room state and re-check authorization
    //    inside the same write lock that will assign
    //    `server_seq`, against the room the envelope names.
    //    The capability gate already ran; re-checking here
    //    keeps the invariant even if a host migration or a
    //    PERMISSION_SET revocation lands between the gate and
    //    this lock, or if the gate is bypassed in a future
    //    refactor.
    let handle = registry
        .get_by_id(room_id)
        .await
        .ok_or(PlaybackError::NotJoined)?;
    let mut state = handle.write().await;

    if state.state == RoomLifecycle::Ended {
        return Err(PlaybackError::RoomClosed);
    }
    let (pubkey_matches, may_control) = {
        let caller = state
            .participants
            .iter()
            .find(|p| p.user_id == user_id)
            .ok_or(PlaybackError::NotJoined)?;
        (
            caller.pubkey == pubkey,
            participant_can(caller, Scope::Playback, Action::IssuePlaybackCommand),
        )
    };
    // The participant record's pubkey is authoritative: a
    // session whose pubkey differs (e.g. a stale identity
    // after a rotation) cannot act as this participant.
    if !pubkey_matches || !may_control {
        return Err(PlaybackError::NotHost);
    }

    // 4. Per-room lifecycle precondition. Per
    //    docs/ARCHITECTURE.md §11.1:
    //    - PLAY from Playing is idempotent (no transition).
    //    - PAUSE from Paused is idempotent (no transition).
    //    - SEEK from Playing/Paused keeps the lifecycle.
    //    - PLAY from Open/Paused transitions to Playing.
    //    - PAUSE from Playing transitions to Paused.
    //    - All other (state, action) pairs are forbidden.
    match (state.state, payload.action) {
        (RoomLifecycle::Open | RoomLifecycle::Paused, PlaybackAction::Play) => {}
        (RoomLifecycle::Playing, PlaybackAction::Play) => {
            // §11.1: idempotent (no state transition). Drops through
            // to monotonic_seq validation below.
        }
        (RoomLifecycle::Playing, PlaybackAction::Pause) => {}
        (RoomLifecycle::Paused, PlaybackAction::Pause) => {
            // §11.1: idempotent. Drops through.
        }
        (RoomLifecycle::Playing | RoomLifecycle::Paused, PlaybackAction::Seek) => {}
        _ => return Err(PlaybackError::RoomClosed),
    }

    // 5. Per-sender monotonic-seq check. The valid window is
    //    `last_acked_seq[sender_id] + 1` exactly: anything
    //    `<=` last is a duplicate (replay) and anything `>`
    //    last+1 is a gap (the server is missing the in-between
    //    commands and cannot apply this one without the
    //    missing range). Both cases are rejected per
    //    docs/ARCHITECTURE.md §13.2.
    let last = state
        .playback
        .last_acked_seq
        .get(&user_id)
        .copied()
        .unwrap_or(0);
    let expected = last.saturating_add(1);
    match payload.monotonic_seq.cmp(&expected) {
        std::cmp::Ordering::Equal => {}
        std::cmp::Ordering::Less => {
            // seq <= last -> duplicate / replay
            return Err(PlaybackError::DuplicateCommand {
                got: payload.monotonic_seq,
                last,
            });
        }
        std::cmp::Ordering::Greater => {
            // seq > last+1 -> gap
            return Err(PlaybackError::StaleCommand {
                got: payload.monotonic_seq,
                expected,
            });
        }
    }

    // 6. Accept. Assign server_seq, update last_acked_seq,
    //    update last_position_ms, transition state.
    state.playback.server_seq = state
        .playback
        .server_seq
        .checked_add(1)
        .ok_or_else(|| PlaybackError::BadPayload("server_seq overflow".into()))?;
    state
        .playback
        .last_acked_seq
        .insert(user_id, payload.monotonic_seq);
    state.playback.last_position_ms = payload.media_position_ms;

    let server_seq = state.playback.server_seq;
    let server_ts_ms = clock.now_ms();

    match payload.action {
        PlaybackAction::Play => state.state = RoomLifecycle::Playing,
        PlaybackAction::Pause => state.state = RoomLifecycle::Paused,
        PlaybackAction::Seek => {
            // SEEK keeps the play/pause state; it only
            // moves the cursor.
        }
    }

    let event = RoomEvent::PlaybackCommand(PlaybackAcceptedEvent {
        sender_id: user_id,
        action: payload.action,
        monotonic_seq: payload.monotonic_seq,
        media_position_ms: payload.media_position_ms,
        client_ts_ms: payload.client_ts_ms,
        server_seq,
        server_ts_ms,
    });

    // 7. Publish while `state` (the room write lock) is still
    //    held: the next accepted command cannot take a
    //    `server_seq` until this one is on the channel, so the
    //    broadcast order is the `server_seq` order. `publish`
    //    never awaits.
    registry.publish_events(std::slice::from_ref(&event), |_| room_id);
    drop(state);

    Ok(event)
}

impl From<PlaybackError> for RoomError {
    fn from(e: PlaybackError) -> Self {
        match e {
            PlaybackError::NoRoomId => RoomError::InvalidState,
            PlaybackError::NotJoined => RoomError::NotJoined,
            PlaybackError::RoomClosed => RoomError::RoomClosed,
            PlaybackError::NotHost => RoomError::NotHost,
            PlaybackError::StaleCommand { got, expected } => {
                RoomError::StaleCommand { got, expected }
            }
            PlaybackError::DuplicateCommand { got, last } => {
                RoomError::DuplicateCommand { got, last }
            }
            PlaybackError::BadPayload(msg) => RoomError::Internal(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    //! P4-T01 focused tests for the playback dispatcher.
    //!
    //! The cap-gate tests (host-only + non-member denied)
    //! live in `caps.rs::tests`. This module covers:
    //!
    //! 1. happy path: host PLAY is accepted, transitions
    //!    state Open -> Playing, increments server_seq
    //!    to 1, updates last_acked_seq[host] = 1.
    //! 2. host PAUSE after PLAY is accepted; transitions
    //!    state Playing -> Paused; server_seq = 2.
    //! 3. host SEEK (after PAUSE) is accepted without
    //!    state transition; server_seq = 3; last_position_ms
    //!    updates.
    //! 4. host PLAY with monotonic_seq = 5 (gap from 1) is
    //!    rejected as StaleCommand.
    //! 5. host PLAY with monotonic_seq = 1 (already
    //!    accepted) is rejected as DuplicateCommand.
    //! 6. PAUSE from Open state is rejected as RoomClosed.
    //! 7. Malformed payload is rejected as BadPayload.

    use super::*;
    use crate::time::MockClock;
    use locast_protocol::envelope::{Envelope, MessageKind, Sender};
    use locast_protocol::room::PlaybackCommandPayload;
    use serde_json::json;
    use uuid::Uuid;

    fn fresh_clock() -> MockClock {
        MockClock::new(1_000_000)
    }

    fn fresh_registry() -> crate::rooms::RoomRegistry {
        let cfg = crate::rooms::registry::RoomRegistryConfig {
            max_participants: 8,
            host_disconnect_grace_ms: 200,
            participant_stale_after_ms: 300_000,
            participant_disconnect_after_ms: 15_000,
        };
        crate::rooms::RoomRegistry::new(cfg)
    }

    fn uid(i: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[0] = i;
        b[15] = i;
        Uuid::from_bytes(b)
    }

    async fn make_room_with_host_viewer(
        reg: &crate::rooms::RoomRegistry,
        clock: &MockClock,
    ) -> (Uuid, [u8; 32], Uuid) {
        let s = crate::rooms::store::NoopRoomStore;
        let host_uid = uid(1);
        let host_pk = [1u8; 32];
        let (room, _self_view) = reg
            .create(&s, "P4-T01".into(), host_uid, host_pk, true, clock.now_ms())
            .await
            .expect("create room");
        // Viewer joins but stays non-host. Not strictly
        // necessary for these tests but mirrors the
        // end-to-end smoke shape.
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
        (room.id, host_pk, host_uid)
    }

    fn envelope(
        room_id: Uuid,
        sender_uid: Uuid,
        sender_pk: [u8; 32],
        action: PlaybackAction,
        monotonic_seq: u64,
        position_ms: u64,
    ) -> Envelope {
        Envelope {
            v: 1,
            r#type: MessageKind::PlaybackCmd,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: Some(Sender {
                user_id: sender_uid,
                pubkey: sender_pk.to_vec(),
                sig: vec![],
            }),
            ts_ms: 0,
            seq: monotonic_seq,
            payload: json!(PlaybackCommandPayload {
                action,
                monotonic_seq,
                media_position_ms: position_ms,
                client_ts_ms: 0,
            }),
        }
    }

    async fn room_state_snapshot(
        registry: &crate::rooms::RoomRegistry,
        room_id: Uuid,
    ) -> std::sync::Arc<tokio::sync::RwLock<crate::rooms::RoomState>> {
        registry.get_by_id(room_id).await.expect("room exists")
    }

    #[tokio::test]
    async fn host_play_advances_state_and_assigns_server_seq_one() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, host_pk, host_uid) = make_room_with_host_viewer(&reg, &clock).await;

        let env = envelope(room_id, host_uid, host_pk, PlaybackAction::Play, 1, 0);
        let evt = handle_playback_cmd(&env, &reg, &clock, host_uid, host_pk)
            .await
            .expect("accept");
        let RoomEvent::PlaybackCommand(accepted) = evt else {
            panic!("expected PlaybackCommand");
        };
        assert_eq!(accepted.server_seq, 1, "first accept -> server_seq 1");
        assert_eq!(accepted.action, PlaybackAction::Play);
        assert_eq!(accepted.sender_id, host_uid);

        let st = room_state_snapshot(&reg, room_id).await;
        let st = st.read().await;
        assert_eq!(st.state, crate::rooms::RoomLifecycle::Playing);
        assert_eq!(st.playback.server_seq, 1);
        assert_eq!(st.playback.last_acked_seq.get(&host_uid).copied(), Some(1));
        assert_eq!(st.playback.last_position_ms, 0);
    }

    #[tokio::test]
    async fn host_play_pause_seek_walks_through_lifecycle_in_order() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, host_pk, host_uid) = make_room_with_host_viewer(&reg, &clock).await;

        for (seq, action, pos, expected_state) in [
            (
                1u64,
                PlaybackAction::Play,
                0u64,
                crate::rooms::RoomLifecycle::Playing,
            ),
            (
                2u64,
                PlaybackAction::Pause,
                1_000u64,
                crate::rooms::RoomLifecycle::Paused,
            ),
            (
                3u64,
                PlaybackAction::Seek,
                5_000u64,
                crate::rooms::RoomLifecycle::Paused,
            ),
        ] {
            let env = envelope(room_id, host_uid, host_pk, action, seq, pos);
            let evt = handle_playback_cmd(&env, &reg, &clock, host_uid, host_pk)
                .await
                .unwrap_or_else(|e| panic!("seq {seq} {action:?} should be accepted, got {e:?}"));
            let RoomEvent::PlaybackCommand(accepted) = evt else {
                panic!("expected PlaybackCommand");
            };
            assert_eq!(
                accepted.server_seq, seq,
                "server_seq matches monotonic order"
            );
            assert_eq!(accepted.media_position_ms, pos);

            let st = room_state_snapshot(&reg, room_id).await;
            let st = st.read().await;
            assert_eq!(
                st.state, expected_state,
                "seq {seq} {action:?} should land in {expected_state:?}"
            );
            assert_eq!(st.playback.last_position_ms, pos);
        }
    }

    #[tokio::test]
    async fn monotonic_seq_gap_is_rejected_as_stale_command() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, host_pk, host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        // First PLAY with seq 1 lands.
        let env = envelope(room_id, host_uid, host_pk, PlaybackAction::Play, 1, 0);
        handle_playback_cmd(&env, &reg, &clock, host_uid, host_pk)
            .await
            .expect("accept first");
        // Second PLAY with seq 5 (gap from 1 -> 5) is rejected.
        let env = envelope(room_id, host_uid, host_pk, PlaybackAction::Play, 5, 2_000);
        let err = handle_playback_cmd(&env, &reg, &clock, host_uid, host_pk)
            .await
            .expect_err("expected stale command rejection");
        match err {
            PlaybackError::StaleCommand { got, expected } => {
                assert_eq!(got, 5);
                assert_eq!(expected, 2);
            }
            other => panic!("expected StaleCommand, got {other:?}"),
        }
        // State must NOT have advanced.
        let st = room_state_snapshot(&reg, room_id).await;
        let st = st.read().await;
        assert_eq!(
            st.playback.server_seq, 1,
            "rejected command must not bump server_seq"
        );
    }

    #[tokio::test]
    async fn replay_with_old_monotonic_seq_is_rejected_as_duplicate() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, host_pk, host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        // First PLAY with seq 1 lands.
        let env = envelope(room_id, host_uid, host_pk, PlaybackAction::Play, 1, 0);
        handle_playback_cmd(&env, &reg, &clock, host_uid, host_pk)
            .await
            .expect("accept first");
        // Second PLAY with seq 1 (duplicate) is rejected.
        let env = envelope(room_id, host_uid, host_pk, PlaybackAction::Play, 1, 0);
        let err = handle_playback_cmd(&env, &reg, &clock, host_uid, host_pk)
            .await
            .expect_err("expected duplicate rejection");
        match err {
            PlaybackError::DuplicateCommand { got, last } => {
                assert_eq!(got, 1);
                assert_eq!(last, 1);
            }
            other => panic!("expected DuplicateCommand, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pause_from_open_state_is_rejected() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, host_pk, host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        // Fresh room is Open. PAUSE is not allowed from Open
        // per docs/ARCHITECTURE.md §11.1.
        let env = envelope(room_id, host_uid, host_pk, PlaybackAction::Pause, 1, 0);
        let err = handle_playback_cmd(&env, &reg, &clock, host_uid, host_pk)
            .await
            .expect_err("expected RoomClosed");
        assert!(matches!(err, PlaybackError::RoomClosed), "got {err:?}");
    }

    #[tokio::test]
    async fn malformed_payload_is_rejected() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, host_pk, host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        let mut env = envelope(room_id, host_uid, host_pk, PlaybackAction::Play, 1, 0);
        env.payload = json!({"this": "is not the right shape"});
        let err = handle_playback_cmd(&env, &reg, &clock, host_uid, host_pk)
            .await
            .expect_err("expected BadPayload");
        assert!(matches!(err, PlaybackError::BadPayload(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn caller_with_stale_pubkey_after_migration_is_rejected() {
        // After host migration, the new host's pubkey is on
        // the ParticipantRecord. The OLD host's pubkey is no
        // longer bound to the room. If the old host keeps
        // trying to issue commands with their (now-stale)
        // pubkey, handle_playback_cmd must reject them with
        // NotHost even if they were the original host.
        //
        // We simulate this by registering the host with
        // pubkey [1u8; 32] and then calling with pubkey
        // [9u8; 32]. The pubkey-bind check in
        // handle_playback_cmd must catch the mismatch.
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, _host_pk, host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        let stale_pk = [9u8; 32];
        let env = envelope(room_id, host_uid, stale_pk, PlaybackAction::Play, 1, 0);
        let err = handle_playback_cmd(&env, &reg, &clock, host_uid, stale_pk)
            .await
            .expect_err("expected NotHost");
        assert!(matches!(err, PlaybackError::NotHost), "got {err:?}");
    }

    /// Grant the viewer (uid(2), pubkey [2u8; 32]) a capability
    /// set through the existing PERMISSION_SET registry path.
    async fn grant(reg: &crate::rooms::RoomRegistry, room_id: Uuid, bits: u32, clock: &MockClock) {
        reg.update_participant_cap_set(room_id, uid(2), bits, clock.now_ms())
            .await
            .expect("grant caps");
    }

    fn decode(item: &crate::rooms::registry::BroadcastItem) -> PlaybackAcceptedEvent {
        assert_eq!(item.kind, MessageKind::PlaybackCmd);
        serde_json::from_value(item.payload.clone()).expect("accepted payload")
    }

    #[tokio::test]
    async fn viewer_play_pause_seek_are_forbidden_and_never_published() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, host_pk, host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        let mut rx = reg.subscribe(room_id).await.expect("room channel");
        // Put the room in Playing so every action is valid for
        // the lifecycle; only authorization can reject them.
        let env = envelope(room_id, host_uid, host_pk, PlaybackAction::Play, 1, 0);
        handle_playback_cmd(&env, &reg, &clock, host_uid, host_pk)
            .await
            .expect("host play");
        assert_eq!(
            decode(&rx.try_recv().expect("host broadcast")).server_seq,
            1
        );

        for (i, action) in [
            PlaybackAction::Play,
            PlaybackAction::Pause,
            PlaybackAction::Seek,
        ]
        .into_iter()
        .enumerate()
        {
            let env = envelope(room_id, uid(2), [2u8; 32], action, i as u64 + 1, 5_000);
            let err = handle_playback_cmd(&env, &reg, &clock, uid(2), [2u8; 32])
                .await
                .expect_err("viewer must be refused");
            assert!(matches!(err, PlaybackError::NotHost), "{action:?}: {err:?}");
        }
        assert!(
            rx.try_recv().is_err(),
            "a refused command must not be published"
        );
        let st = room_state_snapshot(&reg, room_id).await;
        let st = st.read().await;
        assert_eq!(
            st.playback.server_seq, 1,
            "refusals do not consume server_seq"
        );
        assert_eq!(st.state, RoomLifecycle::Playing);
        assert!(!st.playback.last_acked_seq.contains_key(&uid(2)));
    }

    #[tokio::test]
    async fn cohost_with_playback_control_is_accepted_and_shares_the_room_sequence() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, host_pk, host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        grant(
            &reg,
            room_id,
            crate::rooms::caps::cap_bits::PLAYBACK_CONTROL,
            &clock,
        )
        .await;
        let mut rx = reg.subscribe(room_id).await.expect("room channel");

        let env = envelope(room_id, host_uid, host_pk, PlaybackAction::Play, 1, 0);
        handle_playback_cmd(&env, &reg, &clock, host_uid, host_pk)
            .await
            .expect("host play");
        // The co-host's own monotonic_seq starts at 1; the room's
        // server_seq continues from the host's.
        let env = envelope(room_id, uid(2), [2u8; 32], PlaybackAction::Pause, 1, 7_000);
        handle_playback_cmd(&env, &reg, &clock, uid(2), [2u8; 32])
            .await
            .expect("co-host pause");

        let first = decode(&rx.try_recv().expect("host broadcast"));
        let second = decode(&rx.try_recv().expect("co-host broadcast"));
        assert_eq!((first.server_seq, first.sender_id), (1, host_uid));
        assert_eq!((second.server_seq, second.sender_id), (2, uid(2)));
        assert_eq!(second.action, PlaybackAction::Pause);
    }

    #[tokio::test]
    async fn cohost_without_playback_control_is_forbidden() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, host_pk, host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        grant(&reg, room_id, crate::rooms::caps::cap_bits::DRAW, &clock).await;
        let env = envelope(room_id, host_uid, host_pk, PlaybackAction::Play, 1, 0);
        handle_playback_cmd(&env, &reg, &clock, host_uid, host_pk)
            .await
            .expect("host play");
        let mut rx = reg.subscribe(room_id).await.expect("room channel");
        let env = envelope(room_id, uid(2), [2u8; 32], PlaybackAction::Pause, 1, 0);
        let err = handle_playback_cmd(&env, &reg, &clock, uid(2), [2u8; 32])
            .await
            .expect_err("DRAW does not grant playback");
        assert!(matches!(err, PlaybackError::NotHost), "got {err:?}");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn cohost_session_with_a_different_pubkey_is_forbidden() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, _host_pk, _host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        grant(
            &reg,
            room_id,
            crate::rooms::caps::cap_bits::PLAYBACK_CONTROL,
            &clock,
        )
        .await;
        let env = envelope(room_id, uid(2), [7u8; 32], PlaybackAction::Play, 1, 0);
        let err = handle_playback_cmd(&env, &reg, &clock, uid(2), [7u8; 32])
            .await
            .expect_err("pubkey must match the participant record");
        assert!(matches!(err, PlaybackError::NotHost), "got {err:?}");
    }

    #[tokio::test]
    async fn server_ts_is_the_server_clock_at_acceptance_not_the_client_ts() {
        let clock = fresh_clock();
        let reg = fresh_registry();
        let (room_id, host_pk, host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        let mut stamps = Vec::new();
        for (seq, action) in [
            PlaybackAction::Play,
            PlaybackAction::Seek,
            PlaybackAction::Pause,
        ]
        .into_iter()
        .enumerate()
        {
            clock.advance(250);
            let mut env = envelope(room_id, host_uid, host_pk, action, seq as u64 + 1, 0);
            // A client clock far in the past must not leak into server_ts.
            env.payload["client_ts_ms"] = json!(42);
            let evt = handle_playback_cmd(&env, &reg, &clock, host_uid, host_pk)
                .await
                .expect("accepted");
            let RoomEvent::PlaybackCommand(a) = evt else {
                panic!("expected PlaybackCommand");
            };
            assert_eq!(a.server_ts_ms, clock.now_ms());
            assert_eq!(a.client_ts_ms, 42, "client_ts is echoed verbatim");
            stamps.push(a.server_ts_ms);
        }
        assert_eq!(stamps, vec![1_000_250, 1_000_500, 1_000_750]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_host_and_cohost_broadcast_in_server_seq_order() {
        let clock = std::sync::Arc::new(fresh_clock());
        let reg = std::sync::Arc::new(fresh_registry());
        let (room_id, host_pk, host_uid) = make_room_with_host_viewer(&reg, &clock).await;
        grant(
            &reg,
            room_id,
            crate::rooms::caps::cap_bits::PLAYBACK_CONTROL,
            &clock,
        )
        .await;
        let mut rx = reg.subscribe(room_id).await.expect("room channel");
        let env = envelope(room_id, host_uid, host_pk, PlaybackAction::Play, 1, 0);
        handle_playback_cmd(&env, &reg, &*clock, host_uid, host_pk)
            .await
            .expect("host play");

        // Two senders on "separate connections", each with its
        // own in-order monotonic_seq, racing each other.
        const PER_SENDER: u64 = 50;
        let spawn_sender = |sender: Uuid, pk: [u8; 32], first_seq: u64| {
            let reg = reg.clone();
            let clock = clock.clone();
            tokio::spawn(async move {
                for i in 0..PER_SENDER {
                    let env = envelope(room_id, sender, pk, PlaybackAction::Seek, first_seq + i, i);
                    handle_playback_cmd(&env, &reg, &*clock, sender, pk)
                        .await
                        .expect("seek accepted");
                    tokio::task::yield_now().await;
                }
            })
        };
        let a = spawn_sender(host_uid, host_pk, 2);
        let b = spawn_sender(uid(2), [2u8; 32], 1);
        a.await.expect("host task");
        b.await.expect("co-host task");

        let mut seqs = Vec::new();
        while let Ok(item) = rx.try_recv() {
            seqs.push(decode(&item).server_seq);
        }
        let expected: Vec<u64> = (1..=1 + 2 * PER_SENDER).collect();
        assert_eq!(
            seqs, expected,
            "broadcast order must equal server_seq order"
        );
    }
}
