//! SQLite-backed persistence for the signaling server.
//!
//! P2-T02 introduces the schema. The crate owns a single
//! `sqlx::SqlitePool`; queries are issued from the `auth/` and
//! `ws/` modules via the [`Db`] handle. The schema lives in
//! `apps/server/migrations/0001_init.sql` and is applied at
//! startup via `sqlx::migrate!()`.

#![forbid(unsafe_code)]
#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;
use thiserror::Error;
use tracing::info;
use uuid::Uuid;

use crate::config::Config;

/// Pool size. Per the architecture (`docs/ARCHITECTURE.md` §7) the
/// single-writer constraint is enforced by the `journal_mode = WAL`
/// pragma plus a single writer under a `tokio::sync::Mutex`; the
/// pool is sized for the reader side.
pub const POOL_SIZE: u32 = 8;

/// Busy timeout for SQLite lock contention.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Migrator for the schema in `apps/server/migrations/`.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Server-side storage handle. Cheap to clone (the pool is internally
/// `Arc`-shared). Holds a `tokio::sync::Mutex` that serializes the
/// single-writer path described in `docs/ARCHITECTURE.md` section 7
/// (one writer at a time); the pool is used only for the reader side.
#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
    /// Serializes the single-writer path. The mutex is held only
    /// for the duration of a `upsert_user` / `insert_bearer` /
    /// `revoke_bearer` call; the lock is never held across the
    /// WS send / receive boundary.
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Errors raised by [`Db::open`].
#[derive(Debug, Error)]
pub enum DbOpenError {
    #[error("invalid database url: {0}")]
    InvalidUrl(String),

    #[error("database I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlx pool error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// Information returned by [`Db::validate_bearer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BearerInfo {
    pub user_id: Uuid,
    pub pubkey: [u8; 32],
}

impl Db {
    /// Open the database, set the architecture's PRAGMAs, and run
    /// the embedded migrations.
    pub async fn open(config: &Config) -> Result<Self, DbOpenError> {
        let opts = SqliteConnectOptions::from_str(&config.database_url)
            .map_err(|e| DbOpenError::InvalidUrl(format!("{e}")))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(BUSY_TIMEOUT)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(POOL_SIZE)
            .acquire_timeout(BUSY_TIMEOUT)
            .connect_with(opts)
            .await?;

        if let Some(parent) = db_parent(&config.database_url) {
            std::fs::create_dir_all(parent)?;
        }

        MIGRATOR.run(&pool).await?;
        info!(url = %redact_url(&config.database_url), "locast-server database ready");

        Ok(Self {
            pool,
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Open an in-memory database and run migrations. Used by tests.
    pub async fn open_in_memory() -> Result<Self, DbOpenError> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(|e| DbOpenError::InvalidUrl(format!("{e}")))?
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(BUSY_TIMEOUT)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(POOL_SIZE)
            .acquire_timeout(BUSY_TIMEOUT)
            .connect_with(opts)
            .await?;

        MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// The pool. Used by background tasks (e.g. bearer cleanup)
    /// that need direct access.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Look up an existing user by pubkey, or insert a new row and
    /// mint a server-assigned UUID v7 (`user_id`). Updates
    /// `last_seen` regardless of which path was taken.
    ///
    /// Serialized via the single-writer `write_lock` to avoid
    /// `SQLITE_BUSY` deadlocks on concurrent auth attempts.
    pub async fn upsert_user(&self, pubkey: &[u8; 32]) -> Result<Uuid, sqlx::Error> {
        let _g = self.write_lock.lock().await;
        let now_ms = now_ms();
        let mut tx = self.pool.begin().await?;

        let existing: Option<(String,)> =
            sqlx::query_as("SELECT user_id FROM user_identities WHERE pubkey = ?1")
                .bind(&pubkey[..])
                .fetch_optional(&mut *tx)
                .await?;

        let user_id = match existing {
            Some((id,)) => {
                sqlx::query("UPDATE user_identities SET last_seen = ?1 WHERE user_id = ?2")
                    .bind(now_ms)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                Uuid::parse_str(&id).expect("stored user_id is a valid UUID")
            }
            None => {
                let new_id = Uuid::now_v7();
                sqlx::query(
                    "INSERT INTO user_identities (user_id, pubkey, first_seen, last_seen) \
                     VALUES (?1, ?2, ?3, ?3)",
                )
                .bind(new_id.to_string())
                .bind(&pubkey[..])
                .bind(now_ms)
                .execute(&mut *tx)
                .await?;
                new_id
            }
        };

        tx.commit().await?;
        Ok(user_id)
    }

    /// Insert a fresh bearer token row. `token_hash` is the
    /// SHA-256 of the 32-byte plaintext token; the plaintext is
    /// never stored.
    pub async fn insert_bearer(
        &self,
        user_id: Uuid,
        token_hash: [u8; 32],
        expires_ms: i64,
    ) -> Result<(), sqlx::Error> {
        let _g = self.write_lock.lock().await;
        // We also need the pubkey to round-trip the bearer through
        // a single SELECT in `validate_bearer`. Look it up.
        let pubkey: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT pubkey FROM user_identities WHERE user_id = ?1")
                .bind(user_id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let pubkey = pubkey.ok_or_else(|| sqlx::Error::RowNotFound)?;
        let mut pubkey_arr = [0u8; 32];
        if pubkey.0.len() != 32 {
            return Err(sqlx::Error::ColumnDecode {
                index: "pubkey".to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("pubkey row is {} bytes, expected 32", pubkey.0.len()),
                )),
            });
        }
        pubkey_arr.copy_from_slice(&pubkey.0);

        sqlx::query(
            "INSERT INTO bearer_tokens (token_hash, user_id, pubkey, expires_ms, created_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&token_hash[..])
        .bind(user_id.to_string())
        .bind(&pubkey_arr[..])
        .bind(expires_ms)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Validate a bearer token. Returns `Some(BearerInfo)` if the
    /// token is present and not yet expired, `None` otherwise.
    pub async fn validate_bearer(
        &self,
        token_hash: &[u8; 32],
    ) -> Result<Option<BearerInfo>, sqlx::Error> {
        let now = now_ms();
        let row: Option<(String, Vec<u8>, i64)> = sqlx::query_as(
            "SELECT user_id, pubkey, expires_ms FROM bearer_tokens \
             WHERE token_hash = ?1",
        )
        .bind(&token_hash[..])
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some((user_id, pubkey, expires_ms)) if expires_ms > now => {
                let uuid = Uuid::parse_str(&user_id).map_err(|e| sqlx::Error::ColumnDecode {
                    index: "user_id".to_string(),
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid user_id uuid: {e}"),
                    )),
                })?;
                if pubkey.len() != 32 {
                    return Err(sqlx::Error::ColumnDecode {
                        index: "pubkey".to_string(),
                        source: Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("pubkey row is {} bytes, expected 32", pubkey.len()),
                        )),
                    });
                }
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&pubkey);
                Ok(Some(BearerInfo {
                    user_id: uuid,
                    pubkey: arr,
                }))
            }
            _ => Ok(None),
        }
    }

    /// Revoke a single bearer. Used by tests and by the future
    /// AUTH_REFRESH flow.
    pub async fn revoke_bearer(&self, token_hash: &[u8; 32]) -> Result<(), sqlx::Error> {
        let _g = self.write_lock.lock().await;
        sqlx::query("DELETE FROM bearer_tokens WHERE token_hash = ?1")
            .bind(&token_hash[..])
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Purge all bearers that have already expired. The
    /// background task in [`spawn_bearer_cleanup`] calls this
    /// on an interval.
    pub async fn purge_expired_bearers(&self) -> Result<u64, sqlx::Error> {
        let _g = self.write_lock.lock().await;
        let now = now_ms();
        let res = sqlx::query("DELETE FROM bearer_tokens WHERE expires_ms <= ?1")
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    // ------------------------------------------------------------------
    // P2-T04: room + room_participants persistence.
    // ------------------------------------------------------------------

    /// Insert a new `rooms` row.
    pub async fn insert_room(
        &self,
        id: Uuid,
        code: &str,
        title: &str,
        host_user_id: Uuid,
        host_pubkey: &[u8; 32],
        host_migration_enabled: bool,
        created_ms: i64,
    ) -> Result<(), sqlx::Error> {
        let _g = self.write_lock.lock().await;
        sqlx::query(
            "INSERT INTO rooms (id, code, title, host_user_id, host_pubkey, \
             host_migration_enabled, state, created_ms, last_activity_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?7)",
        )
        .bind(id.to_string())
        .bind(code)
        .bind(title)
        .bind(host_user_id.to_string())
        .bind(&host_pubkey[..])
        .bind(if host_migration_enabled { 1i64 } else { 0i64 })
        .bind(created_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Add a participant row. Idempotent on `(room_id, user_id)`;
    /// updates `display_name`, `is_host`, `joined_ms`, `last_seen_ms`,
    /// `status` if the row already exists.
    pub async fn add_room_participant(
        &self,
        room_id: Uuid,
        user_id: Uuid,
        pubkey: &[u8; 32],
        display_name: &str,
        is_host: bool,
        joined_ms: i64,
        cap_set: u32,
    ) -> Result<(), sqlx::Error> {
        let _g = self.write_lock.lock().await;
        sqlx::query(
            "INSERT INTO room_participants (room_id, user_id, pubkey, display_name, \
             is_host, joined_ms, last_seen_ms, status, cap_set) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 'connected', ?7) \
             ON CONFLICT(room_id, user_id) DO UPDATE SET \
             display_name = excluded.display_name, \
             is_host = excluded.is_host, \
             joined_ms = excluded.joined_ms, \
             last_seen_ms = excluded.last_seen_ms, \
             pubkey = excluded.pubkey, \
             cap_set = excluded.cap_set, \
             status = 'connected', \
             left_ms = NULL",
        )
        .bind(room_id.to_string())
        .bind(user_id.to_string())
        .bind(&pubkey[..])
        .bind(display_name)
        .bind(if is_host { 1i64 } else { 0i64 })
        .bind(joined_ms)
        .bind(cap_set as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update a participant's status / `last_seen_ms`. Used by
    /// the stale-cleanup task and by `on_connection_lost`.
    pub async fn update_participant_status(
        &self,
        room_id: Uuid,
        user_id: Uuid,
        status: &str,
        last_seen_ms: i64,
    ) -> Result<(), sqlx::Error> {
        let _g = self.write_lock.lock().await;
        sqlx::query(
            "UPDATE room_participants SET status = ?3, last_seen_ms = ?4, \
             left_ms = CASE WHEN ?3 = 'left' THEN ?4 ELSE left_ms END \
             WHERE room_id = ?1 AND user_id = ?2",
        )
        .bind(room_id.to_string())
        .bind(user_id.to_string())
        .bind(status)
        .bind(last_seen_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List the participants in a room. The order is by
    /// `joined_ms ASC, user_id ASC` to match the host-election
    /// rule.
    pub async fn list_room_participants(
        &self,
        room_id: Uuid,
    ) -> Result<Vec<RoomParticipantRow>, sqlx::Error> {
        let rows: Vec<(String, Vec<u8>, String, i64, i64, Option<i64>, String, i64)> =
            sqlx::query_as(
                "SELECT user_id, pubkey, display_name, is_host, joined_ms, left_ms, \
                 status, cap_set FROM room_participants \
                 WHERE room_id = ?1 \
                 ORDER BY joined_ms ASC, user_id ASC",
            )
            .bind(room_id.to_string())
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (uid_s, pubkey, display_name, is_host, joined_ms, left_ms, status, cap_set) in rows {
            let user_id = Uuid::parse_str(&uid_s).map_err(|e| sqlx::Error::ColumnDecode {
                index: "user_id".to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid user_id: {e}"),
                )),
            })?;
            out.push(RoomParticipantRow {
                user_id,
                pubkey,
                display_name,
                is_host: is_host != 0,
                joined_ms,
                left_ms,
                status,
                cap_set: cap_set as u32,
            });
        }
        Ok(out)
    }

    /// Mark a room ended. The room row stays in the table
    /// for audit; the in-memory registry drops it eagerly.
    pub async fn end_room(&self, room_id: Uuid, ended_ms: i64) -> Result<(), sqlx::Error> {
        let _g = self.write_lock.lock().await;
        sqlx::query(
            "UPDATE rooms SET state = 'ended', ended_ms = ?2, last_activity_ms = ?2 \
             WHERE id = ?1",
        )
        .bind(room_id.to_string())
        .bind(ended_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set the host-disconnect deadline on a room row.
    pub async fn set_host_disconnect_deadline(
        &self,
        room_id: Uuid,
        deadline_ms: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        let _g = self.write_lock.lock().await;
        sqlx::query(
            "UPDATE rooms SET host_disconnect_deadline_ms = ?2, last_activity_ms = ?3 \
             WHERE id = ?1",
        )
        .bind(room_id.to_string())
        .bind(deadline_ms)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Clear the host-disconnect deadline (host reconnected
    /// or migration completed).
    pub async fn clear_host_disconnect_deadline(&self, room_id: Uuid) -> Result<(), sqlx::Error> {
        self.set_host_disconnect_deadline(room_id, None).await
    }

    /// Bump the room's `last_activity_ms`. Cheap; called on
    /// every inbound authenticated message that touches the
    /// registry.
    pub async fn update_room_last_activity(
        &self,
        room_id: Uuid,
        last_activity_ms: i64,
    ) -> Result<(), sqlx::Error> {
        let _g = self.write_lock.lock().await;
        sqlx::query("UPDATE rooms SET last_activity_ms = ?2 WHERE id = ?1")
            .bind(room_id.to_string())
            .bind(last_activity_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Look up an open room by its 6-char code.
    pub async fn get_room_by_code(&self, code: &str) -> Result<Option<RoomRow>, sqlx::Error> {
        let row: Option<(
            String,
            String,
            String,
            String,
            Vec<u8>,
            i64,
            String,
            Option<i64>,
            i64,
            Option<i64>,
            i64,
        )> = sqlx::query_as(
            "SELECT id, code, title, host_user_id, host_pubkey, host_migration_enabled, \
             state, host_disconnect_deadline_ms, created_ms, ended_ms, last_activity_ms \
             FROM rooms WHERE code = ?1",
        )
        .bind(code)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some((
                id,
                code,
                title,
                host_user_id,
                host_pubkey,
                host_migration_enabled,
                state,
                host_disconnect_deadline_ms,
                created_ms,
                ended_ms,
                last_activity_ms,
            )) => {
                let id = Uuid::parse_str(&id).map_err(|e| sqlx::Error::ColumnDecode {
                    index: "id".to_string(),
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid room id: {e}"),
                    )),
                })?;
                let host_user_id =
                    Uuid::parse_str(&host_user_id).map_err(|e| sqlx::Error::ColumnDecode {
                        index: "host_user_id".to_string(),
                        source: Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("invalid host user_id: {e}"),
                        )),
                    })?;
                Ok(Some(RoomRow {
                    id,
                    code,
                    title,
                    host_user_id,
                    host_pubkey,
                    host_migration_enabled: host_migration_enabled != 0,
                    state,
                    host_disconnect_deadline_ms,
                    created_ms,
                    ended_ms,
                    last_activity_ms,
                }))
            }
        }
    }

    /// Look up a room by id.
    pub async fn get_room_by_id(&self, id: Uuid) -> Result<Option<RoomRow>, sqlx::Error> {
        let row: Option<(
            String,
            String,
            String,
            Vec<u8>,
            i64,
            String,
            Option<i64>,
            i64,
            Option<i64>,
            i64,
        )> = sqlx::query_as(
            "SELECT code, title, host_user_id, host_pubkey, host_migration_enabled, \
             state, host_disconnect_deadline_ms, created_ms, ended_ms, last_activity_ms \
             FROM rooms WHERE id = ?1",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let (
            code,
            title,
            host_user_id,
            host_pubkey,
            host_migration_enabled,
            state,
            host_disconnect_deadline_ms,
            created_ms,
            ended_ms,
            last_activity_ms,
        ) = match row {
            None => return Ok(None),
            Some(t) => t,
        };
        let host_user_id =
            Uuid::parse_str(&host_user_id).map_err(|e| sqlx::Error::ColumnDecode {
                index: "host_user_id".to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid host user_id: {e}"),
                )),
            })?;
        Ok(Some(RoomRow {
            id,
            code,
            title,
            host_user_id,
            host_pubkey,
            host_migration_enabled: host_migration_enabled != 0,
            state,
            host_disconnect_deadline_ms,
            created_ms,
            ended_ms,
            last_activity_ms,
        }))
    }

    /// List all open rooms. Used at server startup to
    /// re-hydrate the in-memory registry.
    pub async fn list_open_rooms(&self) -> Result<Vec<RoomRow>, sqlx::Error> {
        let rows: Vec<(
            String,
            String,
            String,
            String,
            Vec<u8>,
            i64,
            String,
            Option<i64>,
            i64,
            Option<i64>,
            i64,
        )> = sqlx::query_as(
            "SELECT id, code, title, host_user_id, host_pubkey, host_migration_enabled, \
             state, host_disconnect_deadline_ms, created_ms, ended_ms, last_activity_ms \
             FROM rooms WHERE state = 'open' ORDER BY created_ms ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (
            id,
            code,
            title,
            host_user_id,
            host_pubkey,
            host_migration_enabled,
            state,
            host_disconnect_deadline_ms,
            created_ms,
            ended_ms,
            last_activity_ms,
        ) in rows
        {
            let id = Uuid::parse_str(&id).map_err(|e| sqlx::Error::ColumnDecode {
                index: "id".to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid room id: {e}"),
                )),
            })?;
            let host_user_id =
                Uuid::parse_str(&host_user_id).map_err(|e| sqlx::Error::ColumnDecode {
                    index: "host_user_id".to_string(),
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid host user_id: {e}"),
                    )),
                })?;
            out.push(RoomRow {
                id,
                code,
                title,
                host_user_id,
                host_pubkey,
                host_migration_enabled: host_migration_enabled != 0,
                state,
                host_disconnect_deadline_ms,
                created_ms,
                ended_ms,
                last_activity_ms,
            });
        }
        Ok(out)
    }

    /// Drop participants whose `last_seen_ms` is older than
    /// the cutoff. Returns the `(room_id, user_id)` pairs that
    /// were removed so the caller can broadcast a
    /// `PARTICIPANT_LEFT` per pair.
    pub async fn purge_stale_participants(
        &self,
        cutoff_ms: i64,
    ) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
        let _g = self.write_lock.lock().await;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT room_id, user_id FROM room_participants \
             WHERE is_host = 0 AND status != 'left' AND last_seen_ms < ?1",
        )
        .bind(cutoff_ms)
        .fetch_all(&self.pool)
        .await?;
        sqlx::query(
            "UPDATE room_participants SET status = 'left', left_ms = ?2 \
             WHERE is_host = 0 AND status != 'left' AND last_seen_ms < ?1",
        )
        .bind(cutoff_ms)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (rid, uid) in rows {
            let rid = Uuid::parse_str(&rid).map_err(|e| sqlx::Error::ColumnDecode {
                index: "room_id".to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid room_id: {e}"),
                )),
            })?;
            let uid = Uuid::parse_str(&uid).map_err(|e| sqlx::Error::ColumnDecode {
                index: "user_id".to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid user_id: {e}"),
                )),
            })?;
            out.push((rid, uid));
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // P3-T03: room_manifests persistence.
    // ------------------------------------------------------------------

    /// Insert a fresh `room_manifests` row. `version` is
    /// per-room monotonic (max+1) and is the caller's
    /// responsibility to compute. The store enforces the
    /// `UNIQUE (room_id, version)` constraint; a duplicate
    /// insert is a caller bug and surfaces as a
    /// `sqlx::Error::Database` with the unique-violation
    /// message.
    pub async fn insert_room_manifest(
        &self,
        id: Uuid,
        room_id: Uuid,
        version: i64,
        created_at: i64,
        manifest_json: &str,
        manifest_hash: &[u8; 32],
        host_user_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        let _g = self.write_lock.lock().await;
        sqlx::query(
            "INSERT INTO room_manifests \
             (id, room_id, version, created_at, manifest_json, manifest_hash, host_user_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(id.to_string())
        .bind(room_id.to_string())
        .bind(version)
        .bind(created_at)
        .bind(manifest_json)
        .bind(&manifest_hash[..])
        .bind(host_user_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Look up the latest `room_manifests` row for `room_id`.
    /// Returns the row in the same shape as
    /// [`insert_room_manifest`] takes so the caller can pass
    /// the values straight back to a broadcast / persistence
    /// path. `None` if no manifest has been published for the
    /// room yet.
    pub async fn get_latest_room_manifest(
        &self,
        room_id: Uuid,
    ) -> Result<Option<RoomManifestRow>, sqlx::Error> {
        let row: Option<(String, i64, i64, String, Vec<u8>, String)> = sqlx::query_as(
            "SELECT id, version, created_at, manifest_json, manifest_hash, host_user_id \
             FROM room_manifests WHERE room_id = ?1 \
             ORDER BY version DESC LIMIT 1",
        )
        .bind(room_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let (id_s, version, created_at, manifest_json, manifest_hash, host_user_id_s) = match row {
            None => return Ok(None),
            Some(t) => t,
        };
        let id = Uuid::parse_str(&id_s).map_err(|e| sqlx::Error::ColumnDecode {
            index: "id".to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid manifest id: {e}"),
            )),
        })?;
        let host_user_id =
            Uuid::parse_str(&host_user_id_s).map_err(|e| sqlx::Error::ColumnDecode {
                index: "host_user_id".to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid host_user_id: {e}"),
                )),
            })?;
        if manifest_hash.len() != 32 {
            return Err(sqlx::Error::ColumnDecode {
                index: "manifest_hash".to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "manifest_hash is {} bytes, expected 32",
                        manifest_hash.len()
                    ),
                )),
            });
        }
        let mut hash_arr = [0u8; 32];
        hash_arr.copy_from_slice(&manifest_hash);
        Ok(Some(RoomManifestRow {
            id,
            version,
            created_at,
            manifest_json,
            manifest_hash: hash_arr,
            host_user_id,
        }))
    }
}

/// A row from the `rooms` table.
#[derive(Debug, Clone)]
pub struct RoomRow {
    pub id: Uuid,
    pub code: String,
    pub title: String,
    pub host_user_id: Uuid,
    pub host_pubkey: Vec<u8>,
    pub host_migration_enabled: bool,
    pub state: String,
    pub host_disconnect_deadline_ms: Option<i64>,
    pub created_ms: i64,
    pub ended_ms: Option<i64>,
    pub last_activity_ms: i64,
}

/// A row from the `room_participants` table.
#[derive(Debug, Clone)]
pub struct RoomParticipantRow {
    pub user_id: Uuid,
    pub pubkey: Vec<u8>,
    pub display_name: String,
    pub is_host: bool,
    pub joined_ms: i64,
    pub left_ms: Option<i64>,
    pub status: String,
    pub cap_set: u32,
}

/// A row from the `room_manifests` table.
#[derive(Debug, Clone)]
pub struct RoomManifestRow {
    pub id: Uuid,
    pub version: i64,
    pub created_at: i64,
    pub manifest_json: String,
    pub manifest_hash: [u8; 32],
    pub host_user_id: Uuid,
}

/// Spawn a background task that periodically purges expired
/// bearers. The task lives for the lifetime of the server.
pub fn spawn_bearer_cleanup(db: Db, interval: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match db.purge_expired_bearers().await {
                Ok(0) => {}
                Ok(n) => tracing::debug!(purged = n, "locast-server bearer cleanup"),
                Err(e) => tracing::warn!(error = %e, "locast-server bearer cleanup failed"),
            }
        }
    });
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Return the parent directory of the SQLite file referenced by
/// `url`, if any. The url is one of:
///
/// - `sqlite::memory:` -> no parent
/// - `sqlite://./relative.db` or `sqlite:///abs/path.db` -> parent
/// - `sqlite://:memory:` -> no parent
fn db_parent(url: &str) -> Option<&Path> {
    if url.contains(":memory:") {
        return None;
    }
    let path_str = url.strip_prefix("sqlite://").unwrap_or(url);
    let path = Path::new(path_str);
    path.parent().filter(|p| !p.as_os_str().is_empty())
}

/// Best-effort redactor for the database URL in log lines. Strips
/// the file path / scheme and leaves only the suffix.
fn redact_url(url: &str) -> String {
    if let Some(idx) = url.rfind(['/', ':']) {
        if url.contains("memory") {
            return "sqlite::memory:".to_string();
        }
        let prefix = &url[..=idx];
        format!("{prefix}…")
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::test_support::null_log_filter;
    use rand::RngCore;

    async fn fresh_db() -> Db {
        Db::open_in_memory().await.expect("open in-memory db")
    }

    fn random_pubkey() -> [u8; 32] {
        let mut arr = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut arr);
        arr
    }

    #[tokio::test]
    async fn upsert_user_assigns_uuid_v7_and_is_stable() {
        let db = fresh_db().await;
        let pk = random_pubkey();
        let a = db.upsert_user(&pk).await.expect("upsert");
        let b = db.upsert_user(&pk).await.expect("upsert");
        assert_eq!(a, b);
        // Version nibble: 7
        assert_eq!(a.get_version_num(), 7);
    }

    #[tokio::test]
    async fn different_pubkeys_get_different_user_ids() {
        let db = fresh_db().await;
        let a = db.upsert_user(&random_pubkey()).await.expect("upsert a");
        let b = db.upsert_user(&random_pubkey()).await.expect("upsert b");
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn bearer_lifecycle() {
        let db = fresh_db().await;
        let pk = random_pubkey();
        let user_id = db.upsert_user(&pk).await.expect("upsert");

        let hash = [1u8; 32];
        let exp = now_ms() + 60_000;
        db.insert_bearer(user_id, hash, exp)
            .await
            .expect("insert bearer");

        let info = db
            .validate_bearer(&hash)
            .await
            .expect("validate")
            .expect("present");
        assert_eq!(info.user_id, user_id);
        assert_eq!(info.pubkey, pk);

        db.revoke_bearer(&hash).await.expect("revoke");
        assert!(db.validate_bearer(&hash).await.expect("validate").is_none());
    }

    #[tokio::test]
    async fn expired_bearer_returns_none() {
        let db = fresh_db().await;
        let pk = random_pubkey();
        let user_id = db.upsert_user(&pk).await.expect("upsert");

        let hash = [2u8; 32];
        db.insert_bearer(user_id, hash, now_ms() - 1)
            .await
            .expect("insert bearer");
        assert!(db.validate_bearer(&hash).await.expect("validate").is_none());
    }

    #[tokio::test]
    async fn purge_expired_bearers_removes_only_expired() {
        let db = fresh_db().await;
        let pk = random_pubkey();
        let user_id = db.upsert_user(&pk).await.expect("upsert");

        let stale = [3u8; 32];
        let live = [4u8; 32];
        db.insert_bearer(user_id, stale, now_ms() - 1)
            .await
            .expect("insert stale");
        db.insert_bearer(user_id, live, now_ms() + 60_000)
            .await
            .expect("insert live");

        let purged = db.purge_expired_bearers().await.expect("purge");
        assert_eq!(purged, 1);
        assert!(db.validate_bearer(&stale).await.expect("v").is_none());
        assert!(db.validate_bearer(&live).await.expect("v").is_some());
    }

    #[tokio::test]
    async fn open_from_config_in_memory() {
        let cfg = Config {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            log_filter: null_log_filter(),
            database_url: "sqlite::memory:".to_string(),
            bearer_ttl_seconds: 900,
            challenge_ttl_ms: 30_000,
            max_frame_bytes: 1024,
            handshake_timeout_ms: 15_000,
            rate_msgs_per_sec: 100,
            rate_msg_burst: 200,
            rate_bytes_per_sec: 1_000_000,
            rate_bytes_burst: 2_000_000,
            room_code_length: 6,
            room_code_alphabet: "ABCDEFGHJKLMNPQRSTUVWXYZ23456789".to_string(),
            room_max_participants: 8,
            host_disconnect_grace_ms: 30_000,
            room_create_max_collisions: 5,
            participant_stale_after_ms: 300_000,
        };
        let db = Db::open(&cfg).await.expect("open from config");
        // A trivial query proves the schema is applied.
        let _: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM user_identities")
            .fetch_one(db.pool())
            .await
            .expect("count");
    }
}
