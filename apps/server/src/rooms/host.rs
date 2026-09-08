//! Host-disconnect grace logic for P7-T03.
//!
//! v1: grace always ends the room after
//! `host_disconnect_grace_ms`. A v1.1 transfer stub is
//! reserved at `try_migrate_host`.

#![forbid(unsafe_code)]

use tracing::debug;
use uuid::Uuid;

use super::state::{RoomLifecycle, RoomState};
use crate::rooms::registry::RoomEvent;
use locast_protocol::room::{HostMigratedPayload, ParticipantStatus, RoomClosedPayload};

/// The close reason when grace expires with no migration.
const GRACE_REASON: &str = "host_disconnected_no_migration";

/// Decide the per-room outcome when the grace deadline has
/// elapsed. Mutates `state` in place:
/// - on migration: clears the deadline, updates host flags
/// - on end-room: sets `RoomLifecycle::Ended`
///
/// v1 behaviour: always ends the room (the migration path
/// in `try_migrate_host` is a v1.1 stub).
pub(super) fn process_room_grace(state: &mut RoomState, now_ms: i64) -> Option<RoomEvent> {
    let deadline = state.host_disconnect_deadline_ms?;
    if now_ms < deadline {
        return None;
    }

    let prev = state.host_user_id;

    if state.host_migration_enabled {
        if let Some(new_host_id) = try_migrate_host(state) {
            state.host_disconnect_deadline_ms = None;
            let p = HostMigratedPayload {
                previous_host_user_id: prev,
                new_host_user_id: new_host_id,
                summary: Some(Box::new(state.snapshot())),
            };
            return Some(RoomEvent::HostMigrated(p));
        }
    }

    state.state = RoomLifecycle::Ended;
    let p = RoomClosedPayload {
        reason: GRACE_REASON.into(),
    };
    Some(RoomEvent::RoomClosed(p))
}

pub(super) fn elect_new_host(state: &mut RoomState) -> Option<Uuid> {
    let mut candidates: Vec<&super::state::ParticipantRecord> = state
        .participants
        .iter()
        .filter(|p| {
            !p.is_host
                && matches!(
                    p.status,
                    ParticipantStatus::Connected | ParticipantStatus::Reconnecting
                )
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| {
        a.joined_ms
            .cmp(&b.joined_ms)
            .then_with(|| a.user_id.as_bytes().cmp(b.user_id.as_bytes()))
    });
    let new_host = candidates[0].user_id;
    let prev_host = state.host_user_id;
    for p in state.participants.iter_mut() {
        if p.user_id == new_host {
            p.is_host = true;
            p.cap_set = locast_protocol::room::cap::PLAYBACK_CONTROL
                | locast_protocol::room::cap::DRAW
                | locast_protocol::room::cap::LASER
                | locast_protocol::room::cap::MANAGE_ROOM
                | locast_protocol::room::cap::KICK
                | locast_protocol::room::cap::PUBLISH_MANIFEST
                | locast_protocol::room::cap::INVITE
                | locast_protocol::room::cap::CHAT;
        } else if p.is_host && p.user_id != new_host {
            p.is_host = false;
            p.cap_set = locast_protocol::room::cap::CHAT;
        }
    }
    state.host_user_id = new_host;
    debug!(
        room_id = %state.id,
        prev_host = %prev_host,
        new_host = %new_host,
        "host migrated"
    );
    Some(new_host)
}

fn try_migrate_host(_state: &mut RoomState) -> Option<Uuid> {
    let _prev = _state.host_user_id;
    // v1.1: implement transfer to new host here.
    // For v1, grace always ends the room.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::store::NoopRoomStore;

    fn cfg() -> super::super::registry::RoomRegistryConfig {
        super::super::registry::RoomRegistryConfig {
            max_participants: 8,
            host_disconnect_grace_ms: 200,
            participant_stale_after_ms: 300_000,
            participant_disconnect_after_ms: 15_000,
        }
    }

    fn keypair(i: u8) -> [u8; 32] {
        [i; 32]
    }

    fn uid(i: u8) -> uuid::Uuid {
        let mut b = [0u8; 16];
        b[0] = i;
        b[15] = i;
        uuid::Uuid::from_bytes(b)
    }

    fn store() -> NoopRoomStore {
        NoopRoomStore
    }

    #[tokio::test]
    async fn v1_grace_expiry_ends_room() {
        let registry = super::super::registry::RoomRegistry::new(cfg());
        let s = store();
        let (summary, _) = registry
            .create(&s, "X".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = registry
            .join(&s, &code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");

        let handle = registry.get_by_id(summary.id).await.expect("handle");
        {
            let mut state = handle.write().await;
            state.host_disconnect_deadline_ms = Some(1_000 + 200);
            state
                .participants
                .iter_mut()
                .find(|p| p.user_id == uid(1))
                .unwrap()
                .status = ParticipantStatus::Reconnecting;
        }

        let events = registry.tick_grace(&s, 1_201).await;
        assert_eq!(events.len(), 1);
        match &events[0].1 {
            RoomEvent::RoomClosed(p) => {
                assert_eq!(p.reason, "host_disconnected_no_migration");
            }
            other => panic!("expected RoomClosed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn grace_not_expired_no_events() {
        let registry = super::super::registry::RoomRegistry::new(cfg());
        let s = store();
        let (summary, _) = registry
            .create(&s, "X".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");

        let handle = registry.get_by_id(summary.id).await.expect("handle");
        {
            let mut state = handle.write().await;
            state.host_disconnect_deadline_ms = Some(1_000 + 200);
        }

        let events = registry.tick_grace(&s, 1_000 + 199).await;
        assert!(events.is_empty(), "grace not yet expired");
    }
}
