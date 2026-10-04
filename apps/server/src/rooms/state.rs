//! In-memory per-room state. The [`RoomState`] struct is held
//! behind a `tokio::sync::RwLock` inside the
//! [`super::registry::RoomRegistry`] so multiple connections
//! can read snapshots while one writer mutates.

#![forbid(unsafe_code)]

use locast_protocol::room::{Participant, ParticipantSelf, ParticipantStatus, RoomSummary};
use std::collections::{HashMap, VecDeque};
use uuid::Uuid;

/// P5-T02: per-stroke binding for the drawing protocol.
///
/// The server records `stroke_id -> (sender_id, sender_pubkey)`
/// when DRAW_BEGIN is accepted. Subsequent DRAW_POINT and
/// DRAW_END envelopes for the same stroke must come from
/// the same bearer identity. The binding is session-only;
/// `RoomState` is in-memory and the map is cleared on
/// `Ended` (the room's normal teardown path).
#[derive(Debug, Clone, Copy)]
pub struct PendingStroke {
    pub sender_id: Uuid,
    pub sender_pubkey: [u8; 32],
    pub started_ms: i64,
    /// P5-T03: a DRAW_CLEAR was accepted while this stroke was
    /// still open. The drawer's remaining DRAW_POINT / DRAW_END
    /// are accepted (so the drawer is not answered with a
    /// ROOM_ERROR) but are neither rebroadcast nor committed, so
    /// the cleared stroke cannot reappear on other screens or be
    /// undone later.
    pub cleared: bool,
}

/// Most committed (ended, visible) strokes the server remembers
/// per room. When a new stroke would exceed it the OLDEST
/// remembered stroke is forgotten: it stays on every screen but
/// can no longer be undone (DRAW_UNDO for it is a no-op).
/// DRAW_CLEAR still removes it from every screen.
pub const MAX_COMMITTED_STROKES: usize = 5_000;

#[derive(Debug, Default)]
pub struct StrokeBookkeeping {
    /// Live strokes keyed by `stroke_id`. The map is
    /// populated by DRAW_BEGIN and removed by DRAW_END.
    /// Stale entries are pruned by the room ticker (a
    /// stroke that has not received any point for
    /// `stroke_timeout_ms` after begin is GC'd; that
    /// timeout is a future task — P5-T02 only
    /// accumulates the bookkeeping).
    pub pending: HashMap<Uuid, PendingStroke>,
    /// P5-T03: committed (ended) strokes still visible in the
    /// room, `stroke_id -> owner user_id`. This is what DRAW_UNDO
    /// is authorized against. It is per room (it lives in this
    /// room's `RoomState`), bounded by [`MAX_COMMITTED_STROKES`],
    /// emptied by DRAW_CLEAR, and dropped with the room.
    committed: HashMap<Uuid, Uuid>,
    /// Commit order of `committed`, oldest first, for the cap.
    committed_order: VecDeque<Uuid>,
}

impl StrokeBookkeeping {
    /// Record `stroke_id` as a committed stroke owned by `owner`.
    /// Returns the ids forgotten to stay within the cap (oldest
    /// first; normally empty).
    pub fn commit(&mut self, stroke_id: Uuid, owner: Uuid) -> Vec<Uuid> {
        if self.committed.insert(stroke_id, owner).is_none() {
            self.committed_order.push_back(stroke_id);
        }
        let mut forgotten = Vec::new();
        while self.committed_order.len() > MAX_COMMITTED_STROKES {
            if let Some(old) = self.committed_order.pop_front() {
                self.committed.remove(&old);
                forgotten.push(old);
            }
        }
        forgotten
    }

    /// The owner of a committed stroke, `None` if it is unknown,
    /// still in progress, already removed, cleared or forgotten.
    pub fn owner_of(&self, stroke_id: &Uuid) -> Option<Uuid> {
        self.committed.get(stroke_id).copied()
    }

    /// Remove one committed stroke. `true` if it was present.
    pub fn remove_committed(&mut self, stroke_id: &Uuid) -> bool {
        if self.committed.remove(stroke_id).is_some() {
            self.committed_order.retain(|id| id != stroke_id);
            true
        } else {
            false
        }
    }

    /// DRAW_CLEAR: forget every committed stroke and mark every
    /// in-progress stroke as cleared. Returns how many committed
    /// strokes were forgotten.
    pub fn clear_all(&mut self) -> usize {
        let n = self.committed.len();
        self.committed.clear();
        self.committed_order.clear();
        for stroke in self.pending.values_mut() {
            stroke.cleared = true;
        }
        n
    }

    /// Number of remembered committed strokes.
    pub fn committed_len(&self) -> usize {
        self.committed.len()
    }
}

/// The mutable per-room state. Lives behind a
/// `tokio::sync::RwLock<RoomState>` so readers (snapshot
/// builds) don't block other readers and don't block writers
/// of other rooms.
#[derive(Debug)]
pub struct RoomState {
    pub id: Uuid,
    pub code: String,
    pub title: String,
    pub host_user_id: Uuid,
    pub host_pubkey: [u8; 32],
    pub host_migration_enabled: bool,
    pub created_ms: i64,
    pub state: RoomLifecycle,
    /// unix-ms when the host's transport was lost and the
    /// server started the 30s grace. `None` while the host is
    /// connected. The dispatch task waits up to the
    /// configured `host_disconnect_grace_ms` from this
    /// timestamp before electing a new host.
    pub host_disconnect_deadline_ms: Option<i64>,
    /// The current participants. The host is always first
    /// (or, more precisely, the entry whose `is_host` is
    /// `true` is the current host; the original creator is
    /// still the first joined).
    pub participants: Vec<ParticipantRecord>,
    /// P4-T01: per-room playback bookkeeping. The current
    /// lifecycle is `RoomLifecycle::Open` for the pre-play
    /// steady state, `Playing` after a host PLAY is accepted,
    /// and `Paused` after a host PAUSE is accepted (or after
    /// the first PLAY in a room whose host paused before any
    /// play). `server_seq` is the per-room monotonic counter
    /// of accepted playback commands; it is assigned by the
    /// server (see docs/ARCHITECTURE.md §13.1 step 2). It is
    /// not persisted across server restarts; v1 in-memory
    /// only.
    ///
    /// `last_acked_seq` tracks the last monotonic_seq the
    /// server has accepted from each sender (`user_id`). A
    /// command with `monotonic_seq <= last_acked_seq[sender]`
    /// is dropped as a duplicate; a command with
    /// `monotonic_seq > last_acked_seq[sender] + 1` is
    /// rejected as a gap. `last_acked_seq` is keyed by
    /// `user_id` (not `pubkey`) because the spec tracks the
    /// sender as `sender_id`; it survives host migration
    /// (a former host's stale commands remain in the table
    /// and continue to be rejected as duplicates, which is
    /// the simplest correct semantic — a demoted host's
    /// post-migration PLAYs cannot poison the new host's
    /// authoritative sequence).
    pub playback: PlaybackBookkeeping,
    /// P5-T02: per-room drawing bookkeeping. The
    /// `pending` map is populated by DRAW_BEGIN and
    /// removed by DRAW_END (or by the room ticker when
    /// a stroke times out; that path is added by a
    /// later task). The state itself does not grow
    /// meaningfully: a stroke is closed within seconds
    /// in normal use and the upper bound on concurrent
    /// strokes is bounded the by `max_participants` *
    /// reasonable per-user limit (which the renderer
    /// enforces client-side at a much smaller
    /// threshold).
    pub drawing: StrokeBookkeeping,
}

/// P4-T01: the per-room playback bookkeeping fields.
#[derive(Debug, Default)]
pub struct PlaybackBookkeeping {
    /// Per-room monotonic counter. Strictly increasing.
    /// First accepted command increments from 0 to 1.
    pub server_seq: u64,
    /// Per-sender last-acked monotonic_seq. Empty for a
    /// fresh room.
    pub last_acked_seq: HashMap<Uuid, u64>,
    /// Last accepted playback position (the room's
    /// authoritative playback position). `0` until the
    /// first PLAY or SEEK is accepted.
    pub last_position_ms: u64,
}

/// The room lifecycle. v1 had only `Open`/`Ended`; P4-T01
/// adds `Playing`/`Paused` to track the authoritative room
/// playback state machine (docs/ARCHITECTURE.md §11.1). The
/// transitions are:
///
/// - `Open -> Playing` on host `PLAY` accepted
/// - `Playing -> Paused` on host `PAUSE` accepted
/// - `Paused -> Playing` on host `PLAY` accepted
/// - `Playing -> Playing` on host `SEEK` accepted
/// - `Paused -> Paused` on host `SEEK` accepted
/// - any state -> `Ended` on `ROOM_CLOSED` / host migration
///   failure / etc.
///
/// `Ended` is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomLifecycle {
    Open,
    Playing,
    Paused,
    Ended,
}

impl RoomState {
    /// Build a brand-new room in the `Open` state.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Uuid,
        code: String,
        title: String,
        host_user_id: Uuid,
        host_pubkey: [u8; 32],
        host_migration_enabled: bool,
        created_ms: i64,
        cap_set: u32,
    ) -> Self {
        let host = ParticipantRecord {
            user_id: host_user_id,
            pubkey: host_pubkey,
            display_name: String::new(),
            joined_ms: created_ms,
            status: ParticipantStatus::Connected,
            last_seen_ms: created_ms,
            is_host: true,
            cap_set,
        };
        Self {
            id,
            code,
            title,
            host_user_id,
            host_pubkey,
            host_migration_enabled,
            created_ms,
            state: RoomLifecycle::Open,
            host_disconnect_deadline_ms: None,
            participants: vec![host],
            playback: PlaybackBookkeeping::default(),
            drawing: StrokeBookkeeping::default(),
        }
    }

    /// The current host record, if any. Returns `None` only
    /// if the participants list is empty (impossible in the
    /// `Open` state).
    pub fn host(&self) -> Option<&ParticipantRecord> {
        self.participants.iter().find(|p| p.is_host)
    }

    /// The current host record (mutable), if any.
    pub fn host_mut(&mut self) -> Option<&mut ParticipantRecord> {
        self.participants.iter_mut().find(|p| p.is_host)
    }

    /// `true` if the host's transport is currently considered
    /// disconnected (i.e. the grace timer is running).
    pub fn host_disconnected(&self) -> bool {
        self.host_disconnect_deadline_ms.is_some()
    }

    /// Build a public-facing summary.
    pub fn snapshot(&self) -> RoomSummary {
        RoomSummary {
            id: self.id,
            code: self.code.clone(),
            title: self.title.clone(),
            host_user_id: self.host_user_id,
            host_migration_enabled: self.host_migration_enabled,
            created_ms: self.created_ms,
            participants: self
                .participants
                .iter()
                .filter(|p| p.status != ParticipantStatus::Left)
                .map(ParticipantRecord::to_public)
                .collect(),
            host_disconnected: self.host_disconnected(),
            host_disconnect_deadline_ms: self.host_disconnect_deadline_ms,
        }
    }

    /// View of the caller's own participant record, for
    /// `ROOM_CREATED` / `ROOM_JOINED`.
    pub fn self_view(&self, user_id: Uuid) -> Option<ParticipantSelf> {
        self.participants
            .iter()
            .find(|p| p.user_id == user_id)
            .map(|p| ParticipantSelf {
                user_id: p.user_id,
                cap_set: p.cap_set,
                joined_ms: p.joined_ms,
            })
    }
}

/// The server-side participant record. Lifted to the public
/// [`Participant`] wire shape via [`ParticipantRecord::to_public`].
#[derive(Debug, Clone)]
pub struct ParticipantRecord {
    pub user_id: Uuid,
    pub pubkey: [u8; 32],
    pub display_name: String,
    pub joined_ms: i64,
    pub status: ParticipantStatus,
    pub last_seen_ms: i64,
    pub is_host: bool,
    pub cap_set: u32,
}

impl ParticipantRecord {
    /// The wire shape that other clients see.
    pub fn to_public(&self) -> Participant {
        Participant {
            user_id: self.user_id,
            pubkey: self.pubkey.to_vec(),
            display_name: self.display_name.clone(),
            joined_ms: self.joined_ms,
            status: self.status,
            last_seen_ms: self.last_seen_ms,
            is_host: self.is_host,
        }
    }
}

#[cfg(test)]
mod stroke_record_tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn commit_remembers_the_owner_and_remove_forgets_it() {
        let mut d = StrokeBookkeeping::default();
        assert!(d.commit(id(1), id(100)).is_empty());
        assert_eq!(d.owner_of(&id(1)), Some(id(100)));
        assert_eq!(d.committed_len(), 1);
        assert!(d.remove_committed(&id(1)));
        assert_eq!(d.owner_of(&id(1)), None);
        // Removing again (a replayed undo) is harmless.
        assert!(!d.remove_committed(&id(1)));
        assert_eq!(d.committed_len(), 0);
    }

    #[test]
    fn a_pending_stroke_is_not_a_committed_stroke() {
        let mut d = StrokeBookkeeping::default();
        d.pending.insert(
            id(7),
            PendingStroke {
                sender_id: id(100),
                sender_pubkey: [0; 32],
                started_ms: 0,
                cleared: false,
            },
        );
        assert_eq!(
            d.owner_of(&id(7)),
            None,
            "in-progress strokes are not undoable"
        );
    }

    #[test]
    fn the_record_is_bounded_and_forgets_the_oldest_first() {
        let mut d = StrokeBookkeeping::default();
        for n in 0..MAX_COMMITTED_STROKES as u128 {
            assert!(d.commit(id(n + 1), id(100)).is_empty());
        }
        assert_eq!(d.committed_len(), MAX_COMMITTED_STROKES);
        let forgotten = d.commit(id(1_000_000), id(101));
        assert_eq!(forgotten, vec![id(1)], "the oldest stroke is forgotten");
        assert_eq!(d.committed_len(), MAX_COMMITTED_STROKES);
        assert_eq!(d.owner_of(&id(1)), None);
        assert_eq!(d.owner_of(&id(2)), Some(id(100)));
        assert_eq!(d.owner_of(&id(1_000_000)), Some(id(101)));
        // An undone stroke frees its slot: no eviction on the next commit.
        assert!(d.remove_committed(&id(2)));
        assert!(d.commit(id(1_000_001), id(101)).is_empty());
        assert_eq!(d.committed_len(), MAX_COMMITTED_STROKES);
    }

    #[test]
    fn clear_all_empties_the_record_and_marks_open_strokes_cleared() {
        let mut d = StrokeBookkeeping::default();
        d.commit(id(1), id(100));
        d.commit(id(2), id(101));
        d.pending.insert(
            id(3),
            PendingStroke {
                sender_id: id(100),
                sender_pubkey: [0; 32],
                started_ms: 0,
                cleared: false,
            },
        );
        assert_eq!(d.clear_all(), 2);
        assert_eq!(d.committed_len(), 0);
        assert_eq!(d.owner_of(&id(1)), None);
        assert!(d.pending.get(&id(3)).expect("still pending").cleared);
        // A second clear is harmless.
        assert_eq!(d.clear_all(), 0);
    }

    #[test]
    fn a_new_room_state_starts_with_an_empty_record() {
        let state = RoomState::new(
            id(1),
            "AAAAAA".into(),
            "T".into(),
            id(2),
            [1; 32],
            true,
            0,
            0,
        );
        assert_eq!(state.drawing.committed_len(), 0);
        assert!(state.drawing.pending.is_empty());
    }
}
