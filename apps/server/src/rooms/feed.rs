//! A connection's subscription to one room's broadcast channel,
//! with recovery for dropped drawing events.
//!
//! The per-room channel is a bounded `tokio::sync::broadcast` ring.
//! A subscriber that falls more than the ring's capacity behind
//! loses the oldest items and is told so once (`RecvError::Lagged`).
//! Dropping a DRAW_UNDO or DRAW_CLEAR (or part of a stroke) that way
//! would leave the client's canvas permanently different from the
//! room's, so after a lag that may have cost a drawing event the
//! feed fetches the room's authoritative drawing snapshot
//! (DRAW_SYNC) and hands that to the client.
//!
//! Why this converges: every drawing event is applied to the room's
//! state and published, with the room's next drawing sequence number
//! (contiguous per room), under the room's write lock; the snapshot
//! is built under its read lock. So the snapshot taken at sequence
//! `S` contains exactly the effects of every event numbered `<= S`.
//! After sending it the feed skips every queued drawing item with
//! `seq <= S` and forwards the rest in order: the client ends with
//! snapshot(S) plus the events `> S`, which is the room's state.
//!
//! When a lag is harmless: the lost items are the oldest ones, so if
//! the first item received after the lag is a drawing event numbered
//! at most one past what the client already has (delivered or
//! covered by a snapshot), no drawing event the client lacks was
//! lost and no snapshot is sent. If the first item is not a drawing
//! event, the room's current drawing sequence decides: past what the
//! client has means a drawing event (maybe a trailing DRAW_UNDO) was
//! lost; otherwise only non-drawing items (e.g. transient lasers)
//! were. On a loss, a snapshot is sent before that item.

#![forbid(unsafe_code)]

use locast_protocol::room::StrokeSyncPayload;
use tokio::sync::broadcast;
use uuid::Uuid;

use super::registry::{BroadcastItem, RoomRegistry};

/// Shortest gap between two DRAW_SYNC snapshots for one subscriber.
/// A subscriber that keeps lagging gets at most one snapshot per
/// interval instead of one per ring overflow (a snapshot can be a
/// few MB). A delayed snapshot still converges: it reflects
/// everything up to the moment it is built.
pub const MIN_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// What [`RoomFeed::next`] produced.
#[derive(Debug)]
pub enum FeedItem {
    /// A broadcast item to forward (subject to the caller's usual
    /// per-item filters).
    Event(BroadcastItem),
    /// Drawing events were dropped for this subscriber: send this
    /// snapshot of the room's drawing state instead.
    DrawSync(StrokeSyncPayload),
    /// The room's channel is gone.
    Closed,
}

/// One connection's subscription to one room.
pub struct RoomFeed {
    room_id: Uuid,
    rx: broadcast::Receiver<BroadcastItem>,
    min_sync_interval: std::time::Duration,
    last_sync: Option<tokio::time::Instant>,
    /// Drawing sequence covered by the last DRAW_SYNC sent: queued
    /// drawing items at or below it are skipped.
    draw_floor: u64,
    /// Highest drawing sequence the client has (delivered or covered).
    last_seen: u64,
    /// `RecvError::Lagged` was reported and the first item after it
    /// has not been examined yet.
    lagged: bool,
    /// A DRAW_SYNC must be produced before anything else.
    sync_pending: bool,
    /// The item that revealed a loss, delivered after the DRAW_SYNC.
    held: Option<BroadcastItem>,
}

impl RoomFeed {
    pub fn new(room_id: Uuid, rx: broadcast::Receiver<BroadcastItem>) -> Self {
        Self::with_min_sync_interval(room_id, rx, MIN_SYNC_INTERVAL)
    }

    /// [`RoomFeed::new`] with a custom snapshot interval (tests).
    pub fn with_min_sync_interval(
        room_id: Uuid,
        rx: broadcast::Receiver<BroadcastItem>,
        min_sync_interval: std::time::Duration,
    ) -> Self {
        Self {
            room_id,
            rx,
            min_sync_interval,
            last_sync: None,
            draw_floor: 0,
            last_seen: 0,
            lagged: false,
            sync_pending: false,
            held: None,
        }
    }

    pub fn room_id(&self) -> Uuid {
        self.room_id
    }

    /// The next item for this subscriber. Every piece of progress is
    /// stored in `self` before an await, so dropping the future (the
    /// forwarder's `select!`) loses nothing.
    pub async fn next(&mut self, rooms: &RoomRegistry) -> FeedItem {
        loop {
            if self.sync_pending {
                if let Some(last) = self.last_sync {
                    // Rate limit. Items published meanwhile are either
                    // in the snapshot or follow it.
                    tokio::time::sleep_until(last + self.min_sync_interval).await;
                }
                let snapshot = rooms.drawing_snapshot(self.room_id).await;
                self.sync_pending = false;
                self.last_sync = Some(tokio::time::Instant::now());
                if let Some(snapshot) = snapshot {
                    self.draw_floor = self.draw_floor.max(snapshot.seq);
                    self.last_seen = self.last_seen.max(snapshot.seq);
                    return FeedItem::DrawSync(snapshot);
                }
                // The room is gone; its channel closes next.
            }
            let mut item = match self.held.take() {
                Some(item) => item,
                None => match self.rx.recv().await {
                    Ok(item) => item,
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(
                            room_id = %self.room_id,
                            missed,
                            "room subscriber fell behind"
                        );
                        self.lagged = true;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return FeedItem::Closed,
                },
            };
            if self.lagged {
                let have = self.draw_floor.max(self.last_seen);
                let lost_drawing = if item.seq == 0 {
                    // A non-drawing item (e.g. a laser) revealed the
                    // lag. A drawing event was lost only if the room
                    // has published one the client lacks: numbers are
                    // contiguous and in publish order, so that is
                    // exactly "the room's drawing sequence is past
                    // `have`". (Without this check, transient laser
                    // traffic would turn every lag into a snapshot.)
                    // Park the item first so a cancelled await loses
                    // nothing; `lagged` stays set until decided.
                    self.held = Some(item);
                    let current = rooms.drawing_seq(self.room_id).await;
                    item = self.held.take().expect("parked above");
                    current.is_some_and(|seq| seq > have)
                } else {
                    // Contiguous drawing numbers: anything lost was
                    // older than `item`, so it is covered iff
                    // `item.seq <= have + 1`.
                    item.seq > have + 1
                };
                self.lagged = false;
                if lost_drawing {
                    self.sync_pending = true;
                    self.held = Some(item);
                    continue;
                }
            }
            if item.seq != 0 {
                if item.seq <= self.draw_floor {
                    // Already part of a DRAW_SYNC sent.
                    continue;
                }
                self.last_seen = self.last_seen.max(item.seq);
            }
            return FeedItem::Event(item);
        }
    }
}
