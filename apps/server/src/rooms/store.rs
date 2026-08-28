//! Persistence seam for [`super::registry::RoomRegistry`].
//!
//! Every room mutation in the registry writes to SQLite
//! before the in-memory state is published. P2-T05 introduces
//! a [`RoomStore`] trait so the registry methods can be
//! unit-tested without a database (via [`NoopRoomStore`])
//! and so the production wiring can use a [`DbRoomStore`]
//! wrapping [`crate::db::Db`].
//!
//! All methods are `async` because every real implementation
//! performs a SQL round-trip. A failure is logged by the
//! caller; the registry rolls back the in-memory state so
//! the DB and the runtime stay in sync (the canonical
//! ordering from the P2-T05 spec is "DB first, then
//! in-memory, then broadcast").
//!
//! The single-writer contract is owned by [`crate::db::Db`].
//! The registry's `write_lock` is internal to `Db`; this
//! trait intentionally does not expose a mutex to keep the
//! abstraction leak-free.

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use tracing::warn;
use uuid::Uuid;

use crate::db::Db;

/// The set of persistence operations the registry needs.
/// Every method is idempotent enough that re-issuing it on
/// a transient DB failure will not corrupt state.
#[async_trait]
#[allow(clippy::too_many_arguments)]
pub trait RoomStore: Send + Sync {
    /// Insert a new `rooms` row.
    async fn insert_room(
        &self,
        id: Uuid,
        code: &str,
        title: &str,
        host_user_id: Uuid,
        host_pubkey: &[u8; 32],
        host_migration_enabled: bool,
        created_ms: i64,
    ) -> Result<(), String>;

    /// Upsert a participant row. Idempotent on
    /// `(room_id, user_id)`.
    async fn add_room_participant(
        &self,
        room_id: Uuid,
        user_id: Uuid,
        pubkey: &[u8; 32],
        display_name: &str,
        is_host: bool,
        joined_ms: i64,
        cap_set: u32,
    ) -> Result<(), String>;

    /// Update a participant's status (and `left_ms` if the
    /// new status is `"left"`).
    async fn update_participant_status(
        &self,
        room_id: Uuid,
        user_id: Uuid,
        status: &str,
        last_seen_ms: i64,
    ) -> Result<(), String>;

    /// Mark a room ended.
    async fn end_room(&self, room_id: Uuid, ended_ms: i64) -> Result<(), String>;

    /// Set or clear the host-disconnect deadline.
    async fn set_host_disconnect_deadline(
        &self,
        room_id: Uuid,
        deadline_ms: Option<i64>,
    ) -> Result<(), String>;

    /// Check whether a room with the given code already
    /// exists in durable storage. Used by the create-time
    /// collision loop to close the race where a concurrent
    /// restart, or a concurrent `create` whose in-memory
    /// row has not yet been observed, occupies the code.
    async fn room_code_taken(&self, code: &str) -> Result<bool, String>;
}

/// Production store. Thin wrapper over [`Db`].
pub struct DbRoomStore {
    db: Db,
}

impl DbRoomStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
    pub fn db(&self) -> &Db {
        &self.db
    }
}

#[async_trait]
impl RoomStore for DbRoomStore {
    async fn insert_room(
        &self,
        id: Uuid,
        code: &str,
        title: &str,
        host_user_id: Uuid,
        host_pubkey: &[u8; 32],
        host_migration_enabled: bool,
        created_ms: i64,
    ) -> Result<(), String> {
        self.db
            .insert_room(
                id,
                code,
                title,
                host_user_id,
                host_pubkey,
                host_migration_enabled,
                created_ms,
            )
            .await
            .map_err(|e| {
                warn!(error = %e, "locast-server room store insert_room failed");
                e.to_string()
            })
    }

    async fn add_room_participant(
        &self,
        room_id: Uuid,
        user_id: Uuid,
        pubkey: &[u8; 32],
        display_name: &str,
        is_host: bool,
        joined_ms: i64,
        cap_set: u32,
    ) -> Result<(), String> {
        self.db
            .add_room_participant(
                room_id,
                user_id,
                pubkey,
                display_name,
                is_host,
                joined_ms,
                cap_set,
            )
            .await
            .map_err(|e| {
                warn!(error = %e, "locast-server room store add_room_participant failed");
                e.to_string()
            })
    }

    async fn update_participant_status(
        &self,
        room_id: Uuid,
        user_id: Uuid,
        status: &str,
        last_seen_ms: i64,
    ) -> Result<(), String> {
        self.db
            .update_participant_status(room_id, user_id, status, last_seen_ms)
            .await
            .map_err(|e| {
                warn!(error = %e, "locast-server room store update_participant_status failed");
                e.to_string()
            })
    }

    async fn end_room(&self, room_id: Uuid, ended_ms: i64) -> Result<(), String> {
        self.db.end_room(room_id, ended_ms).await.map_err(|e| {
            warn!(error = %e, "locast-server room store end_room failed");
            e.to_string()
        })
    }

    async fn set_host_disconnect_deadline(
        &self,
        room_id: Uuid,
        deadline_ms: Option<i64>,
    ) -> Result<(), String> {
        self.db
            .set_host_disconnect_deadline(room_id, deadline_ms)
            .await
            .map_err(|e| {
                warn!(error = %e, "locast-server room store set_host_disconnect_deadline failed");
                e.to_string()
            })
    }

    async fn room_code_taken(&self, code: &str) -> Result<bool, String> {
        self.db
            .get_room_by_code(code)
            .await
            .map(|r| r.is_some())
            .map_err(|e| {
                warn!(error = %e, "locast-server room store room_code_taken failed");
                e.to_string()
            })
    }
}

/// Test store. Every operation succeeds without touching
/// any storage. The P2-T04 registry tests use this to keep
/// the unit-test surface free of `Db` plumbing.
pub struct NoopRoomStore;

#[async_trait]
impl RoomStore for NoopRoomStore {
    async fn insert_room(
        &self,
        _id: Uuid,
        _code: &str,
        _title: &str,
        _host_user_id: Uuid,
        _host_pubkey: &[u8; 32],
        _host_migration_enabled: bool,
        _created_ms: i64,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn add_room_participant(
        &self,
        _room_id: Uuid,
        _user_id: Uuid,
        _pubkey: &[u8; 32],
        _display_name: &str,
        _is_host: bool,
        _joined_ms: i64,
        _cap_set: u32,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn update_participant_status(
        &self,
        _room_id: Uuid,
        _user_id: Uuid,
        _status: &str,
        _last_seen_ms: i64,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn end_room(&self, _room_id: Uuid, _ended_ms: i64) -> Result<(), String> {
        Ok(())
    }

    async fn set_host_disconnect_deadline(
        &self,
        _room_id: Uuid,
        _deadline_ms: Option<i64>,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn room_code_taken(&self, _code: &str) -> Result<bool, String> {
        Ok(false)
    }
}

/// Convenience alias for code that holds a `RoomStore`
/// behind an `Arc`.
pub type SharedRoomStore = Arc<dyn RoomStore>;
