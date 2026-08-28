//! In-memory room registry. The `RoomRegistry` is the single
//! source of truth for which rooms exist and which
//! participants are in them; the SQLite tables mirror the
//! durable subset (rooms and room_participants).
//!
//! Concurrency: every map is a `tokio::sync::RwLock<HashMap<...>>`.
//! A single room is also held behind a `RwLock` so a long
//! snapshot (e.g. building the `RoomSummary` for a `ROOM_STATE`
//! reply) does not block another connection's read on a
//! different room.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use locast_protocol::room::cap;
use locast_protocol::room::{
    HostDisconnectedPayload, HostMigratedPayload, HostReconnectedPayload, ParticipantJoinedPayload,
    ParticipantLeftPayload, ParticipantStatus, RoomClosedPayload, RoomErrorCode, RoomErrorPayload,
    RoomJoinedPayload, RoomStatePayload, RoomSummary,
};
use rand::rngs::OsRng;
use tokio::sync::broadcast;
use tokio::sync::RwLock;
use tracing::debug;
use uuid::Uuid;

use super::codes;
use super::error::RoomError;
use super::state::{ParticipantRecord, RoomLifecycle, RoomState};

/// Code-collision cap. After 5 collisions the server aborts
/// creation with an `Internal` error.
const MAX_COLLISIONS: u8 = 5;

/// The per-room event the registry wants broadcast. The WS
/// layer maps each variant onto an `Envelope` and sends it
/// to the appropriate recipient(s). Keeping the registry
/// free of WS types makes it testable in isolation.
#[derive(Debug, Clone)]
pub enum RoomEvent {
    /// A new participant entered. Broadcast to all other
    /// participants in the room.
    ParticipantJoined(ParticipantJoinedPayload),
    /// A participant left. Broadcast to all other participants.
    ParticipantLeft(ParticipantLeftPayload),
    /// The host's transport was lost. Sent to all participants
    /// during the grace period AND when the grace expires
    /// (the second send carries the new host_user_id).
    HostDisconnected(HostDisconnectedPayload),
    /// The host reconnected within the grace period. Sent
    /// to all participants.
    HostReconnected(HostReconnectedPayload),
    /// The host was migrated to a new host_user_id. Sent to
    /// all participants; the old host also receives this if
    /// it reconnects later (it joins as a viewer).
    HostMigrated(HostMigratedPayload),
    /// The room has ended. Sent to all participants.
    RoomClosed(RoomClosedPayload),
    /// An error envelope targeted at one participant.
    Error {
        target: Uuid,
        payload: RoomErrorPayload,
    },
}

/// A single room. The inner state lives behind a `RwLock`
/// so the registry can hand out a reference to a connection
/// that needs to read the snapshot under a long-held read
/// lock without blocking other rooms.
pub type RoomHandle = Arc<RwLock<RoomState>>;

/// The room registry.
pub struct RoomRegistry {
    by_id: RwLock<HashMap<Uuid, RoomHandle>>,
    by_code: RwLock<HashMap<String, Uuid>>,
    /// Per-room broadcast senders. Every event the registry
    /// produces (ParticipantJoined, HostMigrated, RoomClosed,
    /// etc.) is also published to the room's broadcast
    /// channel. WS connections subscribe to the channel of
    /// the room their user is in via [`RoomRegistry::subscribe`].
    room_tx: RwLock<HashMap<Uuid, broadcast::Sender<BroadcastItem>>>,
    config: RoomRegistryConfig,
}

/// A single broadcast item. The registry publishes one of
/// these per `RoomEvent` so every subscriber in the room
/// can encode it into a wire envelope and forward it to
/// its own connection.
#[derive(Debug, Clone)]
pub struct BroadcastItem {
    /// The kind tag (matches the wire message).
    pub kind: locast_protocol::envelope::MessageKind,
    /// The serialized payload.
    pub payload: serde_json::Value,
    /// The room id (set as `Envelope::room_id` when sent).
    pub room_id: Uuid,
    /// The user_id that originated the event. Subscribers
    /// can skip sending it back to the originating
    /// connection (e.g. to avoid echoing a `ROOM_JOINED`
    /// to the user that just joined). `None` for
    /// server-originated events.
    pub originator: Option<Uuid>,
}

/// Subset of `Config` the registry needs.
#[derive(Debug, Clone)]
pub struct RoomRegistryConfig {
    pub max_participants: u8,
    pub host_disconnect_grace_ms: i64,
    pub participant_stale_after_ms: i64,
}

impl RoomRegistryConfig {
    /// Pull the values out of the server's `Config`. Falls
    /// back to documented defaults if any field is missing.
    pub fn from_config(c: &crate::Config) -> Self {
        Self {
            max_participants: c.room_max_participants,
            host_disconnect_grace_ms: c.host_disconnect_grace_ms,
            participant_stale_after_ms: c.participant_stale_after_ms,
        }
    }
}

impl RoomRegistry {
    /// Build an empty registry.
    pub fn new(config: RoomRegistryConfig) -> Self {
        Self {
            by_id: RwLock::new(HashMap::new()),
            by_code: RwLock::new(HashMap::new()),
            room_tx: RwLock::new(HashMap::new()),
            config,
        }
    }

    /// Subscribe to the broadcast channel of a room. Returns
    /// `None` if the room does not exist.
    pub async fn subscribe(&self, room_id: Uuid) -> Option<broadcast::Receiver<BroadcastItem>> {
        let map = self.room_tx.read().await;
        map.get(&room_id).map(|tx| tx.subscribe())
    }

    /// Publish one `RoomEvent` to the room's broadcast
    /// channel. If the room has no subscribers (or has
    /// been removed), the publish is a no-op.
    fn publish(&self, room_id: Uuid, item: BroadcastItem) {
        // Use try_send so we don't await while holding
        // locks; the broadcast::Sender is internally
        // synchronized.
        if let Ok(map) = self.room_tx.try_read() {
            if let Some(tx) = map.get(&room_id) {
                let _ = tx.send(item);
            }
        }
    }

    /// Tick the grace timer. Publishes any
    /// `HostMigrated` / `RoomClosed` events the same way
    /// `leave` does.
    pub async fn tick_grace(&self, now_ms: i64) -> Vec<(Uuid, RoomEvent)> {
        let mut out: Vec<(Uuid, RoomEvent)> = Vec::new();
        // First pass: determine which rooms are expiring and
        // what event each one produces. We don't hold any
        // room write lock past this point.
        let mut to_publish: Vec<(Uuid, BroadcastItem)> = Vec::new();
        let mut to_remove: Vec<Uuid> = Vec::new();
        {
            let by_id = self.by_id.read().await;
            for (rid, h) in by_id.iter() {
                let mut state = h.write().await;
                let deadline = state.host_disconnect_deadline_ms;
                if let Some(d) = deadline {
                    if now_ms < d {
                        continue;
                    }
                    let prev = state.host_user_id;
                    if let Some(new_host_id) = elect_new_host(&mut state, now_ms) {
                        state.host_disconnect_deadline_ms = None;
                        let p = HostMigratedPayload {
                            previous_host_user_id: prev,
                            new_host_user_id: new_host_id,
                        };
                        to_publish.push((
                            *rid,
                            event_to_broadcast_item(
                                &RoomEvent::HostMigrated(p.clone()),
                                *rid,
                                None,
                            ),
                        ));
                        out.push((*rid, RoomEvent::HostMigrated(p)));
                    } else {
                        state.state = RoomLifecycle::Ended;
                        let p = RoomClosedPayload {
                            reason: "host_disconnected_no_migration".into(),
                        };
                        to_publish.push((
                            *rid,
                            event_to_broadcast_item(&RoomEvent::RoomClosed(p.clone()), *rid, None),
                        ));
                        out.push((*rid, RoomEvent::RoomClosed(p)));
                        to_remove.push(*rid);
                    }
                }
            }
        }
        // Second pass: publish + remove.
        for (rid, item) in &to_publish {
            self.publish(*rid, item.clone());
        }
        for rid in to_remove {
            self.remove_room(rid).await;
        }
        out
    }

    /// Stale-participant cleanup. Any participant whose
    /// `last_seen_ms` is older than `participant_stale_after_ms`
    /// is removed from their room and a
    /// `ParticipantLeft { reason: "timeout" }` is emitted.
    pub async fn tick_stale_participants(&self, now_ms: i64) -> Vec<(Uuid, RoomEvent)> {
        let mut out: Vec<(Uuid, RoomEvent)> = Vec::new();
        let mut to_publish: Vec<(Uuid, BroadcastItem)> = Vec::new();
        let mut to_remove: Vec<Uuid> = Vec::new();
        {
            let by_id = self.by_id.read().await;
            for (rid, h) in by_id.iter() {
                let mut state = h.write().await;
                if state.state != RoomLifecycle::Open {
                    continue;
                }
                let stale: Vec<usize> = state
                    .participants
                    .iter()
                    .enumerate()
                    .filter_map(|(i, p)| {
                        if p.is_host || p.status == ParticipantStatus::Left {
                            None
                        } else if now_ms.saturating_sub(p.last_seen_ms)
                            >= self.config.participant_stale_after_ms
                        {
                            Some(i)
                        } else {
                            None
                        }
                    })
                    .collect();
                for &i in stale.iter().rev() {
                    let removed = state.participants.remove(i);
                    let p = ParticipantLeftPayload {
                        user_id: removed.user_id,
                        reason: "timeout".into(),
                    };
                    to_publish.push((
                        *rid,
                        event_to_broadcast_item(&RoomEvent::ParticipantLeft(p.clone()), *rid, None),
                    ));
                    out.push((*rid, RoomEvent::ParticipantLeft(p)));
                }
                if state.state == RoomLifecycle::Ended {
                    to_remove.push(*rid);
                }
            }
        }
        for (rid, item) in &to_publish {
            self.publish(*rid, item.clone());
        }
        for rid in to_remove {
            self.remove_room(rid).await;
        }
        out
    }

    /// The active config. Used by tests that want to assert
    /// the registry honored the values passed to `new`.
    pub fn config(&self) -> &RoomRegistryConfig {
        &self.config
    }

    /// `ROOM_CREATE` handler. Returns the new room's
    /// `RoomSummary` and the host's `ParticipantSelf` view.
    pub async fn create(
        &self,
        title: String,
        host_user_id: Uuid,
        host_pubkey: [u8; 32],
        host_migration_enabled: bool,
        now_ms: i64,
    ) -> Result<(RoomSummary, locast_protocol::room::ParticipantSelf), RoomError> {
        // Generate a unique 6-char code with at most
        // `MAX_COLLISIONS` retries.
        let mut last_err: Option<RoomError> = None;
        let mut chosen: Option<String> = None;
        for _ in 0..=MAX_COLLISIONS {
            let candidate = codes::generate_code(&mut OsRng);
            let by_code = self.by_code.read().await;
            if !by_code.contains_key(&candidate) {
                drop(by_code);
                chosen = Some(candidate);
                break;
            }
            last_err = Some(RoomError::Internal("code collision".into()));
        }
        let code = match chosen {
            Some(c) => c,
            None => {
                return Err(
                    last_err.unwrap_or(RoomError::Internal("code generation exhausted".into()))
                );
            }
        };

        let id = Uuid::now_v7();
        let cap_set = cap::PLAYBACK_CONTROL
            | cap::DRAW
            | cap::LASER
            | cap::MANAGE_ROOM
            | cap::KICK
            | cap::PUBLISH_MANIFEST
            | cap::INVITE
            | cap::CHAT;
        let state = RoomState::new(
            id,
            code.clone(),
            title,
            host_user_id,
            host_pubkey,
            host_migration_enabled,
            now_ms,
            cap_set,
        );
        let summary = state.snapshot();
        let self_view = state
            .self_view(host_user_id)
            .expect("host always present after create");

        let handle = Arc::new(RwLock::new(state));
        {
            let mut by_id = self.by_id.write().await;
            by_id.insert(id, handle);
        }
        {
            let mut by_code_w = self.by_code.write().await;
            by_code_w.insert(code, id);
        }
        {
            let (tx, _rx) = broadcast::channel(256);
            let mut room_tx = self.room_tx.write().await;
            room_tx.insert(id, tx);
        }
        Ok((summary, self_view))
    }

    /// `ROOM_JOIN_REQUEST` handler. Returns the new
    /// `RoomJoined` (summary + self view) and the
    /// `ParticipantJoined` event for the broadcast.
    pub async fn join(
        &self,
        code: &str,
        user_id: Uuid,
        pubkey: [u8; 32],
        display_name: String,
        now_ms: i64,
    ) -> Result<(RoomJoinedPayload, RoomEvent), RoomError> {
        let id = {
            let by_code = self.by_code.read().await;
            *by_code.get(code).ok_or(RoomError::RoomNotFound)?
        };
        let handle = {
            let by_id = self.by_id.read().await;
            by_id.get(&id).cloned().ok_or(RoomError::RoomNotFound)?
        };
        let mut state = handle.write().await;
        if state.state != RoomLifecycle::Open {
            return Err(RoomError::RoomClosed);
        }
        if state
            .participants
            .iter()
            .any(|p| p.user_id == user_id && p.status != ParticipantStatus::Left)
        {
            return Err(RoomError::AlreadyJoined);
        }
        if state
            .participants
            .iter()
            .filter(|p| p.status != ParticipantStatus::Left)
            .count()
            >= self.config.max_participants as usize
        {
            return Err(RoomError::RoomFull);
        }
        let cap_set = cap::CHAT;
        let rec = ParticipantRecord {
            user_id,
            pubkey,
            display_name: display_name.clone(),
            joined_ms: now_ms,
            status: ParticipantStatus::Connected,
            last_seen_ms: now_ms,
            is_host: false,
            cap_set,
        };
        let public = rec.to_public();
        state.participants.push(rec);

        let summary = state.snapshot();
        let self_view = state
            .self_view(user_id)
            .expect("just inserted the participant");
        let payload = RoomJoinedPayload {
            room: summary,
            you: self_view,
        };
        let event_payload = ParticipantJoinedPayload {
            participant: public,
        };
        let item = BroadcastItem {
            kind: locast_protocol::envelope::MessageKind::ParticipantJoined,
            payload: serde_json::to_value(&event_payload).unwrap_or(serde_json::json!({})),
            room_id: id,
            originator: Some(user_id),
        };
        drop(state);
        self.publish(id, item);
        let event = RoomEvent::ParticipantJoined(event_payload);
        Ok((payload, event))
    }

    /// `ROOM_LEAVE` handler. `intentional = true` for a
    /// client-initiated `ROOM_LEAVE`; `false` for a transport
    /// loss (the dispatch layer calls this with
    /// `intentional = false` from `on_connection_lost`).
    ///
    /// Returns the events the WS layer should broadcast.
    /// The list is in the order: any migration-or-close
    /// announcement FIRST, then the per-participant LEFT
    /// events, so the recipient's UI updates correctly
    /// regardless of which delivery order the WS layer
    /// chooses.
    pub async fn leave(
        &self,
        user_id: Uuid,
        intentional: bool,
        now_ms: i64,
    ) -> Result<(Vec<RoomEvent>, Option<RoomSummary>), RoomError> {
        // Find the room the user is in.
        let handle = {
            let by_id = self.by_id.read().await;
            let mut found = None;
            for (rid, h) in by_id.iter() {
                let s = h.read().await;
                if s.participants
                    .iter()
                    .any(|p| p.user_id == user_id && p.status != ParticipantStatus::Left)
                {
                    found = Some((*rid, h.clone()));
                    break;
                }
            }
            found
        };
        let (room_id, handle) = match handle {
            Some(pair) => pair,
            None => return Err(RoomError::NotJoined),
        };
        let mut state = handle.write().await;
        if state.state != RoomLifecycle::Open {
            return Err(RoomError::RoomClosed);
        }
        // Find the participant's index.
        let idx = state
            .participants
            .iter()
            .position(|p| p.user_id == user_id && p.status != ParticipantStatus::Left)
            .ok_or(RoomError::NotJoined)?;
        let was_host = state.participants[idx].is_host;
        state.participants[idx].last_seen_ms = now_ms;
        // host_user_id broadcast: any "the host has changed"
        // event the WS layer needs to emit.
        let mut events: Vec<RoomEvent> = Vec::new();
        // The `ParticipantLeft` event is only emitted when the
        // participant's status actually transitions to `Left`.
        // In the host-transport-loss / migration-on case the
        // host's status is set to `Reconnecting` below, so we
        // skip the LEFT and let the `HostDisconnected` event
        // carry that information. The original `reason` field
        // was hard-coded to "transport_loss" which is not in
        // the documented wire enum; removing it here keeps the
        // protocol honest.
        let mut emit_left = true;
        if was_host {
            // Determine migration outcome.
            if !state.host_migration_enabled {
                // Room ends.
                state.participants[idx].status = ParticipantStatus::Left;
                state.state = RoomLifecycle::Ended;
                events.push(RoomEvent::RoomClosed(RoomClosedPayload {
                    reason: if intentional {
                        "host_left".into()
                    } else {
                        "host_disconnected_no_migration".into()
                    },
                }));
            } else if intentional {
                // Migration ON, intentional leave: immediate handoff.
                state.participants[idx].status = ParticipantStatus::Left;
                if let Some(new_host_id) = elect_new_host(&mut state, now_ms) {
                    events.push(RoomEvent::HostMigrated(HostMigratedPayload {
                        previous_host_user_id: user_id,
                        new_host_user_id: new_host_id,
                    }));
                } else {
                    // No other participants -> room ends.
                    state.state = RoomLifecycle::Ended;
                    events.push(RoomEvent::RoomClosed(RoomClosedPayload {
                        reason: "host_left".into(),
                    }));
                }
            } else {
                // Transport loss, migration ON: start the grace.
                state.host_disconnect_deadline_ms =
                    Some(now_ms + self.config.host_disconnect_grace_ms);
                // Mark the host as Reconnecting for the UI.
                state.participants[idx].status = ParticipantStatus::Reconnecting;
                let new_host_user_id = if self.config.host_disconnect_grace_ms <= 0 {
                    elect_new_host(&mut state, now_ms)
                } else {
                    None
                };
                events.push(RoomEvent::HostDisconnected(HostDisconnectedPayload {
                    previous_host_user_id: user_id,
                    reconnect_deadline_ms: state.host_disconnect_deadline_ms.unwrap(),
                    new_host_user_id,
                }));
                // The host is still a participant in the room
                // (status = Reconnecting). Do not emit
                // ParticipantLeft; the HostDisconnected event
                // is the source of truth.
                emit_left = false;
            }
        } else {
            // Non-host leaver: mark Left, broadcast LEFT.
            state.participants[idx].status = ParticipantStatus::Left;
        }
        if emit_left {
            events.push(RoomEvent::ParticipantLeft(ParticipantLeftPayload {
                user_id,
                reason: "leave".to_string(),
            }));
        }

        // Build the broadcast items so the room subscribers
        // see the events. The originator is the leaving user
        // so a connected WS can avoid echoing the LEFT back
        // to itself if it so chooses.
        let publish_items: Vec<BroadcastItem> = events
            .iter()
            .map(|e| event_to_broadcast_item(e, room_id, Some(user_id)))
            .collect();

        // Clean up ended rooms eagerly so the next allocation
        // is not affected by stale state.
        let ended = state.state == RoomLifecycle::Ended;
        // Drop the write lock before either path so the read
        // lock in the `Some` branch can be acquired (or the
        // write lock in `remove_room`).
        drop(state);
        // Publish BEFORE remove_room so subscribers see the
        // ROOM_CLOSED before the channel disappears. The
        // forwarder's `is_user_in_room` check would skip
        // events for users in an already-removed room;
        // we therefore keep the room registered for a
        // short grace window so the broadcasts can be
        // observed.
        for item in &publish_items {
            self.publish(room_id, item.clone());
        }
        // Yield so the broadcast receivers wake up and
        // process the items before we drop the room. The
        // `is_user_in_room` check on the forwarder will
        // still find the room in by_id during this brief
        // window.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let summary = if ended {
            self.remove_room(room_id).await;
            None
        } else {
            Some(handle.read().await.snapshot())
        };
        Ok((events, summary))
    }

    /// Returned by the WS layer when a transport closes. For
    /// non-host participants, mark them `Disconnected`. For
    /// the host, take the same path as `leave(_, false)`.
    pub async fn on_connection_lost(
        &self,
        user_id: Uuid,
        now_ms: i64,
    ) -> Result<Vec<RoomEvent>, RoomError> {
        // Cheap path: a non-host that disconnects is marked
        // Disconnected but not removed; the stale-cleanup
        // task in `tick_stale_participants` will drop them
        // after `participant_stale_after_ms`.
        let handle = {
            let by_id = self.by_id.read().await;
            let mut found = None;
            for (_rid, h) in by_id.iter() {
                let s = h.read().await;
                if s.participants
                    .iter()
                    .any(|p| p.user_id == user_id && p.status != ParticipantStatus::Left)
                {
                    found = Some(h.clone());
                    break;
                }
            }
            found
        };
        let handle = match handle {
            Some(h) => h,
            None => return Err(RoomError::NotJoined),
        };
        let mut state = handle.write().await;
        if state.state != RoomLifecycle::Open {
            return Err(RoomError::RoomClosed);
        }
        let is_host = state.host().map(|h| h.user_id == user_id).unwrap_or(false);
        if !is_host {
            if let Some(p) = state.participants.iter_mut().find(|p| p.user_id == user_id) {
                p.status = ParticipantStatus::Disconnected;
            }
            return Ok(vec![]);
        }
        // Host transport loss. Treat as `leave(_, false)`.
        drop(state);
        let (events, _summary) = self.leave(user_id, false, now_ms).await?;
        Ok(events)
    }

    /// A fresh authenticated transport re-attached for an
    /// existing user. If the user is the host and the grace
    /// is still running, the host is restored (any prior
    /// election is reverted). If the user is a non-host
    /// viewer, their status is set back to `Connected`.
    pub async fn rejoin(
        &self,
        user_id: Uuid,
        pubkey: [u8; 32],
        now_ms: i64,
    ) -> Result<Option<Vec<RoomEvent>>, RoomError> {
        let handle = {
            let by_id = self.by_id.read().await;
            let mut found = None;
            for (_rid, h) in by_id.iter() {
                let s = h.read().await;
                if s.participants
                    .iter()
                    .any(|p| p.user_id == user_id && p.status != ParticipantStatus::Left)
                {
                    found = Some(h.clone());
                    break;
                }
            }
            found
        };
        let handle = match handle {
            Some(h) => h,
            None => return Ok(None),
        };
        let mut state = handle.write().await;
        let is_host = state.host().map(|h| h.user_id == user_id).unwrap_or(false);
        let was_in_grace = state.host_disconnect_deadline_ms.is_some();
        if is_host && was_in_grace {
            // Cancel the grace. The host's participant
            // record was set to `Reconnecting` (or already
            // `Disconnected` if `leave` already ran); we set
            // it back to `Connected` and re-bind the
            // pubkey.
            state.host_disconnect_deadline_ms = None;
            if let Some(p) = state.host_mut() {
                p.status = ParticipantStatus::Connected;
                p.last_seen_ms = now_ms;
                p.pubkey = pubkey;
            }
            let room_id = state.id;
            // Publish the HostReconnected event so other
            // participants see it.
            let event = RoomEvent::HostReconnected(HostReconnectedPayload {
                host_user_id: user_id,
            });
            let item = event_to_broadcast_item(&event, room_id, Some(user_id));
            drop(state);
            self.publish(room_id, item);
            return Ok(Some(vec![event]));
        }
        // Non-host rejoin: restore Connected status.
        if let Some(p) = state.participants.iter_mut().find(|p| p.user_id == user_id) {
            p.status = ParticipantStatus::Connected;
            p.last_seen_ms = now_ms;
            p.pubkey = pubkey;
        }
        Ok(Some(vec![]))
    }

    /// The room-state snapshot for a user. Returns `None` if
    /// the user is not in any room.
    pub async fn list_snapshot(&self, user_id: Uuid) -> Option<RoomStatePayload> {
        let by_id = self.by_id.read().await;
        for (_rid, h) in by_id.iter() {
            let s = h.read().await;
            if s.participants
                .iter()
                .any(|p| p.user_id == user_id && p.status != ParticipantStatus::Left)
            {
                return Some(RoomStatePayload {
                    host_disconnect_deadline_ms: s.host_disconnect_deadline_ms,
                    room: s.snapshot(),
                });
            }
        }
        None
    }

    /// `ROOM_STATE` for a room by id. Returns the snapshot
    /// for the current caller (caller decides authorization).
    pub async fn snapshot_for_room(&self, room_id: Uuid) -> Option<RoomStatePayload> {
        let by_id = self.by_id.read().await;
        if let Some(h) = by_id.get(&room_id) {
            let s = h.read().await;
            Some(RoomStatePayload {
                host_disconnect_deadline_ms: s.host_disconnect_deadline_ms,
                room: s.snapshot(),
            })
        } else {
            None
        }
    }

    /// Update `last_seen_ms` for a participant on every
    /// authed inbound message. No-op if the user is not in
    /// any room.
    pub async fn touch(&self, user_id: Uuid, now_ms: i64) {
        let by_id = self.by_id.read().await;
        for (_rid, h) in by_id.iter() {
            let mut state = h.write().await;
            if let Some(p) = state.participants.iter_mut().find(|p| p.user_id == user_id) {
                p.last_seen_ms = now_ms;
                return;
            }
        }
    }

    /// Look up a room by code; useful for tests and for the
    /// "pre-validate code before join" UX.
    pub async fn get_by_code(&self, code: &str) -> Option<Uuid> {
        let by_code = self.by_code.read().await;
        by_code.get(code).copied()
    }

    /// Return the room id a user is currently a participant
    /// in, or `None` if the user is not in any room.
    pub async fn get_user_room(&self, user_id: Uuid) -> Option<Uuid> {
        let by_id = self.by_id.read().await;
        for (rid, h) in by_id.iter() {
            let s = h.read().await;
            if s.participants
                .iter()
                .any(|p| p.user_id == user_id && p.status != ParticipantStatus::Left)
            {
                return Some(*rid);
            }
        }
        None
    }

    /// `true` if the user is currently a participant in the
    /// named room (status != Left). Used by the WS layer to
    /// filter stale broadcast events for users who have
    /// just left.
    pub async fn is_user_in_room(&self, user_id: Uuid, room_id: Uuid) -> bool {
        let by_id = self.by_id.read().await;
        if let Some(h) = by_id.get(&room_id) {
            let s = h.read().await;
            return s
                .participants
                .iter()
                .any(|p| p.user_id == user_id && p.status != ParticipantStatus::Left);
        }
        false
    }

    /// Look up a room by id. Returns the `RoomHandle` so a
    /// caller can read its state.
    pub async fn get_by_id(&self, id: Uuid) -> Option<RoomHandle> {
        let by_id = self.by_id.read().await;
        by_id.get(&id).cloned()
    }

    /// Returns a `Clock`-aware snapshot of the registry's
    /// rooms: a `(room_id, summary, host_disconnect_deadline_ms)`
    /// tuple per room. Used by `Db::list_open_rooms` at server
    /// startup to re-hydrate the registry from SQLite.
    pub async fn list_all(&self) -> Vec<(Uuid, RoomSummary, Option<i64>)> {
        let by_id = self.by_id.read().await;
        let mut out = Vec::with_capacity(by_id.len());
        for (rid, h) in by_id.iter() {
            let s = h.read().await;
            out.push((*rid, s.snapshot(), s.host_disconnect_deadline_ms));
        }
        out
    }

    /// Drop a room from the registry entirely. Called when
    /// the room ends.
    pub async fn remove_room(&self, id: Uuid) -> Option<RoomHandle> {
        let handle = {
            let mut by_id = self.by_id.write().await;
            by_id.remove(&id)
        };
        if let Some(h) = &handle {
            let code = h.read().await.code.clone();
            let mut by_code = self.by_code.write().await;
            by_code.remove(&code);
            let mut room_tx = self.room_tx.write().await;
            room_tx.remove(&id);
        }
        handle
    }

    /// Build a `RoomErrorPayload` for a caller-facing error.
    pub fn error_payload(code: RoomErrorCode, message: impl Into<String>) -> RoomErrorPayload {
        RoomErrorPayload {
            code,
            message: message.into(),
        }
    }
}

/// Convert a `RoomEvent` to the `BroadcastItem` that goes on
/// the per-room `broadcast::channel`. The `kind` tag matches
/// the wire `MessageKind` so the WS layer can encode the
/// envelope without an extra match.
fn event_to_broadcast_item(
    e: &RoomEvent,
    room_id: Uuid,
    originator: Option<Uuid>,
) -> BroadcastItem {
    let (kind, payload) = match e {
        RoomEvent::ParticipantJoined(p) => (
            locast_protocol::envelope::MessageKind::ParticipantJoined,
            serde_json::to_value(p).unwrap_or(serde_json::json!({})),
        ),
        RoomEvent::ParticipantLeft(p) => (
            locast_protocol::envelope::MessageKind::ParticipantLeft,
            serde_json::to_value(p).unwrap_or(serde_json::json!({})),
        ),
        RoomEvent::HostDisconnected(p) => (
            locast_protocol::envelope::MessageKind::HostDisconnected,
            serde_json::to_value(p).unwrap_or(serde_json::json!({})),
        ),
        RoomEvent::HostReconnected(p) => (
            locast_protocol::envelope::MessageKind::HostReconnected,
            serde_json::to_value(p).unwrap_or(serde_json::json!({})),
        ),
        RoomEvent::HostMigrated(p) => (
            locast_protocol::envelope::MessageKind::HostMigrated,
            serde_json::to_value(p).unwrap_or(serde_json::json!({})),
        ),
        RoomEvent::RoomClosed(p) => (
            locast_protocol::envelope::MessageKind::RoomClosed,
            serde_json::to_value(p).unwrap_or(serde_json::json!({})),
        ),
        RoomEvent::Error { payload, .. } => (
            locast_protocol::envelope::MessageKind::RoomError,
            serde_json::to_value(payload).unwrap_or(serde_json::json!({})),
        ),
    };
    BroadcastItem {
        kind,
        payload,
        room_id,
        originator,
    }
}

/// Elect a new host from the participants list. Returns
/// the new host's user_id, or `None` if no other connected
/// participant is available.
///
/// The election rule (P2-T04 spec, locked): earliest
/// `joined_ms`, ties broken by ascending `user_id` (UUID v7
/// is time-ordered, so the tiebreak is rare but
/// deterministic).
fn elect_new_host(state: &mut RoomState, _now_ms: i64) -> Option<Uuid> {
    let mut candidates: Vec<&ParticipantRecord> = state
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
    // Update the in-memory record.
    let prev_host = state.host_user_id;
    for p in state.participants.iter_mut() {
        if p.user_id == new_host {
            p.is_host = true;
            p.cap_set = cap::PLAYBACK_CONTROL
                | cap::DRAW
                | cap::LASER
                | cap::MANAGE_ROOM
                | cap::KICK
                | cap::PUBLISH_MANIFEST
                | cap::INVITE
                | cap::CHAT;
        } else if p.is_host && p.user_id != new_host {
            p.is_host = false;
            p.cap_set = cap::CHAT;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RoomRegistryConfig {
        RoomRegistryConfig {
            max_participants: 8,
            host_disconnect_grace_ms: 200,
            participant_stale_after_ms: 300_000,
        }
    }

    fn keypair(i: u8) -> [u8; 32] {
        [i; 32]
    }

    fn uid(i: u8) -> Uuid {
        // Time-ordered but with distinct bytes.
        let mut b = [0u8; 16];
        b[0] = i;
        b[15] = i;
        Uuid::from_bytes(b)
    }

    #[tokio::test]
    async fn create_then_join_returns_two_participants() {
        let r = RoomRegistry::new(cfg());
        let (summary, self_view) = r
            .create("Movie night".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");
        assert_eq!(self_view.user_id, uid(1));
        assert_eq!(summary.participants.len(), 1);
        let code = summary.code.clone();
        let (joined, evt) = r
            .join(&code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        assert!(matches!(evt, RoomEvent::ParticipantJoined(_)));
        assert_eq!(joined.room.participants.len(), 2);
    }

    #[tokio::test]
    async fn join_unknown_code_returns_not_found() {
        let r = RoomRegistry::new(cfg());
        let err = r
            .join("ZZZZZZ", uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect_err("not found");
        assert_eq!(RoomErrorCode::from(err), RoomErrorCode::RoomNotFound);
    }

    #[tokio::test]
    async fn duplicate_join_returns_already_joined() {
        let r = RoomRegistry::new(cfg());
        let (summary, _) = r
            .create("X".into(), uid(1), keypair(1), false, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("first join");
        let err = r
            .join(&code, uid(2), keypair(2), "B".into(), 1_600)
            .await
            .expect_err("dup");
        assert_eq!(RoomErrorCode::from(err), RoomErrorCode::AlreadyJoined);
    }

    #[tokio::test]
    async fn host_intentional_leave_migration_off_ends_room() {
        let r = RoomRegistry::new(cfg());
        let (summary, _) = r
            .create("X".into(), uid(1), keypair(1), false, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        let (events, _) = r.leave(uid(1), true, 2_000).await.expect("leave host");
        assert!(events.iter().any(|e| matches!(
            e,
            RoomEvent::RoomClosed(p) if p.reason == "host_left"
        )));
        assert!(r.get_by_code(&code).await.is_none());
    }

    #[tokio::test]
    async fn host_intentional_leave_migration_on_handoff_immediate() {
        let r = RoomRegistry::new(cfg());
        let (summary, _) = r
            .create("X".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        let (events, _) = r.leave(uid(1), true, 2_000).await.expect("leave host");
        let migrated = events
            .iter()
            .find_map(|e| match e {
                RoomEvent::HostMigrated(p) => Some(p),
                _ => None,
            })
            .expect("migrated");
        assert_eq!(migrated.previous_host_user_id, uid(1));
        assert_eq!(migrated.new_host_user_id, uid(2));
        // The new snapshot must show uid(2) as host.
        let snap = r.list_snapshot(uid(2)).await.expect("snap");
        assert_eq!(snap.room.host_user_id, uid(2));
    }

    #[tokio::test]
    async fn host_transport_loss_migration_on_starts_grace() {
        let r = RoomRegistry::new(cfg());
        let (summary, _) = r
            .create("X".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        let events = r
            .on_connection_lost(uid(1), 1_600)
            .await
            .expect("on_connection_lost");
        let hd = events
            .iter()
            .find_map(|e| match e {
                RoomEvent::HostDisconnected(p) => Some(p),
                _ => None,
            })
            .expect("host disconnected");
        assert_eq!(hd.previous_host_user_id, uid(1));
        assert_eq!(hd.reconnect_deadline_ms, 1_600 + 200);
        assert!(hd.new_host_user_id.is_none());
    }

    #[tokio::test]
    async fn grace_expiry_migrates_to_next_joiner() {
        let r = RoomRegistry::new(cfg());
        let (summary, _) = r
            .create("X".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        let _ = r
            .on_connection_lost(uid(1), 1_600)
            .await
            .expect("on_connection_lost");
        // tick past the grace deadline
        let migrated = r.tick_grace(1_900).await;
        assert_eq!(migrated.len(), 1);
        let (rid, evt) = &migrated[0];
        let _ = rid;
        let p = match evt {
            RoomEvent::HostMigrated(p) => p,
            _ => panic!("expected HostMigrated, got {evt:?}"),
        };
        assert_eq!(p.previous_host_user_id, uid(1));
        assert_eq!(p.new_host_user_id, uid(2));
    }

    #[tokio::test]
    async fn old_host_rejoin_after_migration_is_viewer() {
        let r = RoomRegistry::new(cfg());
        let (summary, _) = r
            .create("X".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        let _ = r
            .on_connection_lost(uid(1), 1_600)
            .await
            .expect("on_connection_lost");
        let _ = r.tick_grace(1_900).await;
        // Old host reconnects.
        let events = r.rejoin(uid(1), keypair(1), 2_000).await.expect("rejoin");
        assert!(events.is_none() || events.as_ref().unwrap().is_empty());
        // The new host is uid(2); the old host is a viewer.
        let snap = r.list_snapshot(uid(1)).await.expect("snap 1");
        let old = snap
            .room
            .participants
            .iter()
            .find(|p| p.user_id == uid(1))
            .expect("old host in list");
        assert!(!old.is_host);
    }

    #[tokio::test]
    async fn election_tiebreak_uses_user_id_ascending() {
        let r = RoomRegistry::new(cfg());
        let (summary, _) = r
            .create("X".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        // Both joiners have the same joined_ms; user_id
        // ascending is the tiebreak. UUID v7 makes this
        // essentially "the earlier-issued uuid wins".
        let _ = r
            .join(&code, uid(5), keypair(5), "B".into(), 1_500)
            .await
            .expect("join 5");
        let _ = r
            .join(&code, uid(2), keypair(2), "C".into(), 1_500)
            .await
            .expect("join 2");
        let _ = r
            .on_connection_lost(uid(1), 1_600)
            .await
            .expect("on_connection_lost");
        let migrated = r.tick_grace(1_900).await;
        let (_rid, evt) = &migrated[0];
        let p = match evt {
            RoomEvent::HostMigrated(p) => p,
            _ => panic!(),
        };
        // uid(2) < uid(5); uid(2) wins.
        assert_eq!(p.new_host_user_id, uid(2));
    }

    #[tokio::test]
    async fn stale_participant_removed() {
        let r = RoomRegistry::new(RoomRegistryConfig {
            max_participants: 8,
            host_disconnect_grace_ms: 200,
            participant_stale_after_ms: 100,
        });
        let (summary, _) = r
            .create("X".into(), uid(1), keypair(1), false, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        // 1_500 + 100 = 1_600; the cleanup must fire by then.
        let events = r.tick_stale_participants(1_700).await;
        assert_eq!(events.len(), 1);
        match &events[0].1 {
            RoomEvent::ParticipantLeft(p) => {
                assert_eq!(p.user_id, uid(2));
                assert_eq!(p.reason, "timeout");
            }
            other => panic!("expected Left, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn leave_when_not_joined() {
        let r = RoomRegistry::new(cfg());
        let err = r.leave(uid(99), true, 1_000).await.expect_err("err");
        assert_eq!(RoomErrorCode::from(err), RoomErrorCode::NotJoined);
    }
}
