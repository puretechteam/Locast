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

/// Most DRAW_POINTs whose content the server keeps per room for
/// DRAW_SYNC snapshots (about 1 MB in memory, a few MB as JSON).
/// When a new point would
/// exceed it, the content of the OLDEST committed strokes is
/// dropped first; if only strokes in progress remain, the new
/// point's stroke stops being kept. A stroke whose content was
/// dropped is still on every screen and still undoable; a DRAW_SYNC
/// lists it without content and a client that has it keeps its
/// copy.
pub const MAX_RETAINED_POINTS: usize = 50_000;

/// Most DRAW_POINTs kept for one stroke (the protocol's per-stroke
/// limit, architecture §15.8).
pub const MAX_RETAINED_POINTS_PER_STROKE: usize = 10_000;

/// Longest DRAW_BEGIN `color` whose stroke content is kept (CSS
/// colors are short). A stroke with a longer color is still relayed
/// and committed as before; it is only listed without content in a
/// DRAW_SYNC, so a client cannot make the server hold large strings.
pub const MAX_RETAINED_COLOR_BYTES: usize = 64;

/// Most strokes one participant may have open (begun, not yet ended)
/// at once. A real client has one: a stroke ends at pointer-up. The
/// slack covers a lost DRAW_END followed by a new stroke.
pub const MAX_OPEN_STROKES_PER_USER: usize = 8;

/// Most strokes open at once in one room. Participants are capped, so
/// this is only reached by strokes whose owner left without ending
/// them.
pub const MAX_OPEN_STROKES_PER_ROOM: usize = 64;

/// The content of one stroke, kept for DRAW_SYNC.
#[derive(Debug, Clone)]
struct StrokeContent {
    owner: Uuid,
    begin: locast_protocol::room::StrokeBeginPayload,
    points: Vec<locast_protocol::room::StrokeSyncPoint>,
    end_ts_ms: Option<i64>,
    /// Content dropped (budget or per-stroke limit): the stroke is
    /// listed in a snapshot without content from then on.
    dropped: bool,
}

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
    /// The room's drawing sequence: the number of drawing events
    /// (BEGIN / POINT / END / UNDO / CLEAR) published so far. Each
    /// published event carries the value it advanced this to.
    seq: u64,
    /// Content of every stroke on the canvas (pending, not cleared,
    /// and committed), for DRAW_SYNC.
    content: HashMap<Uuid, StrokeContent>,
    /// Points currently held in `content` (bounded by
    /// [`MAX_RETAINED_POINTS`]).
    retained_points: usize,
    /// Committed strokes in commit order, for evicting the oldest
    /// content first in amortized O(1). May hold ids whose content
    /// is already gone; those are skipped.
    evict_order: VecDeque<Uuid>,
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
        // A forgotten stroke stays on screens but is no longer
        // tracked; drop its content too.
        for old in &forgotten {
            self.forget_content(old);
        }
        forgotten
    }

    /// Bookkeeping for a room created or restored now. The drawing
    /// sequence starts from the wall clock (ms x 1000) rather than 0,
    /// so a room restored after a server restart never numbers its
    /// events below what clients already applied (they ignore events
    /// at or below their last applied seq). The values stay below
    /// 2^53, exact in JavaScript.
    pub fn new() -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            seq: now_ms.saturating_mul(1000),
            ..Self::default()
        }
    }

    /// Advance the room's drawing sequence and return the new
    /// value. Called once per published drawing event, under the
    /// room write lock, in publish order.
    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// The last drawing sequence number published.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Keep the content of a stroke DRAW_BEGIN just opened.
    pub fn record_begin(&mut self, owner: Uuid, begin: &locast_protocol::room::StrokeBeginPayload) {
        self.forget_content(&begin.stroke_id);
        let oversized = begin.color.len() > MAX_RETAINED_COLOR_BYTES;
        let mut kept = begin.clone();
        if oversized {
            kept.color = String::new();
        }
        self.content.insert(
            begin.stroke_id,
            StrokeContent {
                owner,
                begin: kept,
                points: Vec::new(),
                end_ts_ms: None,
                dropped: oversized,
            },
        );
    }

    /// Keep one accepted DRAW_POINT, within the budgets.
    pub fn record_point(&mut self, point: &locast_protocol::room::StrokePointPayload) {
        let Some(c) = self.content.get(&point.stroke_id) else {
            return;
        };
        if c.dropped {
            return;
        }
        if c.points.len() >= MAX_RETAINED_POINTS_PER_STROKE {
            self.drop_content(&point.stroke_id);
            self.mark_dropped(point.stroke_id);
            return;
        }
        // Make room: oldest committed strokes' content goes first.
        while self.retained_points >= MAX_RETAINED_POINTS {
            let mut victim = None;
            while let Some(id) = self.evict_order.pop_front() {
                let holds_points = self.committed.contains_key(&id)
                    && self
                        .content
                        .get(&id)
                        .is_some_and(|c| !c.dropped && !c.points.is_empty());
                if holds_points {
                    victim = Some(id);
                    break;
                }
            }
            match victim {
                Some(id) => {
                    self.drop_content(&id);
                    self.mark_dropped(id);
                }
                None => {
                    // Only strokes in progress hold content: stop
                    // keeping this one.
                    self.drop_content(&point.stroke_id);
                    self.mark_dropped(point.stroke_id);
                    return;
                }
            }
        }
        if let Some(c) = self.content.get_mut(&point.stroke_id) {
            c.points.push(locast_protocol::room::StrokeSyncPoint {
                x: point.x,
                y: point.y,
                pressure: point.pressure,
                ts_ms: point.ts_ms,
            });
            self.retained_points += 1;
        }
    }

    /// Record a committed stroke's DRAW_END timestamp.
    pub fn record_end(&mut self, stroke_id: &Uuid, ts_ms: i64) {
        if let Some(c) = self.content.get_mut(stroke_id) {
            c.end_ts_ms = Some(ts_ms);
            self.evict_order.push_back(*stroke_id);
        }
        // Keep the queue from collecting stale ids (undone /
        // forgotten strokes) without bound.
        if self.evict_order.len() > 2 * MAX_COMMITTED_STROKES {
            let committed = &self.committed;
            self.evict_order.retain(|id| committed.contains_key(id));
        }
    }

    /// Make room for one more stroke that `user` is about to begin.
    ///
    /// Only DRAW_END removes an entry from `pending`, so a client that
    /// begins strokes and never ends them (or one that disconnects
    /// mid-stroke) would otherwise grow `pending` and `content`
    /// without bound, and every DRAW_SYNC snapshot lists them all.
    /// While `user` has [`MAX_OPEN_STROKES_PER_USER`] strokes open, or
    /// the room has [`MAX_OPEN_STROKES_PER_ROOM`], finish the OLDEST
    /// one (that user's, then the room's) as DRAW_END would: it is
    /// committed and stays on every screen. A cleared stroke is just
    /// forgotten.
    ///
    /// Returns `(stroke_id, owner)` for each stroke that was committed,
    /// oldest first, so the caller can broadcast its DRAW_END and
    /// every client closes it too.
    pub fn finish_oldest_open_strokes(&mut self, user: Uuid, now_ms: i64) -> Vec<(Uuid, Uuid)> {
        let mut finished = Vec::new();
        loop {
            let user_open = self
                .pending
                .values()
                .filter(|p| p.sender_id == user)
                .count();
            let over_user = user_open >= MAX_OPEN_STROKES_PER_USER;
            let over_room = self.pending.len() >= MAX_OPEN_STROKES_PER_ROOM;
            if !over_user && !over_room {
                return finished;
            }
            let victim = self
                .pending
                .iter()
                .filter(|(_, p)| !over_user || p.sender_id == user)
                .min_by_key(|(id, p)| (p.started_ms, **id))
                .map(|(id, p)| (*id, *p));
            let Some((stroke_id, stroke)) = victim else {
                return finished;
            };
            self.pending.remove(&stroke_id);
            if stroke.cleared {
                self.forget_content(&stroke_id);
            } else {
                self.commit(stroke_id, stroke.sender_id);
                self.record_end(&stroke_id, now_ms);
                finished.push((stroke_id, stroke.sender_id));
            }
        }
    }

    /// Forget a stroke's content entirely (undone, cleared,
    /// cleared-while-open, forgotten).
    pub fn forget_content(&mut self, stroke_id: &Uuid) {
        if let Some(c) = self.content.remove(stroke_id) {
            self.retained_points -= c.points.len();
        }
    }

    /// Free a stroke's points but keep its entry (marked dropped).
    fn drop_content(&mut self, stroke_id: &Uuid) {
        if let Some(c) = self.content.get_mut(stroke_id) {
            self.retained_points -= c.points.len();
            c.points = Vec::new();
        }
    }

    fn mark_dropped(&mut self, stroke_id: Uuid) {
        if let Some(c) = self.content.get_mut(&stroke_id) {
            c.dropped = true;
        }
    }

    /// The room's drawing state as of [`Self::seq`], for DRAW_SYNC:
    /// committed strokes in commit order, then strokes in progress
    /// (not cleared) in the order they began.
    pub fn snapshot(&self) -> locast_protocol::room::StrokeSyncPayload {
        use locast_protocol::room::StrokeSyncStroke;
        let entry = |id: &Uuid, owner: Uuid| -> StrokeSyncStroke {
            match self.content.get(id) {
                Some(c) if !c.dropped => StrokeSyncStroke {
                    stroke_id: *id,
                    owner_id: c.owner,
                    begin: Some(c.begin.clone()),
                    points: c.points.clone(),
                    end_ts_ms: c.end_ts_ms,
                },
                // Content dropped: list the stroke without it (whether
                // it has ended is still known).
                other => StrokeSyncStroke {
                    stroke_id: *id,
                    owner_id: owner,
                    begin: None,
                    points: Vec::new(),
                    end_ts_ms: other.and_then(|c| c.end_ts_ms),
                },
            }
        };
        let mut strokes: Vec<StrokeSyncStroke> = self
            .committed_order
            .iter()
            .filter_map(|id| self.committed.get(id).map(|owner| entry(id, *owner)))
            .collect();
        let mut open: Vec<(&Uuid, &PendingStroke)> =
            self.pending.iter().filter(|(_, p)| !p.cleared).collect();
        open.sort_by(|a, b| a.1.started_ms.cmp(&b.1.started_ms).then(a.0.cmp(b.0)));
        strokes.extend(open.into_iter().map(|(id, p)| entry(id, p.sender_id)));
        locast_protocol::room::StrokeSyncPayload {
            seq: self.seq,
            strokes,
        }
    }

    /// Points currently retained for DRAW_SYNC (tests / metrics).
    pub fn retained_points(&self) -> usize {
        self.retained_points
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
            self.forget_content(stroke_id);
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
        // Nothing is on the canvas any more.
        self.content.clear();
        self.retained_points = 0;
        self.evict_order.clear();
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
            drawing: StrokeBookkeeping::new(),
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
            .rev()
            .find(|p| p.user_id == user_id && p.status != ParticipantStatus::Left)
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
