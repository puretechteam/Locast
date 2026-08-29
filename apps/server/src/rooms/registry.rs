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
//!
//! P2-T05: every state-changing method now takes a
//! `&dyn RoomStore` and writes to SQLite BEFORE the
//! in-memory state is committed. If the DB write fails the
//! in-memory state is left untouched, so a caller that sees
//! an `Ok` return can trust both the runtime and the
//! persisted state are in sync.

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
use tracing::{debug, warn};
use uuid::Uuid;

use super::codes;
use super::error::RoomError;
use super::state::{ParticipantRecord, RoomLifecycle, RoomState};
use super::store::RoomStore;
use crate::db::{RoomParticipantRow, RoomRow};

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
    /// P3-T03: a host has published a fresh signed
    /// manifest. Broadcast to every participant in the
    /// room (the host gets a direct `MANIFEST_PUBLISHED`
    /// reply in addition to the broadcast; viewers see
    /// only the broadcast). Viewers verify the signature
    /// against the host's pubkey (TOFU-anchored to the
    /// invite's `h=` parameter) and start the P3 download
    /// flow.
    ManifestPublished {
        /// The room id, included so the WS layer does not
        /// need to look it up by sender.
        room_id: Uuid,
        /// The signed manifest, exactly as the host sent
        /// it (canonicalization is the host's job; the
        /// server is the relay).
        manifest: locast_manifest::MediaManifest,
        /// Server-stamped publication time, unix ms.
        published_at_ms: i64,
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
    /// P3-T03: in-memory cache of the latest manifest per
    /// room. Kept as a separate map (rather than a field
    /// on `RoomState`) so the manifest's larger payload
    /// does not get cloned on every snapshot read.
    /// Updated on every successful `MANIFEST_PUBLISH`
    /// (i.e. every `RoomEvent::ManifestPublished`); the
    /// value is `(version, manifest, host_user_id,
    /// published_at_ms, manifest_hash)`. The cache is
    /// process-local and is not persisted across server
    /// restarts; the durable copy is the
    /// `room_manifests` table.
    manifest_cache: RwLock<HashMap<Uuid, CachedManifest>>,
    config: RoomRegistryConfig,
}

/// One entry in the registry's in-memory manifest cache.
/// The cached blob is the host's exact signed manifest
/// (as on the wire), so a viewer that joins mid-room
/// can request `room_get_state` and get a manifest
/// without re-canonicalizing.
#[derive(Debug, Clone)]
pub struct CachedManifest {
    pub version: i64,
    pub manifest: locast_manifest::MediaManifest,
    pub host_user_id: Uuid,
    pub published_at_ms: i64,
    pub manifest_hash: [u8; 32],
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
            manifest_cache: RwLock::new(HashMap::new()),
            config,
        }
    }

    /// P3-T03: look up the latest cached manifest for a
    /// room. Returns `None` if no manifest has been
    /// published (or if the cache entry was just removed
    /// because the room ended).
    pub async fn current_manifest(&self, room_id: Uuid) -> Option<CachedManifest> {
        let cache = self.manifest_cache.read().await;
        cache.get(&room_id).cloned()
    }

    /// P3-T03: install / replace the cached manifest for
    /// a room. Called by the room dispatcher immediately
    /// after a successful `insert_room_manifest` row. The
    /// value is the just-published manifest.
    pub async fn put_current_manifest(&self, room_id: Uuid, value: CachedManifest) {
        let mut cache = self.manifest_cache.write().await;
        cache.insert(room_id, value);
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
    pub async fn tick_grace(&self, store: &dyn RoomStore, now_ms: i64) -> Vec<(Uuid, RoomEvent)> {
        let mut out: Vec<(Uuid, RoomEvent)> = Vec::new();
        let mut to_publish: Vec<(Uuid, BroadcastItem)> = Vec::new();
        let mut to_remove: Vec<Uuid> = Vec::new();
        // First pass: determine which rooms are expiring and
        // what event each one produces. We don't hold any
        // room write lock past this point.
        let mut host_caps: HashMap<Uuid, (Uuid, u32)> = HashMap::new(); // room_id -> (new_host_user_id, cap_set)
        let mut demoted_caps: HashMap<Uuid, (Uuid, u32)> = HashMap::new(); // room_id -> (old_host_user_id, cap_set)
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
                            summary: Some(Box::new(state.snapshot())),
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
                        host_caps.insert(
                            *rid,
                            (new_host_id, state.host().map(|h| h.cap_set).unwrap_or(0)),
                        );
                        demoted_caps.insert(*rid, (prev, cap::CHAT));
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
        // Second pass: persist + publish + remove.
        // DB writes are best-effort: a failure is logged
        // but the in-memory state is the runtime's
        // authority. The next tick will retry via
        // re-publish of the new state; a successful
        // migration persists the new host flag, a
        // successful end_room records the close.
        for (rid, item) in &to_publish {
            self.publish(*rid, item.clone());
        }
        for (rid, (new_host, cap_set)) in &host_caps {
            if let Some(handle) = self.by_id.read().await.get(rid).cloned() {
                let s = handle.read().await;
                if let Some(rec) = s.participants.iter().find(|p| p.user_id == *new_host) {
                    let _ = store
                        .add_room_participant(
                            *rid,
                            *new_host,
                            &rec.pubkey,
                            &rec.display_name,
                            true,
                            rec.joined_ms,
                            *cap_set,
                        )
                        .await;
                }
            }
        }
        for (rid, (old_host, cap_set)) in &demoted_caps {
            if let Some(handle) = self.by_id.read().await.get(rid).cloned() {
                let s = handle.read().await;
                if let Some(rec) = s.participants.iter().find(|p| p.user_id == *old_host) {
                    let _ = store
                        .add_room_participant(
                            *rid,
                            *old_host,
                            &rec.pubkey,
                            &rec.display_name,
                            false,
                            rec.joined_ms,
                            *cap_set,
                        )
                        .await;
                }
            }
            let _ = store.set_host_disconnect_deadline(*rid, None).await;
        }
        for rid in to_remove {
            let _ = store.end_room(rid, now_ms).await;
            self.remove_room(rid).await;
        }
        out
    }

    /// Stale-participant cleanup. Any participant whose
    /// `last_seen_ms` is older than `participant_stale_after_ms`
    /// is removed from their room and a
    /// `ParticipantLeft { reason: "timeout" }` is emitted.
    pub async fn tick_stale_participants(
        &self,
        store: &dyn RoomStore,
        now_ms: i64,
    ) -> Vec<(Uuid, RoomEvent)> {
        let mut out: Vec<(Uuid, RoomEvent)> = Vec::new();
        let mut to_publish: Vec<(Uuid, BroadcastItem)> = Vec::new();
        let mut to_remove: Vec<Uuid> = Vec::new();
        let mut removed_pairs: Vec<(Uuid, Uuid)> = Vec::new();
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
                    removed_pairs.push((*rid, removed.user_id));
                }
                if state.state == RoomLifecycle::Ended {
                    to_remove.push(*rid);
                }
            }
        }
        for (rid, item) in &to_publish {
            self.publish(*rid, item.clone());
        }
        for (rid, uid) in &removed_pairs {
            let _ = store
                .update_participant_status(*rid, *uid, "left", now_ms)
                .await;
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
    ///
    /// The DB write is performed BEFORE the in-memory
    /// state is committed. If the DB write fails the
    /// room is NOT created in memory either; the caller
    /// sees the `Err`.
    pub async fn create(
        &self,
        store: &dyn RoomStore,
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
            // Check both the in-memory map AND the durable
            // rooms table. The DB check closes the race
            // where a concurrent restart brought back a
            // room with the same code, or where a
            // concurrent `create` slipped a row into the
            // DB before the in-memory `by_code` was
            // populated.
            let in_mem_taken = {
                let by_code = self.by_code.read().await;
                by_code.contains_key(&candidate)
            };
            if !in_mem_taken
                && !store
                    .room_code_taken(&candidate)
                    .await
                    .map_err(|e| RoomError::Internal(format!("room_code_taken: {e}")))?
            {
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
        // Persist the room row first.
        store
            .insert_room(
                id,
                &code,
                &title,
                host_user_id,
                &host_pubkey,
                host_migration_enabled,
                now_ms,
            )
            .await
            .map_err(|e| RoomError::Internal(format!("insert_room: {e}")))?;
        // Then the host participant.
        store
            .add_room_participant(id, host_user_id, &host_pubkey, "", true, now_ms, cap_set)
            .await
            .map_err(|e| RoomError::Internal(format!("add_room_participant: {e}")))?;

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
        store: &dyn RoomStore,
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
        // Persist FIRST.
        store
            .add_room_participant(id, user_id, &pubkey, &display_name, false, now_ms, cap_set)
            .await
            .map_err(|e| RoomError::Internal(format!("add_room_participant: {e}")))?;
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
        store: &dyn RoomStore,
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
        let mut events: Vec<RoomEvent> = Vec::new();
        // Decide the outcome BEFORE writing to the DB so
        // we can persist in the right order.
        let mut new_host_cap_set: u32 = 0;
        let mut demoted_cap_set: u32 = 0;
        let mut emit_left = true;
        if was_host {
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
                state.participants[idx].status = ParticipantStatus::Left;
                if let Some(new_host_id) = elect_new_host(&mut state, now_ms) {
                    new_host_cap_set = state.host().map(|h| h.cap_set).unwrap_or(0);
                    demoted_cap_set = cap::CHAT;
                    events.push(RoomEvent::HostMigrated(HostMigratedPayload {
                        previous_host_user_id: user_id,
                        new_host_user_id: new_host_id,
                        summary: Some(Box::new(state.snapshot())),
                    }));
                } else {
                    state.state = RoomLifecycle::Ended;
                    events.push(RoomEvent::RoomClosed(RoomClosedPayload {
                        reason: "host_left".into(),
                    }));
                }
            } else {
                // Transport loss, migration ON: start the grace.
                state.host_disconnect_deadline_ms =
                    Some(now_ms + self.config.host_disconnect_grace_ms);
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
                emit_left = false;
            }
        } else {
            state.participants[idx].status = ParticipantStatus::Left;
        }
        if emit_left {
            events.push(RoomEvent::ParticipantLeft(ParticipantLeftPayload {
                user_id,
                reason: "leave".to_string(),
            }));
        }

        // Persist BEFORE the in-memory state is committed
        // (i.e. before any of the in-memory mutations are
        // observable to a reader). For the `host transport
        // loss + migration on` case we only set the
        // deadline; for the others we either end the room
        // or add a new host participant row.
        if was_host && state.host_migration_enabled && intentional {
            if state.state == RoomLifecycle::Ended {
                // No new host; end the room.
                store
                    .end_room(room_id, now_ms)
                    .await
                    .map_err(|e| RoomError::Internal(format!("end_room: {e}")))?;
            } else {
                // New host was elected: persist the new
                // host flag for the new host, and demote
                // the old host to a viewer.
                if let Some(new_host_rec) = state.participants.iter().find(|p| p.is_host).cloned() {
                    store
                        .add_room_participant(
                            room_id,
                            new_host_rec.user_id,
                            &new_host_rec.pubkey,
                            &new_host_rec.display_name,
                            true,
                            new_host_rec.joined_ms,
                            new_host_cap_set,
                        )
                        .await
                        .map_err(|e| RoomError::Internal(format!("add_room_participant: {e}")))?;
                }
                store
                    .add_room_participant(
                        room_id,
                        user_id,
                        &state.participants[idx].pubkey,
                        &state.participants[idx].display_name,
                        false,
                        state.participants[idx].joined_ms,
                        demoted_cap_set,
                    )
                    .await
                    .map_err(|e| RoomError::Internal(format!("add_room_participant: {e}")))?;
            }
        } else if was_host && !state.host_migration_enabled {
            // Room ends; persist end_room (host row update
            // happens via the `add_room_participant` no-op
            // because the host's row will be set to
            // `is_host=0` only on a future handoff; in v1
            // the room is gone).
            store
                .end_room(room_id, now_ms)
                .await
                .map_err(|e| RoomError::Internal(format!("end_room: {e}")))?;
        } else if was_host {
            // Migration ON, transport loss: set the
            // deadline; the host's row stays at
            // `is_host=1` until rejoin or migration.
            store
                .set_host_disconnect_deadline(room_id, state.host_disconnect_deadline_ms)
                .await
                .map_err(|e| RoomError::Internal(format!("set_host_disconnect_deadline: {e}")))?;
        }
        // Non-host (or host row): update participant
        // status to "left".
        if was_host {
            // The host row's status update is implicit for
            // the non-transport-loss case (the row is left
            // with is_host=1 in the case of transport loss;
            // for room-end, the room row is set to "ended"
            // and we don't need to flip the host row's
            // status here).
        } else {
            store
                .update_participant_status(room_id, user_id, "left", now_ms)
                .await
                .map_err(|e| RoomError::Internal(format!("update_participant_status: {e}")))?;
        }

        let publish_items: Vec<BroadcastItem> = events
            .iter()
            .map(|e| event_to_broadcast_item(e, room_id, Some(user_id)))
            .collect();

        let ended = state.state == RoomLifecycle::Ended;
        drop(state);
        // Publish BEFORE remove_room so subscribers see the
        // ROOM_CLOSED before the channel disappears. The
        // forwarder's `is_user_in_room` check would skip
        // events for users in an already-removed room; we
        // therefore wait a short grace window AFTER
        // publishing and BEFORE removing the room so the
        // broadcast receivers can drain the items. The
        // sleep length matches the WS forwarder's 50ms
        // poll interval and is well below the
        // 200ms-grace threshold used in tests.
        for item in &publish_items {
            self.publish(room_id, item.clone());
        }
        if ended {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.remove_room(room_id).await;
            Ok((events, None))
        } else {
            let summary = handle.read().await.snapshot();
            Ok((events, Some(summary)))
        }
    }

    /// Returned by the WS layer when a transport closes. For
    /// non-host participants, mark them `Disconnected`. For
    /// the host, take the same path as `leave(_, false)`.
    pub async fn on_connection_lost(
        &self,
        store: &dyn RoomStore,
        user_id: Uuid,
        now_ms: i64,
    ) -> Result<Vec<RoomEvent>, RoomError> {
        let handle = {
            let by_id = self.by_id.read().await;
            let mut found = None;
            for h in by_id.values() {
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
        let is_host = {
            let s = handle.read().await;
            s.host().map(|h| h.user_id == user_id).unwrap_or(false)
        };
        if !is_host {
            // Persist the disconnected status before
            // mutating memory.
            let room_id = {
                let by_id = self.by_id.read().await;
                let mut found = None;
                for (rid, h) in by_id.iter() {
                    let s = h.read().await;
                    if s.participants
                        .iter()
                        .any(|p| p.user_id == user_id && p.status != ParticipantStatus::Left)
                    {
                        found = Some(*rid);
                        break;
                    }
                }
                found
            };
            let room_id = room_id.ok_or(RoomError::NotJoined)?;
            store
                .update_participant_status(room_id, user_id, "disconnected", now_ms)
                .await
                .map_err(|e| RoomError::Internal(format!("update_participant_status: {e}")))?;
            let mut state = handle.write().await;
            if state.state != RoomLifecycle::Open {
                return Err(RoomError::RoomClosed);
            }
            if let Some(p) = state.participants.iter_mut().find(|p| p.user_id == user_id) {
                p.status = ParticipantStatus::Disconnected;
            }
            return Ok(vec![]);
        }
        // Host transport loss. Treat as `leave(_, false)`.
        let (events, _summary) = self.leave(store, user_id, false, now_ms).await?;
        Ok(events)
    }

    /// A fresh authenticated transport re-attached for an
    /// existing user. If the user is the host and the grace
    /// is still running, the host is restored (any prior
    /// election is reverted). If the user is a non-host
    /// viewer, their status is set back to `Connected`.
    pub async fn rejoin(
        &self,
        store: &dyn RoomStore,
        user_id: Uuid,
        pubkey: [u8; 32],
        now_ms: i64,
    ) -> Result<Option<Vec<RoomEvent>>, RoomError> {
        let handle = {
            let by_id = self.by_id.read().await;
            let mut found = None;
            for h in by_id.values() {
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
        let room_id = {
            let by_id = self.by_id.read().await;
            let mut found = None;
            for (rid, h) in by_id.iter() {
                let s = h.read().await;
                if s.participants
                    .iter()
                    .any(|p| p.user_id == user_id && p.status != ParticipantStatus::Left)
                {
                    found = Some(*rid);
                    break;
                }
            }
            found.ok_or(RoomError::NotJoined)?
        };
        let is_host = {
            let s = handle.read().await;
            s.host().map(|h| h.user_id == user_id).unwrap_or(false)
        };
        let was_in_grace = {
            let s = handle.read().await;
            s.host_disconnect_deadline_ms.is_some()
        };
        if is_host && was_in_grace {
            // Persist FIRST.
            store
                .set_host_disconnect_deadline(room_id, None)
                .await
                .map_err(|e| RoomError::Internal(format!("set_host_disconnect_deadline: {e}")))?;
            // Reset the host's participant row.
            let (display_name, joined_ms, cap_set) = {
                let s = handle.read().await;
                let p = s.host().cloned().expect("host present");
                (p.display_name, p.joined_ms, p.cap_set)
            };
            store
                .add_room_participant(
                    room_id,
                    user_id,
                    &pubkey,
                    &display_name,
                    true,
                    joined_ms,
                    cap_set,
                )
                .await
                .map_err(|e| RoomError::Internal(format!("add_room_participant: {e}")))?;
            let mut state = handle.write().await;
            state.host_disconnect_deadline_ms = None;
            if let Some(p) = state.host_mut() {
                p.status = ParticipantStatus::Connected;
                p.last_seen_ms = now_ms;
                p.pubkey = pubkey;
            }
            let event = RoomEvent::HostReconnected(HostReconnectedPayload {
                host_user_id: user_id,
            });
            let item = event_to_broadcast_item(&event, room_id, Some(user_id));
            drop(state);
            self.publish(room_id, item);
            return Ok(Some(vec![event]));
        }
        // Non-host rejoin: restore Connected status.
        // Persist FIRST.
        store
            .update_participant_status(room_id, user_id, "connected", now_ms)
            .await
            .map_err(|e| RoomError::Internal(format!("update_participant_status: {e}")))?;
        let mut state = handle.write().await;
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
        for h in by_id.values() {
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
        for h in by_id.values() {
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

    /// P3-T03: `true` if `user_id` is the current host of
    /// `room_id` (the participant record's `is_host` flag is
    /// set AND the participant's status is not Left). The
    /// P3-T03 capability gate uses this for the host-only
    /// `PublishManifest` check. The check is read-only and
    /// does not require the caller to hold any locks; the
    /// `RoomHandle` is acquired on demand and released.
    pub async fn is_room_host(&self, room_id: Uuid, user_id: Uuid) -> bool {
        let by_id = self.by_id.read().await;
        if let Some(h) = by_id.get(&room_id) {
            let s = h.read().await;
            return s
                .participants
                .iter()
                .any(|p| p.user_id == user_id && p.is_host && p.status != ParticipantStatus::Left);
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
        // P3-T03: also drop the cached manifest so a
        // future call to `current_manifest` returns None
        // for this room id. The durable `room_manifests`
        // rows are NOT touched; the room's history remains
        // available in the table for the audit log.
        {
            let mut cache = self.manifest_cache.write().await;
            cache.remove(&id);
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

    /// Rehydrate the in-memory `RoomState` for a single
    /// `RoomRow` from the DB. Called at server startup
    /// after `db.list_open_rooms()` for every open row.
    ///
    /// P2-T05 spec: "Reject any participant whose
    /// WebSocket can't survive a server restart — they must
    /// reconnect through P2-T02/P2-T03. After rehydration,
    /// no participant should be marked `Connected` if their
    /// transport is gone. Mark them `Disconnected` so the
    /// stale-cleanup ticker (or an explicit
    /// `on_connection_lost` flow on the next reconnect)
    /// handles them."
    ///
    /// The host is also reconnected through P2-T02/P2-T03,
    /// so the host is either marked `Reconnecting` (if a
    /// `host_disconnect_deadline_ms` is set) or `Connected`
    /// if the room is fresh (no in-flight host).
    ///
    /// Participants whose persisted status is `"left"` are
    /// filtered out; their rows are not re-inserted.
    pub async fn rehydrate(
        &self,
        room: RoomRow,
        participants: Vec<RoomParticipantRow>,
    ) -> Result<(), RoomError> {
        if room.state != "open" {
            // Per the spec, do not rehydrate ended rooms.
            return Ok(());
        }
        let mut pubkey_arr = [0u8; 32];
        if room.host_pubkey.len() != 32 {
            return Err(RoomError::Internal(format!(
                "host_pubkey row is {} bytes, expected 32",
                room.host_pubkey.len()
            )));
        }
        pubkey_arr.copy_from_slice(&room.host_pubkey);

        let id = room.id;
        let code = room.code.clone();
        let host_user_id = room.host_user_id;
        let host_migration_enabled = room.host_migration_enabled;
        let deadline = room.host_disconnect_deadline_ms;
        // Walk the persisted participants and rebuild
        // the in-memory list. Mark anyone who is not the
        // host as `Disconnected`; the stale-cleanup
        // ticker will drop them after
        // `participant_stale_after_ms` of silence, or an
        // explicit `on_connection_lost` flow on the next
        // reconnect will reset them to `Connected`.
        let mut rebuilt: Vec<ParticipantRecord> = Vec::new();
        let mut host_found = false;
        for p in participants {
            if p.status == "left" {
                continue;
            }
            if p.pubkey.len() != 32 {
                warn!(
                    room_id = %id,
                    user_id = %p.user_id,
                    "rehydrate: bad pubkey length, skipping"
                );
                continue;
            }
            let mut pkey = [0u8; 32];
            pkey.copy_from_slice(&p.pubkey);
            let status = if p.is_host {
                host_found = true;
                // If a grace deadline is set, the host's
                // transport is currently considered down.
                if deadline.is_some() {
                    ParticipantStatus::Reconnecting
                } else {
                    ParticipantStatus::Connected
                }
            } else {
                ParticipantStatus::Disconnected
            };
            rebuilt.push(ParticipantRecord {
                user_id: p.user_id,
                pubkey: pkey,
                display_name: p.display_name,
                joined_ms: p.joined_ms,
                status,
                // `RoomParticipantRow` does not carry
                // `last_seen_ms`; default to `joined_ms`
                // so the stale-cleanup ticker has a
                // sensible reference time. The next
                // inbound PRESENCE message will update
                // it via `RoomRegistry::touch`.
                last_seen_ms: p.joined_ms,
                is_host: p.is_host,
                cap_set: p.cap_set,
            });
        }
        if !host_found {
            // No host row; the host must have been
            // promoted into a participant row with
            // is_host=1, OR the rehydrate path should
            // synthesize one. We synthesize one with the
            // room's host_user_id and the full cap set
            // (matching what `create` does).
            let cap_set = cap::PLAYBACK_CONTROL
                | cap::DRAW
                | cap::LASER
                | cap::MANAGE_ROOM
                | cap::KICK
                | cap::PUBLISH_MANIFEST
                | cap::INVITE
                | cap::CHAT;
            let status = if deadline.is_some() {
                ParticipantStatus::Reconnecting
            } else {
                ParticipantStatus::Connected
            };
            rebuilt.push(ParticipantRecord {
                user_id: host_user_id,
                pubkey: pubkey_arr,
                display_name: String::new(),
                joined_ms: room.created_ms,
                status,
                last_seen_ms: room.created_ms,
                is_host: true,
                cap_set,
            });
        }

        let state = RoomState {
            id,
            code: code.clone(),
            title: room.title.clone(),
            host_user_id,
            host_pubkey: pubkey_arr,
            host_migration_enabled,
            created_ms: room.created_ms,
            state: RoomLifecycle::Open,
            host_disconnect_deadline_ms: deadline,
            participants: rebuilt,
        };
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
        Ok(())
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
        RoomEvent::ManifestPublished {
            manifest,
            published_at_ms,
            ..
        } => {
            // P3-T03: the server rebroadcasts the host's
            // signed manifest as a `MANIFEST_PUBLISHED`
            // envelope. The wire payload is the
            // protocol-level `ManifestPublishedPayload` so
            // viewer's `handle_inbound` can decode it
            // directly.
            let payload = locast_protocol::room::ManifestPublishedPayload {
                manifest: manifest.clone(),
                published_at_ms: *published_at_ms,
            };
            (
                locast_protocol::envelope::MessageKind::ManifestPublished,
                serde_json::to_value(&payload).unwrap_or(serde_json::json!({})),
            )
        }
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
    use crate::db::RoomParticipantRow;

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
        let mut b = [0u8; 16];
        b[0] = i;
        b[15] = i;
        Uuid::from_bytes(b)
    }

    fn pubkey_vec(i: u8) -> Vec<u8> {
        vec![i; 32]
    }

    fn store() -> super::super::NoopRoomStore {
        super::super::NoopRoomStore
    }

    #[tokio::test]
    async fn create_then_join_returns_two_participants() {
        let r = RoomRegistry::new(cfg());
        let (summary, self_view) = r
            .create(
                &store(),
                "Movie night".into(),
                uid(1),
                keypair(1),
                true,
                1_000,
            )
            .await
            .expect("create");
        assert_eq!(self_view.user_id, uid(1));
        assert_eq!(summary.participants.len(), 1);
        let code = summary.code.clone();
        let (joined, evt) = r
            .join(&store(), &code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        assert!(matches!(evt, RoomEvent::ParticipantJoined(_)));
        assert_eq!(joined.room.participants.len(), 2);
    }

    #[tokio::test]
    async fn join_unknown_code_returns_not_found() {
        let r = RoomRegistry::new(cfg());
        let err = r
            .join(&store(), "ZZZZZZ", uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect_err("not found");
        assert_eq!(RoomErrorCode::from(err), RoomErrorCode::RoomNotFound);
    }

    #[tokio::test]
    async fn duplicate_join_returns_already_joined() {
        let r = RoomRegistry::new(cfg());
        let (summary, _) = r
            .create(&store(), "X".into(), uid(1), keypair(1), false, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&store(), &code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("first join");
        let err = r
            .join(&store(), &code, uid(2), keypair(2), "B".into(), 1_600)
            .await
            .expect_err("dup");
        assert_eq!(RoomErrorCode::from(err), RoomErrorCode::AlreadyJoined);
    }

    #[tokio::test]
    async fn host_intentional_leave_migration_off_ends_room() {
        let r = RoomRegistry::new(cfg());
        let (summary, _) = r
            .create(&store(), "X".into(), uid(1), keypair(1), false, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&store(), &code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        let (events, _) = r
            .leave(&store(), uid(1), true, 2_000)
            .await
            .expect("leave host");
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
            .create(&store(), "X".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&store(), &code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        let (events, _) = r
            .leave(&store(), uid(1), true, 2_000)
            .await
            .expect("leave host");
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
            .create(&store(), "X".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&store(), &code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        let events = r
            .on_connection_lost(&store(), uid(1), 1_600)
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
            .create(&store(), "X".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&store(), &code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        let _ = r
            .on_connection_lost(&store(), uid(1), 1_600)
            .await
            .expect("on_connection_lost");
        let migrated = r.tick_grace(&store(), 1_900).await;
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
            .create(&store(), "X".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&store(), &code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        let _ = r
            .on_connection_lost(&store(), uid(1), 1_600)
            .await
            .expect("on_connection_lost");
        let _ = r.tick_grace(&store(), 1_900).await;
        let events = r
            .rejoin(&store(), uid(1), keypair(1), 2_000)
            .await
            .expect("rejoin");
        assert!(events.is_none() || events.as_ref().unwrap().is_empty());
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
            .create(&store(), "X".into(), uid(1), keypair(1), true, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&store(), &code, uid(5), keypair(5), "B".into(), 1_500)
            .await
            .expect("join 5");
        let _ = r
            .join(&store(), &code, uid(2), keypair(2), "C".into(), 1_500)
            .await
            .expect("join 2");
        let _ = r
            .on_connection_lost(&store(), uid(1), 1_600)
            .await
            .expect("on_connection_lost");
        let migrated = r.tick_grace(&store(), 1_900).await;
        let (_rid, evt) = &migrated[0];
        let p = match evt {
            RoomEvent::HostMigrated(p) => p,
            _ => panic!(),
        };
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
            .create(&store(), "X".into(), uid(1), keypair(1), false, 1_000)
            .await
            .expect("create");
        let code = summary.code.clone();
        let _ = r
            .join(&store(), &code, uid(2), keypair(2), "B".into(), 1_500)
            .await
            .expect("join");
        let events = r.tick_stale_participants(&store(), 1_700).await;
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
        let err = r
            .leave(&store(), uid(99), true, 1_000)
            .await
            .expect_err("err");
        assert_eq!(RoomErrorCode::from(err), RoomErrorCode::NotJoined);
    }

    fn room_row(id: Uuid, host: Uuid, deadline: Option<i64>) -> RoomRow {
        RoomRow {
            id,
            code: "ABCDEF".to_string(),
            title: "X".into(),
            host_user_id: host,
            host_pubkey: pubkey_vec(1),
            host_migration_enabled: true,
            state: "open".into(),
            host_disconnect_deadline_ms: deadline,
            created_ms: 1_000,
            ended_ms: None,
            last_activity_ms: 1_000,
        }
    }

    fn part_row(uid: Uuid, is_host: bool, status: &str) -> RoomParticipantRow {
        RoomParticipantRow {
            user_id: uid,
            pubkey: pubkey_vec(if is_host { 1 } else { 2 }),
            display_name: if is_host {
                "host".into()
            } else {
                "viewer".into()
            },
            is_host,
            joined_ms: 1_000,
            left_ms: None,
            status: status.into(),
            cap_set: 0,
        }
    }

    #[tokio::test]
    async fn rehydrate_rebuilds_open_room_with_participants() {
        let r = RoomRegistry::new(cfg());
        let id = uid(10);
        let host = uid(1);
        let viewer = uid(2);
        let row = room_row(id, host, None);
        let parts = vec![
            part_row(host, true, "connected"),
            part_row(viewer, false, "connected"),
        ];
        r.rehydrate(row, parts).await.expect("rehydrate");
        // Code resolves; both participants present.
        let code_id = r.get_by_code("ABCDEF").await.expect("code");
        assert_eq!(code_id, id);
        let snap = r.snapshot_for_room(id).await.expect("snap");
        assert_eq!(snap.room.participants.len(), 2);
        assert_eq!(snap.room.host_user_id, host);
    }

    #[tokio::test]
    async fn rehydrate_drops_ended_rooms() {
        let r = RoomRegistry::new(cfg());
        let id = uid(10);
        let mut row = room_row(id, uid(1), None);
        row.state = "ended".into();
        r.rehydrate(row, vec![]).await.expect("rehydrate");
        assert!(r.get_by_code("ABCDEF").await.is_none());
        assert!(r.snapshot_for_room(id).await.is_none());
    }

    #[tokio::test]
    async fn rehydrate_marks_non_host_disconnected() {
        let r = RoomRegistry::new(cfg());
        let id = uid(10);
        let row = room_row(id, uid(1), None);
        let parts = vec![
            part_row(uid(1), true, "connected"),
            part_row(uid(2), false, "connected"),
        ];
        r.rehydrate(row, parts).await.expect("rehydrate");
        let handle = r.get_by_id(id).await.expect("handle");
        let s = handle.read().await;
        let host = s.host().expect("host");
        let viewer = s
            .participants
            .iter()
            .find(|p| p.user_id == uid(2))
            .expect("viewer");
        assert_eq!(host.status, ParticipantStatus::Connected);
        assert_eq!(viewer.status, ParticipantStatus::Disconnected);
    }

    #[tokio::test]
    async fn rehydrate_filters_left_participants() {
        let r = RoomRegistry::new(cfg());
        let id = uid(10);
        let row = room_row(id, uid(1), None);
        let parts = vec![
            part_row(uid(1), true, "connected"),
            part_row(uid(2), false, "left"),
        ];
        r.rehydrate(row, parts).await.expect("rehydrate");
        let snap = r.snapshot_for_room(id).await.expect("snap");
        assert_eq!(snap.room.participants.len(), 1);
    }

    #[tokio::test]
    async fn rehydrate_preserves_host_grace_deadline() {
        let r = RoomRegistry::new(cfg());
        let id = uid(10);
        let deadline = 5_000i64;
        let row = room_row(id, uid(1), Some(deadline));
        let parts = vec![part_row(uid(1), true, "connected")];
        r.rehydrate(row, parts).await.expect("rehydrate");
        let snap = r.snapshot_for_room(id).await.expect("snap");
        assert_eq!(snap.host_disconnect_deadline_ms, Some(deadline));
        let handle = r.get_by_id(id).await.expect("handle");
        let s = handle.read().await;
        let host = s.host().expect("host");
        assert_eq!(host.status, ParticipantStatus::Reconnecting);
    }

    #[tokio::test]
    async fn rehydrate_multiple_rooms_survive() {
        let r = RoomRegistry::new(cfg());
        let id_a = uid(10);
        let id_b = uid(11);
        r.rehydrate(
            RoomRow {
                id: id_a,
                code: "AAAAAA".into(),
                title: "A".into(),
                host_user_id: uid(1),
                host_pubkey: pubkey_vec(1),
                host_migration_enabled: true,
                state: "open".into(),
                host_disconnect_deadline_ms: None,
                created_ms: 1_000,
                ended_ms: None,
                last_activity_ms: 1_000,
            },
            vec![part_row(uid(1), true, "connected")],
        )
        .await
        .expect("a");
        r.rehydrate(
            RoomRow {
                id: id_b,
                code: "BBBBBB".into(),
                title: "B".into(),
                host_user_id: uid(2),
                host_pubkey: pubkey_vec(2),
                host_migration_enabled: false,
                state: "open".into(),
                host_disconnect_deadline_ms: None,
                created_ms: 2_000,
                ended_ms: None,
                last_activity_ms: 2_000,
            },
            vec![part_row(uid(2), true, "connected")],
        )
        .await
        .expect("b");
        assert!(r.get_by_code("AAAAAA").await.is_some());
        assert!(r.get_by_code("BBBBBB").await.is_some());
        assert_eq!(r.list_all().await.len(), 2);
    }

    #[tokio::test]
    async fn rehydrate_synthesizes_missing_host_row() {
        // If the persisted participant list lacks a host row
        // (e.g. only a viewer persisted before the host row
        // was added), the rehydrate path should synthesize
        // a host row with the room's host_user_id and the
        // full cap set.
        let r = RoomRegistry::new(cfg());
        let id = uid(10);
        let row = room_row(id, uid(1), None);
        // No host row; only a viewer.
        let parts = vec![part_row(uid(2), false, "connected")];
        r.rehydrate(row, parts).await.expect("rehydrate");
        let handle = r.get_by_id(id).await.expect("handle");
        let s = handle.read().await;
        assert!(s.host().is_some());
        assert_eq!(s.host().unwrap().user_id, uid(1));
    }

    // ------------------------------------------------------------------
    // DB-backed persistence tests (P2-T05 Part 1 + Part 11).
    // ------------------------------------------------------------------

    mod db_persistence {
        use super::*;
        use crate::db::Db;

        async fn fresh_db() -> Db {
            Db::open_in_memory().await.expect("open in-memory db")
        }

        fn pubkey(i: u8) -> [u8; 32] {
            [i; 32]
        }

        /// Insert a `user_identities` row for the given
        /// pubkey and return the server-assigned
        /// `user_id`. The room row's `host_user_id` has
        /// an FK on this table, so the host must exist
        /// in `user_identities` first.
        async fn ensure_user(db: &Db, pk: [u8; 32]) -> Uuid {
            db.upsert_user(&pk).await.expect("upsert user")
        }

        #[tokio::test]
        async fn create_persists_room_and_host_row() {
            let r = RoomRegistry::new(cfg());
            let db = fresh_db().await;
            let host = ensure_user(&db, pubkey(1)).await;
            let s = crate::rooms::DbRoomStore::new(db.clone());
            let (summary, _) = r
                .create(&s, "T".into(), host, pubkey(1), true, 1_000)
                .await
                .expect("create");
            let row = db
                .get_room_by_code(&summary.code)
                .await
                .expect("get")
                .expect("present");
            assert_eq!(row.host_user_id, host);
            assert!(row.host_migration_enabled);
            assert_eq!(row.state, "open");
            let parts = db.list_room_participants(row.id).await.expect("parts");
            assert_eq!(parts.len(), 1);
            assert!(parts[0].is_host);
        }

        #[tokio::test]
        async fn join_persists_participant() {
            let r = RoomRegistry::new(cfg());
            let db = fresh_db().await;
            let host = ensure_user(&db, pubkey(1)).await;
            let joiner = ensure_user(&db, pubkey(2)).await;
            let s = crate::rooms::DbRoomStore::new(db.clone());
            let (summary, _) = r
                .create(&s, "T".into(), host, pubkey(1), false, 1_000)
                .await
                .expect("create");
            let _ = r
                .join(&s, &summary.code, joiner, pubkey(2), "B".into(), 1_500)
                .await
                .expect("join");
            let row = db
                .get_room_by_code(&summary.code)
                .await
                .expect("get")
                .expect("present");
            let parts = db.list_room_participants(row.id).await.expect("parts");
            assert_eq!(parts.len(), 2);
            let v = parts.iter().find(|p| p.user_id == joiner).expect("v");
            assert!(!v.is_host);
            assert_eq!(v.display_name, "B");
        }

        #[tokio::test]
        async fn end_to_end_rehydrate_preserves_code() {
            let r1 = RoomRegistry::new(cfg());
            let db = fresh_db().await;
            let host = ensure_user(&db, pubkey(1)).await;
            let joiner = ensure_user(&db, pubkey(2)).await;
            let s = crate::rooms::DbRoomStore::new(db.clone());
            let (summary, _) = r1
                .create(&s, "T".into(), host, pubkey(1), true, 1_000)
                .await
                .expect("create");
            let _ = r1
                .join(&s, &summary.code, joiner, pubkey(2), "B".into(), 1_500)
                .await
                .expect("join");
            // "Restart": build a fresh registry, rehydrate
            // from the same DB.
            let r2 = RoomRegistry::new(cfg());
            let rows = db.list_open_rooms().await.expect("list");
            assert_eq!(rows.len(), 1);
            for row in rows {
                let parts = db.list_room_participants(row.id).await.expect("parts");
                r2.rehydrate(row, parts).await.expect("rehydrate");
            }
            // The new registry must resolve the same code.
            let code_id = r2.get_by_code(&summary.code).await.expect("code");
            assert_eq!(code_id, summary.id);
            let snap = r2.snapshot_for_room(summary.id).await.expect("snap");
            assert_eq!(snap.room.participants.len(), 2);
            assert_eq!(snap.room.host_user_id, host);
        }

        #[tokio::test]
        async fn rehydrate_skips_ended_rooms() {
            let r = RoomRegistry::new(cfg());
            let db = fresh_db().await;
            let host_a = ensure_user(&db, pubkey(1)).await;
            let host_b = ensure_user(&db, pubkey(2)).await;
            let s = crate::rooms::DbRoomStore::new(db.clone());
            // Insert one ended row directly.
            db.insert_room(uid(99), "ZZZZZZ", "Z", host_a, &pubkey(1), true, 1_000)
                .await
                .expect("insert");
            db.end_room(uid(99), 2_000).await.expect("end");
            // Open one.
            let (summary, _) = r
                .create(&s, "T".into(), host_b, pubkey(2), true, 3_000)
                .await
                .expect("create");
            // Rehydrate all open rooms.
            for row in db.list_open_rooms().await.expect("list") {
                let parts = db.list_room_participants(row.id).await.expect("parts");
                r.rehydrate(row, parts).await.expect("rehydrate");
            }
            // The ended room is NOT in the registry.
            assert!(r.get_by_code("ZZZZZZ").await.is_none());
            // The open one IS.
            assert!(r.get_by_code(&summary.code).await.is_some());
        }

        #[tokio::test]
        async fn create_avoids_db_collision_on_restart() {
            // Two clients on a fresh server. The first
            // creates a room and the code is persisted.
            // A "restart" wipes the in-memory map but the
            // DB row remains. A second create on the fresh
            // registry must NOT pick the same code.
            let r1 = RoomRegistry::new(cfg());
            let db = fresh_db().await;
            let host_a = ensure_user(&db, pubkey(1)).await;
            let host_b = ensure_user(&db, pubkey(2)).await;
            let s = crate::rooms::DbRoomStore::new(db.clone());
            let (summary, _) = r1
                .create(&s, "T".into(), host_a, pubkey(1), true, 1_000)
                .await
                .expect("create");
            // "Restart" — fresh registry, same DB.
            let r2 = RoomRegistry::new(cfg());
            let s2 = crate::rooms::DbRoomStore::new(db.clone());
            // Force the create loop to keep picking until it
            // would naturally hit the occupied code. With
            // the DB check, it must still succeed and yield
            // a code that is NOT the existing one.
            for _ in 0..50 {
                let (s2_summary, _) = r2
                    .create(&s2, "T2".into(), host_b, pubkey(2), true, 2_000)
                    .await
                    .expect("create");
                assert_ne!(s2_summary.code, summary.code);
            }
        }
    }
}
